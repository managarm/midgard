//! `bifrost --boot`: boots a VM in the foreground (for debugging). bifrost decides the
//! VM's configuration and asks midgard (on midgard's socket) to run it.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{Context, Result, bail};
use midgard_protocol::control::{Request, Response};
use midgard_protocol::{MachineConfig, runtime};

use crate::cli::Cli;

/// The guest's RAM unless --memory says otherwise.
const DEFAULT_MEMORY: u64 = 1 << 30;

/// `path` for midgard, which does not run in our working directory.
fn absolute(path: &Path) -> Result<std::path::PathBuf> {
    std::path::absolute(path).with_context(|| format!("invalid path {}", path.display()))
}

/// The VM that the command line describes.
fn config(cli: &Cli) -> Result<MachineConfig> {
    let boot = &cli.boot;
    let kernel = boot
        .kernel
        .as_deref()
        .expect("clap requires --kernel with --boot");
    Ok(MachineConfig {
        kernel: absolute(kernel)?,
        initrd: boot.initrd.as_deref().map(absolute).transpose()?,
        cmdline: boot.cmdline.clone(),
        append: boot.append.clone(),
        memory: cli.memory.unwrap_or(DEFAULT_MEMORY),
    })
}

/// Asks midgard to boot the VM and prints its console until it stops (or we are killed,
/// which stops it).
pub fn boot(cli: &Cli) -> Result<()> {
    let config = config(cli)?;
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

    #[test]
    fn defaults() {
        let cli = Cli::try_parse_args(["bifrost", "--boot", "--kernel", "bzImage"]).unwrap();
        let config = config(&cli).unwrap();
        assert!(config.kernel.is_absolute());
        assert!(config.kernel.ends_with("bzImage"));
        assert_eq!(config.initrd, None);
        assert_eq!(config.cmdline, None);
        assert!(config.append.is_empty());
        assert_eq!(config.memory, DEFAULT_MEMORY);
    }
}
