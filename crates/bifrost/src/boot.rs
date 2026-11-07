//! `bifrost --boot`: boots a VM in the foreground (for debugging). bifrost decides the
//! VM's configuration and replaces itself with midgard, which runs the VM.

use std::convert::Infallible;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use midgard_protocol::MachineConfig;

use crate::cli::Cli;

/// The guest's RAM unless --memory says otherwise.
const DEFAULT_MEMORY: u64 = 1 << 30;
/// The configuration goes through a pipe that midgard only reads after we exec it, so it
/// must fit into the pipe's buffer (at least a page on the systems that we run on).
const MAX_CONFIG: usize = 4096;

/// The VM that the command line describes.
fn config(cli: &Cli) -> MachineConfig {
    let boot = &cli.boot;
    MachineConfig {
        kernel: boot
            .kernel
            .clone()
            .expect("clap requires --kernel with --boot"),
        initrd: boot.initrd.clone(),
        cmdline: boot.cmdline.clone(),
        append: boot.append.clone(),
        memory: cli.memory.unwrap_or(DEFAULT_MEMORY),
    }
}

/// midgard's executable: `midgard` next to ours.
fn midgard_executable() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("failed to find our executable")?;
    Ok(executable.with_file_name("midgard"))
}

/// Replaces bifrost with midgard, which boots the VM; returns only if that fails.
pub fn boot(cli: &Cli) -> Result<Infallible> {
    let mut message = Vec::new();
    midgard_protocol::write_message(&mut message, &config(cli))?;
    if message.len() > MAX_CONFIG {
        bail!("the VM configuration is too large");
    }
    let (reader, mut writer) = io::pipe().context("failed to create a pipe")?;
    writer
        .write_all(&message)
        .context("failed to write the VM configuration")?;
    drop(writer);
    // midgard inherits the read end (pipes are created close-on-exec).
    let fd = reader.as_raw_fd();
    // SAFETY: fcntl() only takes a file descriptor (which we own).
    if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } < 0 {
        return Err(io::Error::last_os_error()).context("failed to pass the pipe to midgard");
    }
    let midgard = midgard_executable()?;
    let error = Command::new(&midgard)
        .args(["boot", "--config-fd", &fd.to_string()])
        .exec();
    Err(error).with_context(|| format!("failed to run {}", midgard.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let cli = Cli::try_parse_args(["bifrost", "--boot", "--kernel", "bzImage"]).unwrap();
        let config = config(&cli);
        assert_eq!(config.kernel, PathBuf::from("bzImage"));
        assert_eq!(config.initrd, None);
        assert_eq!(config.cmdline, None);
        assert!(config.append.is_empty());
        assert_eq!(config.memory, DEFAULT_MEMORY);
    }
}
