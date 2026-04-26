//! Lima configuration YAML generation.

use anyhow::Result;
use serde_yaml::Value;

use super::mounts::Mount;
use crate::config::SandboxConfig;

/// Generate the shell commands to install a specific agent in a Lima VM.
///
/// Each agent has different install requirements mirroring the container
/// Dockerfiles. Unknown agents get a no-op comment.
fn lima_install_script_for_agent(agent: &str) -> String {
    match agent {
        "claude" => r#"# Install Claude Code CLI
curl -fsSL https://claude.ai/install.sh | bash

# Symlink Claude config from mounted state directory (seeded from host)
# This preserves onboarding state, tips history, etc. across VM recreations
ln -sfn "$HOME/.workmux-state/.claude.json" "$HOME/.claude.json"
"#
        .to_string(),

        "codex" => r#"# Install Codex CLI from GitHub releases (use musl for glibc compatibility)
mkdir -p "$HOME/.local/bin"
ARCH=$(uname -m)
if [ "$ARCH" != "aarch64" ]; then ARCH="x86_64"; fi
curl -fsSL "https://github.com/openai/codex/releases/latest/download/codex-${ARCH}-unknown-linux-musl.tar.gz" | \
  tar xz -C "$HOME/.local/bin/"
mv "$HOME/.local/bin/codex-${ARCH}-unknown-linux-musl" "$HOME/.local/bin/codex"
"#
        .to_string(),

        "gemini" => r#"# Install Node.js (required for Gemini CLI)
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt-get install -y --no-install-recommends nodejs

# Install Gemini CLI
sudo npm install -g @google/gemini-cli
"#
        .to_string(),

        "opencode" => r#"# Install OpenCode
curl -fsSL https://opencode.ai/install | bash
mkdir -p "$HOME/.local/bin"
[ -x "$HOME/.opencode/bin/opencode" ] && ln -sfn "$HOME/.opencode/bin/opencode" "$HOME/.local/bin/opencode"
"#
        .to_string(),

        "pi" => r#"# Install Node.js (required for pi coding agent CLI)
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
sudo apt-get install -y --no-install-recommends nodejs

# Install pi coding agent CLI to /usr/local (avoids sudo permission issues)
sudo mkdir -p /usr/local/bin /usr/local/lib/node_modules
npm config set prefix /usr/local
npm install -g @mariozechner/pi-coding-agent
"#
        .to_string(),

        other => format!("# No built-in install script for agent: {other}\n\
                          # Use sandbox.lima.provision to install it manually.\n"),
    }
}

/// Generate Lima configuration YAML.
///
/// The `agent` parameter determines which CLI tool is installed during
/// provisioning (e.g. "claude", "codex", "gemini", "opencode").
pub fn generate_lima_config(
    _instance_name: &str,
    mounts: &[Mount],
    sandbox_config: &SandboxConfig,
    agent: &str,
    needs_nix: bool,
) -> Result<String> {
    let mut config = serde_yaml::Mapping::new();

    // Use custom image if configured, otherwise default to minimal Debian 12
    // Debian genericcloud images are ~330MB vs Ubuntu's ~600MB
    let arch = std::env::consts::ARCH;
    let image_arch = if arch == "aarch64" || arch == "arm64" {
        "aarch64"
    } else {
        "x86_64"
    };

    let mut image_config = serde_yaml::Mapping::new();
    if let Some(custom_image) = &sandbox_config.image {
        image_config.insert("location".into(), custom_image.as_str().into());
    } else {
        let default_url = if image_arch == "aarch64" {
            "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-arm64.qcow2"
        } else {
            "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2"
        };
        image_config.insert("location".into(), default_url.into());
        image_config.insert("arch".into(), image_arch.into());
    }

    config.insert("images".into(), vec![Value::Mapping(image_config)].into());

    // Use VZ backend on macOS (fastest), QEMU on Linux
    #[cfg(target_os = "macos")]
    {
        config.insert("vmType".into(), "vz".into());

        // Enable Rosetta for x86 binaries on ARM (use new nested format)
        if arch == "aarch64" || arch == "arm64" {
            let mut rosetta = serde_yaml::Mapping::new();
            rosetta.insert("enabled".into(), true.into());
            rosetta.insert("binfmt".into(), true.into());

            let mut vz = serde_yaml::Mapping::new();
            vz.insert("rosetta".into(), rosetta.into());

            let mut vm_opts = serde_yaml::Mapping::new();
            vm_opts.insert("vz".into(), vz.into());

            config.insert("vmOpts".into(), vm_opts.into());
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        config.insert("vmType".into(), "qemu".into());
    }

    // Resource allocation
    config.insert(
        "cpus".into(),
        Value::Number(sandbox_config.lima.cpus().into()),
    );
    config.insert("memory".into(), sandbox_config.lima.memory().into());
    config.insert("disk".into(), sandbox_config.lima.disk().into());

    // CRITICAL: Disable containerd (saves 30-40 seconds boot time)
    let mut containerd = serde_yaml::Mapping::new();
    containerd.insert("system".into(), false.into());
    containerd.insert("user".into(), false.into());
    config.insert("containerd".into(), containerd.into());

    // Generate mounts
    let mount_list: Vec<Value> = mounts
        .iter()
        .map(|m| {
            let mut mount_config = serde_yaml::Mapping::new();
            mount_config.insert(
                "location".into(),
                m.host_path.to_string_lossy().to_string().into(),
            );
            mount_config.insert("writable".into(), (!m.read_only).into());

            if m.host_path != m.guest_path {
                mount_config.insert(
                    "mountPoint".into(),
                    m.guest_path.to_string_lossy().to_string().into(),
                );
            }

            Value::Mapping(mount_config)
        })
        .collect();
    config.insert("mounts".into(), mount_list.into());

    // Provision scripts (run on first VM creation only)
    let mut provisions = Vec::new();

    if !sandbox_config.lima.skip_default_provision() {
        let system_script = r#"#!/bin/bash
set -eux
apt-get update
apt-get install -y --no-install-recommends curl ca-certificates git xz-utils

# Ensure host-exec shim directory is on PATH for login shells.
# Agents like Codex run commands via login shell (bash -lc) which sources
# /etc/profile, resetting PATH and losing the shim directory.
cat > /etc/profile.d/workmux-shims.sh <<'PROFILESCRIPT'
if [ -d "$HOME/.workmux-state/shims/bin" ]; then
    PATH="$HOME/.workmux-state/shims/bin:$PATH"
    export PATH
fi
PROFILESCRIPT
"#;

        let agent_install = lima_install_script_for_agent(agent);

        // Only install Nix/Devbox when the project actually needs it
        let nix_devbox_install = if needs_nix {
            r#"
# Install Nix via Determinate Systems installer (needs root for /nix)
if ! command -v nix >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | \
      sudo sh -s -- install linux --init none --no-confirm
    # Single-user VM: make nix store writable by the user so nix/devbox
    # can install packages without root
    sudo chown -R "$(id -u):$(id -g)" /nix
fi

# Source nix profile for this script and future login shells
if [ -f /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh ]; then
    . /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh
fi
if ! grep -q 'nix-daemon.sh' ~/.profile 2>/dev/null; then
    echo '. /nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh' >> ~/.profile
fi

# Install Devbox (needs root for /usr/local/bin)
if ! command -v devbox >/dev/null 2>&1; then
    curl -fsSL https://get.jetify.com/devbox | sudo bash -s -- -f
    # Launcher script has execute-only perms; bash needs read permission
    sudo chmod +r /usr/local/bin/devbox
    # Trigger download of the real binary during provisioning
    devbox version
fi
"#
        } else {
            ""
        };

        let user_script = format!(
            r#"#!/bin/bash
set -eux
{agent_install}
curl -fsSL https://raw.githubusercontent.com/raine/workmux/main/scripts/install.sh | bash
# Ensure ~/.local/bin is on PATH for non-interactive shells
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.profile
{nix_devbox_install}"#
        );

        let mut system_provision = serde_yaml::Mapping::new();
        system_provision.insert("mode".into(), "system".into());
        system_provision.insert("script".into(), system_script.into());

        let mut user_provision = serde_yaml::Mapping::new();
        user_provision.insert("mode".into(), "user".into());
        user_provision.insert("script".into(), user_script.into());

        provisions.push(Value::Mapping(system_provision));
        provisions.push(Value::Mapping(user_provision));
    }

    if let Some(script) = sandbox_config.lima.provision_script() {
        let mut custom_provision = serde_yaml::Mapping::new();
        custom_provision.insert("mode".into(), "user".into());
        custom_provision.insert("script".into(), script.into());
        provisions.push(Value::Mapping(custom_provision));
    }

    config.insert("provision".into(), provisions.into());

    Ok(serde_yaml::to_string(&config)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_generate_lima_config() {
        let mounts = vec![
            Mount::rw(PathBuf::from("/Users/test/code")),
            Mount {
                host_path: PathBuf::from("/Users/test/.claude"),
                guest_path: PathBuf::from("/root/.claude"),
                read_only: false,
            },
        ];

        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        // Basic sanity checks
        assert!(yaml.contains("images:"));
        assert!(yaml.contains("mounts:"));
        assert!(yaml.contains("/Users/test/code"));
        assert!(yaml.contains("containerd:"));
        assert!(yaml.contains("provision:"));
        assert!(yaml.contains("cpus: 4"));
        assert!(yaml.contains("memory: 4GiB"));
        assert!(yaml.contains("disk: 100GiB"));
    }

    #[test]
    fn test_generate_lima_config_provision_scripts() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        // System provision installs dependencies
        assert!(yaml.contains("mode: system"));
        assert!(yaml.contains("apt-get install"));
        assert!(yaml.contains("curl"));
        assert!(yaml.contains("git"));
        assert!(yaml.contains("xz-utils"));

        // User provision installs Claude Code and workmux
        assert!(yaml.contains("mode: user"));
        assert!(yaml.contains("claude.ai/install.sh"));
        assert!(yaml.contains("workmux/main/scripts/install.sh"));

        // User provision installs Nix and Devbox
        assert!(yaml.contains("install.determinate.systems/nix"));
        assert!(yaml.contains("get.jetify.com/devbox"));

        // User provision symlinks Claude config from state directory
        assert!(
            yaml.contains(r#"ln -sfn "$HOME/.workmux-state/.claude.json" "$HOME/.claude.json""#)
        );
    }

    #[test]
    fn test_generate_lima_config_default_provision_count() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let provisions = parsed["provision"].as_sequence().unwrap();
        assert_eq!(provisions.len(), 2, "default should have 2 provision steps");
    }

    #[test]
    fn test_generate_lima_config_custom_provision() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig {
            lima: crate::config::LimaConfig {
                provision: Some("sudo apt-get install -y ripgrep\necho done".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let provisions = parsed["provision"].as_sequence().unwrap();
        assert_eq!(
            provisions.len(),
            3,
            "should have 3 provision steps with custom script"
        );

        let custom = &provisions[2];
        assert_eq!(custom["mode"].as_str().unwrap(), "user");
        let script = custom["script"].as_str().unwrap();
        assert!(script.contains("sudo apt-get install -y ripgrep"));
        assert!(script.contains("echo done"));
    }

    #[test]
    fn test_generate_lima_config_custom_image() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig {
            image: Some("file:///Users/me/.lima/images/workmux-golden.qcow2".to_string()),
            ..Default::default()
        };
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let images = parsed["images"].as_sequence().unwrap();
        let image = &images[0];
        assert_eq!(
            image["location"].as_str().unwrap(),
            "file:///Users/me/.lima/images/workmux-golden.qcow2"
        );
        // Custom images should not have arch set (user provides arch-appropriate image)
        assert!(image["arch"].is_null());
    }

    #[test]
    fn test_generate_lima_config_default_image() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let images = parsed["images"].as_sequence().unwrap();
        let image = &images[0];
        let location = image["location"].as_str().unwrap();
        assert!(location.contains("debian-12-genericcloud"));
        assert!(image["arch"].as_str().is_some());
    }

    #[test]
    fn test_generate_lima_config_skip_default_provision() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig {
            lima: crate::config::LimaConfig {
                skip_default_provision: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let provisions = parsed["provision"].as_sequence().unwrap();
        assert_eq!(
            provisions.len(),
            0,
            "should have no provision steps when skipping defaults"
        );
        assert!(!yaml.contains("apt-get"));
        assert!(!yaml.contains("claude.ai/install.sh"));
    }

    #[test]
    fn test_generate_lima_config_skip_default_provision_with_custom() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig {
            lima: crate::config::LimaConfig {
                skip_default_provision: Some(true),
                provision: Some("echo custom setup".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let provisions = parsed["provision"].as_sequence().unwrap();
        assert_eq!(
            provisions.len(),
            1,
            "should have only custom provision step"
        );

        let custom = &provisions[0];
        assert_eq!(custom["mode"].as_str().unwrap(), "user");
        assert!(
            custom["script"]
                .as_str()
                .unwrap()
                .contains("echo custom setup")
        );
    }

    #[test]
    fn test_generate_lima_config_extra_mounts() {
        let mounts = vec![
            Mount::rw(PathBuf::from("/tmp/project")),
            // Simulate an extra mount: read-only with different guest path
            Mount {
                host_path: PathBuf::from("/tmp/notes"),
                guest_path: PathBuf::from("/mnt/notes"),
                read_only: true,
            },
        ];

        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let mount_list = parsed["mounts"].as_sequence().unwrap();
        assert_eq!(mount_list.len(), 2);

        // First mount: read-write, same host/guest
        let m0 = &mount_list[0];
        assert_eq!(m0["location"].as_str().unwrap(), "/tmp/project");
        assert_eq!(m0["writable"].as_bool().unwrap(), true);
        assert!(m0["mountPoint"].is_null());

        // Second mount: read-only, different guest path
        let m1 = &mount_list[1];
        assert_eq!(m1["location"].as_str().unwrap(), "/tmp/notes");
        assert_eq!(m1["writable"].as_bool().unwrap(), false);
        assert_eq!(m1["mountPoint"].as_str().unwrap(), "/mnt/notes");
    }

    #[test]
    fn test_generate_lima_config_codex_agent() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "codex", true).unwrap();

        // Should install codex, not claude
        assert!(yaml.contains("codex"));
        assert!(yaml.contains("openai/codex/releases"));
        assert!(!yaml.contains("claude.ai/install.sh"));
        assert!(!yaml.contains(".claude.json"));

        // Common infrastructure should still be present
        assert!(yaml.contains("workmux/main/scripts/install.sh"));
        assert!(yaml.contains("install.determinate.systems/nix"));
        assert!(yaml.contains("get.jetify.com/devbox"));
    }

    #[test]
    fn test_generate_lima_config_gemini_agent() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "gemini", true).unwrap();

        // Should install Node.js and Gemini CLI
        assert!(yaml.contains("nodesource.com"));
        assert!(yaml.contains("@google/gemini-cli"));
        assert!(!yaml.contains("claude.ai/install.sh"));
        assert!(!yaml.contains(".claude.json"));
    }

    #[test]
    fn test_generate_lima_config_opencode_agent() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "opencode", true).unwrap();

        assert!(yaml.contains("opencode.ai/install"));
        assert!(!yaml.contains("claude.ai/install.sh"));
        assert!(!yaml.contains(".claude.json"));
    }

    #[test]
    fn test_generate_lima_config_pi_agent() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "pi", true).unwrap();

        assert!(yaml.contains("@mariozechner/pi-coding-agent"));
        assert!(yaml.contains("npm install -g"));
        assert!(!yaml.contains("claude.ai/install.sh"));
        assert!(!yaml.contains(".claude.json"));
    }

    #[test]
    fn test_generate_lima_config_unknown_agent() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml = generate_lima_config("test-vm", &mounts, &sandbox_config, "custom-agent", true)
            .unwrap();

        // Should have a comment about no built-in script
        assert!(yaml.contains("No built-in install script for agent: custom-agent"));
        assert!(!yaml.contains("claude.ai/install.sh"));

        // Common infrastructure should still be present
        assert!(yaml.contains("workmux/main/scripts/install.sh"));
    }

    #[test]
    fn test_generate_lima_config_claude_includes_config_symlink() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", true).unwrap();

        // Claude agent should include config symlink
        assert!(
            yaml.contains(r#"ln -sfn "$HOME/.workmux-state/.claude.json" "$HOME/.claude.json""#)
        );
    }

    #[test]
    fn test_generate_lima_config_no_nix_when_not_needed() {
        let mounts = vec![Mount::rw(PathBuf::from("/tmp/test"))];
        let sandbox_config = SandboxConfig::default();
        let yaml =
            generate_lima_config("test-vm", &mounts, &sandbox_config, "claude", false).unwrap();

        // Should NOT install Nix or Devbox
        assert!(!yaml.contains("install.determinate.systems/nix"));
        assert!(!yaml.contains("get.jetify.com/devbox"));

        // Should still install agent and workmux
        assert!(yaml.contains("claude.ai/install.sh"));
        assert!(yaml.contains("workmux/main/scripts/install.sh"));
    }

    #[test]
    fn test_lima_install_script_for_agent_claude() {
        let script = lima_install_script_for_agent("claude");
        assert!(script.contains("claude.ai/install.sh"));
        assert!(script.contains(".claude.json"));
    }

    #[test]
    fn test_lima_install_script_for_agent_codex() {
        let script = lima_install_script_for_agent("codex");
        assert!(script.contains("openai/codex/releases"));
        assert!(script.contains("tar xz"));
    }

    #[test]
    fn test_lima_install_script_for_agent_gemini() {
        let script = lima_install_script_for_agent("gemini");
        assert!(script.contains("nodesource.com"));
        assert!(script.contains("@google/gemini-cli"));
    }

    #[test]
    fn test_lima_install_script_for_agent_opencode() {
        let script = lima_install_script_for_agent("opencode");
        assert!(script.contains("opencode.ai/install"));
    }

    #[test]
    fn test_lima_install_script_for_agent_pi() {
        let script = lima_install_script_for_agent("pi");
        assert!(script.contains("@mariozechner/pi-coding-agent"));
        assert!(script.contains("npm install -g"));
    }

    #[test]
    fn test_lima_install_script_for_agent_unknown() {
        let script = lima_install_script_for_agent("my-custom-agent");
        assert!(script.contains("No built-in install script"));
        assert!(script.contains("my-custom-agent"));
    }
}
