use anyhow::{anyhow, Context, Result};
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file;
use std::path::{Path, PathBuf};

pub fn run_tui(dashboard_args: &[String]) -> Result<()> {
    ensure_dx_installed()?;

    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    build_dev_binary(project_root)?;
    install_live_binary_symlink(project_root)?;

    let (devserver_ip, devserver_port) = devserver_endpoint();

    let mut cmd = std::process::Command::new("dx");
    cmd.args(dx_tui_args(&devserver_ip, &devserver_port));
    cmd.arg(format!("--args={}", app_args_for_dashboard(dashboard_args)));
    cmd.current_dir(project_root);

    let dev_bin_dirs = dev_bin_dirs(project_root);
    let app_path = prepend_path_for_dev(&dev_bin_dirs, std::env::var_os("PATH"))?;
    cmd.env("PATH", app_path);

    let status = cmd
        .status()
        .context("failed to start dx dev server for workmux dashboard")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("dx dev server exited with status {status}")
    }
}

pub fn run_dashboard(dashboard_args: &[String]) -> Result<()> {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    build_dev_binary(project_root)?;
    install_live_binary_symlink(project_root)?;

    let mut cmd = std::process::Command::new(dev_binary_path(project_root));
    cmd.arg("dashboard");
    cmd.args(dashboard_args);
    cmd.current_dir(project_root);

    let (devserver_ip, devserver_port) = devserver_endpoint();
    cmd.env("DIOXUS_DEVSERVER_IP", devserver_ip);
    cmd.env("DIOXUS_DEVSERVER_PORT", devserver_port);

    let status = cmd
        .status()
        .context("failed to start dev dashboard sidecar")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("dev dashboard sidecar exited with status {status}")
    }
}

fn ensure_dx_installed() -> Result<()> {
    if which::which("dx").is_ok() {
        Ok(())
    } else {
        Err(anyhow!(
            "dx CLI is required for dev hot-reload. Install it with: cargo install dioxus-cli"
        ))
    }
}

fn dx_tui_args(devserver_ip: &str, devserver_port: &str) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--hot-patch".to_string(),
        "--desktop".to_string(),
        "--addr".to_string(),
        devserver_ip.to_string(),
        "--port".to_string(),
        devserver_port.to_string(),
        "--bin".to_string(),
        "workmux".to_string(),
        "--features".to_string(),
        "dev-hotpatch".to_string(),
    ]
}

fn build_dev_binary(project_root: &Path) -> Result<()> {
    let status = std::process::Command::new("cargo")
        .args(["build", "--bin", "workmux", "--features", "dev-hotpatch"])
        .current_dir(project_root)
        .status()
        .context("failed to build workmux dev binary")?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "cargo build for workmux dev mode failed with status {status}"
        ))
    }
}

fn install_live_binary_symlink(project_root: &Path) -> Result<()> {
    let live_bin = live_bin_path()?;
    let dev_bin = dev_binary_path(project_root);

    if let Some(parent) = live_bin.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    if fs::symlink_metadata(&live_bin).is_ok() {
        fs::remove_file(&live_bin)
            .with_context(|| format!("failed to replace existing {}", live_bin.display()))?;
    }

    create_symlink(&dev_bin, &live_bin).with_context(|| {
        format!(
            "failed to symlink {} -> {}",
            live_bin.display(),
            dev_bin.display()
        )
    })
}

fn app_args_for_dashboard(extra_dashboard_args: &[String]) -> String {
    if extra_dashboard_args.is_empty() {
        return "dashboard".to_string();
    }

    format!("dashboard {}", extra_dashboard_args.join(" "))
}

fn debug_bin_dir(project_root: &Path) -> PathBuf {
    project_root.join("target").join("debug")
}

fn dev_binary_path(project_root: &Path) -> PathBuf {
    let target_triple = host_target_triple();
    dev_binary_path_for_target(project_root, target_triple.as_deref())
}

fn dev_binary_path_for_target(project_root: &Path, target_triple: Option<&str>) -> PathBuf {
    if let Some(target_triple) = target_triple {
        project_root
            .join("target")
            .join(target_triple)
            .join("desktop-dev")
            .join("workmux")
    } else {
        debug_bin_dir(project_root).join("workmux")
    }
}

fn live_bin_path() -> Result<PathBuf> {
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    live_bin_path_for_env(cargo_home.as_deref(), home.as_deref())
}

fn live_bin_path_for_env(cargo_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    if let Some(cargo_home) = cargo_home {
        return Ok(cargo_home.join("bin").join("workmux"));
    }

    if let Some(home) = home {
        return Ok(home.join(".cargo").join("bin").join("workmux"));
    }

    Err(anyhow!(
        "could not determine install path for workmux (set CARGO_HOME or HOME)"
    ))
}

fn dev_bin_dirs(project_root: &Path) -> Vec<PathBuf> {
    let target_triple = host_target_triple();
    dev_bin_dirs_for_target(project_root, target_triple.as_deref())
}

fn dev_bin_dirs_for_target(project_root: &Path, target_triple: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(target_triple) = target_triple {
        dirs.push(
            project_root
                .join("target")
                .join(target_triple)
                .join("desktop-dev"),
        );
    }

    dirs.push(debug_bin_dir(project_root));
    dirs
}

fn host_target_triple() -> Option<String> {
    let output = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn devserver_endpoint() -> (String, String) {
    let ip = std::env::var("DIOXUS_DEVSERVER_IP").ok();
    let port = std::env::var("DIOXUS_DEVSERVER_PORT").ok();
    devserver_endpoint_from_env(ip.as_deref(), port.as_deref())
}

fn devserver_endpoint_from_env(ip: Option<&str>, port: Option<&str>) -> (String, String) {
    (
        ip.unwrap_or("127.0.0.1").to_string(),
        port.unwrap_or("8080").to_string(),
    )
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    symlink_file(target, link)
}

fn prepend_path_for_dev(dirs: &[PathBuf], existing_path: Option<OsString>) -> Result<OsString> {
    let mut paths = dirs.to_vec();
    if let Some(existing) = existing_path {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).context("failed to compose dev PATH")
}

#[cfg(test)]
mod tests {
    use super::{
        app_args_for_dashboard, dev_bin_dirs_for_target, dev_binary_path_for_target,
        devserver_endpoint_from_env, dx_tui_args, live_bin_path_for_env, prepend_path_for_dev,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn dx_tui_args_enable_hot_patch() {
        let args = dx_tui_args("127.0.0.1", "8080");
        assert!(args.iter().any(|a| a == "--hot-patch"));
        assert!(args.iter().any(|a| a == "--features"));
    }

    #[test]
    fn dx_tui_args_sets_devserver_endpoint() {
        let args = dx_tui_args("127.0.0.1", "8080");
        assert!(args.iter().any(|a| a == "--addr"));
        assert!(args.iter().any(|a| a == "127.0.0.1"));
        assert!(args.iter().any(|a| a == "--port"));
        assert!(args.iter().any(|a| a == "8080"));
    }

    #[test]
    fn prepend_path_for_dev_puts_dev_dirs_first() {
        let existing =
            std::env::join_paths([PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")])
                .expect("test path should be valid");

        let dev_dirs = [
            PathBuf::from("target/aarch64-apple-darwin/desktop-dev"),
            PathBuf::from("target/debug"),
        ];
        let composed = prepend_path_for_dev(&dev_dirs, Some(existing))
            .expect("path composition should succeed");
        let parts: Vec<PathBuf> = std::env::split_paths(&composed).collect();

        assert_eq!(
            parts.first(),
            Some(&PathBuf::from("target/aarch64-apple-darwin/desktop-dev"))
        );
        assert_eq!(parts.get(1), Some(&PathBuf::from("target/debug")));
    }

    #[test]
    fn dev_bin_dirs_falls_back_to_debug_when_target_unknown() {
        let root = Path::new("/repo");
        let dirs = dev_bin_dirs_for_target(root, None);

        assert_eq!(dirs, vec![root.join("target").join("debug")]);
    }

    #[test]
    fn dev_bin_dirs_includes_desktop_dev_when_target_known() {
        let root = Path::new("/repo");
        let expected = root
            .join("target")
            .join("aarch64-apple-darwin")
            .join("desktop-dev");

        let dirs = dev_bin_dirs_for_target(root, Some("aarch64-apple-darwin"));

        assert_eq!(dirs.first(), Some(&expected));
        assert_eq!(dirs.get(1), Some(&root.join("target").join("debug")));
    }

    #[test]
    fn app_args_for_dashboard_defaults_to_dashboard() {
        let args = app_args_for_dashboard(&[]);
        assert_eq!(args, "dashboard");
    }

    #[test]
    fn app_args_for_dashboard_appends_extra_args() {
        let args = app_args_for_dashboard(&[
            "--diff".to_string(),
            "--preview-size".to_string(),
            "70".to_string(),
        ]);
        assert_eq!(args, "dashboard --diff --preview-size 70");
    }

    #[test]
    fn dev_binary_path_prefers_desktop_dev_target() {
        let root = Path::new("/repo");
        let path = dev_binary_path_for_target(root, Some("aarch64-apple-darwin"));
        assert_eq!(
            path,
            root.join("target")
                .join("aarch64-apple-darwin")
                .join("desktop-dev")
                .join("workmux")
        );
    }

    #[test]
    fn live_bin_path_prefers_cargo_home() {
        let path =
            live_bin_path_for_env(Some(Path::new("/tmp/cargo")), Some(Path::new("/tmp/home")))
                .expect("cargo home should resolve");
        assert_eq!(path, Path::new("/tmp/cargo").join("bin").join("workmux"));
    }

    #[test]
    fn live_bin_path_falls_back_to_home_dot_cargo() {
        let path = live_bin_path_for_env(None, Some(Path::new("/tmp/home")))
            .expect("home fallback should resolve");
        assert_eq!(
            path,
            Path::new("/tmp/home")
                .join(".cargo")
                .join("bin")
                .join("workmux")
        );
    }

    #[test]
    fn devserver_defaults_to_localhost_8080() {
        let (ip, port) = devserver_endpoint_from_env(None, None);
        assert_eq!(ip, "127.0.0.1");
        assert_eq!(port, "8080");
    }

    #[test]
    fn devserver_uses_existing_env_values() {
        let (ip, port) = devserver_endpoint_from_env(Some("10.0.0.2"), Some("9090"));
        assert_eq!(ip, "10.0.0.2");
        assert_eq!(port, "9090");
    }
}
