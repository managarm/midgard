//! How the agent sets up the guest and runs sessions. midgard decides it (it is part of
//! a VM instance's settings) and passes it to the agent in the initramfs, as the file
//! [`CONFIG_FILE`]: two frames (see [`crate::write_message`]), midgard's [`Hello`] and the
//! [`GuestConfig`]. The agent checks the former before it reads the latter, which another
//! version of the guest protocol may encode differently.

use std::io::{self, Read};

use serde::{Deserialize, Serialize};

use crate::version::{Hello, Mismatch, Versions};

/// The name of the guest configuration in the initramfs.
pub const CONFIG_FILE: &str = "guest-config";

/// The contents of [`CONFIG_FILE`].
pub fn encode_config(config: &GuestConfig) -> io::Result<Vec<u8>> {
    let mut file = Vec::new();
    crate::write_message(&mut file, &Hello::OURS)?;
    crate::write_message(&mut file, config)?;
    Ok(file)
}

/// Why the agent cannot use [`CONFIG_FILE`].
#[derive(Debug)]
pub enum ConfigError {
    /// midgard speaks no version of the guest protocol that the agent speaks.
    Mismatch(Mismatch),
    Io(io::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Mismatch(mismatch) => write!(f, "midgard speaks {mismatch}"),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads [`CONFIG_FILE`]: the guest configuration and the version of the guest protocol
/// to speak with midgard.
pub fn read_config(reader: &mut impl Read) -> Result<(u32, GuestConfig), ConfigError> {
    let eof = || ConfigError::Io(io::ErrorKind::UnexpectedEof.into());
    let hello: Hello = crate::read_message(reader)
        .map_err(ConfigError::Io)?
        .ok_or_else(eof)?;
    let version = Versions::OURS
        .negotiate(hello.protocol)
        .map_err(ConfigError::Mismatch)?;
    let config = crate::read_message(reader)
        .map_err(ConfigError::Io)?
        .ok_or_else(eof)?;
    Ok((version, config))
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestConfig {
    /// The distribution's init (an absolute path in the guest), which the agent executes
    /// as PID 1 once it set up the guest; the agent keeps running as its child. Without
    /// it, the agent stays PID 1 and no services run.
    pub init: Option<String>,
    /// How sessions wait for the distribution's init to finish booting.
    pub ready: Ready,
    /// The user that sessions run as unless they name one (root if absent).
    pub user: Option<String>,
    /// A command that the agent runs on every boot (as root, before the distribution's
    /// init) with [`GuestConfig::user`] as an additional argument, e.g., to create the
    /// user. It must do nothing if there is nothing to do.
    pub user_setup: Option<Vec<String>>,
    /// How the agent runs the commands of sessions.
    pub launcher: Launcher,
    /// Who starts the Wayland display that forwards the guest's Wayland clients to the
    /// host (if midgard gives the guest a waypipe).
    pub display: Display,
    /// The guest's waypipe logs debug messages.
    pub waypipe_debug: bool,
}

/// Who starts the guest's Wayland display: a waypipe server that runs as a user and
/// listens on `$XDG_RUNTIME_DIR/wayland-0` (or another socket).
/// `/run/midgard/midgard-init waypipe [<socket>]` runs it (with the right options; it
/// connects to [`crate::WAYPIPE_PORT`] for each client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Display {
    /// The agent, for the default user (on a socket in /run/midgard). Sessions of that
    /// user get `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
    #[default]
    Agent,
    /// Every user's systemd instance, through a user unit that the agent installs in
    /// /run/systemd/user; /run/environment.d sets `WAYLAND_DISPLAY` for the instance's
    /// services (and thus for the commands of [`Launcher::Systemd`]; sessions of other
    /// launchers need the guest to set it).
    Systemd,
    /// The guest itself (e.g., a user service of its init system); the agent only
    /// installs waypipe.
    Guest,
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

/// How the agent runs a session's command as its user.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Launcher {
    /// The agent runs the command itself with the user's credentials and a minimal
    /// environment (outside of the distribution's sessions).
    #[default]
    Direct,
    /// As a transient service of the user's systemd instance (`systemd-run --user`),
    /// such that it gets the environment of the user's session. The instance must run
    /// (e.g., because the user lingers).
    Systemd,
    /// Through a command of the guest that opens a session for the user and runs a
    /// program as the user (e.g., `runuser -l {user} -s {helper}`). `{user}` stands for
    /// the user's name and `{helper}` for the agent's executable in the guest, which the
    /// command must run with the file descriptors that it got (see [`Launch`]); commands
    /// that close them (e.g., sudo) do not work.
    Wrapper(Vec<String>),
}

/// What the agent's executable runs when [`Launcher::Wrapper`] runs it in the user's
/// session. The agent passes it as one frame (see [`crate::write_message`]) in file
/// descriptor [`LAUNCH_FD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    /// The command; empty means the user's login shell.
    pub argv: Vec<String>,
    /// Environment variables in addition to the session's.
    pub env: Vec<(String, String)>,
    /// The working directory.
    pub cwd: String,
}

/// The file descriptor with the [`Launch`] request.
pub const LAUNCH_FD: i32 = 3;
