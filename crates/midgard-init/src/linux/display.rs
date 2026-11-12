//! Wayland forwarding: a waypipe server, running as a user, is the guest's Wayland
//! display (`$XDG_RUNTIME_DIR/wayland-0`). For each Wayland client, it connects to
//! midgard (vsock port [`WAYPIPE_PORT`]), which hands the connection to `waypipe client`
//! next to the host's compositor. `midgard-init waypipe` runs the server (see
//! [`server`]); who runs that depends on the guest configuration (see [`Display`]).
//!
//! midgard puts its static waypipe into the initramfs; we copy it into /run/midgard so
//! that any distribution can use it.
//!
//! With [`Display::Agent`], X11 clients use xwayland-satellite if the distribution has
//! it: it runs a rootless Xwayland on display :0 as a client of our Wayland display.

use std::ffi::CStr;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_io::Timer;

use midgard_protocol::guest::{Display, GuestConfig};
use midgard_protocol::{HOST_CID, WAYPIPE_PORT};

use super::process::{self, Command, Credentials};
use super::users::User;
use super::{HELPER, PATH, RUN_DIR, log};

/// The display's name in the user's runtime directory.
const DISPLAY: &str = "wayland-0";
const WAYPIPE: &str = "/run/midgard/waypipe";
/// Exists if waypipe logs debug messages.
const DEBUG_FLAG: &str = "/run/midgard/waypipe-debug";

/// The systemd user unit that runs the server (with [`Display::Systemd`]).
const UNIT: &str = "midgard-waypipe.service";
const UNIT_FILE: &str = "\
[Unit]
Description=Wayland forwarding to midgard's host

[Service]
ExecStart=/run/midgard/midgard-init waypipe
Restart=on-failure

[Install]
WantedBy=default.target
";

/// Reads the initramfs's waypipe (before we leave the initramfs).
pub fn load() -> Option<Vec<u8>> {
    let waypipe = std::fs::read("/waypipe").ok();
    // The initramfs stays in memory.
    let _ = std::fs::remove_file("/waypipe");
    waypipe
}

/// Installs waypipe in /run/midgard (once /run is mounted, before the distribution's
/// init starts) and what the configuration's [`Display`] needs.
pub fn install(waypipe: &[u8], config: &GuestConfig) {
    let result = std::fs::write(WAYPIPE, waypipe)
        .and_then(|()| std::fs::set_permissions(WAYPIPE, PermissionsExt::from_mode(0o755)));
    if let Err(error) = result {
        log!("installing {WAYPIPE}: {error}");
        return;
    }
    if config.waypipe_debug {
        let _ = std::fs::write(DEBUG_FLAG, "");
    }
    if config.display == Display::Systemd
        && let Err(error) = install_systemd_unit()
    {
        log!("installing the systemd user unit {UNIT}: {error}");
    }
}

/// Every user's systemd instance runs the server, and its environment names the display
/// (for the session's services and the commands of sessions).
fn install_systemd_unit() -> io::Result<()> {
    let units = Path::new("/run/systemd/user");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(units.join("default.target.wants"))?;
    std::fs::write(units.join(UNIT), UNIT_FILE)?;
    let link = units.join("default.target.wants").join(UNIT);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(Path::new("..").join(UNIT), link)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create("/run/environment.d")?;
    std::fs::write(
        "/run/environment.d/50-midgard.conf",
        format!("WAYLAND_DISPLAY={DISPLAY}\n"),
    )
}

/// The display that the agent runs for a user (with [`Display::Agent`]): the user's ID and
/// the socket.
static AGENT_DISPLAY: OnceLock<(u32, String)> = OnceLock::new();

fn runtime_dir(user: &User) -> String {
    format!("/run/user/{}", user.uid)
}

/// Creates a directory that belongs to `user` (if it does not exist).
fn create_user_dir(dir: &str, user: &User) -> io::Result<()> {
    if Path::new(dir).exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    std::os::unix::fs::chown(dir, Some(user.uid), Some(user.gid))
}

/// Starts the server for `user` (with [`Display::Agent`], once the guest booted). Its
/// socket is not in the user's runtime directory: logind may mount a tmpfs over that
/// (when the user logs in for the first time), which would hide the socket.
pub async fn start(user: &User) {
    let dir = runtime_dir(user);
    let socket_dir = format!("{RUN_DIR}/display-{}", user.uid);
    let result = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create("/run/user")
        .and_then(|()| create_user_dir(&dir, user))
        .and_then(|()| create_user_dir(&socket_dir, user));
    if let Err(error) = result {
        log!("creating the directories of {}: {error}", user.name);
        return;
    }
    let socket = format!("{socket_dir}/{DISPLAY}");
    let argv = ["midgard-init", "waypipe", &socket]
        .map(String::from)
        .into();
    let env = vec![
        format!("PATH={PATH}"),
        format!("HOME={}", user.home),
        format!("XDG_RUNTIME_DIR={dir}"),
    ];
    if let Err(error) = process::spawn_background(&user_command(user, HELPER, argv, env)) {
        log!("starting waypipe: {error}");
        return;
    }
    // Sessions find the socket, which waypipe creates soon after it started.
    wait_for(Duration::from_secs(2), || Path::new(&socket).exists()).await;
    log!("started the Wayland display of {}", user.name);
    let _ = AGENT_DISPLAY.set((user.uid, socket));
}

/// A command that runs as `user`.
fn user_command(user: &User, path: &str, argv: Vec<String>, env: Vec<String>) -> Command {
    let mut command = Command::new(path, argv, env);
    command.credentials = (user.uid != 0).then(|| Credentials {
        uid: user.uid,
        gid: user.gid,
        groups: user.groups.clone(),
    });
    command
}

/// The X11 display that xwayland-satellite provides.
const X_DISPLAY: &str = ":0";
const X_SOCKETS: &str = "/tmp/.X11-unix";
const X_SOCKET: &str = "/tmp/.X11-unix/X0";
/// [`X_SOCKET`] in [`X_SOCKETS`].
const X_SOCKET_NAME: &CStr = c"X0";
const X_LOCK: &str = "/tmp/.X0-lock";
/// The running xwayland-satellite.
static XWAYLAND: Mutex<Option<i32>> = Mutex::new(None);

/// Starts xwayland-satellite for `user` (after [`start`]) if the distribution has it.
pub async fn start_x11(user: &User) {
    let Some(path) = process::resolve("xwayland-satellite", PATH) else {
        return;
    };
    // It needs the Wayland display (see [`start`]).
    let Some((_, wayland)) = AGENT_DISPLAY.get().filter(|(uid, _)| *uid == user.uid) else {
        return;
    };
    if !Path::new(wayland).exists() {
        log!("{wayland} does not exist; X11 forwarding is off");
        return;
    }
    let dir = runtime_dir(user);
    // Left over from an earlier boot (/tmp may be on the disk).
    let _ = std::fs::remove_file(X_LOCK);
    if let Err(error) = prepare_socket_dir(Path::new(X_SOCKETS), 0, X_SOCKET_NAME) {
        log!("preparing {X_SOCKETS}: {error}; X11 forwarding is off");
        return;
    }
    let argv = vec!["xwayland-satellite".to_string(), X_DISPLAY.to_string()];
    let env = vec![
        format!("PATH={PATH}"),
        format!("HOME={}", user.home),
        format!("XDG_RUNTIME_DIR={dir}"),
        format!("WAYLAND_DISPLAY={wayland}"),
    ];
    let pid = match process::spawn_background(&user_command(user, &path, argv, env)) {
        Ok(pid) => pid,
        Err(error) => {
            log!("starting xwayland-satellite: {error}");
            return;
        }
    };
    *XWAYLAND.lock().unwrap() = Some(pid);
    // It creates the socket once Xwayland started, which takes a while on slow hosts. A
    // session gets the display as long as it runs (it exits, e.g., if the compositor
    // lacks a protocol that it needs).
    wait_for(Duration::from_secs(2), || {
        Path::new(X_SOCKET).exists() || !alive(pid)
    })
    .await;
    if alive(pid) {
        log!("started xwayland-satellite on {X_DISPLAY}");
    }
}

/// Makes `path` a directory of `owner` in which everyone may create sockets (sticky, like
/// /tmp), without the stale socket `stale`. Its parent is writable by everyone (/tmp,
/// which may be on the disk), so its users may have left anything there, e.g., a
/// symlink to a directory that we must not change: we only trust a directory that we
/// opened without following symlinks.
fn prepare_socket_dir(path: &Path, owner: u32, stale: &CStr) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && metadata.uid() == owner => {}
        // (remove_dir_all() does not follow symlinks.)
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match std::fs::create_dir(path) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    // Someone may have replaced it meanwhile.
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    if dir.metadata()?.uid() != owner {
        return Err(io::Error::other(format!(
            "{} does not belong to {owner}",
            path.display()
        )));
    }
    dir.set_permissions(PermissionsExt::from_mode(0o1777))?;
    // SAFETY: `stale` is a C string; the descriptor is open.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), stale.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    Ok(())
}

fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.starts_with("xwayland"))
}

/// Polls `condition` until it holds, for at most `timeout`.
async fn wait_for(timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() && Instant::now() < deadline {
        Timer::after(Duration::from_millis(10)).await;
    }
}

/// The variables that sessions of `user` get for the agent's display (if it runs one
/// for them).
pub fn session_environment(user: &User) -> Vec<(String, String)> {
    let Some((_, socket)) = AGENT_DISPLAY.get().filter(|(uid, _)| *uid == user.uid) else {
        return Vec::new();
    };
    let mut env = vec![
        ("XDG_RUNTIME_DIR".into(), runtime_dir(user)),
        // (Wayland clients take a path, too.)
        ("WAYLAND_DISPLAY".into(), socket.clone()),
    ];
    if XWAYLAND.lock().unwrap().is_some_and(alive) {
        env.push(("DISPLAY".into(), X_DISPLAY.into()));
    }
    env
}

/// `midgard-init waypipe [<socket>]`: runs the server (as whoever runs us) on the socket
/// (default: `$XDG_RUNTIME_DIR/wayland-0`). It runs as long as its command, which is us
/// in a mode that waits forever (see [`pause`]).
pub fn server(socket: Option<String>) -> Result<(), String> {
    let socket = match socket {
        Some(socket) => socket,
        None => {
            let dir = std::env::var("XDG_RUNTIME_DIR").map_err(|_| "XDG_RUNTIME_DIR is not set")?;
            format!("{dir}/{DISPLAY}")
        }
    };
    if UnixStream::connect(&socket).is_ok() {
        return Err(format!("{socket} is in use"));
    }
    // Left over from a server that is gone.
    let _ = std::fs::remove_file(&socket);
    let mut command = std::process::Command::new(WAYPIPE);
    if Path::new(DEBUG_FLAG).exists() {
        command.arg("--debug");
    }
    command
        .arg("--vsock")
        .arg("--socket")
        .arg(format!("{HOST_CID}:{WAYPIPE_PORT}"))
        // midgard's waypipe has no compression, and the host only takes shared memory.
        .args(["--compress", "none", "--no-gpu", "--display"])
        .arg(&socket)
        .args(["server", "--", HELPER, "pause"]);
    // (exec() restores SIGPIPE, which Rust ignores.)
    let error = command.exec();
    Err(format!("{WAYPIPE}: {error}"))
}

/// `midgard-init pause`: the server's command.
pub fn pause() -> ! {
    loop {
        // SAFETY: pause() has no preconditions.
        unsafe { libc::pause() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_dirs() {
        let base = std::env::temp_dir().join(format!("midgard-init-x11-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: geteuid() cannot fail.
        let owner = unsafe { libc::geteuid() };
        let sockets = base.join(".X11-unix");
        let stale = c"X0";

        // A symlink is replaced, and what it points to stays as it was.
        let target = base.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, PermissionsExt::from_mode(0o700)).unwrap();
        std::fs::write(target.join("X0"), b"").unwrap();
        std::os::unix::fs::symlink(&target, &sockets).unwrap();
        prepare_socket_dir(&sockets, owner, stale).unwrap();
        let metadata = std::fs::symlink_metadata(&sockets).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.mode() & 0o7777, 0o1777);
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o7777, 0o700);
        assert!(target.join("X0").exists());

        // A directory of ours loses its stale socket only.
        std::fs::write(sockets.join("X0"), b"").unwrap();
        std::fs::write(sockets.join("X1"), b"").unwrap();
        prepare_socket_dir(&sockets, owner, stale).unwrap();
        assert!(!sockets.join("X0").exists());
        assert!(sockets.join("X1").exists());

        // A directory of someone else is replaced (as we are not `owner + 1`, the new one
        // is not trusted either).
        prepare_socket_dir(&sockets, owner + 1, stale).unwrap_err();
        assert!(sockets.is_dir());
        assert!(!sockets.join("X1").exists());

        // So is a file.
        std::fs::remove_dir(&sockets).unwrap();
        std::fs::write(&sockets, b"").unwrap();
        prepare_socket_dir(&sockets, owner, stale).unwrap();
        assert!(sockets.is_dir());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
