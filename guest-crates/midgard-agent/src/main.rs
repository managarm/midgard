//! midgard-agent: midgard's guest agent in Linux (and Managarm) VMs.
//!
//! midgard-init (PID 1) installs it in the guest and starts it (`midgard-agent serve
//! --from-midgard-init`) before the distribution's init; a guest whose image has the
//! agent starts it itself, e.g., as a service (`midgard-agent serve`; e.g., Managarm,
//! which runs the Linux code, as it has Linux's ABI), and the agent installs itself. It
//! sets up the guest as midgard configures it, runs sessions (commands for midgard's
//! CLI) that arrive over vsock, watches the guest boot and runs the Wayland display. The
//! guest runs it under another name, `/run/midgard/midgard-helper`, and in other modes,
//! e.g., to tell it that the guest booted.

#[cfg(any(target_os = "linux", target_os = "managarm"))]
mod linux;

#[cfg(any(target_os = "linux", target_os = "managarm"))]
fn main() {
    linux::main();
}

#[cfg(not(any(target_os = "linux", target_os = "managarm")))]
fn main() {
    eprintln!("midgard-agent only runs in Linux and Managarm guests");
    std::process::exit(1);
}
