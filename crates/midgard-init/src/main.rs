//! midgard-init: PID 1 of midgard's Linux VMs.
//!
//! midgard boots the guest with an initramfs that contains this program as `/init`.
//! It mounts the distribution's root file system, moves into it and stays PID 1:
//! it reaps all processes and runs sessions (commands for midgard's CLI) that arrive
//! over vsock.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("midgard-init only runs in Linux guests");
    std::process::exit(1);
}
