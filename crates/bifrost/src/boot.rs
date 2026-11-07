//! `bifrost boot`: boots a VM in the foreground (for debugging). bifrost decides the
//! VM's configuration and asks midgard (on midgard's socket) to run it.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, bail};
use midgard_protocol::control::{Request, Response};
use midgard_protocol::machine::MachineConfig;
use midgard_protocol::runtime;

use crate::cli::BootArgs;

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
    Ok(MachineConfig {
        kernel: absolute(&boot.kernel)?,
        initrd: boot.initrd.as_deref().map(absolute).transpose()?,
        cmdline: boot.cmdline.clone(),
        append: boot.append.clone(),
        memory: boot.memory.unwrap_or(DEFAULT_MEMORY),
        cpus: boot.cpus.unwrap_or_else(default_cpus),
    })
}

/// Asks midgard to boot the VM and prints its console until it stops (or we are killed,
/// which stops it).
pub fn boot(args: &BootArgs) -> Result<()> {
    let config = config(args)?;
    let socket = runtime::daemon_socket();
    let mut stream = UnixStream::connect(&socket).with_context(|| {
        format!(
            "failed to connect to midgard at {} (is midgard.socket running?)",
            socket.display()
        )
    })?;
    midgard_protocol::write_message(&mut stream, &Request::Boot(config))?;
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
        assert_eq!(config.cmdline, None);
        assert!(config.append.is_empty());
        assert_eq!(config.memory, DEFAULT_MEMORY);
        assert!((1..=DEFAULT_MAX_CPUS).contains(&config.cpus));
    }
}
