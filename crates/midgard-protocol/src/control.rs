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
    /// Download boot files and templates from a channel and install them. midgard says
    /// what it does ([`Response::Pull`]) until it is done ([`Response::Pulled`]).
    Pull(Pull),
}

/// What to install or update: the boot files that are installed or that the templates
/// and VM instances need, and templates.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Pull {
    /// The templates to install or update.
    pub templates: Vec<String>,
    /// Update all templates that are installed, too.
    pub all: bool,
    /// The channel (a URL or a directory); midgard's default if absent.
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Output of the VM's console.
    Console(#[serde(with = "serde_bytes")] Vec<u8>),
    /// The VM stopped (why, for the user).
    Stopped(String),
    /// The VM of the VM instance boots.
    Starting,
    /// The connection now goes to the agent, which speaks this version of the guest
    /// protocol with midgard (see [`crate::version`]): bifrost must not send what that
    /// version lacks.
    Ready {
        protocol: u32,
    },
    /// There is no such VM instance.
    NoInstance,
    Running(Vec<String>),
    List(Listing),
    /// Progress of [`Request::Pull`].
    Pull(PullEvent),
    /// [`Request::Pull`] succeeded.
    Pulled(Pulled),
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

/// What midgard installs from a channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullItem {
    BootFiles(String),
    Template(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PullEvent {
    /// midgard downloaded `done` of the `total` bytes of a file.
    Downloading { file: String, done: u64, total: u64 },
    /// midgard installed the item, or updated it.
    Installed(PullItem),
    /// The item is installed already.
    UpToDate(PullItem),
}

/// The templates after [`Request::Pull`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Pulled {
    /// The templates that are installed.
    pub installed: Vec<String>,
    /// The templates of the channel that are not installed.
    pub available: Vec<String>,
}
