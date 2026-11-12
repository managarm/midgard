//! What bifrost and the agent say in a session.
//!
//! A session starts with [`control::Request::Session`]. Once midgard answered
//! [`control::Response::Ready`], it forwards the connection to the agent over vsock
//! without looking at it. One connection carries one session: the client's first message
//! to the agent is [`Request::Exec`] (or [`Request::Ping`] / [`Request::Shutdown`]); then
//! stdin, resizes and signals flow to the agent and output flows back until
//! [`Response::Exited`].
//!
//! [`control::Request::Session`]: crate::control::Request::Session
//! [`control::Response::Ready`]: crate::control::Response::Ready

use serde::{Deserialize, Serialize};

/// The size of a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exec {
    /// The command; empty means the user's login shell.
    pub argv: Vec<String>,
    /// Environment variables in addition to the agent's defaults.
    pub env: Vec<(String, String)>,
    /// The working directory (the user's home if absent, or if the user may not enter
    /// it).
    pub cwd: Option<String>,
    /// The client's working directory on the host. Without `cwd`, the command starts
    /// where the guest sees it in [`crate::SHARE_MOUNT`] (if it does, see
    /// [`crate::guest::GuestConfig::share`]).
    pub host_cwd: Option<String>,
    /// The user that runs the command (the guest configuration's default if absent).
    pub user: Option<String>,
    /// Run the command on a pseudo terminal of this size (otherwise, on pipes).
    pub tty: Option<WindowSize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Exec(Exec),
    Stdin(Vec<u8>),
    /// The client's stdin ended.
    StdinEof,
    Resize(WindowSize),
    /// Deliver a signal to the command.
    Signal(i32),
    /// Shut down the guest (the VM stops).
    Shutdown,
    /// Answered with [`Response::Pong`]; checks that the agent is up.
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Started {
        pid: u32,
    },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command exited with `code` or was killed by `signal`.
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    Error(String),
    Pong,
}
