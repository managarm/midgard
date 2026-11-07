//! The configuration of a VM.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Everything that bifrost decided about a VM; midgard boots it as described. bifrost
/// sends it to midgard when it launches midgard (see [`crate::write_message`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    /// The guest kernel (a bzImage).
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
    /// Replaces midgard's base kernel parameters.
    pub cmdline: Option<String>,
    /// Kernel parameters in addition to the base ones.
    pub append: Vec<String>,
    /// The guest's RAM in bytes.
    pub memory: u64,
    /// The number of vCPUs.
    pub cpus: u32,
}
