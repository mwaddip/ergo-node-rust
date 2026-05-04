//! Peer lifecycle state machine.
//!
//! # Contract
//! - State transitions follow: Connecting → Handshaking → Active → Disconnected.
//!   Handshaking can also transition to Failed.
//! - `set_active` produces a `PeerConnected` event.
//! - `set_disconnected` produces a `PeerDisconnected` event.
//! - Invariant: a peer in any state other than `Active` cannot produce `Message` events.
//! - Invariant: state transitions are one-way; no state can transition backwards.

use crate::protocol::messages::ProtocolMessage;
use crate::transport::handshake::PeerSpec;
use crate::types::{Direction, PeerId};
use std::net::SocketAddr;

/// Peer connection states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    Handshaking,
    Active,
    Disconnected,
    Failed,
}

/// Events produced by the protocol layer.
#[derive(Debug, Clone)]
pub enum ProtocolEvent {
    PeerConnected {
        peer_id: PeerId,
        spec: PeerSpec,
        direction: Direction,
        addr: SocketAddr,
    },
    PeerDisconnected {
        peer_id: PeerId,
        reason: String,
    },
    Message {
        peer_id: PeerId,
        message: ProtocolMessage,
    },
}

/// State machine for a single peer's lifecycle.
pub struct PeerStateMachine {
    peer_id: PeerId,
    direction: Direction,
    state: PeerState,
    spec: Option<PeerSpec>,
}

impl PeerStateMachine {
    pub fn new(peer_id: PeerId, direction: Direction) -> Self {
        Self {
            peer_id,
            direction,
            state: PeerState::Connecting,
            spec: None,
        }
    }

    pub fn state(&self) -> PeerState {
        self.state
    }

    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn spec(&self) -> Option<&PeerSpec> {
        self.spec.as_ref()
    }

    /// Transition to Handshaking.
    ///
    /// # Contract
    /// - Precondition: state is Connecting.
    /// - Postcondition: state is Handshaking.
    pub fn set_handshaking(&mut self) {
        debug_assert_eq!(self.state, PeerState::Connecting);
        self.state = PeerState::Handshaking;
    }

    /// Transition to Failed.
    ///
    /// # Contract
    /// - Precondition: state is Handshaking.
    /// - Postcondition: state is Failed.
    pub fn set_failed(&mut self, _reason: String) {
        debug_assert_eq!(self.state, PeerState::Handshaking);
        self.state = PeerState::Failed;
    }

    /// Transition to Active. Returns PeerConnected event.
    ///
    /// # Contract
    /// - Precondition: state is Handshaking.
    /// - Postcondition: state is Active, PeerConnected event is returned.
    pub fn set_active(&mut self, spec: PeerSpec, addr: SocketAddr) -> ProtocolEvent {
        debug_assert_eq!(self.state, PeerState::Handshaking);
        self.spec = Some(spec.clone());
        self.state = PeerState::Active;
        ProtocolEvent::PeerConnected {
            peer_id: self.peer_id,
            spec,
            direction: self.direction,
            addr,
        }
    }

    /// Transition to Disconnected. Returns PeerDisconnected event.
    ///
    /// # Contract
    /// - Precondition: state is Active.
    /// - Postcondition: state is Disconnected, PeerDisconnected event is returned.
    pub fn set_disconnected(&mut self, reason: String) -> ProtocolEvent {
        debug_assert_eq!(self.state, PeerState::Active);
        self.state = PeerState::Disconnected;
        ProtocolEvent::PeerDisconnected {
            peer_id: self.peer_id,
            reason,
        }
    }

    /// Wrap a parsed message into a ProtocolEvent.
    ///
    /// # Contract
    /// - Precondition: state is Active.
    pub fn message_event(&self, message: ProtocolMessage) -> ProtocolEvent {
        debug_assert_eq!(self.state, PeerState::Active);
        ProtocolEvent::Message {
            peer_id: self.peer_id,
            message,
        }
    }
}
