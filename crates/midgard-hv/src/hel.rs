//! The native backend: thor's virtualization API. Thor emulates the local APIC (of a
//! single vCPU); the I/O APIC and CPUID are emulated here, and so are the MMIO
//! instructions (thor reports the faulting address only).

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use hel_sys::{
    HelVmexitCompletion, HelX86DescriptorTable, HelX86SegmentRegister,
    HelX86VirtualizationBasicRegs, HelX86VirtualizationMsrs, HelX86VirtualizationRegs,
};

use crate::counters::{ExitCounts, kinds};
use crate::decode::{self, Instruction, Operation};
use crate::ioapic::{self, Interrupt, Ioapic};
use crate::x86::cpuid::LEAF1_ECX_OSXSAVE;
use crate::x86::{CR4_OSXSAVE, IOAPIC_BASE, xsave_compacted_size, xsave_size};
use crate::{
    BasicRegs, Completion, CountersOps, CpuidEntry, DescriptorTable, Exit, GuestMemory,
    HypervisorOps, KickerOps, Msrs, Regs, Segment, VcpuOps,
};

/// Thor does not support more than one vCPU per VM yet.
pub const MAX_VCPUS: u32 = 1;

/// The size of the I/O APIC's MMIO window.
const IOAPIC_SIZE: u64 = 0x100;

/// The mapping of guest RAM into our address space, which [`GuestMemory`] accesses. The
/// VM maps the memory object itself (see [`Hypervisor`]), so it does not depend on this
/// mapping.
pub(crate) struct Mapping {
    memory: hel::Handle,
    local: hel::Mapping<u8>,
}

// SAFETY: Handles are plain numbers; Hel operations on them work from any thread. The
// mapping is only accessed through raw pointers.
unsafe impl Send for Mapping {}
// SAFETY: Mapping has no methods that take `&self` and access the memory or the handles.
unsafe impl Sync for Mapping {}

impl Mapping {
    pub(crate) fn new(size: usize) -> Result<Self> {
        // Hel allocates and maps whole pages.
        let size = size.next_multiple_of(0x1000);
        let memory = hel::allocate_memory(
            managarm::posix::hierarchy_handle(),
            size,
            hel::AllocFlags::empty(),
            None,
        )
        .context("failed to allocate guest memory")?;
        // SAFETY: The guest's memory is only accessed through raw pointers.
        let local = unsafe {
            hel::Mapping::new(
                &memory,
                None,
                0,
                size,
                hel::MappingFlags::READ | hel::MappingFlags::WRITE,
            )
        }
        .context("failed to map guest memory")?;
        Ok(Self { memory, local })
    }

    pub(crate) fn base(&self) -> *mut u8 {
        // SAFETY: Local mappings are never at address zero.
        unsafe { self.local.as_ptr() }
            .expect("local mapping at address zero")
            .as_ptr()
    }
}

pub struct Hypervisor {
    /// Guest RAM in the virtualized space. Declared before `space`, such that it is
    /// unmapped before the space's handle is closed.
    _ram: hel::Mapping<u8>,
    space: hel::Handle,
    /// Guest RAM as we access it (the vCPUs read the MMIO instructions from it).
    memory: GuestMemory,
    interrupts: Arc<Interrupts>,
}

/// The I/O APIC and the vCPU that its interrupts go to.
struct Interrupts {
    ioapic: Mutex<Ioapic>,
    vcpu: OnceLock<Arc<hel::Handle>>,
}

impl Interrupts {
    /// Raises an interrupt that the I/O APIC sends (once the vCPU exists).
    // Delivery after dropping the lock is like a message in flight on hardware: Remote IRR
    // stays set until the EOI, and guests do not change the vector before (Linux checks it).
    fn raise(&self, interrupt: Option<Interrupt>) -> Result<()> {
        let (Some(interrupt), Some(vcpu)) = (interrupt, self.vcpu.get()) else {
            return Ok(());
        };
        hel::assert_virtualized_irq(vcpu, interrupt.vector as u64, interrupt.level)
            .with_context(|| format!("failed to raise vector {:#x}", interrupt.vector))
    }
}

// SAFETY: Handles (including the mapping's) are plain numbers; Hel operations on them work
// from any thread.
unsafe impl Send for Hypervisor {}
// SAFETY: See above.
unsafe impl Sync for Hypervisor {}

pub(crate) fn create(memory_size: u64) -> Result<(Hypervisor, GuestMemory)> {
    let space = hel::create_virtualized_space()
        .context("failed to create a virtualized space (does the CPU support virtualization?)")?;
    let mapping = Mapping::new(memory_size as usize)?;
    // SAFETY: Mappings in the virtualized space are never accessed through Rust references.
    // A null pointer with FIXED maps at address zero.
    let ram = unsafe {
        hel::Mapping::new_with_space(
            &mapping.memory,
            &space,
            None,
            0,
            memory_size as usize,
            hel::MappingFlags::READ
                | hel::MappingFlags::WRITE
                | hel::MappingFlags::EXECUTE
                | hel::MappingFlags::FIXED,
        )
    }
    .context("failed to map guest memory into the virtualized space")?;
    // SAFETY: The GuestMemory owns the mapping.
    let memory = unsafe { GuestMemory::new(mapping.base(), memory_size, Arc::new(mapping)) };
    let interrupts = Arc::new(Interrupts {
        ioapic: Mutex::new(Ioapic::new()),
        vcpu: OnceLock::new(),
    });
    Ok((
        Hypervisor {
            _ram: ram,
            space,
            memory: memory.clone(),
            interrupts,
        },
        memory,
    ))
}

impl HypervisorOps for Hypervisor {
    fn create_vcpu(&self, id: u32, cpuid: &[CpuidEntry]) -> Result<Vcpu> {
        if id >= MAX_VCPUS {
            bail!("thor supports only {MAX_VCPUS} vCPU per VM");
        }
        let handle =
            Arc::new(hel::create_virtualized_cpu(&self.space).context("failed to create a vCPU")?);
        if self.interrupts.vcpu.set(handle.clone()).is_err() {
            bail!("vCPU {id} already exists");
        }
        Ok(Vcpu {
            handle,
            counts: Arc::new(ExitCounts::new()),
            cpuid: cpuid.to_vec(),
            memory: self.memory.clone(),
            interrupts: self.interrupts.clone(),
            mmio: None,
        })
    }

    fn set_irq_line(&self, pin: u32, level: bool) -> Result<()> {
        if pin as usize >= ioapic::NUM_PINS {
            bail!("I/O APIC pin {pin} does not exist");
        }
        let interrupt = self
            .interrupts
            .ioapic
            .lock()
            .unwrap()
            .set_irq(pin as usize, level);
        self.interrupts.raise(interrupt)
    }

    fn tsc_hz(&self) -> Result<u64> {
        managarm::clock::tsc_hz().context("failed to read the TSC frequency from thor's clock page")
    }
}

#[derive(Clone)]
pub struct Kicker {
    /// Co-owns the vCPU's handle, such that it stays open while the kicker exists.
    handle: Arc<hel::Handle>,
}

// SAFETY: See Hypervisor.
unsafe impl Send for Kicker {}
unsafe impl Sync for Kicker {}

impl KickerOps for Kicker {
    fn kick(&self) {
        // Only fails for invalid handles, and the kicker keeps its handle open.
        let _ = hel::kick_virtualized_cpu(&self.handle);
    }
}

kinds! {
    /// The exits that the backend handles itself (see [`ExitCounts`]).
    enum Internal {
        Cpuid => "exit.cpuid",
        Eoi => "exit.eoi",
        /// An access to the I/O APIC.
        Ioapic => "exit.ioapic",
    }
}

#[derive(Clone)]
pub struct Counters {
    exits: Arc<ExitCounts<Internal>>,
}

impl CountersOps for Counters {
    fn snapshot(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        self.exits.snapshot(&mut out);
        Ok(out)
    }
}

pub struct Vcpu {
    handle: Arc<hel::Handle>,
    counts: Arc<ExitCounts<Internal>>,
    cpuid: Vec<CpuidEntry>,
    memory: GuestMemory,
    interrupts: Arc<Interrupts>,
    /// The MMIO instruction that waits for a [`Completion`].
    mmio: Option<Instruction>,
}

// SAFETY: See Hypervisor.
unsafe impl Send for Vcpu {}

impl Vcpu {
    /// # Safety
    /// `T` must be the image type of the register set `set`.
    unsafe fn load<T: Copy>(&self, set: u32) -> Result<T> {
        // SAFETY: See above.
        Ok(unsafe { hel::load_registers(&self.handle, set) }?)
    }

    /// # Safety
    /// See load().
    unsafe fn store<T: Copy>(&self, set: u32, image: &T) -> Result<()> {
        // SAFETY: See above.
        Ok(unsafe { hel::store_registers(&self.handle, set, image) }?)
    }

    fn find_cpuid(&self, function: u32, index: u32) -> Option<&CpuidEntry> {
        self.cpuid.iter().find(|entry| {
            entry.function == function && (!entry.significant_index || entry.index == index)
        })
    }

    /// Emulates CPUID from the table, with the bits that reflect the vCPU's state (like
    /// KVM): OSXSAVE and the XSAVE area sizes.
    fn cpuid(&mut self, mut function: u32, index: u32) -> Result<[u32; 4]> {
        let mut entry = self.find_cpuid(function, index);
        // Like Intel CPUs (and KVM), leaves above the maximum leaf of their range return
        // the highest basic leaf.
        if entry.is_none() {
            if let Some(basic) = self.find_cpuid(0, 0) {
                let range = match function {
                    0x4000_0000..=0x4FFF_FFFF => function & 0xFFFF_FF00,
                    0xC000_0000.. => 0xC000_0000,
                    _ => function & 0x8000_0000,
                };
                if self
                    .find_cpuid(range, 0)
                    .is_none_or(|range| function > range.eax)
                {
                    function = basic.eax;
                    entry = self.find_cpuid(function, index);
                }
            }
        }
        let mut result = entry.map_or([0; 4], |e| [e.eax, e.ebx, e.ecx, e.edx]);
        match (function, index) {
            (1, _) => {
                result[2] &= !LEAF1_ECX_OSXSAVE;
                if self.regs()?.cr4 & CR4_OSXSAVE != 0 {
                    result[2] |= LEAF1_ECX_OSXSAVE;
                }
            }
            (0xD, 0) if result[1] != 0 => result[1] = xsave_size(self.msrs()?.xcr0),
            (0xD, 1) if result[1] != 0 => result[1] = xsave_compacted_size(self.msrs()?.xcr0),
            _ => {}
        }
        Ok(result)
    }

    /// Decodes the instruction that accessed `address` (which is not RAM). Returns the
    /// exit for the VMM, or None if the access was to the I/O APIC (and is complete).
    fn mmio(&mut self, address: u64) -> Result<Option<Exit>> {
        let regs = self.regs()?;
        let mut bytes = [0u8; 15];
        let length = decode::fetch(&self.memory, &regs, &mut bytes);
        let instruction = match decode::decode(&bytes[..length], &regs) {
            Ok(instruction) => instruction,
            Err(error) => {
                return Ok(Some(Exit::Failed(format!(
                    "cannot emulate the access to {address:#x}: {error} (bytes {:02x?})",
                    &bytes[..length]
                ))));
            }
        };
        if (IOAPIC_BASE..IOAPIC_BASE + IOAPIC_SIZE).contains(&address) {
            let value = self.ioapic_access(address - IOAPIC_BASE, &instruction.operation)?;
            self.complete_mmio(regs.basic, &instruction, value)?;
            return Ok(None);
        }
        let exit = match instruction.operation {
            Operation::Load { size, .. } => Exit::Mmio {
                address,
                size,
                write: false,
                data: 0,
            },
            Operation::Store { size, value } => Exit::Mmio {
                address,
                size,
                write: true,
                data: value,
            },
        };
        self.mmio = Some(instruction);
        Ok(Some(exit))
    }

    /// Accesses the I/O APIC's registers, which are 32 bits wide. Narrower reads see the
    /// low bits; narrower or unaligned writes are ignored (their effect is undefined).
    fn ioapic_access(&self, offset: u64, operation: &Operation) -> Result<u64> {
        let mut ioapic = self.interrupts.ioapic.lock().unwrap();
        match *operation {
            Operation::Load { .. } => {
                let value = ioapic.read(offset & !3);
                Ok((value as u64) >> (8 * (offset & 3)))
            }
            Operation::Store { size, value } => {
                if offset & 3 == 0 && size >= 4 {
                    let interrupt = ioapic.write(offset, value as u32);
                    drop(ioapic);
                    self.interrupts.raise(interrupt)?;
                }
                Ok(0)
            }
        }
    }

    /// Completes an MMIO instruction (`regs` are the current registers): a load writes
    /// `value` to its register, and RIP moves past the instruction.
    fn complete_mmio(
        &mut self,
        mut regs: BasicRegs,
        instruction: &Instruction,
        value: u64,
    ) -> Result<()> {
        decode::complete_load(&mut regs, &instruction.operation, value);
        regs.rip = regs.rip.wrapping_add(instruction.length as u64);
        self.set_basic_regs(&regs)
    }
}

fn to_hel_completion(completion: Completion) -> HelVmexitCompletion {
    let (status, data) = match completion {
        Completion::Done(value) => (hel_sys::kHelVmexitCompletionDone, [value, 0, 0, 0]),
        Completion::Fault => (hel_sys::kHelVmexitCompletionFault, [0; 4]),
    };
    HelVmexitCompletion {
        status: status as u32,
        reserved: 0,
        data,
    }
}

fn from_hel_basic(regs: &HelX86VirtualizationBasicRegs) -> BasicRegs {
    BasicRegs {
        rax: regs.rax,
        rbx: regs.rbx,
        rcx: regs.rcx,
        rdx: regs.rdx,
        rsi: regs.rsi,
        rdi: regs.rdi,
        rbp: regs.rbp,
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
        r11: regs.r11,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        rsp: regs.rsp,
        rip: regs.rip,
        rflags: regs.rflags,
    }
}

fn to_hel_basic(regs: &BasicRegs) -> HelX86VirtualizationBasicRegs {
    HelX86VirtualizationBasicRegs {
        rax: regs.rax,
        rbx: regs.rbx,
        rcx: regs.rcx,
        rdx: regs.rdx,
        rsi: regs.rsi,
        rdi: regs.rdi,
        rbp: regs.rbp,
        r8: regs.r8,
        r9: regs.r9,
        r10: regs.r10,
        r11: regs.r11,
        r12: regs.r12,
        r13: regs.r13,
        r14: regs.r14,
        r15: regs.r15,
        rsp: regs.rsp,
        rip: regs.rip,
        rflags: regs.rflags,
    }
}

fn from_hel_segment(s: &HelX86SegmentRegister) -> Segment {
    Segment {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.type_,
        present: s.present,
        dpl: s.dpl,
        db: s.db,
        s: s.s,
        l: s.l,
        g: s.g,
        avl: s.avl,
        unusable: s.unusable,
    }
}

fn to_hel_segment(s: &Segment) -> HelX86SegmentRegister {
    HelX86SegmentRegister {
        base: s.base,
        limit: s.limit,
        selector: s.selector,
        type_: s.type_,
        present: s.present,
        dpl: s.dpl,
        db: s.db,
        s: s.s,
        l: s.l,
        g: s.g,
        avl: s.avl,
        unusable: s.unusable,
    }
}

fn from_hel_regs(r: &HelX86VirtualizationRegs) -> Regs {
    Regs {
        basic: BasicRegs {
            rax: r.rax,
            rbx: r.rbx,
            rcx: r.rcx,
            rdx: r.rdx,
            rsi: r.rsi,
            rdi: r.rdi,
            rbp: r.rbp,
            r8: r.r8,
            r9: r.r9,
            r10: r.r10,
            r11: r.r11,
            r12: r.r12,
            r13: r.r13,
            r14: r.r14,
            r15: r.r15,
            rsp: r.rsp,
            rip: r.rip,
            rflags: r.rflags,
        },
        cs: from_hel_segment(&r.cs),
        ds: from_hel_segment(&r.ds),
        es: from_hel_segment(&r.es),
        fs: from_hel_segment(&r.fs),
        gs: from_hel_segment(&r.gs),
        ss: from_hel_segment(&r.ss),
        tr: from_hel_segment(&r.tr),
        ldt: from_hel_segment(&r.ldt),
        gdt: DescriptorTable {
            base: r.gdt.base,
            limit: r.gdt.limit,
        },
        idt: DescriptorTable {
            base: r.idt.base,
            limit: r.idt.limit,
        },
        cr0: r.cr0,
        cr2: r.cr2,
        cr3: r.cr3,
        cr4: r.cr4,
        cr8: r.cr8,
        efer: r.efer,
    }
}

fn to_hel_regs(r: &Regs) -> HelX86VirtualizationRegs {
    let b = &r.basic;
    HelX86VirtualizationRegs {
        rax: b.rax,
        rbx: b.rbx,
        rcx: b.rcx,
        rdx: b.rdx,
        rsi: b.rsi,
        rdi: b.rdi,
        rbp: b.rbp,
        r8: b.r8,
        r9: b.r9,
        r10: b.r10,
        r11: b.r11,
        r12: b.r12,
        r13: b.r13,
        r14: b.r14,
        r15: b.r15,
        rsp: b.rsp,
        rip: b.rip,
        rflags: b.rflags,
        cs: to_hel_segment(&r.cs),
        ds: to_hel_segment(&r.ds),
        es: to_hel_segment(&r.es),
        fs: to_hel_segment(&r.fs),
        gs: to_hel_segment(&r.gs),
        ss: to_hel_segment(&r.ss),
        tr: to_hel_segment(&r.tr),
        ldt: to_hel_segment(&r.ldt),
        gdt: HelX86DescriptorTable {
            base: r.gdt.base,
            limit: r.gdt.limit,
        },
        idt: HelX86DescriptorTable {
            base: r.idt.base,
            limit: r.idt.limit,
        },
        cr0: r.cr0,
        cr2: r.cr2,
        cr3: r.cr3,
        cr4: r.cr4,
        cr8: r.cr8,
        efer: r.efer,
        // Thor ignores it: the local APIC is always in x2APIC mode.
        apic_base: 0,
    }
}

impl VcpuOps for Vcpu {
    fn run(&mut self, completion: Option<Completion>) -> Result<Exit> {
        // Thor completes the other exits; MMIO instructions are ours.
        let mut completion = match (self.mmio.take(), completion) {
            (Some(instruction), Some(Completion::Done(value))) => {
                let regs = self.basic_regs()?;
                self.complete_mmio(regs, &instruction, value)?;
                None
            }
            (Some(_), Some(Completion::Fault)) => bail!("faulting MMIO is not supported"),
            (Some(_), None) => bail!("the previous exit needs a completion"),
            (None, completion) => completion.map(to_hel_completion),
        };
        loop {
            let reason = match hel::run_virtualized_cpu(&self.handle, completion.take().as_ref()) {
                Ok(reason) => reason,
                // A signal interrupted the thread (thor applied the completion already).
                Err(hel::Error::Cancelled) => {
                    self.counts.count(&Exit::Kicked);
                    return Ok(Exit::Kicked);
                }
                Err(error) => return Err(error).context("helRunVirtualizedCpu() failed"),
            };
            let flags = reason.flags as u32;
            let exit = match reason.exitReason as i32 {
                hel_sys::kHelVmexitIo => Exit::Io {
                    port: reason.address as u16,
                    size: reason.code as u8,
                    input: flags & hel_sys::kHelVmexitIoIn as u32 != 0,
                    string: flags & hel_sys::kHelVmexitIoString as u32 != 0,
                    data: reason.data as u32,
                },
                // Thor could not resolve a fault on RAM (e.g., it is out of memory).
                hel_sys::kHelVmexitTranslationFault
                    if (reason.address as u64) < self.memory.size() =>
                {
                    Exit::Failed(format!(
                        "unresolvable fault on RAM at {:#x}",
                        reason.address
                    ))
                }
                hel_sys::kHelVmexitTranslationFault => match self.mmio(reason.address as u64)? {
                    Some(exit) => exit,
                    None => {
                        self.counts.count_internal(Internal::Ioapic);
                        continue;
                    }
                },
                hel_sys::kHelVmexitRdmsr => Exit::Rdmsr {
                    index: reason.address as u32,
                },
                hel_sys::kHelVmexitWrmsr => Exit::Wrmsr {
                    index: reason.address as u32,
                    value: reason.data,
                },
                hel_sys::kHelVmexitCpuid => {
                    self.counts.count_internal(Internal::Cpuid);
                    let result = self.cpuid(reason.address as u32, reason.data as u32)?;
                    let mut cpuid = to_hel_completion(Completion::Done(0));
                    cpuid.data = result.map(u64::from);
                    completion = Some(cpuid);
                    continue;
                }
                hel_sys::kHelVmexitEoi => {
                    self.counts.count_internal(Internal::Eoi);
                    let interrupt = self
                        .interrupts
                        .ioapic
                        .lock()
                        .unwrap()
                        .eoi(reason.code as u8);
                    self.interrupts.raise(interrupt)?;
                    continue;
                }
                hel_sys::kHelVmexitKicked => Exit::Kicked,
                hel_sys::kHelVmexitShutdown => Exit::Shutdown,
                hel_sys::kHelVmexitHyperCall => Exit::Hypercall,
                hel_sys::kHelVmexitError => {
                    Exit::Failed(format!("VM entry failed with error {}", reason.code))
                }
                other => Exit::Failed(format!("unexpected exit {other} (code {:#x})", reason.code)),
            };
            self.counts.count(&exit);
            return Ok(exit);
        }
    }

    fn basic_regs(&mut self) -> Result<BasicRegs> {
        // SAFETY: HelX86VirtualizationBasicRegs is the image of kHelRegsVirtualizationBasic.
        let regs: HelX86VirtualizationBasicRegs =
            unsafe { self.load(hel_sys::kHelRegsVirtualizationBasic) }?;
        Ok(from_hel_basic(&regs))
    }

    fn set_basic_regs(&mut self, regs: &BasicRegs) -> Result<()> {
        // SAFETY: See basic_regs().
        unsafe { self.store(hel_sys::kHelRegsVirtualizationBasic, &to_hel_basic(regs)) }
    }

    fn regs(&mut self) -> Result<Regs> {
        // SAFETY: HelX86VirtualizationRegs is the image of kHelRegsVirtualization.
        let regs: HelX86VirtualizationRegs = unsafe { self.load(hel_sys::kHelRegsVirtualization) }?;
        Ok(from_hel_regs(&regs))
    }

    fn set_regs(&mut self, regs: &Regs) -> Result<()> {
        // SAFETY: See regs().
        unsafe { self.store(hel_sys::kHelRegsVirtualization, &to_hel_regs(regs)) }
    }

    fn msrs(&mut self) -> Result<Msrs> {
        // SAFETY: HelX86VirtualizationMsrs is the image of kHelRegsVirtualizationMsrs.
        let m: HelX86VirtualizationMsrs =
            unsafe { self.load(hel_sys::kHelRegsVirtualizationMsrs) }?;
        Ok(Msrs {
            kernel_gs_base: m.kernelGsBase,
            star: m.star,
            lstar: m.lstar,
            cstar: m.cstar,
            sfmask: m.sfmask,
            sysenter_cs: m.sysenterCs,
            sysenter_esp: m.sysenterEsp,
            sysenter_eip: m.sysenterEip,
            xcr0: m.xcr0,
        })
    }

    fn set_msrs(&mut self, m: &Msrs) -> Result<()> {
        let image = HelX86VirtualizationMsrs {
            kernelGsBase: m.kernel_gs_base,
            star: m.star,
            lstar: m.lstar,
            cstar: m.cstar,
            sfmask: m.sfmask,
            sysenterCs: m.sysenter_cs,
            sysenterEsp: m.sysenter_esp,
            sysenterEip: m.sysenter_eip,
            xcr0: m.xcr0,
        };
        // SAFETY: See msrs().
        unsafe { self.store(hel_sys::kHelRegsVirtualizationMsrs, &image) }
    }

    fn kicker(&self) -> Kicker {
        Kicker {
            handle: self.handle.clone(),
        }
    }

    fn counters(&self) -> Counters {
        Counters {
            exits: self.counts.clone(),
        }
    }
}
