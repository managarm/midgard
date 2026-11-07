//! A vCPU: the thread that runs it (running blocks, also while the guest halts) and
//! handles its exits.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use midgard_hv::x86::{EFER_LMA, msr};
use midgard_hv::{Completion, Exit, MmioAccess, Msrs, Regs};

use crate::clock;
use crate::log;
use crate::logging::ReportOnce;
use crate::vm::{Stop, Vm};

mod cpuid;
mod kvmclock;

use kvmclock::Kvmclock;

/// KVM's hypercall error for unknown hypercalls.
const KVM_ENOSYS: i64 = -1000;

/// Fast strings are enabled, PEBS and BTS are unavailable.
const MISC_ENABLE_DEFAULT: u64 = 1 | 1 << 11 | 1 << 12;
const PAT_DEFAULT: u64 = 0x0007_0406_0007_0406;
/// The microcode revision (in the high half), like KVM.
const MICROCODE_REVISION: u64 = 0x1_0000_0000;

/// What the guest did that we do not support (reported once, see [`ReportOnce`]).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Unsupported {
    Msr(u32),
    Hypercall(u64),
}

pub struct Vcpu {
    id: u32,
    backend: midgard_hv::Vcpu,
    vm: Arc<Vm>,
    kvmclock: Kvmclock,
    /// MSRs that we only store.
    msrs: BTreeMap<u32, u64>,
    reported: ReportOnce<Unsupported>,
}

impl Vcpu {
    /// Creates the vCPU. vCPU 0 is the BSP; the others wait for a startup IPI.
    pub fn new(vm: Arc<Vm>, id: u32) -> Result<Self> {
        let backend = vm
            .hypervisor()
            .create_vcpu(id, &cpuid::table(id, vm.num_cpus))?;
        vm.add_kicker(backend.kicker());
        let kvmclock = Kvmclock::new(vm.tsc_hz);
        Ok(Self {
            id,
            backend,
            vm,
            kvmclock,
            msrs: BTreeMap::new(),
            reported: ReportOnce::new(),
        })
    }

    pub fn set_registers(&mut self, regs: &Regs) -> Result<()> {
        self.backend.set_regs(regs)
    }

    /// Runs the vCPU until the guest stops it.
    pub fn run(&mut self) -> Result<Stop> {
        let mut completion = None;
        loop {
            let exit = self.backend.run(completion.take())?;
            match self.handle_exit(exit)? {
                Handled::Continue => {}
                Handled::Complete(result) => completion = Some(result),
                Handled::Stop(stop) => {
                    self.vm.flush_console();
                    return Ok(stop);
                }
            }
        }
    }

    fn handle_exit(&mut self, exit: Exit) -> Result<Handled> {
        Ok(match exit {
            Exit::Io {
                port,
                size,
                input,
                string,
                data,
            } => self.port_io(port, size, input, string, data)?,
            Exit::Rdmsr { index } => Handled::Complete(self.rdmsr(index)?),
            Exit::Wrmsr { index, value } => Handled::Complete(self.wrmsr(index, value)?),
            // E.g., a signal interrupted the run, or the VM stops.
            Exit::Kicked if self.vm.stopping() => Handled::Stop(Stop::Requested),
            Exit::Kicked => {
                // A failure was reported when the guest set the MSR.
                let _ = self.kvmclock.update(&self.vm.memory);
                Handled::Continue
            }
            Exit::Shutdown => {
                log!("vCPU {}: triple fault", self.id);
                Handled::Stop(Stop::Reset)
            }
            Exit::Mmio { address, access } => {
                self.mmio(address, access)?;
                Handled::Continue
            }
            Exit::Hypercall => Handled::Complete(self.hypercall()?),
            Exit::Failed(reason) => {
                let rip = self.backend.basic_regs().map(|regs| regs.rip).unwrap_or(0);
                bail!("vCPU {}: {reason} at RIP {rip:#x}", self.id);
            }
        })
    }

    /// Handles an access to device memory that the backend decoded.
    fn mmio(&mut self, address: u64, access: Option<MmioAccess>) -> Result<()> {
        let Some(access) = access else {
            let rip = self.backend.basic_regs()?.rip;
            bail!(
                "vCPU {}: cannot emulate the access to {address:#x} at RIP {rip:#x}",
                self.id
            );
        };
        if access.write {
            self.vm.mmio_write(address, access.size, access.data);
        } else {
            let value = self.vm.mmio_read(address, access.size);
            self.backend.complete_mmio_read(value)?;
        }
        Ok(())
    }

    /// KVM hypercalls (the number is in RAX). We implement none of them; like KVM, we
    /// return -KVM_ENOSYS.
    fn hypercall(&mut self) -> Result<Completion> {
        let number = self.backend.basic_regs()?.rax;
        if self.reported.first(Unsupported::Hypercall(number)) {
            log!("vCPU {}: unsupported hypercall {number}", self.id);
        }
        Ok(Completion::Done(KVM_ENOSYS as u64))
    }

    fn port_io(
        &mut self,
        port: u16,
        size: u8,
        input: bool,
        string: bool,
        data: u32,
    ) -> Result<Handled> {
        if string {
            let rip = self.backend.basic_regs()?.rip;
            bail!(
                "vCPU {}: string I/O to port {port:#x} at RIP {rip:#x} is not supported",
                self.id
            );
        }
        let size = size as u32;
        if input {
            return Ok(Handled::Complete(Completion::Done(
                self.vm.pio_in(port, size) as u64,
            )));
        }
        let mask = if size >= 4 {
            u32::MAX
        } else {
            (1u32 << (8 * size)) - 1
        };
        Ok(match self.vm.pio_out(port, size, data & mask) {
            Some(stop) => Handled::Stop(stop),
            None => Handled::Complete(Completion::Done(0)),
        })
    }

    fn report_msr(&mut self, index: u32, what: &str) {
        if self.reported.first(Unsupported::Msr(index)) {
            log!("vCPU {}: #GP on {what} of MSR {index:#x}", self.id);
        }
    }

    fn rdmsr(&mut self, index: u32) -> Result<Completion> {
        let value = match index {
            msr::EFER => Some(self.backend.regs()?.efer),
            msr::FS_BASE => Some(self.backend.regs()?.fs.base),
            msr::GS_BASE => Some(self.backend.regs()?.gs.base),
            // The MSRs that the hypervisor switches.
            i if Msrs::INDICES.contains(&i) => self.backend.msrs()?.get(i),
            msr::TSC => Some(clock::rdtsc()),
            msr::KVM_SYSTEM_TIME | msr::KVM_SYSTEM_TIME_NEW => Some(self.kvmclock.msr()),
            msr::KVM_WALL_CLOCK | msr::KVM_WALL_CLOCK_NEW => {
                Some(self.msrs.get(&index).copied().unwrap_or(0))
            }
            msr::MISC_ENABLE => Some(
                self.msrs
                    .get(&index)
                    .copied()
                    .unwrap_or(MISC_ENABLE_DEFAULT),
            ),
            msr::PAT => Some(self.msrs.get(&index).copied().unwrap_or(PAT_DEFAULT)),
            // Locked, VMX disabled.
            msr::FEATURE_CONTROL => Some(1),
            msr::BIOS_SIGN_ID => Some(MICROCODE_REVISION),
            msr::MISC_FEATURES_ENABLES => Some(0),
            _ => None,
        };
        Ok(match value {
            Some(value) => Completion::Done(value),
            None => {
                self.report_msr(index, "read");
                Completion::Fault
            }
        })
    }

    fn wrmsr(&mut self, index: u32, value: u64) -> Result<Completion> {
        let ok = match index {
            msr::EFER => {
                let mut full = self.backend.regs()?;
                // LMA is read-only; the CPU sets it when the guest enables paging.
                full.efer = (value & !EFER_LMA) | (full.efer & EFER_LMA);
                self.backend.set_regs(&full)?;
                true
            }
            msr::FS_BASE | msr::GS_BASE => {
                let mut full = self.backend.regs()?;
                if index == msr::FS_BASE {
                    full.fs.base = value;
                } else {
                    full.gs.base = value;
                }
                self.backend.set_regs(&full)?;
                true
            }
            i if Msrs::INDICES.contains(&i) => {
                let mut msrs = self.backend.msrs()?;
                if let Some(field) = msrs.get_mut(i) {
                    *field = value;
                }
                self.backend.set_msrs(&msrs)?;
                true
            }
            // We do not offset the TSC.
            msr::TSC => true,
            msr::KVM_SYSTEM_TIME | msr::KVM_SYSTEM_TIME_NEW => {
                if let Err(error) = self.kvmclock.set_msr(&self.vm.memory, value) {
                    log!("vCPU {}: kvmclock: {error}", self.id);
                }
                true
            }
            msr::KVM_WALL_CLOCK | msr::KVM_WALL_CLOCK_NEW => {
                self.msrs.insert(index, value);
                if let Err(error) = kvmclock::write_wall_clock(&self.vm.memory, value) {
                    log!("vCPU {}: wall clock: {error}", self.id);
                }
                true
            }
            msr::MISC_ENABLE | msr::PAT => {
                self.msrs.insert(index, value);
                true
            }
            // Microcode updates trigger CPUID to update the revision; it does not change.
            msr::BIOS_SIGN_ID => true,
            // CPUID faulting is not supported.
            msr::MISC_FEATURES_ENABLES => value == 0,
            _ => false,
        };
        Ok(if ok {
            Completion::Done(0)
        } else {
            self.report_msr(index, "write");
            Completion::Fault
        })
    }
}

/// What to do after an exit.
enum Handled {
    Continue,
    /// Run with this completion.
    Complete(Completion),
    Stop(Stop),
}
