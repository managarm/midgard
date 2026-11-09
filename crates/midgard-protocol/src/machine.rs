//! The configuration of a VM.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::guest::GuestConfig;

/// Everything that bifrost decided about a VM; midgard boots it as described. bifrost
/// sends it to midgard when it launches midgard (see [`crate::write_message`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    /// The guest kernel (a bzImage).
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    /// Boot midgard-init (from this file) as the guest's agent.
    pub agent: Option<PathBuf>,
    /// Replaces midgard's base kernel parameters.
    pub cmdline: Option<String>,
    /// Kernel parameters in addition to the base ones.
    pub append: Vec<String>,
    /// The guest's RAM in bytes.
    pub memory: u64,
    /// The number of vCPUs (None: midgard chooses).
    pub cpus: Option<u32>,
    /// The guest's console.
    pub console: ConsoleKind,
    /// The first disk is the root file system.
    pub disks: Vec<Disk>,
    /// User-mode networking.
    pub network: bool,
    /// The host directory that the guest sees in [`crate::SHARE_MOUNT`] (virtio-fs).
    pub share: Option<PathBuf>,
    /// How the agent sets up the guest (with the agent).
    pub guest: GuestConfig,
    /// The guest's waypipe (with the agent), which forwards the guest's Wayland clients
    /// to midgard (see [`crate::guest::Display`]).
    pub waypipe: Option<PathBuf>,
}

/// The device that the guest uses as its console (its output goes to midgard).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleKind {
    /// virtio-console (hvc0).
    Virtio,
    /// The serial port (ttyS0), which also works before the kernel finds its virtio
    /// devices (e.g., for debugging early boot).
    Serial,
}

/// A disk image (raw or qcow2) that the guest sees as a virtio block device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}
