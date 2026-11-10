//! `bifrost boot`: boots a VM in the foreground (for debugging). bifrost decides the
//! VM's configuration and asks midgard (on midgard's socket) to run it.

use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use midgard_protocol::control::{Request, Response};
use midgard_protocol::guest::GuestConfig;
use midgard_protocol::machine::{ConsoleKind, Disk, MachineConfig};

use crate::cli::{BootArgs, Console};
use crate::client;

/// The guest's RAM unless --memory says otherwise.
const DEFAULT_MEMORY: u64 = 1 << 30;
/// The default number of vCPUs: the host's CPUs, up to this many.
const DEFAULT_MAX_CPUS: u32 = 8;

/// The host's CPUs, up to [`DEFAULT_MAX_CPUS`].
fn default_cpus() -> u32 {
    let host = std::thread::available_parallelism().map_or(1, |n| n.get());
    (host as u32).clamp(1, DEFAULT_MAX_CPUS)
}

/// `path` for midgard, which does not run in our working directory.
fn absolute(path: &Path) -> Result<std::path::PathBuf> {
    std::path::absolute(path).with_context(|| format!("invalid path {}", path.display()))
}

/// The VM that the command line describes.
fn config(boot: &BootArgs) -> Result<MachineConfig> {
    let disks = boot
        .disk
        .iter()
        .map(|disk| {
            Ok(Disk {
                path: absolute(&disk.path)?,
                read_only: disk.read_only,
            })
        })
        .collect::<Result<_>>()?;
    Ok(MachineConfig {
        kernel: absolute(&boot.kernel)?,
        initrd: boot.initrd.as_deref().map(absolute).transpose()?,
        agent: boot.agent.as_deref().map(absolute).transpose()?,
        cmdline: boot.cmdline.clone(),
        append: boot.append.clone(),
        memory: boot.memory.unwrap_or(DEFAULT_MEMORY),
        cpus: boot.cpus.unwrap_or_else(default_cpus),
        console: match boot.console {
            Console::Virtio => ConsoleKind::Virtio,
            Console::Serial => ConsoleKind::Serial,
        },
        disks,
        network: !boot.no_network,
        share: match (&boot.share, boot.no_share) {
            (_, true) => None,
            (Some(share), false) => Some(absolute(share)?),
            (None, false) => Some("/".into()),
        },
        // The agent stays PID 1.
        guest: GuestConfig::default(),
        waypipe: None,
    })
}

/// Asks midgard to boot the VM and prints its console until it stops (or we are killed,
/// which stops it).
pub fn boot(args: &BootArgs) -> Result<()> {
    let config = config(args)?;
    let mut stream = client::connect_daemon()?;
    midgard_protocol::write_message(&mut stream, &Request::Boot(Box::new(config)))?;
    let mut stdout = io::stdout().lock();
    loop {
        match midgard_protocol::read_message(&mut stream)? {
            Some(Response::Console(data)) => {
                stdout.write_all(&data)?;
                stdout.flush()?;
            }
            Some(Response::Stopped(reason)) => {
                eprintln!("bifrost: {reason}");
                return Ok(());
            }
            Some(Response::Error(error)) => bail!("{error}"),
            Some(other) => bail!("unexpected answer from midgard: {other:?}"),
            None => bail!("midgard closed the connection"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Action, Cli};

    #[test]
    fn defaults() {
        let cli = Cli::try_parse_args(["bifrost", "boot", "--kernel", "bzImage"]).unwrap();
        let Some(Action::Boot(boot)) = cli.action else {
            panic!("not boot: {:?}", cli.action);
        };
        let config = config(&boot).unwrap();
        assert!(config.kernel.is_absolute());
        assert!(config.kernel.ends_with("bzImage"));
        assert_eq!(config.initrd, None);
        assert_eq!(config.agent, None);
        assert_eq!(config.cmdline, None);
        assert!(config.append.is_empty());
        assert_eq!(config.memory, DEFAULT_MEMORY);
        assert!((1..=DEFAULT_MAX_CPUS).contains(&config.cpus));
        assert_eq!(config.console, ConsoleKind::Virtio);
        assert!(config.disks.is_empty());
        assert!(config.network);
        assert_eq!(config.share, Some("/".into()));
    }
}
