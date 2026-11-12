//! The versions of the guest protocol: what midgard and the agent (and the rest of the
//! guest files that midgard boots it with) assume about each other. It comprises the
//! guest configuration ([`crate::guest`]) and how it gets into the initramfs, the session
//! protocol ([`crate::session`]) and the constants in the crate's root.
//!
//! The agent is distributed apart from midgard, so both sides say which versions they
//! speak ([`Hello`]) and use the newest one that both do ([`Versions::negotiate`]): midgard
//! in the guest configuration's file, the agent when midgard connects to it once it
//! started.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The version of the guest protocol that this build speaks.
pub const PROTOCOL: u32 = 1;
/// The oldest version of the guest protocol that this build still speaks with a peer.
pub const OLDEST_PROTOCOL: u32 = 1;

/// The versions of the guest protocol that one side speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    /// The oldest version that this side still speaks.
    pub oldest: u32,
    /// The version that this side was built with (the newest that it speaks).
    pub current: u32,
}

impl Versions {
    /// The versions that this build speaks.
    pub const OURS: Self = Self {
        oldest: OLDEST_PROTOCOL,
        current: PROTOCOL,
    };

    /// The version to speak with `peer`: the newest one that both sides speak.
    pub fn negotiate(self, peer: Self) -> Result<u32, Mismatch> {
        let version = self.current.min(peer.current);
        if version < self.oldest {
            Err(Mismatch::PeerTooOld { ours: self, peer })
        } else if version < peer.oldest {
            Err(Mismatch::PeerTooNew { ours: self, peer })
        } else {
            Ok(version)
        }
    }
}

impl fmt::Display for Versions {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.oldest == self.current {
            write!(f, "version {}", self.current)
        } else {
            write!(f, "versions {} to {}", self.oldest, self.current)
        }
    }
}

/// The two sides speak no common version of the guest protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// The peer needs to be updated.
    PeerTooOld { ours: Versions, peer: Versions },
    /// This side needs to be updated.
    PeerTooNew { ours: Versions, peer: Versions },
}

impl Mismatch {
    /// The mismatch as the peer sees it.
    pub fn reversed(self) -> Self {
        match self {
            Self::PeerTooOld { ours, peer } => Self::PeerTooNew {
                ours: peer,
                peer: ours,
            },
            Self::PeerTooNew { ours, peer } => Self::PeerTooOld {
                ours: peer,
                peer: ours,
            },
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (Self::PeerTooOld { ours, peer } | Self::PeerTooNew { ours, peer }) = self;
        write!(f, "{peer} of the guest protocol instead of {ours}")
    }
}

impl std::error::Error for Mismatch {}

/// What each side tells the other first. Its encoding must not change (other than by new
/// fields), as every version of the other side needs to understand it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The versions of the guest protocol that the sender speaks.
    pub protocol: Versions,
}

impl Hello {
    /// What this build says.
    pub const OURS: Self = Self {
        protocol: Versions::OURS,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(oldest: u32, current: u32) -> Versions {
        Versions { oldest, current }
    }

    #[test]
    fn negotiation() {
        assert_eq!(Versions::OURS.negotiate(Versions::OURS), Ok(PROTOCOL));
        assert_eq!(versions(2, 5).negotiate(versions(1, 3)), Ok(3));
        assert_eq!(versions(1, 3).negotiate(versions(2, 5)), Ok(3));
        assert_eq!(versions(3, 3).negotiate(versions(1, 7)), Ok(3));
        let (ours, peer) = (versions(4, 5), versions(1, 3));
        let too_old = Mismatch::PeerTooOld { ours, peer };
        assert_eq!(ours.negotiate(peer), Err(too_old));
        assert_eq!(peer.negotiate(ours), Err(too_old.reversed()));
        assert_eq!(
            too_old.to_string(),
            "versions 1 to 3 of the guest protocol instead of versions 4 to 5"
        );
    }

    /// Every version of the other side decodes this.
    #[test]
    fn hello_wire_format() {
        let hello = Hello {
            protocol: versions(1, 2),
        };
        assert_eq!(
            rmp_serde::to_vec_named(&hello).unwrap(),
            b"\x81\xa8protocol\x82\xa6oldest\x01\xa7current\x02"
        );
    }
}
