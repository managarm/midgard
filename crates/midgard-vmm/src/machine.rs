//! Setting up and starting a VM.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use midgard_protocol::MachineConfig;

use crate::log;
use crate::vm::{Stop, Vm};
use crate::{acpi, boot, clock, platform, vcpu};

/// Kernel parameters that every guest needs (unless the configuration replaces them);
/// the console is added.
const BASE_CMDLINE: &str = "pci=off reboot=k panic=-1";

/// Checks that midgard can run the VM.
fn validate(config: &MachineConfig) -> Result<()> {
    if !config.memory.is_multiple_of(4096) || config.memory < (64 << 20) {
        bail!("guest memory must be a multiple of 4 KiB and at least 64 MiB");
    }
    if config.memory > platform::MAX_LOW_RAM {
        bail!(
            "guest memory above {:#x} is not supported yet",
            platform::MAX_LOW_RAM
        );
    }
    Ok(())
}

/// The guest kernel's command line.
fn kernel_cmdline(config: &MachineConfig) -> String {
    let base = config.cmdline.as_deref().unwrap_or(BASE_CMDLINE);
    let mut cmdline = vec![base.to_string(), "console=ttyS0".to_string()];
    cmdline.extend(config.append.iter().cloned());
    cmdline.join(" ")
}

fn read(path: &Path, what: &str) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read {what} {path:?}"))
}

/// A VM with a single vCPU, which [`Machine::run`] runs on the calling thread.
pub struct Machine {
    pub vm: Arc<Vm>,
    vcpu: vcpu::Vcpu,
}

impl Machine {
    /// Runs the vCPU until the guest stops it.
    pub fn run(mut self) -> Result<Stop> {
        self.vcpu.run()
    }
}

/// Starts a VM whose serial console writes to `console`.
pub fn start(config: &MachineConfig, console: Box<dyn Write + Send>) -> Result<Machine> {
    validate(config)?;
    let cmdline = kernel_cmdline(config);
    let kernel = read(&config.kernel, "the kernel")?;
    let initrd = match &config.initrd {
        Some(path) => Some(read(path, "the initrd")?),
        None => None,
    };

    let tsc_hz = clock::calibrate();
    log!("TSC runs at {} kHz", tsc_hz / 1000);

    let (hypervisor, memory) = midgard_hv::create(config.memory)?;
    let entry = boot::load(&memory, &kernel, initrd.as_deref(), &cmdline)?;
    drop((kernel, initrd));
    acpi::write(&memory, platform::ACPI_TABLES, 1).context("failed to write the ACPI tables")?;

    let vm = Arc::new(Vm::new(memory, 1, tsc_hz, hypervisor, console));
    let mut vcpu = vcpu::Vcpu::new(vm.clone(), 0)?;
    vcpu.set_registers(&boot::entry_registers(&entry))?;

    log!(
        "booting {} with {} MiB of memory: {cmdline}",
        config.kernel.display(),
        config.memory >> 20,
    );
    Ok(Machine { vm, vcpu })
}

/// Describes why the VM stopped.
pub fn describe(stop: Stop) -> &'static str {
    match stop {
        Stop::Reset => "the guest reset the machine",
        Stop::PowerOff => "the guest powered off",
        Stop::Requested => "the VM was stopped",
    }
}
