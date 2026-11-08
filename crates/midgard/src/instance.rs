//! VM instances: a disk in the state directory (`<name>.qcow2` or `<name>.ext4`, see
//! [`paths::state_dir`]) that midgard boots with its guest kernel and the agent.

use anyhow::{Result, bail};
use midgard_protocol::guest::GuestConfig;
use midgard_protocol::machine::{Disk, MachineConfig};

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
        guest: GuestConfig::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("alpine").is_ok());
        assert!(validate_name("dev_2-x").is_ok());
        for name in ["", "-x", "../etc", "a/b", "a b", &"x".repeat(MAX_NAME + 1)] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
    }
}
