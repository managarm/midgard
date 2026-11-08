//! How the agent sets up the guest and runs sessions. midgard decides it (it is part of
//! a VM instance's settings) and passes it to the agent in the initramfs, as the file
//! [`CONFIG_FILE`] (one frame, see [`crate::write_message`]).

use serde::{Deserialize, Serialize};

/// The name of the guest configuration in the initramfs.
pub const CONFIG_FILE: &str = "guest-config";

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GuestConfig {
    /// The distribution's init (an absolute path in the guest), which the agent executes
    /// as PID 1 once it set up the guest; the agent keeps running as its child. Without
    /// it, the agent stays PID 1 and no services run.
    pub init: Option<String>,
    /// How sessions wait for the distribution's init to finish booting.
    pub ready: Ready,
}

/// How the agent learns that the guest finished booting (sessions wait for it, such
/// that they find the distribution's services running).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Ready {
    /// Sessions do not wait.
    #[default]
    Immediately,
    /// Until `systemctl is-system-running --wait` returns.
    Systemd,
    /// Until root in the guest runs `/run/midgard/midgard-init ready` (e.g., from its
    /// init's configuration, after the services started).
    Notify,
}
