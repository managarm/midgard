//! midgard: runs Linux in lightweight VMs on managarm (see [`midgard_vmm`]). bifrost
//! decides how a VM looks and launches midgard with its configuration (see
//! [`midgard_protocol`]); `bifrost --boot` runs a VM in the foreground.

use std::fs::File;
use std::os::fd::{FromRawFd, RawFd};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use midgard_protocol::MachineConfig;
use midgard_vmm::{log, machine};

/// Runs Linux VMs (bifrost launches it).
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot a VM in the foreground (for `bifrost --boot`)
    Boot {
        /// The file descriptor on which bifrost passes the VM's configuration
        #[arg(long, value_name = "FD", value_parser = clap::value_parser!(RawFd).range(3..))]
        config_fd: RawFd,
    },
}

/// Reads the VM's configuration from the file descriptor that bifrost passed.
fn read_config(fd: RawFd) -> Result<MachineConfig> {
    // SAFETY: bifrost passes the file descriptor to us; nothing else uses it.
    let mut file = unsafe { File::from_raw_fd(fd) };
    midgard_protocol::read_message(&mut file)
        .context("failed to read the VM configuration")?
        .context("bifrost did not pass a VM configuration")
}

/// Boots a VM with its console on stdin / stdout.
fn boot(config_fd: RawFd) -> Result<()> {
    let config = read_config(config_fd)?;
    let machine = machine::start(&config, Box::new(std::io::stdout()))?;
    let vm = machine.vm.clone();
    let result = machine.run();
    vm.flush_console();
    log!("{}", machine::describe(result?));
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Boot { config_fd } => boot(config_fd),
    };
    if let Err(error) = result {
        log!("{error:#}");
        std::process::exit(1);
    }
}
