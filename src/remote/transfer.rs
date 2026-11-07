//! Provisioning stream: one gzip member containing a big-endian u32 JSON
//! length, that JSON metadata, then file bodies in entry order. File sizes
//! live in the metadata. No filesystem changes are applied until EOF and the
//! gzip checksum have been verified.

use std::fs::File;
use std::io::{self, BufRead, Read, Write};

use anyhow::{Result, ensure};
use flate2::{Compression, bufread::GzDecoder, write::GzEncoder};
use serde::{Serialize, de::DeserializeOwned};

use super::protocol::{FileEntry, FileEntryKind};

/// Combined uncompressed size of copies and missing symlink sources.
pub const TRANSFER_LIMIT: u64 = 64 * 1024 * 1024;
const METADATA_LIMIT: usize = 8 * 1024 * 1024;

pub fn encoder<W: Write, T: Serialize>(writer: W, metadata: &T) -> Result<GzEncoder<W>> {
    let json = serde_json::to_vec(metadata)?;
    ensure!(
        json.len() <= METADATA_LIMIT,
        "transfer metadata exceeds 8 MiB"
    );
    let mut writer = GzEncoder::new(writer, Compression::fast());
    writer.write_all(&(json.len() as u32).to_be_bytes())?;
    writer.write_all(&json)?;
    Ok(writer)
}

pub fn metadata<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(length <= METADATA_LIMIT, "transfer metadata exceeds 8 MiB");
    let mut json = vec![0; length];
    reader.read_exact(&mut json)?;
    Ok(serde_json::from_slice(&json)?)
}

pub fn write_files<'a>(
    writer: &mut impl Write,
    entries: impl Iterator<Item = &'a FileEntry>,
) -> Result<()> {
    let mut total = 0u64;
    for entry in entries {
        if let FileEntryKind::File { size, source, .. } = &entry.kind {
            ensure!(
                *size <= TRANSFER_LIMIT - total,
                "file transfer exceeds 64 MiB"
            );
            total += size;
            let mut file = File::open(source)?;
            ensure!(
                file.metadata()?.len() == *size,
                "file changed during transfer: {}",
                source.display()
            );
            ensure!(
                io::copy(&mut (&mut file).take(*size), writer)? == *size,
                "file truncated during transfer: {}",
                source.display()
            );
            ensure!(
                file.read(&mut [0])? == 0,
                "file grew during transfer: {}",
                source.display()
            );
        }
    }
    Ok(())
}

/// Bodies are staged under numeric names, never paths supplied by the sender.
/// The returned guard must outlive every use of the entries' source paths.
pub fn read_files<'a, R: BufRead>(
    reader: &mut GzDecoder<R>,
    entries: impl Iterator<Item = &'a mut FileEntry>,
) -> Result<tempfile::TempDir> {
    let stage = tempfile::tempdir()?;
    let mut total = 0u64;
    for (index, entry) in entries.enumerate() {
        super::node::provision::safe_relative(&entry.path)?;
        if let FileEntryKind::File { size, source, .. } = &mut entry.kind {
            ensure!(
                *size <= TRANSFER_LIMIT - total,
                "file transfer exceeds 64 MiB"
            );
            total += *size;
            *source = stage.path().join(index.to_string());
            let mut file = File::create(source)?;
            ensure!(
                io::copy(&mut (&mut *reader).take(*size), &mut file)? == *size,
                "truncated file body"
            );
        }
    }
    // Reading past the final body validates the gzip trailer, including CRC.
    ensure!(reader.read(&mut [0])? == 0, "unexpected file bytes");
    ensure!(
        reader.get_mut().fill_buf()?.is_empty(),
        "unexpected bytes after gzip stream"
    );
    Ok(stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: path.into(),
            kind: FileEntryKind::File {
                mode: 0o755,
                size,
                source: Default::default(),
            },
        }
    }

    fn stream(entries: &[FileEntry], bytes: &[u8]) -> Vec<u8> {
        let mut writer = encoder(Vec::new(), &entries).unwrap();
        writer.write_all(bytes).unwrap();
        writer.finish().unwrap()
    }

    fn decode(bytes: &[u8]) -> Result<(Vec<FileEntry>, tempfile::TempDir)> {
        let mut reader = GzDecoder::new(bytes);
        let mut entries: Vec<FileEntry> = metadata(&mut reader)?;
        let stage = read_files(&mut reader, entries.iter_mut())?;
        Ok((entries, stage))
    }

    #[test]
    fn binary_roundtrip_and_invalid_streams() {
        let source = tempfile::NamedTempFile::new().unwrap();
        let bytes: Vec<u8> = (0..=255).collect();
        std::fs::write(source.path(), &bytes).unwrap();
        let mut file = entry("binary", bytes.len() as u64);
        if let FileEntryKind::File { source: path, .. } = &mut file.kind {
            *path = source.path().to_path_buf();
        }
        let entries = [file, entry("empty", 0)];
        let mut writer = encoder(Vec::new(), &entries).unwrap();
        // The empty entry also needs an actual source on the sending side.
        write_files(&mut writer, entries[..1].iter()).unwrap();
        let valid = writer.finish().unwrap();
        let (decoded, _stage) = decode(&valid).unwrap();
        if let FileEntryKind::File { source, .. } = &decoded[0].kind {
            assert_eq!(std::fs::read(source).unwrap(), bytes);
        }
        assert_eq!(decoded[0].path, "binary");
        for end in [0, 8, valid.len() / 2, valid.len() - 1, valid.len() - 8] {
            assert!(decode(&valid[..end]).is_err());
        }
        let mut corrupt = valid.clone();
        let crc = corrupt.len() - 8;
        corrupt[crc] ^= 1;
        assert!(decode(&corrupt).is_err());
        let mut trailing = valid;
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        assert!(decode(&stream(&entries, &bytes[..128])).is_err());
        assert!(decode(&stream(&entries[..1], &[0; 257])).is_err());
        assert!(decode(&stream(&[entry("../escape", 0)], &[])).is_err());
        assert!(decode(&stream(&[entry("/absolute", 0)], &[])).is_err());
    }

    #[test]
    fn bound_file_bytes_and_metadata() {
        for size in [TRANSFER_LIMIT + 1, u64::MAX] {
            assert!(decode(&stream(&[entry("large", size)], &[])).is_err());
        }
        // The limit is shared by all entries, not a per-file allowance.
        let entries = [entry("a", TRANSFER_LIMIT), entry("b", 1)];
        let mut writer = encoder(Vec::new(), &entries).unwrap();
        io::copy(&mut io::repeat(0).take(TRANSFER_LIMIT), &mut writer).unwrap();
        writer.write_all(&[0]).unwrap();
        assert!(decode(&writer.finish().unwrap()).is_err());
        let entries = [entry("at-limit", TRANSFER_LIMIT)];
        let mut writer = encoder(Vec::new(), &entries).unwrap();
        io::copy(&mut io::repeat(0).take(TRANSFER_LIMIT), &mut writer).unwrap();
        assert!(decode(&writer.finish().unwrap()).is_ok());
        let mut length = ((METADATA_LIMIT + 1) as u32)
            .to_be_bytes()
            .as_slice()
            .to_owned();
        assert!(metadata::<serde_json::Value>(&mut length.as_slice()).is_err());
        length = vec![0, 0, 0, 1];
        assert!(metadata::<serde_json::Value>(&mut length.as_slice()).is_err());
    }

    #[test]
    fn refuse_changed_source_and_ignore_wire_source() {
        let source = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"longer").unwrap();
        let mut file = entry("a", 1);
        if let FileEntryKind::File { source: path, .. } = &mut file.kind {
            *path = source.path().to_path_buf();
        }
        assert!(write_files(&mut io::sink(), [&file].into_iter()).is_err());
        let entry: FileEntry = serde_json::from_value(serde_json::json!({
            "path": "a", "kind": "file", "mode": 420, "size": 1, "source": "/etc/passwd"
        }))
        .unwrap();
        if let FileEntryKind::File { source, .. } = entry.kind {
            assert!(source.as_os_str().is_empty());
        }
    }
}
