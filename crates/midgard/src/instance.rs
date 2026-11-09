//! VM instances: a directory in midgard's instances directory (see
//! [`paths::instances_dir`]) with the instance's settings (`instance.toml`, see
//! [`crate::settings`]) and its disk (`disk.qcow2`), which midgard boots with its guest
//! kernel and the agent.

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use midgard_protocol::machine::{ConsoleKind, Disk, MachineConfig};

use crate::paths;
use crate::settings::{self, Settings};

/// The longest name of an instance.
const MAX_NAME: usize = 64;
/// The guest's RAM unless the instance's settings say otherwise.
const DEFAULT_MEMORY: u64 = 1 << 30;

/// The instance file in an instance's directory.
const INSTANCE_FILE: &str = "instance.toml";
/// The disk in an instance's directory.
const DISK: &str = "disk.qcow2";

/// Checks the name of an instance: it names a directory, so it consists of letters,
/// digits, `-` and `_` and does not start with `-`.
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

/// A VM instance.
pub struct Instance {
    /// The instance's directory.
    dir: PathBuf,
    settings: Settings,
}

impl Instance {
    /// Reads the instance `name`.
    pub fn load(name: &str) -> Result<Self> {
        validate_name(name)?;
        let dir = paths::instances_dir().join(name);
        let path = dir.join(INSTANCE_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound && !dir.is_dir() => {
                bail!(
                    "no VM instance {name} (in {})",
                    paths::instances_dir().display()
                )
            }
            Err(error) => return Err(error).with_context(|| format!("failed to read {path:?}")),
        };
        let settings = settings::parse(&text).with_context(|| format!("invalid {path:?}"))?;
        Ok(Self { dir, settings })
    }

    /// The instance's disk.
    pub fn disk(&self) -> PathBuf {
        self.dir.join(DISK)
    }

    /// The instance's VM.
    pub fn config(&self) -> MachineConfig {
        MachineConfig {
            kernel: paths::kernel(),
            initrd: None,
            agent: Some(paths::agent()),
            cmdline: None,
            append: Vec::new(),
            memory: self.settings.memory.unwrap_or(DEFAULT_MEMORY),
            cpus: None,
            console: ConsoleKind::Virtio,
            disks: vec![Disk {
                path: self.disk(),
                read_only: false,
            }],
            network: true,
            share: Some("/".into()),
            guest: self.settings.guest.clone(),
            waypipe: paths::guest_waypipe(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("alpine").is_ok());
        assert!(validate_name("dev_2-x").is_ok());
        for name in [
            "",
            "-x",
            "../etc",
            "a/b",
            "a b",
            "a.b",
            &"x".repeat(MAX_NAME + 1),
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }
}
