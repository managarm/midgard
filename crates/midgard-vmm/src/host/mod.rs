//! The host side of the devices: what the guest's console, disks and network end up in
//! on the host. Nothing here is visible to the guest.

pub(crate) mod console;
pub(crate) mod image;
pub(crate) mod net;
pub(crate) mod ping;
