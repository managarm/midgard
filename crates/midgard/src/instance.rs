//! VM instances: a disk in the state directory (`<name>.qcow2` or `<name>.ext4`, see
//! [`paths::state_dir`]) that midgard boots with its guest kernel and the agent, and an
//! optional instance file next to it (`<name>.toml`) with the instance's settings:
//!
//! ```toml
//! [guest]
//! # The distribution's init, which the agent starts as PID 1 (default: none, the agent
//! # stays PID 1).
//! init = "/sbin/init"
//! # How sessions wait for the guest to boot: "none" (default), "systemd" (until
//! # `systemctl is-system-running --wait` returns) or "notify" (until the guest runs
//! # `/run/midgard/midgard-init ready`).
//! ready = "systemd"
//! # The user that sessions run as unless bifrost names one (default: root).
//! user = "alice"
//! # A command that the agent runs on every boot (as root, before the distribution's
//! # init) with the user's name as an additional argument, e.g., to create the user.
//! user-setup = ["/etc/midgard/user-setup"]
//! # How the agent runs the commands of sessions: "direct" (default: the agent runs
//! # them itself), "systemd" (as transient services of the user's systemd instance) or
//! # "wrapper" (through the command `wrapper`, which runs `{helper}` in a session of
//! # `{user}`).
//! launcher = "wrapper"
//! wrapper = ["runuser", "-l", "{user}", "-s", "{helper}"]
//! ```
//!
//! The settings describe the disk's contents, so they will be part of the templates that
//! instances are created from.

use std::path::Path;

use anyhow::{Context, Result, bail};
use midgard_protocol::guest::{GuestConfig, Launcher, Ready};
use midgard_protocol::machine::{Disk, MachineConfig};
use serde::Deserialize;

use crate::paths;

/// The longest name of an instance.
const MAX_NAME: usize = 64;
/// The guest's RAM.
const DEFAULT_MEMORY: u64 = 1 << 30;
/// The default number of vCPUs: the host's CPUs, up to this many.
const DEFAULT_MAX_CPUS: u32 = 8;

/// Checks the name of an instance: it names files, so it consists of letters, digits,
/// `-` and `_` and does not start with `-`.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!(
            "invalid name {name:?} (use up to {MAX_NAME} letters, digits, '-' and '_', \
             not starting with '-')"
        );
    }
    Ok(())
}

/// The host's CPUs, up to [`DEFAULT_MAX_CPUS`].
fn default_cpus() -> u32 {
    let host = std::thread::available_parallelism().map_or(1, |n| n.get());
    (host as u32).clamp(1, DEFAULT_MAX_CPUS)
}

/// The contents of an instance file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceFile {
    #[serde(default)]
    guest: GuestSettings,
}

/// How the agent sets up the guest (see [`GuestConfig`]).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct GuestSettings {
    init: Option<String>,
    ready: Option<ReadySetting>,
    user: Option<String>,
    user_setup: Option<Vec<String>>,
    launcher: Option<LauncherSetting>,
    wrapper: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ReadySetting {
    None,
    Systemd,
    Notify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LauncherSetting {
    Direct,
    Systemd,
    Wrapper,
}

impl GuestSettings {
    fn to_config(&self) -> Result<GuestConfig> {
        let ready = match self.ready {
            None | Some(ReadySetting::None) => Ready::Immediately,
            Some(ReadySetting::Systemd) => Ready::Systemd,
            Some(ReadySetting::Notify) => Ready::Notify,
        };
        if let Some(init) = &self.init
            && !init.starts_with('/')
        {
            bail!("init must be an absolute path (not {init:?})");
        }
        if self.init.is_none() && ready != Ready::Immediately {
            bail!("ready needs init");
        }
        if let Some(user_setup) = &self.user_setup {
            if user_setup.is_empty() {
                bail!("user-setup must not be empty");
            }
            if self.user.is_none() {
                bail!("user-setup needs user");
            }
        }
        let launcher = match (self.launcher, &self.wrapper) {
            (Some(LauncherSetting::Wrapper), None) => bail!("launcher = \"wrapper\" needs wrapper"),
            (Some(LauncherSetting::Wrapper), Some(wrapper)) => {
                if !wrapper.iter().any(|arg| arg.contains("{helper}")) {
                    bail!("wrapper must run {{helper}}");
                }
                Launcher::Wrapper(wrapper.clone())
            }
            (_, Some(_)) => bail!("wrapper needs launcher = \"wrapper\""),
            (Some(LauncherSetting::Systemd), None) => {
                if self.init.is_none() {
                    bail!("launcher = \"systemd\" needs init");
                }
                Launcher::Systemd
            }
            (None | Some(LauncherSetting::Direct), None) => Launcher::Direct,
        };
        Ok(GuestConfig {
            init: self.init.clone(),
            ready,
            user: self.user.clone(),
            user_setup: self.user_setup.clone(),
            launcher,
        })
    }
}

/// Parses an instance file.
fn parse_instance_file(text: &str) -> Result<GuestConfig> {
    let file: InstanceFile = toml::from_str(text)?;
    file.guest.to_config()
}

/// The guest configuration from the instance file at `path` (the default without one).
fn guest_config(path: &Path) -> Result<GuestConfig> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_instance_file(&text).with_context(|| format!("invalid {path:?}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(GuestConfig::default()),
        Err(error) => Err(error).with_context(|| format!("failed to read {path:?}")),
    }
}

/// The VM of the instance `name`.
pub fn config(name: &str) -> Result<MachineConfig> {
    validate_name(name)?;
    let Some(disk) = paths::disk_candidates(name).find(|disk| disk.exists()) else {
        let expected: Vec<_> = paths::disk_candidates(name)
            .map(|disk| disk.display().to_string())
            .collect();
        bail!(
            "no VM instance {name} (its disk would be {})",
            expected.join(" or ")
        );
    };
    Ok(MachineConfig {
        kernel: paths::kernel(),
        initrd: None,
        agent: Some(paths::agent()),
        cmdline: None,
        append: Vec::new(),
        memory: DEFAULT_MEMORY,
        cpus: default_cpus(),
        disks: vec![Disk {
            path: disk,
            read_only: false,
        }],
        network: true,
        share: Some("/".into()),
        guest: guest_config(&paths::instance_file(name))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_files() {
        assert_eq!(parse_instance_file("").unwrap(), GuestConfig::default());
        let config = parse_instance_file(
            r#"
            [guest]
            init = "/sbin/init"
            ready = "notify"
            "#,
        )
        .unwrap();
        assert_eq!(config.init.as_deref(), Some("/sbin/init"));
        assert_eq!(config.ready, Ready::Notify);
        assert_eq!(config.launcher, Launcher::Direct);
        let config = parse_instance_file(
            r#"
            [guest]
            user = "alice"
            launcher = "wrapper"
            wrapper = ["runuser", "-l", "{user}", "-s", "{helper}"]
            "#,
        )
        .unwrap();
        assert_eq!(config.user.as_deref(), Some("alice"));
        assert!(matches!(config.launcher, Launcher::Wrapper(argv) if argv.len() == 5));
        for invalid in [
            "[guest]\ninit = \"sbin/init\"",
            "[guest]\nready = \"systemd\"",
            "[guest]\nready = \"soon\"",
            "[guest]\nlauncher = \"systemd\"",
            "[guest]\nlauncher = \"wrapper\"",
            "[guest]\nlauncher = \"wrapper\"\nwrapper = [\"su\"]",
            "[guest]\nwrapper = [\"{helper}\"]",
            "[guest]\nuser = \"alice\"\nuser-setup = []",
            "[guest]\nuser-setup = [\"/setup\"]",
            "[guest]\nunknown = 1",
            "[other]",
        ] {
            assert!(parse_instance_file(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn names() {
        assert!(validate_name("alpine").is_ok());
        assert!(validate_name("dev_2-x").is_ok());
        for name in ["", "-x", "../etc", "a/b", "a b", &"x".repeat(MAX_NAME + 1)] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }
}
