//! midgard's virtual machine monitor: runs Linux in a lightweight VM on top of thor's
//! virtualization API (or on KVM, for development on Linux; see [`midgard_hv`]).
//!
//! The guest boots directly into the kernel (no firmware) with a minimal PC-like
//! platform: x2APICs with TSC-deadline timers and an I/O APIC (both emulated by the
//! hypervisor), kvmclock, a CMOS RTC, a 16550A UART and virtio-mmio devices (console,
//! block, network, vsock). Minimal ACPI tables describe the CPUs and the legacy devices
//! and power off.
//!
//! Each vCPU runs on its own thread; everything else on the host side (disk I/O,
//! the network stack, midgard's sessions) runs as tasks on one executor per VM (see [`Machine::run`]).
//!
//! [`start`] boots a guest and returns a [`Machine`], which the calling thread owns and
//! drives. Everything else that wants to talk to the running VM (e.g., the daemon's
//! session tasks) holds a [`MachineHandle`]. The guest's hardware itself (RAM, devices, I/O routing) is
//! private to this crate.

#![warn(unreachable_pub)]

mod acpi;
mod aml;
mod board;
mod boot;
mod clock;
mod counters;
mod devices;
mod host;
mod logging;
mod machine;
mod platform;
mod vcpu;
mod virtio;

pub use machine::{Machine, MachineHandle, Stop, start, validate_memory};
pub use virtio::vsock::{VsockListener, VsockStream};
