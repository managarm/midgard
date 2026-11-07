//! What bifrost asks midgard on midgard's socket ([`crate::runtime::daemon_socket`]).
//! bifrost sends one [`Request`] per connection; midgard answers with [`Response`]s.

use serde::{Deserialize, Serialize};

use crate::machine::MachineConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// Boot a VM. midgard sends its console output until it stops, then
    /// [`Response::Stopped`]; the VM stops when bifrost closes the connection.
    Boot(MachineConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Output of the VM's console.
    Console(Vec<u8>),
    /// The VM stopped (why, for the user).
    Stopped(String),
    /// The request failed.
    Error(String),
}
