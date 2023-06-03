// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Versioned bootstore protocol schemes
//!
//! Contains code shared by all schemes

use serde::{Deserialize, Serialize};

mod v0;

// We keep these in a module to prevent naming conflicts
#[allow(unused)]
pub mod params {
    #[derive(Default, Debug, Clone, Copy)]
    pub struct ChaCha20Poly1305;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct Sha3_256;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct Hkdf;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct Tcp;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct No;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct Curve25519;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct Bcs;

    #[derive(Default, Debug, Clone, Copy)]
    pub struct U32BigEndian;
}

/// A common message supported across all schemes.
///
/// The Hello message must remain backwards compatible for all time, and so it
/// is serialized as two big-endian 4-byte integers, followed by 1 8 byte big-
/// endian reserved integer. This allows serialization format to change across
/// schemes but not break version negotiation. The use of big-endian integers
/// allows easier reading of wire dumps.
///
/// At the initiation of each session, each side sends a `Hello` message
/// indicating the current scheme and version they are operating at.
///
/// Each side must determine interoperability based on the the scheme and
/// versions exchanged.
///
/// The rules are as simple as possible:
///
///  1. Each side must have a matching scheme. The connecting side will first
///  send a `Hello` message and the receiving side will send one in response.
///  If the peers do not have matching schemes, connecting side will close
///  the connection.
///  2. If schemes match, then only messages supported by the lower of the two
/// versions will be exchanged.
///
/// This works because in practice because we expect upgrade to work in one of
/// two ways:
///
///  1. Each scheme runs on a separate port, and old schemes are disabled once all
///  current members of the quorum have been updated to the latest version of software.
///  2. All scheme versions use the same port, but switch to using the newest scheme
///  when all members of the quorum have the latest software installed.
///
/// In both cases, an `Upgrade` signal is sent by the control plane to trigger
/// the switch to a new scheme. This signal is transitive, such that if one
/// node receives it and switches to the new schem, and then the rack reboots,
/// any node running a version of the software that supports the new scheme
/// will switch over to running the new scheme when it sees a `Hello` message
/// with the latest scheme. Note that we may also choose to use a more robust
/// strategy than this with quorum based switchover, but the result is the
/// same.
///
/// If a sled has been down or outside a rack when the switchover happens and
/// does not have the software to participate in the latest scheme, it must be upgraded.
/// It may or may not be a member of the current trust quorum membership at this point.
///
/// It's important to note that prior to the use of sprockets, such as in
/// scheme v0, upgrade will have to be untrusted since `Hello` messages are
/// unauthenticated.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// The version of the bootstore protocol itself
    pub scheme: u32,
    /// The version of the protocol for given bootstore scheme
    ///
    /// Each scheme has independently numbered protocol versions, such
    /// that new messages can be added.
    pub version: u32,

    /// Reserved for future usage. May be interpreted differently depending
    /// upon current scheme in use.
    pub reserved: u64,
}
