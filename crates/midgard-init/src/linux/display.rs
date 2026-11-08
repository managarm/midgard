//! Wayland forwarding: a waypipe server, running as a user, is the guest's Wayland
//! display (`$XDG_RUNTIME_DIR/wayland-0`). For each Wayland client, it connects to
//! midgard (vsock port [`WAYPIPE_PORT`]), which hands the connection to `waypipe client`
//! next to the host's compositor. `midgard-init waypipe` runs the server (see
//! [`server`]); who runs that depends on the guest configuration (see [`Display`]).
//!
//! midgard puts its static waypipe into the initramfs; we copy it into /run/midgard so
//! that any distribution can use it.

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::OnceLock;
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
    let mut command = Command::new(HELPER, argv, env);
    command.credentials = (user.uid != 0).then(|| Credentials {
        uid: user.uid,
        gid: user.gid,
        groups: user.groups.clone(),
    });
    if let Err(error) = process::spawn_background(&command) {
        log!("starting waypipe: {error}");
        return;
    }
    // Sessions find the socket, which waypipe creates soon after it started.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !Path::new(&socket).exists() && Instant::now() < deadline {
        Timer::after(Duration::from_millis(10)).await;
    }
    log!("started the Wayland display of {}", user.name);
    let _ = AGENT_DISPLAY.set((user.uid, socket));
}

/// The variables that sessions of `user` get for the agent's display (if it runs one
/// for them).
pub fn session_environment(user: &User) -> Vec<(String, String)> {
    let Some((_, socket)) = AGENT_DISPLAY.get().filter(|(uid, _)| *uid == user.uid) else {
        return Vec::new();
    };
    vec![
        ("XDG_RUNTIME_DIR".into(), runtime_dir(user)),
        // (Wayland clients take a path, too.)
        ("WAYLAND_DISPLAY".into(), socket.clone()),
    ]
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
