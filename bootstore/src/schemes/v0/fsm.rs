// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V0 protocol state machine
//!
//! This state machine is entirely synchronous. It performs actions and returns
//! results. This is where the bulk of the protocol logic lives. It's
//! written this way to enable easy testing and auditing.

use super::messages::{Envelope, Error, Msg, Request, Response};
use crate::trust_quorum::{LearnedSharePkgV0, SharePkgV0};
use serde::{Deserialize, Serialize};
use sled_hardware::Baseboard;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

/// A number of clock ticks from some unknown epoch
type Ticks = usize;

// An index into an encrypted share
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareIdx(usize);

/// Output from a a call to [`Fsm::Handle`]
pub struct Output<'a> {
    /// Possible state that needs persisting before any messages are sent
    pub persist: Option<&'a PersistentState>,

    /// Messages wrapped with a destination
    pub envelopes: Vec<Envelope>,
}

/// State stored on the M.2s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentState {
    // The generation number for ledger writing purposes on both M.2s
    pub version: u32,
    pub state: State,
}

/// The possible states of a peer FSM
///
/// The following are valid state transitions
///
///   * Uninitialized -> InitialMember
///   * Uninitialized -> Learning -> Learned
///
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum State {
    /// The peer does not yet know whether it is an initial member of the group
    /// or a learner.
    Uninitialized,

    /// The peer is an initial member of the group setup via RSS
    InitialMember {
        pkg: SharePkgV0,

        /// Shares given to other sleds. We mark them as used so that we don't
        /// hand them out twice. If the same sled asks us for a share, because
        /// it crashes or there is a network blip, we will return the same
        /// share each time.
        ///
        /// Note that this is a fairly optimistic strategy as the requesting
        /// sled can always go ask another sled after a network blip. However,
        /// this guarantees that a single sled never hands out more than one of
        /// its shares to any given sled.
        ///
        /// We can't do much better than this without some sort of centralized
        /// distributor which is part of the reconfiguration mechanism in later
        /// versions of the trust quourum protocol.
        distributed_shares: BTreeMap<Baseboard, ShareIdx>,
    },

    /// The peer has been instructed to learn a share
    ///
    /// There is no need to record what are online attempts. That could
    /// cause too many write cycles to the M.2s.
    Learning(#[serde(skip)] Option<LearnAttempt>),

    /// The peer has learned its share
    Learned { pkg: LearnedSharePkgV0 },
}

/// An attempt to learn a key share
///
/// When an attempt is started, a peer is selected from those known to the FSM,
/// and a `Request::Learn` message is sent to that peer. This peer is recorded
/// along with the current clock as `start`. If `Config.learn_timeout` ticks
/// have fired since `start` based on the current FSM clock, without this
/// FSM having received a `Response::Pkg`, then the `LearnAttempt` will be
/// cancelled, and the next peer in order known to the FSM will be contacted in
/// a new attempt.
#[derive(Debug, Clone)]
pub struct LearnAttempt {
    peer: Baseboard,
    start: Ticks,
}

/// The state machine for a [`$crate::Peer`]
///
/// This FSM assumes a network layer above it that can map peer IDs
/// ( [`Baseboard`]s) to TCP sockets. When an attempt is made to send a
/// message to a given peer the network layer will send it over an established
/// connection if one exists. If there is no connection already to that peer,
/// but the prefix is present and known, the network layer will attempt to
/// establish a connection and then transmit the message. If the peer is not
/// known then it must have just had its network prefix removed. In this case
/// the message will be dropped and the FSM will be told to remove the peer.
pub struct Fsm {
    // Unique IDs of this peer
    id: Baseboard,

    // Configuration of the FSM
    config: Config,

    // The state of the FSM and the version of it written to the M.2 SSDs
    state: PersistentState,

    // Unique IDs of known peers
    peers: BTreeSet<Baseboard>,

    // The current time in ticks since the creation of the FSM
    clock: Ticks,
}

/// Configuration of the FSM
pub struct Config {
    pub retry_timeout: Ticks,
    pub learn_timeout: Ticks,
}

impl Fsm {
    pub fn new(id: Baseboard, config: Config, state: PersistentState) -> Fsm {
        Fsm { id, config, state, peers: BTreeSet::new(), clock: 0 }
    }

    /// An abstraction of a timer tick.
    ///
    /// Ticks mutate state and can result in message retries.
    ///
    /// Each tick represents some abstract duration of time.
    /// Timeouts are represented by number of ticks in this module, which
    /// allows for deterministic property based tests in a straightforward manner.
    /// On each tick, the current duration since start is passed in. We only
    /// deal in relative time needed for timeouts, and not absolute time. This
    /// strategy allows for deterministic property based tests.
    pub fn tick(&mut self) -> Vec<Envelope> {
        self.clock += 1;

        // TODO: Check to see if messages need to be transmitted,
        // learn attempts need to be made, etc...
        unimplemented!()
    }

    /// A connection has been established an a peer has been learned.
    /// This peer may or may not already be known by the FSM.
    pub fn insert_peer(&mut self, peer: Baseboard) {
        self.peers.insert(peer);
    }

    /// When a the upper layer sees the advertised prefix for a peer go away,
    /// and not just a dropped connection, it will inform the FSM to remove the peer.
    ///
    /// This is a useful mechanism to prevent generating requests for failed sleds.
    pub fn remove_peer(&mut self, peer: Baseboard) {
        self.peers.remove(&peer);
    }

    /// Handle a message from a peer.
    ///
    /// Return whether persistent state needs syncing to disk and a set of
    /// messages to send to other peers. Persistant state must be saved by
    /// the caller and safely persisted before messages are sent, or the next
    /// message is handled here.
    pub fn handle(&mut self, from: Baseboard, msg: Msg) -> Output<'_> {
        match msg {
            Msg::Req(req) => self.handle_request(from, req),
            Msg::Rsp(rsp) => self.handle_response(from, rsp),
        }
    }

    // Handle a `Request` message
    fn handle_request(
        &mut self,
        from: Baseboard,
        request: Request,
    ) -> Output<'_> {
        match request {
            Request::Init(new_pkg) => self.on_init(from, new_pkg),
            Request::InitLearner => self.on_init_learner(from),
            _ => unimplemented!(),
        }
    }

    // Handle a `Request::Init` message
    fn on_init(&mut self, from: Baseboard, new_pkg: SharePkgV0) -> Output<'_> {
        let version = &mut self.state.version;
        match &mut self.state.state {
            state @ State::Uninitialized => {
                *state = State::InitialMember {
                    pkg: new_pkg,
                    distributed_shares: BTreeMap::new(),
                };
                *version += 1;
                self.persist_and_respond(from, Response::InitAck)
            }
            State::InitialMember { pkg, .. } => {
                if new_pkg == *pkg {
                    // Idempotent response given same pkg
                    self.respond(from, Response::InitAck)
                } else {
                    let rack_uuid = pkg.rack_uuid;
                    self.respond(
                        from,
                        Error::AlreadyInitialized { rack_uuid }.into(),
                    )
                }
            }
            State::Learning(_) => {
                self.respond(from, Error::AlreadyLearning.into())
            }
            State::Learned { pkg } => {
                let rack_uuid = pkg.rack_uuid;
                self.respond(from, Error::AlreadyLearned { rack_uuid }.into())
            }
        }
    }

    // Handle a `Request::InitLearner` message
    fn on_init_learner(&mut self, from: Baseboard) -> Output<'_> {
        let version = &mut self.state.version;
        match &mut self.state.state {
            state @ State::Uninitialized => {
                *state = State::Learning(None);
                *version += 1;
                self.persist_and_respond(from, Response::InitAck)
            }
            State::InitialMember { pkg, .. } => {
                let rack_uuid = pkg.rack_uuid;
                self.respond(
                    from,
                    Error::AlreadyInitialized { rack_uuid }.into(),
                )
            }
            State::Learning(_) | State::Learned { .. } => {
                // Idempotent
                self.persist_and_respond(from, Response::InitAck)
            }
        }
    }

    fn handle_response(
        &mut self,
        from: Baseboard,
        response: Response,
    ) -> Output<'_> {
        unimplemented!()
    }

    // Return a response directly to a peer that doesn't require persistence
    fn respond(&self, to: Baseboard, response: Response) -> Output<'_> {
        Output {
            persist: None,
            envelopes: vec![Envelope { to, msg: response.into() }],
        }
    }

    // Indicate to the caller that state must be perisisted and then a response
    // returned to the peer.
    fn persist_and_respond(
        &self,
        to: Baseboard,
        response: Response,
    ) -> Output<'_> {
        Output {
            persist: Some(&self.state),
            envelopes: vec![Envelope { to, msg: response.into() }],
        }
    }
}
