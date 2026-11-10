//! What bifrost asks midgard on midgard's socket ([`crate::runtime::daemon_socket`]).
//! bifrost starts each connection with one [`Request`]; midgard answers with
//! [`Response`]s.

use serde::{Deserialize, Serialize};

use crate::machine::MachineConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Boot a VM. bifrost then sends [`Request::Input`] for its console; midgard sends
    /// its console output until it stops, then [`Response::Stopped`]. The VM stops when
    /// bifrost closes the connection.
    Boot(Box<MachineConfig>),
    /// Input for the console of the VM that [`Request::Boot`] started.
    Input(Vec<u8>),
    /// Connect to the agent in the VM of a VM instance, which midgard starts first if it
    /// does not run (and `start`). midgard answers [`Response::Starting`] if it starts
    /// the VM and [`Response::Ready`] once the agent answers; from then on, the
    /// connection carries the session protocol (see [`crate::session`]).
    Session { name: String, start: bool },
    /// The VM instances whose VMs run ([`Response::Running`]).
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Output of the VM's console.
    Console(Vec<u8>),
    /// The VM stopped (why, for the user).
    Stopped(String),
    /// The VM of the VM instance boots.
    Starting,
    /// The connection now goes to the agent.
    Ready,
    Running(Vec<String>),
    /// The request failed.
    Error(String),
}
