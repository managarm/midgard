//! Where midgard keeps its runtime files (the socket that bifrost connects to and the
//! VM instances' console logs), which bifrost and midgard both need to agree on.

use std::path::PathBuf;

/// The VM instance that bifrost uses unless it is given one.
pub const DEFAULT_VM: &str = "alpine";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// The directory with midgard's runtime files: $MIDGARD_RUNTIME_DIR,
/// $XDG_RUNTIME_DIR/midgard or /run/midgard.
pub fn runtime_root() -> PathBuf {
    env_path("MIDGARD_RUNTIME_DIR")
        .or_else(|| env_path("XDG_RUNTIME_DIR").map(|dir| dir.join("midgard")))
        .unwrap_or_else(|| "/run/midgard".into())
}

/// The socket that midgard listens on (systemd's midgard.socket creates it).
pub fn daemon_socket() -> PathBuf {
    runtime_root().join("midgard.sock")
}

/// The directory with the runtime files of a VM instance (in [`runtime_root`]).
pub fn runtime_dir(name: &str) -> PathBuf {
    runtime_root().join(name)
}

/// The console output of a VM instance's VM (since it started last).
pub fn console_log(name: &str) -> PathBuf {
    runtime_dir(name).join("console.log")
}
