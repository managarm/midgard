//! Where midgard's files are: its inputs (the guest kernel and the agent) and the VM
//! instances' disks and instance files. The daemon's runtime files are in [`midgard_protocol::runtime`].

use std::path::PathBuf;

/// The extensions of distribution images: qcow2 or raw ext4 (in this order).
const IMAGE_EXTENSIONS: [&str; 2] = ["qcow2", "ext4"];

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

/// `$HOME/<relative>`.
fn in_home(relative: &str) -> Option<PathBuf> {
    env_path("HOME").map(|home| home.join(relative))
}

/// Where the guest kernel and the agent are, in the order in which midgard searches
/// them: ~/.local/lib/midgard (files that a user built) and /usr/lib/midgard (the
/// packages). MIDGARD_DATA_DIR replaces both.
pub fn data_dirs() -> Vec<PathBuf> {
    if let Some(dir) = env_path("MIDGARD_DATA_DIR") {
        return vec![dir];
    }
    let mut dirs: Vec<PathBuf> = in_home(".local/lib/midgard").into_iter().collect();
    dirs.push("/usr/lib/midgard".into());
    dirs
}

/// Returns the first data directory's `name` that exists (else the one in the last
/// directory, such that errors name it).
fn data_file(name: &str) -> PathBuf {
    let dirs = data_dirs();
    let path = dirs
        .iter()
        .map(|dir| dir.join(name))
        .find(|path| path.exists());
    path.unwrap_or_else(|| dirs.last().unwrap().join(name))
}

/// The guest kernel.
pub fn kernel() -> PathBuf {
    data_file("vmlinuz")
}

/// The guest agent (midgard-init).
pub fn agent() -> PathBuf {
    data_file("midgard-init")
}

/// Where the VM instances' disks live: MIDGARD_STATE_DIR, else /var/lib/midgard for
/// root and $XDG_DATA_HOME/midgard (~/.local/share/midgard) for other users.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = env_path("MIDGARD_STATE_DIR") {
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

/// The disks that a VM instance may have (in [`state_dir`]; one at most exists).
pub fn disk_candidates(name: &str) -> impl Iterator<Item = PathBuf> {
    let dir = state_dir();
    IMAGE_EXTENSIONS
        .into_iter()
        .map(move |extension| dir.join(format!("{name}.{extension}")))
}

/// The instance file of a VM instance (in [`state_dir`], next to its disk), which it
/// does not need to have.
pub fn instance_file(name: &str) -> PathBuf {
    state_dir().join(format!("{name}.toml"))
}
