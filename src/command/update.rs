use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const REPO: &str = "raine/workmux";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const NOTIFY_COOLDOWN_SECS: u64 = 7 * 24 * 60 * 60;
const NOTIFY_BURST_COUNT: u64 = 3;

/// Release artifact suffix of the running binary's platform.
fn platform_suffix() -> Result<&'static str> {
    release_platform(std::env::consts::OS, std::env::consts::ARCH)
}

/// Map an OS/arch pair, spelled like `std::env::consts`, to the release
/// artifact suffix used in GitHub releases.
pub(crate) fn release_platform(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-amd64"),
        ("linux", "x86_64") => Ok("linux-amd64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => bail!("Unsupported platform: {os}/{arch}"),
    }
}

/// Check if the binary is managed by Homebrew.
fn is_homebrew_install(exe_path: &std::path::Path) -> bool {
    let path_str = exe_path.to_string_lossy();
    path_str.contains("/Cellar/") || path_str.contains("/homebrew/")
}

/// Fetch the latest release tag from GitHub API using curl.
fn fetch_latest_version() -> Result<String> {
    fetch_latest_version_with_options(None, "Failed to run curl. Is curl installed?")
}

/// Fetch latest release tag from GitHub API using curl with optional timeouts.
fn fetch_latest_version_with_timeout() -> Result<String> {
    fetch_latest_version_with_options(Some((5, 10)), "Failed to run curl")
}

fn fetch_latest_version_with_options(
    timeout_secs: Option<(u64, u64)>,
    run_ctx: &str,
) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut command = Command::new("curl");
    command.arg("-sSf");

    if let Some((connect_timeout, max_time)) = timeout_secs {
        command.arg("--connect-timeout");
        command.arg(connect_timeout.to_string());
        command.arg("--max-time");
        command.arg(max_time.to_string());
    }

    let output = command
        .arg(&url)
        .output()
        .with_context(|| run_ctx.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to fetch latest release: {}", stderr.trim());
    }

    parse_latest_release_tag(&output.stdout)
}

fn parse_latest_release_tag(payload: &[u8]) -> Result<String> {
    let body: serde_json::Value =
        serde_json::from_slice(payload).context("Failed to parse GitHub API response")?;

    let tag = body["tag_name"]
        .as_str()
        .context("No tag_name in GitHub API response")?;

    Ok(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// Download a URL to a file path using curl.
fn download(url: &str, dest: &std::path::Path) -> Result<()> {
    let status = Command::new("curl")
        .args(["-sSLf", "-o"])
        .arg(dest)
        .arg(url)
        .status()
        .context("Failed to run curl")?;

    if !status.success() {
        bail!("Download failed: {url}");
    }
    Ok(())
}

/// Extract a tar.gz archive into a directory.
fn extract_tar(archive: &std::path::Path, dest: &std::path::Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("Failed to run tar")?;

    if !status.success() {
        bail!("Failed to extract archive");
    }
    Ok(())
}

/// Lowercase hex SHA-256 of a file's contents.
pub(crate) fn sha256_of(path: &std::path::Path) -> Result<String> {
    use sha2::Digest;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Verify SHA-256 checksum of a file against the expected checksum line.
fn verify_checksum(file: &std::path::Path, expected_line: &str) -> Result<()> {
    // The .sha256 file format is: "<hash>  <filename>"
    let expected_hash = expected_line
        .split_whitespace()
        .next()
        .context("Invalid checksum file format")?;

    let actual_hash = sha256_of(file)?;
    if actual_hash != expected_hash {
        bail!("Checksum mismatch!\n  Expected: {expected_hash}\n  Got:      {actual_hash}");
    }
    Ok(())
}

/// Download release `version` of `artifact_name` (e.g. `workmux-linux-arm64`)
/// into `dir`, verify its published sha256, and return the extracted binary.
pub(crate) fn fetch_release_binary(
    version: &str,
    artifact_name: &str,
    dir: &std::path::Path,
    on_step: &dyn Fn(&str),
) -> Result<std::path::PathBuf> {
    let tar_path = dir.join(format!("{artifact_name}.tar.gz"));
    let sha_path = dir.join(format!("{artifact_name}.sha256"));

    let base_url = format!("https://github.com/{REPO}/releases/download/v{version}");

    download(&format!("{base_url}/{artifact_name}.tar.gz"), &tar_path)?;
    download(&format!("{base_url}/{artifact_name}.sha256"), &sha_path)?;

    on_step("Verifying checksum...");
    let sha_content = std::fs::read_to_string(&sha_path).context("Failed to read checksum file")?;
    verify_checksum(&tar_path, &sha_content)?;

    let extract_dir = dir.join("extract");
    std::fs::create_dir(&extract_dir)?;
    extract_tar(&tar_path, &extract_dir)?;

    let binary = extract_dir.join("workmux");
    if !binary.exists() {
        bail!("Extracted archive does not contain 'workmux' binary");
    }
    Ok(binary)
}

/// Replace the current binary with the new one, with rollback on failure.
fn replace_binary(new_binary: &std::path::Path, current_exe: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let exe_dir = current_exe
        .parent()
        .context("Could not determine binary directory")?;

    // Copy to destination directory to avoid EXDEV (cross-device rename)
    let staged = exe_dir.join(".workmux.new");
    std::fs::copy(new_binary, &staged).context("Failed to copy new binary to install directory")?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;

    // Rename current -> .old, then staged -> current
    let backup = exe_dir.join(".workmux.old");
    std::fs::rename(current_exe, &backup).context("Failed to move current binary aside")?;

    if let Err(e) = std::fs::rename(&staged, current_exe) {
        // Rollback: restore the original
        let _ = std::fs::rename(&backup, current_exe);
        return Err(e).context("Failed to install new binary (rolled back)");
    }

    // Cleanup
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

fn do_update(
    pb: &indicatif::ProgressBar,
    artifact_name: &str,
    current_exe: &std::path::Path,
) -> Result<String> {
    let latest_version = fetch_latest_version()?;

    if latest_version == CURRENT_VERSION {
        return Ok(format!("Already up to date (v{CURRENT_VERSION})"));
    }

    pb.set_message(format!("Downloading v{latest_version}..."));

    let tmp = tempfile::tempdir().context("Failed to create temp directory")?;
    let new_binary = fetch_release_binary(&latest_version, artifact_name, tmp.path(), &|step| {
        pb.set_message(step.to_string())
    })?;

    pb.set_message("Installing...");
    replace_binary(&new_binary, current_exe)?;

    Ok(format!(
        "Updated workmux v{CURRENT_VERSION} -> v{latest_version}"
    ))
}

pub fn run() -> Result<()> {
    let current_exe =
        std::env::current_exe().context("Could not determine current executable path")?;

    // Guard: Homebrew-managed installs (canonicalize to resolve symlinks)
    let canonical_exe = std::fs::canonicalize(&current_exe).unwrap_or(current_exe.clone());
    if is_homebrew_install(&canonical_exe) {
        bail!("workmux is managed by Homebrew. Run `brew upgrade workmux` instead.");
    }

    let platform = platform_suffix()?;
    let artifact_name = format!("workmux-{platform}");

    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
            .template("{spinner:.blue} {msg}")
            .unwrap(),
    );
    pb.enable_steady_tick(std::time::Duration::from_millis(120));
    pb.set_message("Checking for updates...");

    match do_update(&pb, &artifact_name, &current_exe) {
        Ok(msg) => {
            pb.finish_with_message(format!("✔ {msg}"));
            Ok(())
        }
        Err(e) => {
            pb.finish_with_message("✘ Update failed".to_string());
            Err(e)
        }
    }
}

// --- Auto-update check ---

#[derive(Debug, Serialize, Deserialize, Default)]
struct UpdateCache {
    latest_version: Option<String>,
    last_checked: Option<u64>,
    last_notified: Option<u64>,
    notify_count: Option<u64>,
}

fn update_cache_path() -> Option<std::path::PathBuf> {
    let dir = crate::xdg::cache_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("update_check.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn load_cache(path: &std::path::Path) -> UpdateCache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(path: &std::path::Path, cache: &UpdateCache) {
    if let Ok(json) = serde_json::to_string(cache) {
        let _ = std::fs::write(path, json);
    }
}

/// Compare two version strings as numeric tuples (e.g. "0.1.10" > "0.1.9").
fn is_newer_version(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').filter_map(|s| s.parse().ok()).collect() };
    let l = parse(latest);
    let c = parse(current);
    l > c
}

/// Called on CLI startup to show an update notice if one is cached.
/// Also spawns a background check if the cache is stale.
/// Designed to be completely non-blocking and fail-silent.
pub fn check_and_notify(config: &crate::config::Config) {
    // Opt-out via config
    if config.auto_update_check == Some(false) {
        return;
    }

    // Opt-out via environment variable
    if std::env::var("WORKMUX_NO_UPDATE_CHECK").is_ok() {
        return;
    }

    // Don't print notices in non-interactive contexts
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() || !std::io::stderr().is_terminal() {
        return;
    }

    let cache_path = match update_cache_path() {
        Some(p) => p,
        None => return,
    };

    let mut cache = load_cache(&cache_path);
    let now = now_secs();

    // Spawn background check if cache is stale
    if now.saturating_sub(cache.last_checked.unwrap_or(0)) > CHECK_INTERVAL_SECS {
        let spawned = std::env::current_exe().ok().and_then(|exe| {
            Command::new(exe)
                .arg("_check-update")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()
        });

        // Only mark as checked if spawn succeeded to avoid silent blackout
        if spawned.is_some() {
            cache.last_checked = Some(now);
            save_cache(&cache_path, &cache);
        }
    }

    // Show notice if a newer version is available.
    // Pattern: burst of 3 consecutive notifications, then 7-day cooldown, repeat.
    if let Some(ref latest) = cache.latest_version
        && is_newer_version(latest, CURRENT_VERSION)
    {
        let count = cache.notify_count.unwrap_or(0);
        let in_cooldown = count >= NOTIFY_BURST_COUNT
            && now.saturating_sub(cache.last_notified.unwrap_or(0)) <= NOTIFY_COOLDOWN_SECS;

        if !in_cooldown {
            // Reset burst counter after cooldown expires
            let count = if count >= NOTIFY_BURST_COUNT {
                0
            } else {
                count
            };

            let is_brew = std::env::current_exe()
                .ok()
                .and_then(|p| std::fs::canonicalize(&p).ok())
                .is_some_and(|p| is_homebrew_install(&p));

            let update_cmd = if is_brew {
                "brew upgrade workmux"
            } else {
                "workmux update"
            };

            eprintln!(
                "Update available: workmux v{CURRENT_VERSION} -> v{latest} (run `{update_cmd}`)"
            );

            cache.last_notified = Some(now);
            cache.notify_count = Some(count + 1);
            save_cache(&cache_path, &cache);
        }
    }
}

/// Hidden subcommand handler: fetch the latest version and update the cache.
pub fn run_background_check() -> Result<()> {
    let latest = fetch_latest_version_with_timeout()?;
    let now = now_secs();

    let cache_path = update_cache_path().context("Could not determine cache path")?;
    let mut cache = load_cache(&cache_path);

    // Reset notification counter when a new version is discovered
    if cache.latest_version.as_deref() != Some(&latest) {
        cache.notify_count = Some(0);
    }

    cache.latest_version = Some(latest);
    cache.last_checked = Some(now);
    save_cache(&cache_path, &cache);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_suffix_current() {
        // Should succeed on any supported CI/dev platform
        let suffix = platform_suffix().unwrap();
        assert!(["darwin-arm64", "darwin-amd64", "linux-amd64", "linux-arm64"].contains(&suffix));
    }

    #[test]
    fn test_verify_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("workmux.tar.gz");
        std::fs::write(&file, b"abc").unwrap();
        let line =
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  workmux.tar.gz\n";
        verify_checksum(&file, line).unwrap();
        let err = verify_checksum(&file, &line.replacen("ba78", "0000", 1)).unwrap_err();
        assert!(err.to_string().contains("Checksum mismatch"));
    }

    #[test]
    fn test_is_homebrew_cellar() {
        assert!(is_homebrew_install(std::path::Path::new(
            "/opt/homebrew/Cellar/workmux/0.1.124/bin/workmux"
        )));
    }

    #[test]
    fn test_is_homebrew_prefix() {
        assert!(is_homebrew_install(std::path::Path::new(
            "/usr/local/Cellar/workmux/0.1.124/bin/workmux"
        )));
    }

    #[test]
    fn test_is_not_homebrew_local_bin() {
        assert!(!is_homebrew_install(std::path::Path::new(
            "/usr/local/bin/workmux"
        )));
    }

    #[test]
    fn test_is_not_homebrew_home() {
        assert!(!is_homebrew_install(std::path::Path::new(
            "/home/user/.local/bin/workmux"
        )));
    }

    #[test]
    fn test_is_newer_version_patch() {
        assert!(is_newer_version("0.1.10", "0.1.9"));
    }

    #[test]
    fn test_is_newer_version_minor() {
        assert!(is_newer_version("0.2.0", "0.1.124"));
    }

    #[test]
    fn test_is_newer_version_major() {
        assert!(is_newer_version("1.0.0", "0.99.99"));
    }

    #[test]
    fn test_is_not_newer_same() {
        assert!(!is_newer_version("0.1.124", "0.1.124"));
    }

    #[test]
    fn test_is_not_newer_older() {
        assert!(!is_newer_version("0.1.9", "0.1.10"));
    }

    #[test]
    fn test_parse_latest_release_tag_strips_v_prefix() {
        let payload = br#"{"tag_name":"v1.2.3"}"#;

        let parsed = parse_latest_release_tag(payload).unwrap();

        assert_eq!(parsed, "1.2.3");
    }

    #[test]
    fn test_parse_latest_release_tag_keeps_plain_version() {
        let payload = br#"{"tag_name":"2.0.0"}"#;

        let parsed = parse_latest_release_tag(payload).unwrap();

        assert_eq!(parsed, "2.0.0");
    }
}
