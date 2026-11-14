use std::path::Path;

#[cfg(target_os = "linux")]
pub fn filesystem_birth_time(path: &Path) -> Option<u64> {
    use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

    let metadata = statx(CWD, path, AtFlags::empty(), StatxFlags::BTIME).ok()?;
    birth_time_from_statx(
        StatxFlags::from_bits_retain(metadata.stx_mask),
        metadata.stx_btime.tv_sec,
        metadata.stx_btime.tv_nsec,
    )
}

#[cfg(target_os = "linux")]
fn birth_time_from_statx(
    mask: rustix::fs::StatxFlags,
    seconds: i64,
    nanoseconds: u32,
) -> Option<u64> {
    if !mask.contains(rustix::fs::StatxFlags::BTIME) || nanoseconds >= 1_000_000_000 {
        return None;
    }

    seconds.try_into().ok()
}

#[cfg(not(target_os = "linux"))]
pub fn filesystem_birth_time(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .created()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{birth_time_from_statx, filesystem_birth_time};
    use rustix::fs::StatxFlags;

    #[test]
    fn extracts_supported_birth_time() {
        assert_eq!(
            birth_time_from_statx(StatxFlags::BTIME, 1_700_000_000, 123_456_789),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn rejects_missing_birth_time() {
        assert_eq!(birth_time_from_statx(StatxFlags::empty(), 1, 0), None);
    }

    #[test]
    fn rejects_unrepresentable_birth_time() {
        assert_eq!(birth_time_from_statx(StatxFlags::BTIME, -1, 0), None);
        assert_eq!(
            birth_time_from_statx(StatxFlags::BTIME, 1, 1_000_000_000),
            None
        );
    }

    #[test]
    fn returns_none_when_statx_fails() {
        let tempdir = tempfile::tempdir().unwrap();
        assert_eq!(filesystem_birth_time(&tempdir.path().join("missing")), None);
    }
}
