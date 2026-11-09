//! Where midgard's files are: its inputs (the guest kernel, the agent and the guest's
//! waypipe) and the VM instances' disks and instance files. The daemon's runtime files
//! are in [`midgard_protocol::runtime`].

use std::path::PathBuf;

/// The extensions of distribution images: qcow2 or raw ext4 (in this order).
const IMAGE_EXTENSIONS: [&str; 2] = ["qcow2", "ext4"];

/// Where packages install the guest kernel and the agent.
const PACKAGE_DIR: &str = "/usr/lib/midgard";

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// `$HOME/<relative>`.
fn in_home(relative: &str) -> Option<PathBuf> {
    env_path("HOME").map(|home| home.join(relative))
}

/// midgard's directory: MIDGARD_DATA_DIR, else /var/lib/midgard for root and
/// $XDG_DATA_HOME/midgard (~/.local/share/midgard) for other users. It has the VM
/// instances and the guest files that the user installed (see guest/build.py).
pub fn midgard_dir() -> PathBuf {
    if let Some(dir) = env_path("MIDGARD_DATA_DIR") {
        return dir;
    }
    // SAFETY: getuid() has no preconditions.
    if unsafe { libc::getuid() } == 0 {
        return "/var/lib/midgard".into();
    }
    env_path("XDG_DATA_HOME")
        .or_else(|| in_home(".local/share"))
        .unwrap_or_else(|| "/var/lib".into())
        .join("midgard")
}

/// Returns `name` from [`midgard_dir`] if it exists there, else the one that the
/// packages install (such that errors name it).
fn guest_file(name: &str) -> PathBuf {
    let path = midgard_dir().join(name);
    if path.exists() {
        return path;
    }
    PathBuf::from(PACKAGE_DIR).join(name)
}

/// The guest kernel.
pub fn kernel() -> PathBuf {
    guest_file("vmlinuz")
}

/// The guest agent (midgard-init).
pub fn agent() -> PathBuf {
    guest_file("midgard-init")
}

/// The guest's waypipe (a static Linux build), if there is one.
pub fn guest_waypipe() -> Option<PathBuf> {
    Some(guest_file("guest-waypipe")).filter(|path| path.exists())
}

/// The disks that a VM instance may have (in [`midgard_dir`]; one at most exists).
pub fn disk_candidates(name: &str) -> impl Iterator<Item = PathBuf> {
    let dir = midgard_dir();
    IMAGE_EXTENSIONS
        .into_iter()
        .map(move |extension| dir.join(format!("{name}.{extension}")))
}

/// The instance file of a VM instance (in [`midgard_dir`], next to its disk), which it
/// does not need to have.
pub fn instance_file(name: &str) -> PathBuf {
    midgard_dir().join(format!("{name}.toml"))
}
