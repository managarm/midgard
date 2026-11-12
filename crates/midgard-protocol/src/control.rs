//! What bifrost asks midgard on midgard's socket ([`crate::runtime::daemon_socket`]).
//! bifrost starts each connection with one [`Request`]; midgard answers with
//! [`Response`]s.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::machine::MachineConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Boot a VM. bifrost then sends [`Request::Input`] for its console; midgard sends
    /// its console output until it stops, then [`Response::Stopped`]. The VM stops when
    /// bifrost closes the connection.
    Boot(Box<MachineConfig>),
    /// Input for the console of the VM that [`Request::Boot`] started.
    Input(#[serde(with = "serde_bytes")] Vec<u8>),
    /// Connect to the agent in the VM of a VM instance, which midgard starts first if it
    /// does not run (and `start`). midgard answers [`Response::NoInstance`] if there is
    /// no such instance, [`Response::Starting`] if it starts the VM and
    /// [`Response::Ready`] once the agent answers; from then on, the
    /// connection carries the session protocol (see [`crate::session`]).
    Session { name: String, start: bool },
    /// The VM instances whose VMs run ([`Response::Running`]).
    Running,
    /// The VM instances and the templates ([`Response::List`]).
    List,
    /// Create a VM instance from a template ([`Response::Done`]), with the user that
    /// sessions run as (which the template's user-setup creates) and its own memory size.
    Create {
        name: String,
        template: String,
        user: Option<String>,
        memory: Option<u64>,
    },
    /// Delete a VM instance, whose VM must not run, and its disk ([`Response::Done`]).
    Delete { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Output of the VM's console.
    Console(#[serde(with = "serde_bytes")] Vec<u8>),
    /// The VM stopped (why, for the user).
    Stopped(String),
    /// The VM of the VM instance boots.
    Starting,
    /// The connection now goes to the agent.
    Ready,
    /// There is no such VM instance.
    NoInstance,
    Running(Vec<String>),
    List(Listing),
    /// The request succeeded.
    Done,
    /// The request failed.
    Error(String),
}

/// The VM instances and the templates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub instances: Vec<InstanceInfo>,
    pub templates: Vec<TemplateInfo>,
    /// Where the templates are.
    pub templates_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub name: String,
    /// The template that the instance was created from.
    pub template: Option<String>,
    /// Whether its VM runs.
    pub running: bool,
    /// The size of its disk's file.
    pub disk_size: Option<u64>,
    /// Why the instance is broken (e.g., an invalid instance file).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateInfo {
    pub name: String,
    pub description: Option<String>,
    /// Whether instances need a user (for the template's user-setup).
    pub needs_user: bool,
    /// Why the template is broken (e.g., it has no disk).
    pub error: Option<String>,
}
