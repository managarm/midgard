//! The hypervisor interface that midgard runs on.
//!
//! The model: the backend emulates the local APICs (including HLT, IPIs and INIT / SIPI),
//! the I/O APIC and CPUID (from a table). (On Hel, thor emulates the local APIC of a
//! single vCPU; the backend emulates the I/O APIC and CPUID, and decodes MMIO
//! instructions. On KVM, the kernel emulates the local APICs, and the backend the I/O
//! APIC.) Exits report *what* the guest did; port I/O, MMIO, MSR and hypercall
//! exits leave the instruction unfinished until the next run completes it (see
//! [`Completion`]). A kick makes the running (or next) run return. Other backends (KVM,
//! for development on Linux) translate to this model.
//!
//! A kick may be observed more than once: a run may return [`Exit::Kicked`] for other
//! reasons (e.g., a signal on KVM), and one kick may make more than one run return.
//! Consumers must handle [`Exit::Kicked`] idempotently and never count them.
//!
//! On Linux, [`create`] installs a process-wide handler for SIGRTMIN, the signal with
//! which kicks interrupt KVM_RUN. The rest of the process must not use that signal.
//!
//! The backend is chosen at compile time: [`Hypervisor`], [`Vcpu`], [`Kicker`] and
//! [`Counters`] are the backend's own types, whose methods (see `backend!`) are the same
//! on all of them. Nothing here is a trait object.

#![warn(unreachable_pub)]

use anyhow::Result;

mod counters;
#[cfg(any(target_os = "managarm", test))]
mod decode;
mod ioapic;
pub mod memory;
pub mod x86;

pub use memory::{GuestMemory, MemoryError};
use x86::msr;

/// Gives a backend's type (`$ty`, which the backend defines) the inherent methods that
/// every backend has: they forward to the trait `$tr`, which each backend implements
/// for its type. The trait stays private, such that users of the crate call the
/// methods on the concrete type (statically dispatched) and never see a trait object,
/// and the compiler checks that each backend has all of them.
macro_rules! backend {
    (
        $(#[$type_attr:meta])*
        pub struct $ty:ident;
        trait $tr:ident {
            $(
                $(#[$attr:meta])*
                fn $name:ident(&$($self_:ident)+ $(, $arg:ident: $argty:ty)*) -> $ret:ty;
            )*
        }
    ) => {
        $(#[$type_attr])*
        trait $tr {
            $(fn $name(&$($self_)+ $(, $arg: $argty)*) -> $ret;)*
        }
        impl $ty {
            $(
                $(#[$attr])*
                #[inline(always)]
                pub fn $name(&$($self_)+ $(, $arg: $argty)*) -> $ret {
                    <Self as $tr>::$name(receiver!($($self_)+) $(, $arg)*)
                }
            )*
        }
    };
}

/// The `self` of `self` / `mut self` (for `backend!`: by hygiene, only the caller's
/// `self` token refers to the method's receiver).
macro_rules! receiver {
    ($self_:ident) => {
        $self_
    };
    (mut $self_:ident) => {
        $self_
    };
}

#[cfg(target_os = "managarm")]
mod hel;
#[cfg(target_os = "managarm")]
use hel as imp;
#[cfg(target_os = "linux")]
mod kvm;
#[cfg(target_os = "linux")]
use kvm as imp;

#[cfg(not(any(target_os = "managarm", target_os = "linux")))]
compile_error!("midgard-hv has no backend for this target OS");

pub use imp::{Counters, Hypervisor, Kicker, MAX_VCPUS, Vcpu};

/// Guest-physical addresses that the backend may use for itself, which the VMM must
/// leave alone (KVM puts its identity map and real-mode TSS there).
pub const RESERVED_BASE: u64 = 0xFFFB_C000;
pub const RESERVED_SIZE: u64 = 0x4000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    /// The segment register does not hold a usable segment (e.g., a null selector).
    pub unusable: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DescriptorTable {
    pub base: u64,
    pub limit: u16,
}

/// The registers that the VMM needs on most exits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BasicRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rsp: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// The full register state (the basic registers plus the system registers).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    pub basic: BasicRegs,
    pub cs: Segment,
    pub ds: Segment,
    pub es: Segment,
    pub fs: Segment,
    pub gs: Segment,
    pub ss: Segment,
    pub tr: Segment,
    pub ldt: Segment,
    pub gdt: DescriptorTable,
    pub idt: DescriptorTable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
}

/// Guest state that the CPU does not switch on its own (the hypervisor does).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Msrs {
    pub kernel_gs_base: u64,
    pub star: u64,
    pub lstar: u64,
    pub cstar: u64,
    pub sfmask: u64,
    pub sysenter_cs: u64,
    pub sysenter_esp: u64,
    pub sysenter_eip: u64,
    /// XCR0 (not an MSR, but switched in the same way).
    pub xcr0: u64,
}

impl Msrs {
    /// The MSRs in this struct (all fields but XCR0).
    pub const INDICES: [u32; 8] = [
        msr::KERNEL_GS_BASE,
        msr::STAR,
        msr::LSTAR,
        msr::CSTAR,
        msr::SFMASK,
        msr::SYSENTER_CS,
        msr::SYSENTER_ESP,
        msr::SYSENTER_EIP,
    ];

    /// The field that holds the MSR `index` (one of [`Msrs::INDICES`]).
    pub fn get_mut(&mut self, index: u32) -> Option<&mut u64> {
        match index {
            msr::KERNEL_GS_BASE => Some(&mut self.kernel_gs_base),
            msr::STAR => Some(&mut self.star),
            msr::LSTAR => Some(&mut self.lstar),
            msr::CSTAR => Some(&mut self.cstar),
            msr::SFMASK => Some(&mut self.sfmask),
            msr::SYSENTER_CS => Some(&mut self.sysenter_cs),
            msr::SYSENTER_ESP => Some(&mut self.sysenter_esp),
            msr::SYSENTER_EIP => Some(&mut self.sysenter_eip),
            _ => None,
        }
    }

    pub fn get(&self, index: u32) -> Option<u64> {
        let mut copy = *self;
        copy.get_mut(index).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Port I/O (needs a [`Completion`]; for IN, its value is the result). `data` is the
    /// value of OUT. String instructions are not supported.
    Io {
        port: u16,
        size: u8,
        input: bool,
        string: bool,
        data: u32,
    },
    /// An access to guest-physical memory that is not RAM and not a device of the
    /// backend (needs a [`Completion`]; for a read, its value is the result). `size` is
    /// 1, 2, 4 or 8 bytes; `data` is the written value (of `size` bytes).
    Mmio {
        address: u64,
        size: u8,
        write: bool,
        data: u64,
    },
    /// RDMSR (needs a [`Completion`] with the value).
    Rdmsr { index: u32 },
    /// WRMSR (needs a [`Completion`]).
    Wrmsr { index: u32, value: u64 },
    /// [`Kicker::kick`] was called.
    Kicked,
    /// A triple fault.
    Shutdown,
    /// VMCALL / VMMCALL (needs a [`Completion`]; its value goes to RAX).
    /// (KVM handles hypercalls itself.)
    Hypercall,
    /// The hypervisor could not run the vCPU (e.g., the VM entry failed).
    Failed(String),
}

/// Finishes an instruction that exited (I/O, MMIO, MSR and hypercall exits): the next run
/// either completes it or makes it raise #GP(0) (which port I/O and MMIO do not support).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// The instruction completes with this result (if it has one).
    Done(u64),
    /// The instruction raises #GP(0).
    Fault,
}

backend! {
    /// What a backend's [`Kicker`] does: it makes a vCPU's run return with
    /// [`Exit::Kicked`]; if the vCPU does not run, its next run returns immediately.
    /// A `Kicker` is cheap to clone and works from any thread.
    pub struct Kicker;
    trait KickerOps {
        /// Kicks the vCPU.
        fn kick(&self) -> ();
    }
}

backend! {
    /// What a backend's [`Counters`] do: they count what a vCPU does (e.g., its exits by
    /// kind), for diagnostics. The names are the backend's: `exit.*` for the exits that
    /// the VMM sees, which all backends count (except those on which the VM stops), and
    /// whatever else the backend counts (the exits that it handles itself, e.g., the I/O
    /// APIC's; on KVM, the exits that KVM handles are in KVM's statistics). The values
    /// only grow. `Counters` are cheap to clone and work from any thread (also while the
    /// vCPU runs).
    pub struct Counters;
    trait CountersOps {
        /// The current values, by name.
        fn snapshot(&self) -> Result<Vec<(String, u64)>>;
    }
}

backend! {
    /// What a backend's [`Vcpu`] does (it can move to another thread between runs).
    pub struct Vcpu;
    trait VcpuOps {
        /// Runs the vCPU until it exits. `completion` must be given if (and only if) the
        /// previous exit needs one.
        fn run(&mut self, completion: Option<Completion>) -> Result<Exit>;
        fn basic_regs(&mut self) -> Result<BasicRegs>;
        fn set_basic_regs(&mut self, regs: &BasicRegs) -> Result<()>;
        fn regs(&mut self) -> Result<Regs>;
        fn set_regs(&mut self, regs: &Regs) -> Result<()>;
        fn msrs(&mut self) -> Result<Msrs>;
        fn set_msrs(&mut self, msrs: &Msrs) -> Result<()>;
        /// The vCPU's kicker (which other threads use while it runs).
        fn kicker(&self) -> Kicker;
        /// The vCPU's counters (which other threads read while it runs).
        fn counters(&self) -> Counters;
    }
}

/// An entry of a vCPU's CPUID table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuidEntry {
    pub function: u32,
    pub index: u32,
    /// The entry only applies to its subleaf (`index`).
    pub significant_index: bool,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

backend! {
    /// What a backend's [`Hypervisor`] (a VM) does; it is shared by all vCPU threads.
    pub struct Hypervisor;
    trait HypervisorOps {
        /// Creates a vCPU (`id` is its x2APIC ID, below [`MAX_VCPUS`]). The local APIC
        /// starts in x2APIC mode.
        /// vCPU 0 is the bootstrap processor (it starts in its reset state; the VMM sets
        /// its registers); the others wait for a startup IPI.
        fn create_vcpu(&self, id: u32, cpuid: &[CpuidEntry]) -> Result<Vcpu>;
        /// Sets the level of an I/O APIC pin. An edge-triggered pin interrupts on the
        /// rising edge.
        fn set_irq_line(&self, pin: u32, level: bool) -> Result<()>;
        /// The frequency (in Hz) of the vCPUs' TSC, which is the host's TSC, as the host
        /// kernel knows it (such that the VMM does not have to measure it).
        fn tsc_hz(&self) -> Result<u64>;
    }
}

/// Creates a VM with `memory_size` bytes of RAM at guest-physical address zero.
pub fn create(memory_size: u64) -> Result<(Hypervisor, GuestMemory)> {
    imp::create(memory_size)
}

/// The threads that the VMM uses the backend's types from.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<Hypervisor>();
    send_sync::<Kicker>();
    send::<Vcpu>();
    send_sync::<GuestMemory>();
};
