//! midgard's virtual machine monitor: runs Linux in a lightweight VM on top of thor's
//! virtualization API (or on KVM, for development on Linux; see [`midgard_hv`]).
//!
//! The guest boots directly into the kernel (no firmware) with a minimal PC-like
//! platform: x2APICs with TSC-deadline timers and an I/O APIC (both emulated by the
//! hypervisor), kvmclock and a 16550A UART. Minimal ACPI tables describe the CPU and
//! power off.
//!
//! [`start`] boots a guest and returns a [`Machine`], which the calling thread owns and
//! drives. Everything else that wants to talk to the running VM (e.g., the daemon's
//! connection tasks) holds a [`MachineHandle`]. The guest's hardware itself (RAM,
//! devices, I/O routing) is private to this crate.

#![warn(unreachable_pub)]

mod acpi;
mod board;
mod boot;
mod clock;
mod devices;
mod logging;
mod machine;
mod platform;
mod vcpu;

pub use machine::{Machine, MachineHandle, Stop, start};
