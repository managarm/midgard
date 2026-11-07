//! The host side of the devices: what the guest's disks and its network end up in on the
//! host. Nothing here is visible to the guest.

pub(crate) mod image;
pub(crate) mod net;
