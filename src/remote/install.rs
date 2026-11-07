//! Install the pinned node binary on a remote.
//!
//! The pinned name identifies its bytes (a release version or a content
//! hash), so an existing pinned binary that answers `node hello` is final.
//! New bytes are streamed into a private stage file in `bin/`, size-checked,
//! and run before an atomic rename gives them the pinned name; an
//! interrupted install leaves no file under the pinned name.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use super::RemoteSpec;
use super::protocol::{self, Hello, HelloFailure};
use super::transport::{Input, Output, Transport};

const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Exit status of the hello script when the pinned binary is absent.
const MISSING_STATUS: i32 = 100;
/// Exit status of the stage script when the upload was truncated.
const TRUNCATED_STATUS: i32 = 101;
/// Exit status of the stage script when the stage file cannot be written.
const STAGE_FAILED_STATUS: i32 = 102;

/// The binary this workmux expects on a remote.
pub struct Pin {
    /// File name under `bin/`.
    pub name: String,
    source: PinSource,
}

enum PinSource {
    Release,
    File(PathBuf),
}

impl Pin {
    pub fn for_spec(spec: &RemoteSpec) -> Result<Self> {
        let Some(path) = &spec.binary else {
            return Ok(Self {
                name: format!("workmux-{}", env!("CARGO_PKG_VERSION")),
                source: PinSource::Release,
            });
        };
        let name = dev_pin_name(path)
            .with_context(|| format!("remotes.{}.binary: {}", spec.name, path.display()))?;
        Ok(Self {
            name,
            source: PinSource::File(path.clone()),
        })
    }

    /// A binary already on the node under `name`, for callers that only run
    /// it (an execution keeps the binary that started it).
    pub fn installed(name: String) -> Self {
        Self {
            name,
            source: PinSource::Release,
        }
    }

    /// Path of the pinned binary on the remote, as shown to users.
    pub fn path(&self, spec: &RemoteSpec) -> String {
        format!("{}/bin/{}", spec.path.trim_end_matches('/'), self.name)
    }

    /// What to do when the pinned binary speaks another protocol.
    pub fn mismatch_hint(&self, spec: &RemoteSpec) -> String {
        match self.source {
            PinSource::Release => format!(
                "This workmux is not the v{} release; point remotes.{}.binary at a Linux \
                 build of it (see `just remote-dev-binary`)",
                env!("CARGO_PKG_VERSION"),
                spec.name
            ),
            PinSource::File(_) => format!(
                "Rebuild remotes.{}.binary from the same source as this workmux \
                 (see `just remote-dev-binary`)",
                spec.name
            ),
        }
    }

    fn mismatch_error(&self, spec: &RemoteSpec, hello: &Hello) -> anyhow::Error {
        anyhow!(
            "{}. {}",
            protocol::mismatch_message(hello, &self.path(spec)),
            self.mismatch_hint(spec)
        )
    }
}

/// Content-addressed pin name of a local binary.
fn dev_pin_name(path: &Path) -> Result<String> {
    let hash = crate::command::update::sha256_of(path)?;
    Ok(format!("workmux-dev-{}", &hash[..12]))
}

/// State of the pinned binary on a remote.
pub enum NodeStatus {
    Ready(Hello),
    Mismatch(Hello),
    Missing,
    Broken(HelloFailure),
}

/// Interpret the output of `node hello`; never [`NodeStatus::Missing`].
fn classify_hello(output: &Output) -> NodeStatus {
    match protocol::parse_hello(output.status, &output.stdout, &output.stderr) {
        Ok(hello) if hello.protocol == protocol::NODE_PROTOCOL => NodeStatus::Ready(hello),
        Ok(hello) => NodeStatus::Mismatch(hello),
        Err(failure) => NodeStatus::Broken(failure),
    }
}

/// Ask the pinned binary for `node hello`.
pub fn node_status(transport: &Transport, spec: &RemoteSpec, pin: &Pin) -> Result<NodeStatus> {
    let missing = MISSING_STATUS.to_string();
    let script = spec.script(
        &[("pin", &pin.name), ("missing", &missing)],
        "bin=\"$root/bin/$pin\"\n[ -x \"$bin\" ] || exit \"$missing\"\nexec \"$bin\" node hello --json </dev/null\n",
    );
    let output = transport.run(&script, Input::Null, Some(PROBE_TIMEOUT))?;
    if output.status == Some(MISSING_STATUS) {
        return Ok(NodeStatus::Missing);
    }
    Ok(classify_hello(&output))
}

/// `workmux remote setup <name>`.
pub fn setup(spec: &RemoteSpec) -> Result<()> {
    let transport = Transport::open(spec)?;
    transport.connect()?;
    let pin = Pin::for_spec(spec)?;
    let node_path = pin.path(spec);

    match node_status(&transport, spec, &pin)? {
        NodeStatus::Ready(hello) => {
            println!(
                "✓ {}: workmux {} (protocol {}) is already installed at {node_path}",
                spec.name, hello.version, hello.protocol
            );
            return Ok(());
        }
        NodeStatus::Mismatch(hello) => return Err(pin.mismatch_error(spec, &hello)),
        NodeStatus::Missing => {}
        NodeStatus::Broken(failure) => {
            tracing::warn!(?failure, path = %node_path, "replacing broken pinned node binary");
        }
    }

    let platform = probe_platform(&transport)?;
    let temp_dir = tempfile::tempdir().context("Failed to create temp directory")?;
    let local = match &pin.source {
        // Upload a private copy so the bytes match the hash in the pin name
        // even if the configured file is rebuilt meanwhile.
        PinSource::File(path) => {
            let copy = temp_dir.path().join("workmux");
            fs::copy(path, &copy).with_context(|| format!("Failed to read {}", path.display()))?;
            if dev_pin_name(&copy)? != pin.name {
                bail!(
                    "remotes.{}.binary changed while setup was running; run setup again",
                    spec.name
                );
            }
            copy
        }
        PinSource::Release => {
            let version = env!("CARGO_PKG_VERSION");
            println!("Downloading workmux v{version} for {platform}...");
            crate::command::update::fetch_release_binary(
                version,
                &format!("workmux-{platform}"),
                temp_dir.path(),
                &|_| {},
            )
            .map_err(|err| {
                anyhow!(
                    "Could not get the workmux v{version} release for {platform}: {err:#}. \
                     For an unreleased build, set remotes.{}.binary (see `just remote-dev-binary`)",
                    spec.name
                )
            })?
        }
    };

    let size = fs::metadata(&local)
        .with_context(|| format!("Failed to read {}", local.display()))?
        .len();
    println!(
        "Uploading {} ({:.1} MB) to {}...",
        pin.name,
        size as f64 / 1_000_000.0,
        spec.name
    );
    let hello = stage_and_install(&transport, spec, &pin, &node_path, &local, size, platform)?;
    println!(
        "✓ {}: installed workmux {} (protocol {}) at {node_path}",
        spec.name, hello.version, hello.protocol
    );
    Ok(())
}

/// Release platform of the remote, from `uname`.
fn probe_platform(transport: &Transport) -> Result<&'static str> {
    let output = transport.run("uname -s; uname -m\n", Input::Null, Some(PROBE_TIMEOUT))?;
    if !output.success() {
        bail!(
            "Failed to detect the remote platform: {}",
            output.stderr_line()
        );
    }
    let text = output.stdout_text();
    let mut lines = text.lines().map(str::trim);
    release_platform(lines.next().unwrap_or(""), lines.next().unwrap_or(""))
        .context("No workmux build exists for the remote platform")
}

/// Map `uname -s` / `uname -m` to a release platform suffix.
pub fn release_platform(os: &str, machine: &str) -> Result<&'static str> {
    let os = match os {
        "Linux" => "linux",
        "Darwin" => "macos",
        other => other,
    };
    let arch = match machine {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    };
    crate::command::update::release_platform(os, arch)
}

/// Upload `local` to a fresh stage file, prove it runs as a node speaking
/// this protocol, then rename it to the pinned name.
fn stage_and_install(
    transport: &Transport,
    spec: &RemoteSpec,
    pin: &Pin,
    node_path: &str,
    local: &Path,
    size: u64,
    platform: &str,
) -> Result<Hello> {
    let mut random = [0u8; 8];
    getrandom::fill(&mut random)
        .map_err(|err| anyhow!("Failed to generate a stage name: {err}"))?;
    let stage_name = format!(
        ".workmux.{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let size_text = size.to_string();
    let truncated = TRUNCATED_STATUS.to_string();
    let stage_failed = STAGE_FAILED_STATUS.to_string();
    let vars = [
        ("stage_name", stage_name.as_str()),
        ("size", size_text.as_str()),
        ("truncated", truncated.as_str()),
        ("stage_failed", stage_failed.as_str()),
    ];

    // Stage files older than an hour belong to interrupted installs.
    let upload = spec.script(
        &vars,
        r#"mkdir -p "$root/bin" || exit "$stage_failed"
find "$root/bin" -maxdepth 1 -name '.workmux.*' -mmin +60 -exec rm -f {} + 2>/dev/null
stage="$root/bin/$stage_name"
if ! (set -C; cat >"$stage"); then rm -f "$stage"; exit "$stage_failed"; fi
got=$(wc -c <"$stage" | tr -d ' ')
if [ "$got" != "$size" ]; then
  rm -f "$stage"
  echo "received $got of $size bytes" >&2
  exit "$truncated"
fi
chmod 755 "$stage" || { rm -f "$stage"; exit "$stage_failed"; }
"$stage" node hello --json </dev/null
"#,
    );
    let output = transport.run(&upload, Input::File(local.to_path_buf()), None)?;
    if output.status == Some(TRUNCATED_STATUS) {
        bail!(
            "Upload to {} was incomplete ({})",
            spec.name,
            output.stderr_line()
        );
    }
    if output.status == Some(STAGE_FAILED_STATUS) {
        bail!(
            "Failed to stage the upload in {}/bin on {}: {}",
            spec.path.trim_end_matches('/'),
            spec.name,
            output.stderr_line()
        );
    }
    let staged = match classify_hello(&output) {
        NodeStatus::Ready(hello) => Ok(hello),
        NodeStatus::Mismatch(hello) => Err(pin.mismatch_error(spec, &hello)),
        NodeStatus::Broken(failure) => Err(anyhow!(staged_failure(spec, pin, platform, failure))),
        NodeStatus::Missing => unreachable!("classify_hello never reports Missing"),
    };
    let staged = match staged {
        Ok(hello) => hello,
        Err(err) => {
            let discard = spec.script(&vars, "rm -f \"$root/bin/$stage_name\"\n");
            let _ = transport.run(&discard, Input::Null, Some(PROBE_TIMEOUT));
            return Err(err);
        }
    };
    let script = spec.script(
        &[("stage_name", &stage_name), ("pin", &pin.name)],
        "mv -f \"$root/bin/$stage_name\" \"$root/bin/$pin\"\n",
    );
    let output = transport.run(&script, Input::Null, Some(PROBE_TIMEOUT))?;
    if !output.success() {
        bail!("Failed to install {node_path}: {}", output.stderr_line());
    }
    Ok(staged)
}

fn staged_failure(spec: &RemoteSpec, pin: &Pin, platform: &str, failure: HelloFailure) -> String {
    let source = match &pin.source {
        PinSource::Release => format!("The workmux v{} release", env!("CARGO_PKG_VERSION")),
        PinSource::File(path) => path.display().to_string(),
    };
    match failure {
        HelloFailure::PredatesNodes => format!(
            "{source} predates remote nodes (it has no `workmux node` command). Point \
             remotes.{}.binary at a Linux build of this workmux (see `just remote-dev-binary`)",
            spec.name
        ),
        HelloFailure::NotANode { detail } => format!(
            "{source} does not run as workmux on {platform}: {detail}. remotes.{}.binary must \
             be a {platform} build of workmux (see `just remote-dev-binary`)",
            spec.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_mapping() {
        assert_eq!(release_platform("Linux", "x86_64").unwrap(), "linux-amd64");
        assert_eq!(release_platform("Linux", "aarch64").unwrap(), "linux-arm64");
        assert_eq!(release_platform("Linux", "amd64").unwrap(), "linux-amd64");
        assert_eq!(
            release_platform("Darwin", "x86_64").unwrap(),
            "darwin-amd64"
        );
        assert_eq!(release_platform("Darwin", "arm64").unwrap(), "darwin-arm64");
        assert!(release_platform("Linux", "riscv64").is_err());
        assert!(release_platform("FreeBSD", "amd64").is_err());
    }

    #[test]
    fn dev_pin_is_content_addressed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("workmux");
        fs::write(&path, b"abc").unwrap();
        let spec = RemoteSpec {
            name: "r".into(),
            host: "h".into(),
            path: "~/n".into(),
            isolation: Default::default(),
            isolation_explicit: false,
            binary: Some(path.clone()),
        };
        // sha256("abc") = ba7816bf8f01cfea...
        assert_eq!(
            Pin::for_spec(&spec).unwrap().name,
            "workmux-dev-ba7816bf8f01"
        );

        let release = RemoteSpec {
            binary: None,
            ..spec.clone()
        };
        assert_eq!(
            Pin::for_spec(&release).unwrap().name,
            format!("workmux-{}", env!("CARGO_PKG_VERSION"))
        );

        let missing = RemoteSpec {
            binary: Some(tmp.path().join("nope")),
            ..spec
        };
        let err = format!("{:#}", Pin::for_spec(&missing).err().unwrap());
        assert!(err.contains("remotes.r.binary"), "{err}");
    }
}
