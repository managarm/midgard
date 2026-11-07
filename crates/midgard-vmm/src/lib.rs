//! midgard's virtual machine monitor: runs Linux in a lightweight VM on top of thor's
//! virtualization API (or on KVM, for development on Linux; see [`midgard_hv`]).
//!
//! The guest boots directly into the kernel (no firmware) with a minimal PC-like
//! platform: x2APICs with TSC-deadline timers and an I/O APIC (both emulated by the
//! hypervisor), kvmclock and a 16550A UART. Minimal ACPI tables describe the CPU and
//! power off.

#![warn(unreachable_pub)]

mod acpi;
mod boot;
mod clock;
mod devices;
mod logging;
pub mod machine;
mod platform;
mod vcpu;
mod vm;

pub use vm::{Stop, Vm};
