//! A backend for Linux' KVM API, for developing midgard on Linux.
//!
//! KVM runs with a split irqchip, like Hel: the local APICs are in the kernel, the I/O
//! APIC is [`Ioapic`] (whose interrupts we send as MSIs) and there are no 8259 PICs. An
//! MSR filter sends all MSR accesses except the local APIC's to midgard. KVM's model is
//! close to Hel's (see [`crate`]):
//! - KVM completes I/O, MMIO and MSR instructions on the next KVM_RUN, with the data in
//!   `kvm_run`; we put the [`Completion`] there.
//! - KVM decodes MMIO instructions; we pass the decoded access on.
//! - KVM gets the CPUID table when the vCPU is created.

use std::cell::Cell;
use std::fs::File;
use std::mem::offset_of;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};

use anyhow::{Context, Result, bail};
use kvm_bindings::{
    CpuId, KVM_CPUID_FLAG_SIGNIFCANT_INDEX, KVM_IRQ_ROUTING_MSI, KVM_MAX_CPUID_ENTRIES,
    KVM_MSR_EXIT_REASON_FILTER, KVM_MSR_EXIT_REASON_INVAL, KVM_MSR_EXIT_REASON_UNKNOWN,
    KVM_STATS_TYPE_CUMULATIVE, KVM_STATS_TYPE_MASK, KVM_VCPU_TSC_CTRL, KVM_VCPU_TSC_OFFSET,
    KvmIrqRouting, Msrs as KvmMsrs, kvm_cpuid_entry2, kvm_device_attr, kvm_dtable, kvm_enable_cap,
    kvm_irq_routing_entry, kvm_msi, kvm_msr_entry, kvm_regs, kvm_run, kvm_segment, kvm_stats_desc,
    kvm_stats_header, kvm_userspace_memory_region, kvm_xcrs,
};
use kvm_ioctls::{
    Cap, Kvm, MsrFilterDefaultAction, MsrFilterRange, MsrFilterRangeFlags, VcpuExit, VcpuFd, VmFd,
};

use crate::counters::{ExitCounts, kinds};
use crate::ioapic::{self, Interrupt, Ioapic};
use crate::x86::{IOAPIC_BASE, LAPIC_BASE, cpuid, msr};
use crate::{
    BasicRegs, Completion, CountersOps, CpuidEntry, DescriptorTable, Exit, GuestMemory,
    HypervisorOps, KickerOps, Msrs, RESERVED_BASE, Regs, Segment, VcpuOps,
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
// kvm-ioctls does not wrap KVM_GET_STATS_FD.
vmm_sys_util::ioctl_io_nr!(KVM_GET_STATS_FD, kvm_bindings::KVMIO, 0xCE);

/// Where KVM puts its identity map (one page) and real-mode TSS (three pages; both are
/// only needed on Intel without unrestricted guest), in [`RESERVED_BASE`].
const KVM_IDENTITY_MAP: u64 = RESERVED_BASE;
const KVM_TSS: u64 = RESERVED_BASE + 0x1000;
const _: () = assert!(KVM_TSS + 0x3000 <= RESERVED_BASE + crate::RESERVED_SIZE);

/// The I/O APIC's MMIO window.
const IOAPIC_SIZE: u64 = 0x100;

// The MSI address and data of an interrupt from the I/O APIC.
const MSI_ADDRESS_BASE: u32 = 0xFEE0_0000;
const MSI_ADDRESS_DESTINATION_SHIFT: u32 = 12;
const MSI_ADDRESS_LOGICAL: u32 = 1 << 2;
const MSI_DATA_LOWEST_PRIORITY: u32 = 1 << 8;
const MSI_DATA_ASSERT: u32 = 1 << 14;
const MSI_DATA_LEVEL: u32 = 1 << 15;

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

/// KVM has no limit of its own below the platform's.
pub const MAX_VCPUS: u32 = u32::MAX;

pub struct Hypervisor {
    vm: Arc<VmFd>,
    interrupts: Arc<Interrupts>,
    supported_cpuid: Vec<kvm_cpuid_entry2>,
    /// Declared after `vm`, such that the VM is destroyed first.
    mapping: Arc<Mapping>,
}

/// The MSI address and data of `interrupt`.
fn msi(interrupt: &Interrupt) -> (u32, u32) {
    let mut address =
        MSI_ADDRESS_BASE | u32::from(interrupt.destination) << MSI_ADDRESS_DESTINATION_SHIFT;
    if interrupt.logical {
        address |= MSI_ADDRESS_LOGICAL;
    }
    let mut data = u32::from(interrupt.vector) | MSI_DATA_ASSERT;
    if interrupt.lowest_priority {
        data |= MSI_DATA_LOWEST_PRIORITY;
    }
    if interrupt.level {
        data |= MSI_DATA_LEVEL;
    }
    (address, data)
}

/// The I/O APIC and the VM whose local APICs its interrupts go to.
struct Interrupts {
    vm: Arc<VmFd>,
    state: Mutex<IoapicState>,
}

struct IoapicState {
    ioapic: Ioapic,
    /// The MSIs of the level-triggered pins, which KVM has routes for (GSI n is pin n):
    /// they tell KVM which vectors' EOIs to report (as KVM_EXIT_IOAPIC_EOI).
    routes: Vec<(u32, (u32, u32))>,
}

impl Interrupts {
    /// Sends an interrupt that the I/O APIC returned.
    // Delivery after dropping the lock is like a message in flight on hardware: Remote IRR
    // stays set until the EOI, and guests do not change the vector before (Linux checks it).
    fn raise(&self, interrupt: Option<Interrupt>) -> Result<()> {
        let Some(interrupt) = interrupt else {
            return Ok(());
        };
        let (address_lo, data) = msi(&interrupt);
        let msi = kvm_msi {
            address_lo,
            data,
            ..Default::default()
        };
        self.vm
            .signal_msi(msi)
            .with_context(|| format!("KVM_SIGNAL_MSI for vector {:#x} failed", interrupt.vector))?;
        Ok(())
    }

    fn set_irq(&self, pin: usize, level: bool) -> Result<()> {
        let interrupt = self.state.lock().unwrap().ioapic.set_irq(pin, level);
        self.raise(interrupt)
    }

    fn eoi(&self, vector: u8) -> Result<()> {
        let interrupt = self.state.lock().unwrap().ioapic.eoi(vector);
        self.raise(interrupt)
    }

    /// Reads from the I/O APIC's registers, which are 32 bits wide; narrower reads see
    /// the low bits.
    fn read(&self, offset: u64) -> u64 {
        let value = self.state.lock().unwrap().ioapic.read(offset & !3);
        u64::from(value) >> (8 * (offset & 3))
    }

    /// Writes to the I/O APIC's registers. Narrower or unaligned writes are ignored
    /// (their effect is undefined).
    fn write(&self, offset: u64, size: usize, value: u64) -> Result<()> {
        if offset & 3 != 0 || size < 4 {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap();
        let interrupt = state.ioapic.write(offset, value as u32);
        self.update_routes(&mut state)?;
        drop(state);
        self.raise(interrupt)
    }

    /// Updates KVM's routes after a redirection entry might have changed.
    fn update_routes(&self, state: &mut IoapicState) -> Result<()> {
        let routes: Vec<_> = (0..ioapic::NUM_PINS)
            .filter_map(|pin| Some((pin as u32, state.ioapic.redirection(pin)?)))
            .filter(|(_, interrupt)| interrupt.level)
            .map(|(pin, interrupt)| (pin, msi(&interrupt)))
            .collect();
        if routes == state.routes {
            return Ok(());
        }
        let entries: Vec<_> = routes
            .iter()
            .map(|&(gsi, (address, data))| {
                let mut entry = kvm_irq_routing_entry {
                    gsi,
                    type_: KVM_IRQ_ROUTING_MSI,
                    ..Default::default()
                };
                entry.u.msi.address_lo = address;
                entry.u.msi.data = data;
                entry
            })
            .collect();
        let routing = KvmIrqRouting::from_entries(&entries)
            .map_err(|error| anyhow::anyhow!("bad routing table: {error:?}"))?;
        self.vm
            .set_gsi_routing(&routing)
            .context("KVM_SET_GSI_ROUTING failed")?;
        state.routes = routes;
        Ok(())
    }
}

pub(crate) fn create(memory_size: u64) -> Result<(Hypervisor, GuestMemory)> {
    let kvm = Kvm::new().context("failed to open /dev/kvm")?;
    for cap in [
        Cap::SplitIrqchip,
        Cap::SignalMsi,
        Cap::IrqRouting,
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
    // The local APICs are in the kernel, as on Hel, with GSIs for the I/O APIC's pins
    // (whose routes say which EOIs to report). This must happen before vCPUs are
    // created; then the APs wait for INIT / SIPI.
    let mut cap = kvm_enable_cap {
        cap: kvm_bindings::KVM_CAP_SPLIT_IRQCHIP,
        ..Default::default()
    };
    cap.args[0] = ioapic::NUM_PINS as u64;
    vm.enable_cap(&cap)
        .context("enabling the split irqchip failed")?;
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
    let vm = Arc::new(vm);
    let interrupts = Arc::new(Interrupts {
        vm: vm.clone(),
        state: Mutex::new(IoapicState {
            ioapic: Ioapic::new(),
            routes: Vec::new(),
        }),
    });
    Ok((
        Hypervisor {
            vm,
            interrupts,
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
        let stats = KvmStats::open(&fd, self.mapping.clone()).map(Arc::new);
        Ok(Vcpu {
            fd,
            kicker,
            interrupts: self.interrupts.clone(),
            counts: Arc::new(ExitCounts::new()),
            stats,
            incomplete: None,
            _mapping: self.mapping.clone(),
        })
    }

    fn set_irq_line(&self, pin: u32, level: bool) -> Result<()> {
        if pin as usize >= ioapic::NUM_PINS {
            bail!("I/O APIC pin {pin} does not exist");
        }
        self.interrupts.set_irq(pin as usize, level)
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

/// A vCPU's statistics from KVM (its binary statistics file): the counters that only
/// grow, which include the exits that KVM handles itself (e.g., `kvm.halt_exits`).
struct KvmStats {
    /// The file holds a reference to the VM.
    file: File,
    /// The counters' names (`kvm.` and KVM's name) and their indices into the data
    /// block.
    counters: Vec<(String, usize)>,
    /// Where the data block (of u64 values) is in the file.
    data_offset: u64,
    /// The number of values in the data block that we read.
    data_len: usize,
    /// Declared after `file`, such that the VM (which the file keeps alive) is
    /// destroyed first (see [`Hypervisor`]).
    _mapping: Arc<Mapping>,
}

impl KvmStats {
    /// None if KVM has no binary statistics (before Linux 5.14) or reading them fails.
    fn open(vcpu: &VcpuFd, mapping: Arc<Mapping>) -> Option<Self> {
        // SAFETY: KVM_GET_STATS_FD takes no argument.
        let fd = unsafe { vmm_sys_util::ioctl::ioctl(vcpu, KVM_GET_STATS_FD()) };
        if fd < 0 {
            return None;
        }
        // SAFETY: KVM returned a new file descriptor, which nothing else owns.
        let file = unsafe { File::from_raw_fd(fd) };
        let mut header = [0u8; size_of::<kvm_stats_header>()];
        file.read_exact_at(&mut header, 0).ok()?;
        let field = |offset| u32_at(&header, offset);
        let name_size = field(offset_of!(kvm_stats_header, name_size)) as usize;
        let num_desc = field(offset_of!(kvm_stats_header, num_desc)) as usize;
        let desc_offset = field(offset_of!(kvm_stats_header, desc_offset));
        let data_offset = field(offset_of!(kvm_stats_header, data_offset));
        let mut descs = vec![0u8; (size_of::<kvm_stats_desc>() + name_size) * num_desc];
        file.read_exact_at(&mut descs, desc_offset.into()).ok()?;
        let counters = counters(&descs, name_size);
        let data_len = counters.iter().map(|&(_, index)| index + 1).max();
        Some(Self {
            file,
            data_offset: data_offset.into(),
            data_len: data_len.unwrap_or(0),
            counters,
            _mapping: mapping,
        })
    }

    /// Appends the counters' current values to `out`.
    fn snapshot(&self, out: &mut Vec<(String, u64)>) -> Result<()> {
        let mut data = vec![0u8; self.data_len * size_of::<u64>()];
        self.file
            .read_exact_at(&mut data, self.data_offset)
            .context("reading KVM's vCPU statistics failed")?;
        out.extend(self.counters.iter().map(|(name, index)| {
            let bytes = &data[index * size_of::<u64>()..][..size_of::<u64>()];
            (name.clone(), u64::from_ne_bytes(bytes.try_into().unwrap()))
        }));
        Ok(())
    }
}

/// The counters (cumulative statistics with a single value) that the descriptors
/// `descs` describe (each a struct kvm_stats_desc followed by a name of `name_size`
/// bytes): their names (`kvm.` and KVM's name) and their indices into the data block.
fn counters(descs: &[u8], name_size: usize) -> Vec<(String, usize)> {
    let desc_size = size_of::<kvm_stats_desc>() + name_size;
    descs
        .chunks_exact(desc_size)
        .filter_map(|desc| {
            let flags = u32_at(desc, offset_of!(kvm_stats_desc, flags));
            let size = offset_of!(kvm_stats_desc, size);
            let size = u16::from_ne_bytes([desc[size], desc[size + 1]]);
            // In bytes, from the start of the data block.
            let offset = u32_at(desc, offset_of!(kvm_stats_desc, offset)) as usize;
            if flags & KVM_STATS_TYPE_MASK != KVM_STATS_TYPE_CUMULATIVE || size != 1 {
                return None;
            }
            let name = &desc[offset_of!(kvm_stats_desc, name)..];
            let name = name.split(|&b| b == 0).next().unwrap_or_default();
            let name = format!("kvm.{}", String::from_utf8_lossy(name));
            Some((name, offset / size_of::<u64>()))
        })
        .collect()
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

kinds! {
    /// The exits that the backend handles itself (see [`ExitCounts`]).
    enum Internal {
        Eoi => "exit.eoi",
        /// An access to the I/O APIC.
        Ioapic => "exit.ioapic",
    }
}

#[derive(Clone)]
pub struct Counters {
    exits: Arc<ExitCounts<Internal>>,
    stats: Option<Arc<KvmStats>>,
}

impl CountersOps for Counters {
    fn snapshot(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        self.exits.snapshot(&mut out);
        if let Some(stats) = &self.stats {
            stats.snapshot(&mut out)?;
        }
        Ok(out)
    }
}

pub struct Vcpu {
    fd: VcpuFd,
    kicker: Arc<KickState>,
    interrupts: Arc<Interrupts>,
    counts: Arc<ExitCounts<Internal>>,
    stats: Option<Arc<KvmStats>>,
    /// The instruction that waits for a [`Completion`].
    incomplete: Option<Incomplete>,
    /// Declared after `fd`, such that the vCPU (and thus maybe the VM) is destroyed first.
    _mapping: Arc<Mapping>,
}

/// An exit that needs a [`Completion`].
#[derive(Clone, Copy)]
enum Incomplete {
    In { size: u8 },
    Out,
    MmioRead { size: u8 },
    MmioWrite,
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
            (Incomplete::MmioRead { size }, Completion::Done(value)) => {
                // SAFETY: The last exit was KVM_EXIT_MMIO.
                let mmio = unsafe { &mut run.__bindgen_anon_1.mmio };
                mmio.data[..size as usize].copy_from_slice(&value.to_le_bytes()[..size as usize]);
            }
            (Incomplete::Out | Incomplete::MmioWrite, Completion::Done(_)) => {}
            (Incomplete::In { .. } | Incomplete::Out, Completion::Fault) => {
                bail!("faulting port I/O is not supported")
            }
            (Incomplete::MmioRead { .. } | Incomplete::MmioWrite, Completion::Fault) => {
                bail!("faulting MMIO is not supported")
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
        let is_ioapic = |address| (IOAPIC_BASE..IOAPIC_BASE + IOAPIC_SIZE).contains(&address);
        let (incomplete, exit) = match exit {
            VcpuExit::IrqWindowOpen => return Ok(None),
            // We emulate the I/O APIC here; KVM completes the access on the next run.
            VcpuExit::MmioRead(address, data) if is_ioapic(address) => {
                self.counts.count_internal(Internal::Ioapic);
                let value = self.interrupts.read(address - IOAPIC_BASE).to_le_bytes();
                let size = data.len().min(8);
                data[..size].copy_from_slice(&value[..size]);
                return Ok(None);
            }
            VcpuExit::MmioWrite(address, data) if is_ioapic(address) => {
                self.counts.count_internal(Internal::Ioapic);
                let mut bytes = [0u8; 8];
                let size = data.len().min(8);
                bytes[..size].copy_from_slice(&data[..size]);
                let value = u64::from_le_bytes(bytes);
                self.interrupts.write(address - IOAPIC_BASE, size, value)?;
                return Ok(None);
            }
            VcpuExit::IoapicEoi(vector) => {
                self.counts.count_internal(Internal::Eoi);
                self.interrupts.eoi(vector)?;
                return Ok(None);
            }
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
                let exit = Exit::Mmio {
                    address,
                    size,
                    write: false,
                    data: 0,
                };
                (Some(Incomplete::MmioRead { size }), exit)
            }
            VcpuExit::MmioWrite(address, data) => {
                let mut bytes = [0u8; 8];
                let size = data.len().min(8);
                bytes[..size].copy_from_slice(&data[..size]);
                let exit = Exit::Mmio {
                    address,
                    size: size as u8,
                    write: true,
                    data: u64::from_le_bytes(bytes),
                };
                (Some(Incomplete::MmioWrite), exit)
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
                // Not an exit: we did not enter the guest.
                return Ok(Exit::Kicked);
            }
            if let Some(exit) = self.enter()? {
                self.counts.count(&exit);
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

    fn kicker(&self) -> Kicker {
        Kicker(self.kicker.clone())
    }

    fn counters(&self) -> Counters {
        Counters {
            exits: self.counts.clone(),
            stats: self.stats.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use kvm_bindings::{KVM_STATS_TYPE_INSTANT, KVM_STATS_TYPE_LOG_HIST};

    use super::*;

    /// A struct kvm_stats_desc with a name of `NAME_SIZE` bytes.
    fn descriptor(flags: u32, size: u16, offset: u32, name: &str) -> Vec<u8> {
        let mut desc = vec![0u8; size_of::<kvm_stats_desc>() + NAME_SIZE];
        desc[offset_of!(kvm_stats_desc, flags)..][..4].copy_from_slice(&flags.to_ne_bytes());
        desc[offset_of!(kvm_stats_desc, size)..][..2].copy_from_slice(&size.to_ne_bytes());
        desc[offset_of!(kvm_stats_desc, offset)..][..4].copy_from_slice(&offset.to_ne_bytes());
        desc[offset_of!(kvm_stats_desc, name)..][..name.len()].copy_from_slice(name.as_bytes());
        desc
    }

    const NAME_SIZE: usize = 48;

    #[test]
    fn finds_the_counters() {
        let descs = [
            descriptor(KVM_STATS_TYPE_CUMULATIVE, 1, 0, "exits"),
            descriptor(KVM_STATS_TYPE_INSTANT, 1, 8, "guest_mode"),
            descriptor(KVM_STATS_TYPE_LOG_HIST, 32, 16, "halt_poll_success_hist"),
            descriptor(KVM_STATS_TYPE_CUMULATIVE, 1, 272, "halt_exits"),
        ]
        .concat();
        assert_eq!(
            counters(&descs, NAME_SIZE),
            [
                ("kvm.exits".to_string(), 0),
                ("kvm.halt_exits".to_string(), 34)
            ]
        );
    }
}
