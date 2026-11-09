//! A backend for Linux' KVM API, for developing midgard on Linux.
//!
//! KVM runs with its in-kernel irqchip (local APICs, the I/O APIC and the 8259 PICs,
//! which Hel does not have, but Linux masks them) and with an MSR filter that sends all
//! MSR accesses except the local APIC's to midgard. KVM's model is close to Hel's (see
//! [`crate`]):
//! - KVM completes I/O and MSR instructions on the next KVM_RUN, with the data in
//!   `kvm_run`; we put the [`Completion`] there.
//! - KVM decodes MMIO instructions; we pass the decoded access on.
//! - KVM gets the CPUID table when the vCPU is created.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    CpuId, KVM_CPUID_FLAG_SIGNIFCANT_INDEX, KVM_MAX_CPUID_ENTRIES, KVM_MSR_EXIT_REASON_FILTER,
    KVM_MSR_EXIT_REASON_INVAL, KVM_MSR_EXIT_REASON_UNKNOWN, KVM_VCPU_TSC_CTRL, KVM_VCPU_TSC_OFFSET,
    Msrs as KvmMsrs, kvm_cpuid_entry2, kvm_device_attr, kvm_dtable, kvm_enable_cap, kvm_msr_entry,
    kvm_regs, kvm_run, kvm_segment, kvm_userspace_memory_region, kvm_xcrs,
};
use kvm_ioctls::{
    Cap, Kvm, MsrFilterDefaultAction, MsrFilterRange, MsrFilterRangeFlags, VcpuExit, VcpuFd, VmFd,
};

use crate::x86::{LAPIC_BASE, cpuid, msr};
use crate::{
    BasicRegs, Completion, CpuidEntry, DescriptorTable, Exit, GuestMemory, HypervisorOps,
    KickerOps, MmioAccess, Msrs, RESERVED_BASE, Regs, Segment, VcpuOps,
};

// kvm-ioctls only wraps KVM_SET_DEVICE_ATTR for vCPUs on aarch64.
vmm_sys_util::ioctl_iow_nr!(
    KVM_SET_DEVICE_ATTR,
    kvm_bindings::KVMIO,
    0xE1,
    kvm_device_attr
);
// kvm-ioctls only wraps KVM_GET_TSC_KHZ for vCPUs.
vmm_sys_util::ioctl_io_nr!(KVM_GET_TSC_KHZ, kvm_bindings::KVMIO, 0xA3);

/// Where KVM puts its identity map (one page) and real-mode TSS (three pages; both are
/// only needed on Intel without unrestricted guest), in [`RESERVED_BASE`].
const KVM_IDENTITY_MAP: u64 = RESERVED_BASE;
const KVM_TSS: u64 = RESERVED_BASE + 0x1000;
const _: () = assert!(KVM_TSS + 0x3000 <= RESERVED_BASE + crate::RESERVED_SIZE);

/// Like on Hel, the local APIC starts in x2APIC mode at the default base.
const APIC_BASE_X2APIC: u64 = LAPIC_BASE | msr::APIC_BASE_ENABLE | msr::APIC_BASE_EXTD;

/// Bits of feature leaves that KVM emulates even though KVM_GET_SUPPORTED_CPUID does not
/// necessarily report them (the TSC-deadline timer is a capability instead): x2APIC, TSC
/// deadline, hypervisor (leaf 1 ECX) and the APIC (leaf 1 EDX).
const LEAF1_ECX_EMULATED: u32 =
    cpuid::LEAF1_ECX_X2APIC | cpuid::LEAF1_ECX_TSC_DEADLINE | cpuid::LEAF1_ECX_HYPERVISOR;
const LEAF1_EDX_EMULATED: u32 = cpuid::LEAF1_EDX_APIC;

/// The mapping of guest RAM into our address space (an anonymous shared mapping), which
/// [`GuestMemory`] accesses. The VM refers to it, too (unless the memory is detached);
/// everything that can run the guest holds a reference such that it stays mapped while
/// the guest can access it.
pub(crate) struct Mapping {
    base: *mut u8,
    size: usize,
}

// SAFETY: The mapping is only accessed through raw pointers.
unsafe impl Send for Mapping {}
// SAFETY: Mapping has no methods that take `&self` and access the memory; it only unmaps
// the memory on drop.
unsafe impl Sync for Mapping {}

impl Mapping {
    pub(crate) fn new(size: usize) -> std::io::Result<Self> {
        // SAFETY: A new anonymous mapping does not alias anything.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            base: base.cast(),
            size,
        })
    }

    pub(crate) fn base(&self) -> *mut u8 {
        self.base
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: We own the mapping, and nothing can access it anymore: GuestMemory, the VM
        // and the vCPUs all hold a reference to it.
        unsafe { libc::munmap(self.base.cast(), self.size) };
    }
}

pub struct Hypervisor {
    vm: VmFd,
    supported_cpuid: Vec<kvm_cpuid_entry2>,
    /// Declared after `vm`, such that the VM is destroyed first.
    mapping: Arc<Mapping>,
}

pub(crate) fn create(memory_size: u64) -> Result<(Hypervisor, GuestMemory)> {
    let kvm = Kvm::new().context("failed to open /dev/kvm")?;
    for cap in [
        Cap::Irqchip,
        Cap::TscDeadlineTimer,
        Cap::X86UserSpaceMsr,
        Cap::X86MsrFilter,
    ] {
        if !kvm.check_extension(cap) {
            bail!("KVM lacks {cap:?}");
        }
    }
    let vm = kvm.create_vm().context("KVM_CREATE_VM failed")?;
    vm.set_tss_address(KVM_TSS as usize)
        .context("KVM_SET_TSS_ADDR failed")?;
    vm.set_identity_map_address(KVM_IDENTITY_MAP)
        .context("KVM_SET_IDENTITY_MAP_ADDR failed")?;
    // The local APICs and the I/O APIC (and PICs) are in the kernel, as on Hel. This must
    // happen before vCPUs are created; then the APs wait for INIT / SIPI.
    vm.create_irq_chip().context("KVM_CREATE_IRQCHIP failed")?;
    // MSR accesses go to us, except for the local APIC's: the filter denies everything
    // else, and accesses that KVM cannot handle exit, too.
    let mut cap = kvm_enable_cap {
        cap: kvm_bindings::KVM_CAP_X86_USER_SPACE_MSR,
        ..Default::default()
    };
    cap.args[0] = (KVM_MSR_EXIT_REASON_INVAL
        | KVM_MSR_EXIT_REASON_UNKNOWN
        | KVM_MSR_EXIT_REASON_FILTER) as u64;
    vm.enable_cap(&cap)
        .context("enabling userspace MSR exits failed")?;
    // (Newer kernels never filter the x2APIC range.)
    let allow = |base: u32, msr_count: u32, bitmap: &'static [u8]| MsrFilterRange {
        flags: MsrFilterRangeFlags::READ | MsrFilterRangeFlags::WRITE,
        base,
        msr_count,
        bitmap,
    };
    let ranges = [
        allow(msr::APIC_BASE, 1, &[1]),
        allow(msr::TSC_DEADLINE, 1, &[1]),
        allow(msr::X2APIC_FIRST, 256, &[0xFF; 32]),
    ];
    vm.set_msr_filter(MsrFilterDefaultAction::DENY, &ranges)
        .context("KVM_X86_SET_MSR_FILTER failed")?;

    let mapping =
        Arc::new(Mapping::new(memory_size as usize).context("failed to allocate guest memory")?);
    let region = kvm_userspace_memory_region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size,
        userspace_addr: mapping.base() as u64,
    };
    // SAFETY: The region stays mapped as long as the VM exists: the Hypervisor and the
    // vCPUs (whose file descriptors keep the VM alive) hold a reference to the mapping.
    unsafe { vm.set_user_memory_region(region) }.context("KVM_SET_USER_MEMORY_REGION failed")?;
    // SAFETY: The GuestMemory holds a reference to the mapping.
    let memory = unsafe { GuestMemory::new(mapping.base(), memory_size, mapping.clone()) };

    let supported_cpuid = kvm
        .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
        .context("KVM_GET_SUPPORTED_CPUID failed")?
        .as_slice()
        .to_vec();
    install_kick_handler();
    Ok((
        Hypervisor {
            vm,
            supported_cpuid,
            mapping,
        },
        memory,
    ))
}

impl Hypervisor {
    /// Builds KVM's CPUID table: midgard's leaves, with the features limited to what KVM
    /// supports.
    fn cpuid_table(&self, table: &[CpuidEntry]) -> Result<CpuId> {
        let supported = |function: u32, index: u32| {
            self.supported_cpuid.iter().find(|entry| {
                entry.function == function
                    && (entry.flags & KVM_CPUID_FLAG_SIGNIFCANT_INDEX == 0 || entry.index == index)
            })
        };
        let mut entries = Vec::new();
        for leaf in table {
            let mut entry = kvm_cpuid_entry2 {
                function: leaf.function,
                index: leaf.index,
                flags: if leaf.significant_index {
                    KVM_CPUID_FLAG_SIGNIFCANT_INDEX
                } else {
                    0
                },
                eax: leaf.eax,
                ebx: leaf.ebx,
                ecx: leaf.ecx,
                edx: leaf.edx,
                ..Default::default()
            };
            let host = supported(leaf.function, leaf.index)
                .copied()
                .unwrap_or_default();
            match (leaf.function, leaf.index) {
                (1, _) => {
                    entry.ecx &= host.ecx | LEAF1_ECX_EMULATED;
                    entry.edx &= host.edx | LEAF1_EDX_EMULATED;
                }
                (7, 0) | (0x8000_0001, _) => {
                    entry.ebx &= host.ebx;
                    entry.ecx &= host.ecx;
                    entry.edx &= host.edx;
                }
                (0xD, 0) | (0xD, 1) => entry.eax &= host.eax,
                _ => {}
            }
            entries.push(entry);
        }
        if entries.len() > KVM_MAX_CPUID_ENTRIES {
            bail!("the CPUID table has too many entries ({})", entries.len());
        }
        CpuId::from_entries(&entries).map_err(|error| anyhow::anyhow!("bad CPUID table: {error:?}"))
    }
}

impl HypervisorOps for Hypervisor {
    fn create_vcpu(&self, id: u32, cpuid: &[CpuidEntry]) -> Result<Vcpu> {
        let fd = self
            .vm
            .create_vcpu(id as u64)
            .context("KVM_CREATE_VCPU failed")?;
        fd.set_cpuid2(&self.cpuid_table(cpuid)?)
            .context("KVM_SET_CPUID2 failed")?;
        // midgard's devices use the host's TSC; make the guest's TSC the same.
        let offset = 0u64;
        let attr = kvm_device_attr {
            flags: 0,
            group: KVM_VCPU_TSC_CTRL,
            attr: KVM_VCPU_TSC_OFFSET as u64,
            addr: &offset as *const u64 as u64,
        };
        // SAFETY: The argument is a struct kvm_device_attr that points to a u64.
        if unsafe { vmm_sys_util::ioctl::ioctl_with_ref(&fd, KVM_SET_DEVICE_ATTR(), &attr) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("setting the TSC offset failed (KVM is too old?)");
        }
        // Like on Hel, the local APIC starts in x2APIC mode (KVM starts in xAPIC mode).
        // This needs x2APIC in the CPUID table.
        let mut sregs = fd.get_sregs().context("KVM_GET_SREGS failed")?;
        sregs.apic_base = APIC_BASE_X2APIC | if id == 0 { msr::APIC_BASE_BSP } else { 0 };
        fd.set_sregs(&sregs).context("KVM_SET_SREGS failed")?;
        let kicker = Arc::new(KickState {
            thread: Mutex::new(None),
            kicked: AtomicBool::new(false),
        });
        Ok(Vcpu {
            fd,
            kicker,
            incomplete: None,
            mmio_read: None,
            _mapping: self.mapping.clone(),
        })
    }

    fn set_irq_line(&self, pin: u32, level: bool) -> Result<()> {
        // The default routing connects GSI n to pin n of the I/O APIC (and of the PICs).
        self.vm
            .set_irq_line(pin, level)
            .with_context(|| format!("KVM_IRQ_LINE for pin {pin} failed"))
    }

    fn tsc_hz(&self) -> Result<u64> {
        // The VM's default TSC frequency, which its vCPUs get: the host's (KVM only
        // scales the TSC if it is set to something else), as the host kernel calibrated it.
        // SAFETY: KVM_GET_TSC_KHZ takes no argument.
        let khz = unsafe { vmm_sys_util::ioctl::ioctl(&self.vm, KVM_GET_TSC_KHZ()) };
        if khz <= 0 {
            return Err(std::io::Error::last_os_error())
                .context("KVM_GET_TSC_KHZ for the VM failed (KVM is too old?)");
        }
        Ok(khz as u64 * 1000)
    }
}

thread_local! {
    /// `immediate_exit` of the vCPU that runs on this thread (for the kick signal handler).
    static IMMEDIATE_EXIT: Cell<*mut u8> = const { Cell::new(std::ptr::null_mut()) };
}

/// Kicks interrupt KVM_RUN with this signal.
fn kick_signal() -> i32 {
    libc::SIGRTMIN()
}

extern "C" fn kick_handler(_signal: i32) {
    // Make KVM_RUN return (or not enter the guest) if the signal arrives before it. The
    // signal may also arrive after the run (the flag is null then) or during a later run
    // on this thread (which then returns early).
    let flag = IMMEDIATE_EXIT.with(|flag| flag.get());
    if !flag.is_null() {
        // SAFETY: The flag is only set while a vCPU runs on this thread, so its kvm_run
        // is mapped.
        unsafe { std::ptr::write_volatile(flag, 1) };
    }
}

fn install_kick_handler() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // SAFETY: The handler only performs a volatile store.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = kick_handler as *const () as usize;
            // No SA_RESTART: KVM_RUN must return EINTR.
            action.sa_flags = 0;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(kick_signal(), &action, std::ptr::null_mut());
        }
    });
}

/// What a vCPU and its kickers share.
struct KickState {
    /// The pthread that is inside [`Vcpu::run`] (None outside of it).
    thread: Mutex<Option<libc::pthread_t>>,
    kicked: AtomicBool,
}

#[derive(Clone)]
pub struct Kicker(Arc<KickState>);

impl KickerOps for Kicker {
    fn kick(&self) {
        self.0.kicked.store(true, Ordering::SeqCst);
        let thread = self.0.thread.lock().unwrap();
        if let Some(thread) = *thread {
            // SAFETY: The thread is inside Vcpu::run (and thus alive) until it clears
            // `thread`, which needs the lock that we hold.
            unsafe { libc::pthread_kill(thread, kick_signal()) };
        }
    }
}

/// Makes kicks signal the calling thread while it runs a vCPU (until dropped).
struct Running<'a> {
    kick: &'a KickState,
}

impl<'a> Running<'a> {
    fn enter(kick: &'a KickState, immediate_exit: *mut u8) -> Self {
        IMMEDIATE_EXIT.with(|cell| cell.set(immediate_exit));
        // SAFETY: pthread_self() cannot fail.
        *kick.thread.lock().unwrap() = Some(unsafe { libc::pthread_self() });
        Self { kick }
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        // No kick signals this thread from here on (but one may still be pending).
        *self.kick.thread.lock().unwrap() = None;
        IMMEDIATE_EXIT.with(|cell| cell.set(std::ptr::null_mut()));
    }
}

pub struct Vcpu {
    fd: VcpuFd,
    kicker: Arc<KickState>,
    /// The instruction that waits for a [`Completion`].
    incomplete: Option<Incomplete>,
    /// The size of an MMIO read that midgard completes.
    mmio_read: Option<u8>,
    /// Declared after `fd`, such that the vCPU (and thus maybe the VM) is destroyed first.
    _mapping: Arc<Mapping>,
}

/// An exit that needs a [`Completion`].
#[derive(Clone, Copy)]
enum Incomplete {
    In { size: u8 },
    Out,
    Msr,
}

impl Vcpu {
    fn kvm_run(&mut self) -> &mut kvm_run {
        self.fd.get_kvm_run()
    }

    /// Puts the result of the completed instruction into `kvm_run` (KVM finishes the
    /// instruction on the next KVM_RUN).
    fn complete(&mut self, completion: Option<Completion>) -> Result<()> {
        let (incomplete, completion) = match (self.incomplete.take(), completion) {
            (None, None) => return Ok(()),
            (Some(incomplete), Some(completion)) => (incomplete, completion),
            (None, Some(_)) => bail!("completion without an exit that needs one"),
            (Some(_), None) => bail!("the previous exit needs a completion"),
        };
        let run = self.kvm_run();
        match (incomplete, completion) {
            (Incomplete::In { size }, Completion::Done(value)) => {
                // SAFETY: The last exit was KVM_EXIT_IO; its data lies within the kvm_run
                // mapping.
                unsafe {
                    let offset = run.__bindgen_anon_1.io.data_offset as usize;
                    let data = (run as *mut kvm_run).cast::<u8>().add(offset);
                    let bytes = value.to_le_bytes();
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, size as usize);
                }
            }
            (Incomplete::Out, Completion::Done(_)) => {}
            (Incomplete::In { .. } | Incomplete::Out, Completion::Fault) => {
                bail!("faulting port I/O is not supported")
            }
            (Incomplete::Msr, completion) => {
                // SAFETY: The last exit was KVM_EXIT_X86_RDMSR / KVM_EXIT_X86_WRMSR. KVM
                // injects #GP if `error` is set.
                let msr = unsafe { &mut run.__bindgen_anon_1.msr };
                match completion {
                    Completion::Done(value) => {
                        msr.error = 0;
                        msr.data = value;
                    }
                    Completion::Fault => msr.error = 1,
                }
            }
        }
        Ok(())
    }

    /// Runs the guest once and translates the exit; None re-enters the guest.
    fn enter(&mut self) -> Result<Option<Exit>> {
        let exit = match self.fd.run() {
            Ok(exit) => exit,
            Err(error) if error.errno() == libc::EINTR => return Ok(Some(Exit::Kicked)),
            // An AP that waited for INIT / SIPI woke up.
            Err(error) if error.errno() == libc::EAGAIN => return Ok(None),
            Err(error) => return Err(error).context("KVM_RUN failed"),
        };
        let (incomplete, exit) = match exit {
            VcpuExit::IrqWindowOpen => return Ok(None),
            VcpuExit::IoIn(port, data) => {
                let size = data.len() as u8;
                // SAFETY: The exit reason is KVM_EXIT_IO.
                let count = unsafe { self.kvm_run().__bindgen_anon_1.io.count };
                let exit = Exit::Io {
                    port,
                    size,
                    input: true,
                    string: count != 1,
                    data: 0,
                };
                (Some(Incomplete::In { size }), exit)
            }
            VcpuExit::IoOut(port, data) => {
                let mut bytes = [0u8; 4];
                let size = data.len().min(4);
                bytes[..size].copy_from_slice(&data[..size]);
                // SAFETY: The exit reason is KVM_EXIT_IO.
                let io = unsafe { self.kvm_run().__bindgen_anon_1.io };
                let exit = Exit::Io {
                    port,
                    size: io.size,
                    input: false,
                    string: io.count != 1,
                    data: u32::from_le_bytes(bytes),
                };
                (Some(Incomplete::Out), exit)
            }
            VcpuExit::MmioRead(address, data) => {
                let size = data.len() as u8;
                self.mmio_read = Some(size);
                let access = MmioAccess {
                    write: false,
                    size,
                    data: 0,
                };
                (
                    None,
                    Exit::Mmio {
                        address,
                        access: Some(access),
                    },
                )
            }
            VcpuExit::MmioWrite(address, data) => {
                let mut bytes = [0u8; 8];
                bytes[..data.len().min(8)].copy_from_slice(&data[..data.len().min(8)]);
                let access = MmioAccess {
                    write: true,
                    size: data.len() as u8,
                    data: u64::from_le_bytes(bytes),
                };
                (
                    None,
                    Exit::Mmio {
                        address,
                        access: Some(access),
                    },
                )
            }
            VcpuExit::X86Rdmsr(exit) => (Some(Incomplete::Msr), Exit::Rdmsr { index: exit.index }),
            VcpuExit::X86Wrmsr(exit) => (
                Some(Incomplete::Msr),
                Exit::Wrmsr {
                    index: exit.index,
                    value: exit.data,
                },
            ),
            VcpuExit::Shutdown | VcpuExit::SystemEvent(..) => (None, Exit::Shutdown),
            VcpuExit::Intr => (None, Exit::Kicked),
            VcpuExit::FailEntry(reason, _) => (
                None,
                Exit::Failed(format!("VM entry failed with hardware reason {reason:#x}")),
            ),
            other => (None, Exit::Failed(format!("unexpected KVM exit {other:?}"))),
        };
        self.incomplete = incomplete;
        Ok(Some(exit))
    }
}

fn from_kvm_regs(r: &kvm_regs) -> BasicRegs {
    BasicRegs {
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
    }
}

fn to_kvm_regs(r: &BasicRegs) -> kvm_regs {
    kvm_regs {
        rax: r.rax,
        rbx: r.rbx,
        rcx: r.rcx,
        rdx: r.rdx,
        rsi: r.rsi,
        rdi: r.rdi,
        rsp: r.rsp,
        rbp: r.rbp,
        r8: r.r8,
        r9: r.r9,
        r10: r.r10,
        r11: r.r11,
        r12: r.r12,
        r13: r.r13,
        r14: r.r14,
        r15: r.r15,
        rip: r.rip,
        rflags: r.rflags,
    }
}

fn from_kvm_segment(s: &kvm_segment) -> Segment {
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

fn to_kvm_segment(s: &Segment) -> kvm_segment {
    kvm_segment {
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
        padding: 0,
    }
}

impl VcpuOps for Vcpu {
    fn run(&mut self, completion: Option<Completion>) -> Result<Exit> {
        self.complete(completion)?;
        let kicker = self.kicker.clone();
        let _running = Running::enter(&kicker, &raw mut self.kvm_run().immediate_exit);
        loop {
            // A kick that arrives from here on makes KVM_RUN return immediately.
            self.fd.set_kvm_immediate_exit(0);
            if self.kicker.kicked.swap(false, Ordering::SeqCst) {
                return Ok(Exit::Kicked);
            }
            if let Some(exit) = self.enter()? {
                return Ok(exit);
            }
        }
    }

    fn basic_regs(&mut self) -> Result<BasicRegs> {
        Ok(from_kvm_regs(
            &self.fd.get_regs().context("KVM_GET_REGS failed")?,
        ))
    }

    fn set_basic_regs(&mut self, regs: &BasicRegs) -> Result<()> {
        self.fd
            .set_regs(&to_kvm_regs(regs))
            .context("KVM_SET_REGS failed")
    }

    fn regs(&mut self) -> Result<Regs> {
        let s = self.fd.get_sregs().context("KVM_GET_SREGS failed")?;
        Ok(Regs {
            basic: self.basic_regs()?,
            cs: from_kvm_segment(&s.cs),
            ds: from_kvm_segment(&s.ds),
            es: from_kvm_segment(&s.es),
            fs: from_kvm_segment(&s.fs),
            gs: from_kvm_segment(&s.gs),
            ss: from_kvm_segment(&s.ss),
            tr: from_kvm_segment(&s.tr),
            ldt: from_kvm_segment(&s.ldt),
            gdt: DescriptorTable {
                base: s.gdt.base,
                limit: s.gdt.limit,
            },
            idt: DescriptorTable {
                base: s.idt.base,
                limit: s.idt.limit,
            },
            cr0: s.cr0,
            cr2: s.cr2,
            cr3: s.cr3,
            cr4: s.cr4,
            cr8: s.cr8,
            efer: s.efer,
        })
    }

    fn set_regs(&mut self, r: &Regs) -> Result<()> {
        // Keep the APIC base and the interrupt bitmap.
        let mut s = self.fd.get_sregs().context("KVM_GET_SREGS failed")?;
        s.cs = to_kvm_segment(&r.cs);
        s.ds = to_kvm_segment(&r.ds);
        s.es = to_kvm_segment(&r.es);
        s.fs = to_kvm_segment(&r.fs);
        s.gs = to_kvm_segment(&r.gs);
        s.ss = to_kvm_segment(&r.ss);
        s.tr = to_kvm_segment(&r.tr);
        s.ldt = to_kvm_segment(&r.ldt);
        s.gdt = kvm_dtable {
            base: r.gdt.base,
            limit: r.gdt.limit,
            ..Default::default()
        };
        s.idt = kvm_dtable {
            base: r.idt.base,
            limit: r.idt.limit,
            ..Default::default()
        };
        s.cr0 = r.cr0;
        s.cr2 = r.cr2;
        s.cr3 = r.cr3;
        s.cr4 = r.cr4;
        s.cr8 = r.cr8;
        s.efer = r.efer;
        self.fd.set_sregs(&s).context("KVM_SET_SREGS failed")?;
        self.set_basic_regs(&r.basic)
    }

    fn msrs(&mut self) -> Result<Msrs> {
        let entries: Vec<kvm_msr_entry> = Msrs::INDICES
            .iter()
            .map(|&index| kvm_msr_entry {
                index,
                ..Default::default()
            })
            .collect();
        let mut kvm_msrs = KvmMsrs::from_entries(&entries).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let count = self
            .fd
            .get_msrs(&mut kvm_msrs)
            .context("KVM_GET_MSRS failed")?;
        if count != Msrs::INDICES.len() {
            bail!("KVM_GET_MSRS read {count} MSRs");
        }
        let xcrs = self.fd.get_xcrs().context("KVM_GET_XCRS failed")?;
        let mut msrs = Msrs {
            xcr0: xcrs.xcrs[..xcrs.nr_xcrs as usize]
                .iter()
                .find(|x| x.xcr == 0)
                .map_or(1, |x| x.value),
            ..Default::default()
        };
        for entry in kvm_msrs.as_slice() {
            if let Some(field) = msrs.get_mut(entry.index) {
                *field = entry.data;
            }
        }
        Ok(msrs)
    }

    fn set_msrs(&mut self, m: &Msrs) -> Result<()> {
        let entries: Vec<kvm_msr_entry> = Msrs::INDICES
            .iter()
            .map(|&index| kvm_msr_entry {
                index,
                data: m.get(index).unwrap_or_default(),
                ..Default::default()
            })
            .collect();
        let msrs = KvmMsrs::from_entries(&entries).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let count = self.fd.set_msrs(&msrs).context("KVM_SET_MSRS failed")?;
        if count != Msrs::INDICES.len() {
            bail!("KVM_SET_MSRS wrote {count} MSRs");
        }
        let mut xcrs = kvm_xcrs {
            nr_xcrs: 1,
            ..Default::default()
        };
        xcrs.xcrs[0].xcr = 0;
        xcrs.xcrs[0].value = m.xcr0;
        self.fd.set_xcrs(&xcrs).context("KVM_SET_XCRS failed")
    }

    fn kernel_mmio(&mut self, _address: u64, _write: Option<u32>) -> Result<Option<u32>> {
        // KVM handles the accesses to its devices itself.
        Ok(None)
    }

    fn complete_mmio_read(&mut self, value: u64) -> Result<()> {
        let Some(size) = self.mmio_read.take() else {
            bail!("no MMIO read to complete");
        };
        let bytes = value.to_le_bytes();
        // SAFETY: The last exit was KVM_EXIT_MMIO; KVM completes the instruction with this
        // data on the next KVM_RUN.
        let mmio = unsafe { &mut self.kvm_run().__bindgen_anon_1.mmio };
        mmio.data[..size as usize].copy_from_slice(&bytes[..size as usize]);
        Ok(())
    }

    fn kicker(&self) -> Kicker {
        Kicker(self.kicker.clone())
    }
}
