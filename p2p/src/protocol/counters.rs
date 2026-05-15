//! Cumulative traffic counters at the framing boundary.
//!
//! Operator-facing instrumentation. Increments are lock-free atomics on
//! the hot path (every frame in and every frame out); reads are
//! `Ordering::Relaxed` and tolerate sample skew. The contract that
//! consumes the snapshot is `facts/stats.md`.
//!
//! # Boundary
//! Inbound: counted once the frame has been parsed into a
//! [`ProtocolMessage`] (transport→protocol boundary). Outbound: counted
//! once the typed message has been serialized to a [`Frame`] and is
//! about to leave the protocol→transport boundary. The byte total
//! includes the framing header (9 bytes for empty bodies, 13 bytes
//! otherwise).
//!
//! # Snapshot shape
//! [`TrafficSnapshot`] uses `u8` keys for modifier-type and snapshot
//! code buckets. The name-string mapping lives in the api adapter so
//! this crate does not depend on api.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::protocol::messages::{MessageCode, ProtocolMessage};
use crate::transport::frame::Frame;

/// Inclusive range of message codes treated as snapshot-sync messages
/// (request_manifest=76 .. utxo_chunk=81). Anything outside this range
/// that wasn't parsed into a typed variant falls into [`unknown`].
const SNAPSHOT_CODE_MIN: u8 = 76;
const SNAPSHOT_CODE_MAX: u8 = 81;

/// Wire bytes consumed by a frame, including the framing header.
///
/// Empty bodies omit the 4-byte checksum (matching the JVM wire
/// format); otherwise the header is 13 bytes (magic 4 + code 1 +
/// length 4 + checksum 4) plus the body.
pub fn frame_wire_bytes(frame: &Frame) -> u64 {
    if frame.body.is_empty() {
        9
    } else {
        13 + frame.body.len() as u64
    }
}

/// Plain-data snapshot of one (message-code, direction) cell.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectionalCounter {
    pub in_count: u64,
    pub in_bytes: u64,
    pub out_count: u64,
    pub out_bytes: u64,
}

/// Plain-data snapshot returned by [`TrafficCounters::snapshot`].
///
/// Maps are sparse — only non-zero buckets appear. The
/// `since_unix_seconds` field carries the process-start timestamp so
/// RRD consumers can detect counter resets.
#[derive(Debug, Clone, Default)]
pub struct TrafficSnapshot {
    pub since_unix_seconds: u64,
    pub handshake: DirectionalCounter,
    pub get_peers: DirectionalCounter,
    pub peers: DirectionalCounter,
    pub sync_info: DirectionalCounter,
    pub inv_by_modifier: BTreeMap<u8, DirectionalCounter>,
    pub modifier_request_by_modifier: BTreeMap<u8, DirectionalCounter>,
    pub modifier_response_by_modifier: BTreeMap<u8, DirectionalCounter>,
    pub snapshot_by_code: BTreeMap<u8, DirectionalCounter>,
    pub unknown: DirectionalCounter,
}

#[derive(Debug, Default)]
struct DirectionalAtomic {
    in_count: AtomicU64,
    in_bytes: AtomicU64,
    out_count: AtomicU64,
    out_bytes: AtomicU64,
}

impl DirectionalAtomic {
    fn record_in(&self, bytes: u64) {
        self.in_count.fetch_add(1, Ordering::Relaxed);
        self.in_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn record_out(&self, bytes: u64) {
        self.out_count.fetch_add(1, Ordering::Relaxed);
        self.out_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self) -> DirectionalCounter {
        DirectionalCounter {
            in_count: self.in_count.load(Ordering::Relaxed),
            in_bytes: self.in_bytes.load(Ordering::Relaxed),
            out_count: self.out_count.load(Ordering::Relaxed),
            out_bytes: self.out_bytes.load(Ordering::Relaxed),
        }
    }

    fn is_empty(&self) -> bool {
        self.snapshot() == DirectionalCounter::default()
    }
}

/// 256-bucket array indexed by `u8` (modifier-type byte or snapshot
/// code). Snapshot emits only non-zero buckets.
struct PerByte {
    buckets: Box<[DirectionalAtomic]>,
}

impl PerByte {
    fn new() -> Self {
        let mut buckets = Vec::with_capacity(256);
        for _ in 0..256 {
            buckets.push(DirectionalAtomic::default());
        }
        Self {
            buckets: buckets.into_boxed_slice(),
        }
    }

    fn record_in(&self, key: u8, bytes: u64) {
        self.buckets[key as usize].record_in(bytes);
    }

    fn record_out(&self, key: u8, bytes: u64) {
        self.buckets[key as usize].record_out(bytes);
    }

    fn snapshot(&self) -> BTreeMap<u8, DirectionalCounter> {
        let mut map = BTreeMap::new();
        for (i, c) in self.buckets.iter().enumerate() {
            if !c.is_empty() {
                map.insert(i as u8, c.snapshot());
            }
        }
        map
    }
}

/// Atomic counter store, shared across peer tasks via `Arc`.
pub struct TrafficCounters {
    since_unix_seconds: u64,
    handshake: DirectionalAtomic,
    get_peers: DirectionalAtomic,
    peers: DirectionalAtomic,
    sync_info: DirectionalAtomic,
    inv_by_modifier: PerByte,
    modifier_request_by_modifier: PerByte,
    modifier_response_by_modifier: PerByte,
    snapshot_by_code: PerByte,
    unknown: DirectionalAtomic,
}

impl Default for TrafficCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl TrafficCounters {
    pub fn new() -> Self {
        let since = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            since_unix_seconds: since,
            handshake: DirectionalAtomic::default(),
            get_peers: DirectionalAtomic::default(),
            peers: DirectionalAtomic::default(),
            sync_info: DirectionalAtomic::default(),
            inv_by_modifier: PerByte::new(),
            modifier_request_by_modifier: PerByte::new(),
            modifier_response_by_modifier: PerByte::new(),
            snapshot_by_code: PerByte::new(),
            unknown: DirectionalAtomic::default(),
        }
    }

    pub fn record_handshake_in(&self, bytes: u64) {
        self.handshake.record_in(bytes);
    }

    pub fn record_handshake_out(&self, bytes: u64) {
        self.handshake.record_out(bytes);
    }

    /// Record an inbound message after the transport→protocol parse.
    /// `wire_bytes` is the full on-wire size (use [`frame_wire_bytes`]).
    pub fn record_in_message(&self, message: &ProtocolMessage, wire_bytes: u64) {
        match message {
            ProtocolMessage::GetPeers => self.get_peers.record_in(wire_bytes),
            ProtocolMessage::Peers { .. } => self.peers.record_in(wire_bytes),
            ProtocolMessage::SyncInfo { .. } => self.sync_info.record_in(wire_bytes),
            ProtocolMessage::Inv { modifier_type, .. } => {
                self.inv_by_modifier.record_in(*modifier_type, wire_bytes)
            }
            ProtocolMessage::ModifierRequest { modifier_type, .. } => {
                self.modifier_request_by_modifier
                    .record_in(*modifier_type, wire_bytes)
            }
            ProtocolMessage::ModifierResponse { modifier_type, .. } => {
                self.modifier_response_by_modifier
                    .record_in(*modifier_type, wire_bytes)
            }
            ProtocolMessage::Unknown { code, .. } => {
                if (SNAPSHOT_CODE_MIN..=SNAPSHOT_CODE_MAX).contains(code) {
                    self.snapshot_by_code.record_in(*code, wire_bytes)
                } else {
                    self.unknown.record_in(wire_bytes)
                }
            }
        }
    }

    /// Record an outbound frame at the protocol→transport boundary.
    ///
    /// The writer task only sees `Frame`, not the typed message; the
    /// first byte of the body is the modifier-type discriminant for
    /// `Inv`/`ModifierRequest`/`ModifierResponse` — equivalent to the
    /// typed variant's field, but read directly from the encoded body
    /// to avoid re-parsing.
    pub fn record_out_frame(&self, frame: &Frame, wire_bytes: u64) {
        match frame.code {
            MessageCode::GET_PEERS => self.get_peers.record_out(wire_bytes),
            MessageCode::PEERS => self.peers.record_out(wire_bytes),
            MessageCode::SYNC_INFO => self.sync_info.record_out(wire_bytes),
            MessageCode::INV => {
                let mtype = frame.body.first().copied().unwrap_or(0);
                self.inv_by_modifier.record_out(mtype, wire_bytes);
            }
            MessageCode::MODIFIER_REQUEST => {
                let mtype = frame.body.first().copied().unwrap_or(0);
                self.modifier_request_by_modifier
                    .record_out(mtype, wire_bytes);
            }
            MessageCode::MODIFIER_RESPONSE => {
                let mtype = frame.body.first().copied().unwrap_or(0);
                self.modifier_response_by_modifier
                    .record_out(mtype, wire_bytes);
            }
            code if (SNAPSHOT_CODE_MIN..=SNAPSHOT_CODE_MAX).contains(&code) => {
                self.snapshot_by_code.record_out(code, wire_bytes);
            }
            _ => self.unknown.record_out(wire_bytes),
        }
    }

    pub fn snapshot(&self) -> TrafficSnapshot {
        TrafficSnapshot {
            since_unix_seconds: self.since_unix_seconds,
            handshake: self.handshake.snapshot(),
            get_peers: self.get_peers.snapshot(),
            peers: self.peers.snapshot(),
            sync_info: self.sync_info.snapshot(),
            inv_by_modifier: self.inv_by_modifier.snapshot(),
            modifier_request_by_modifier: self.modifier_request_by_modifier.snapshot(),
            modifier_response_by_modifier: self.modifier_response_by_modifier.snapshot(),
            snapshot_by_code: self.snapshot_by_code.snapshot(),
            unknown: self.unknown.snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ModifierId;

    fn empty_frame(code: u8) -> Frame {
        Frame { code, body: vec![] }
    }

    fn frame_with_body(code: u8, body: Vec<u8>) -> Frame {
        Frame { code, body }
    }

    fn dummy_id(i: u8) -> ModifierId {
        let mut id = [0u8; 32];
        id[0] = i;
        id
    }

    #[test]
    fn frame_wire_bytes_empty_body() {
        let f = empty_frame(MessageCode::GET_PEERS);
        assert_eq!(frame_wire_bytes(&f), 9);
    }

    #[test]
    fn frame_wire_bytes_non_empty_body() {
        let f = frame_with_body(MessageCode::PEERS, vec![0u8; 17]);
        assert_eq!(frame_wire_bytes(&f), 13 + 17);
    }

    #[test]
    fn record_handshake_increments_both_directions() {
        let c = TrafficCounters::new();
        c.record_handshake_in(120);
        c.record_handshake_in(80);
        c.record_handshake_out(150);

        let s = c.snapshot();
        assert_eq!(s.handshake.in_count, 2);
        assert_eq!(s.handshake.in_bytes, 200);
        assert_eq!(s.handshake.out_count, 1);
        assert_eq!(s.handshake.out_bytes, 150);
    }

    #[test]
    fn record_in_message_routes_by_variant() {
        let c = TrafficCounters::new();

        c.record_in_message(&ProtocolMessage::GetPeers, 9);
        c.record_in_message(&ProtocolMessage::Peers { body: vec![0u8; 10] }, 23);
        c.record_in_message(
            &ProtocolMessage::SyncInfo { body: vec![0u8; 32] },
            45,
        );
        c.record_in_message(
            &ProtocolMessage::Inv {
                modifier_type: 1,
                ids: vec![dummy_id(0)],
            },
            13 + 1 + 1 + 32,
        );
        c.record_in_message(
            &ProtocolMessage::ModifierRequest {
                modifier_type: 2,
                ids: vec![dummy_id(0); 2],
            },
            13 + 1 + 1 + 64,
        );
        c.record_in_message(
            &ProtocolMessage::ModifierResponse {
                modifier_type: 3,
                modifiers: vec![(dummy_id(0), vec![0u8; 4])],
            },
            13 + 1 + 1 + 32 + 1 + 4,
        );
        c.record_in_message(
            &ProtocolMessage::Unknown {
                code: 76,
                body: vec![0u8; 1],
            },
            14,
        );
        c.record_in_message(
            &ProtocolMessage::Unknown {
                code: 200,
                body: vec![0u8; 1],
            },
            14,
        );

        let s = c.snapshot();
        assert_eq!(s.get_peers.in_count, 1);
        assert_eq!(s.get_peers.in_bytes, 9);

        assert_eq!(s.peers.in_count, 1);
        assert_eq!(s.peers.in_bytes, 23);

        assert_eq!(s.sync_info.in_count, 1);
        assert_eq!(s.sync_info.in_bytes, 45);

        let inv_header = s.inv_by_modifier.get(&1).copied().unwrap_or_default();
        assert_eq!(inv_header.in_count, 1);
        assert_eq!(inv_header.in_bytes, 13 + 1 + 1 + 32);

        let req_tx = s.modifier_request_by_modifier.get(&2).copied().unwrap_or_default();
        assert_eq!(req_tx.in_count, 1);
        assert_eq!(req_tx.in_bytes, 13 + 1 + 1 + 64);

        let resp_blktx = s
            .modifier_response_by_modifier
            .get(&3)
            .copied()
            .unwrap_or_default();
        assert_eq!(resp_blktx.in_count, 1);
        assert_eq!(resp_blktx.in_bytes, 13 + 1 + 1 + 32 + 1 + 4);

        let snap76 = s.snapshot_by_code.get(&76).copied().unwrap_or_default();
        assert_eq!(snap76.in_count, 1);
        assert_eq!(snap76.in_bytes, 14);

        assert_eq!(s.unknown.in_count, 1);
        assert_eq!(s.unknown.in_bytes, 14);

        // Snapshot maps stay sparse — buckets we never touched are absent.
        assert!(!s.inv_by_modifier.contains_key(&2));
        assert!(!s.snapshot_by_code.contains_key(&200));
    }

    #[test]
    fn record_out_frame_routes_by_code() {
        let c = TrafficCounters::new();

        c.record_out_frame(&empty_frame(MessageCode::GET_PEERS), 9);
        c.record_out_frame(
            &frame_with_body(MessageCode::PEERS, vec![0u8; 10]),
            23,
        );
        c.record_out_frame(
            &frame_with_body(MessageCode::SYNC_INFO, vec![0u8; 32]),
            45,
        );

        // INV body starts with modifier_type byte.
        let mut inv_body = vec![1u8]; // modifier_type = 1 (Header)
        inv_body.extend_from_slice(&[0u8; 33]); // VLQ count + 1 ID
        let inv_wire = frame_wire_bytes(&frame_with_body(MessageCode::INV, inv_body.clone()));
        c.record_out_frame(&frame_with_body(MessageCode::INV, inv_body), inv_wire);

        let mut req_body = vec![2u8];
        req_body.extend_from_slice(&[0u8; 33]);
        let req_wire = frame_wire_bytes(&frame_with_body(MessageCode::MODIFIER_REQUEST, req_body.clone()));
        c.record_out_frame(
            &frame_with_body(MessageCode::MODIFIER_REQUEST, req_body),
            req_wire,
        );

        let mut resp_body = vec![3u8];
        resp_body.extend_from_slice(&[0u8; 33]);
        resp_body.push(0); // data_len VLQ
        let resp_wire = frame_wire_bytes(&frame_with_body(MessageCode::MODIFIER_RESPONSE, resp_body.clone()));
        c.record_out_frame(
            &frame_with_body(MessageCode::MODIFIER_RESPONSE, resp_body),
            resp_wire,
        );

        // Snapshot code 76 (request_manifest).
        c.record_out_frame(&frame_with_body(76, vec![0u8; 1]), 14);

        // Unknown code outside snapshot range.
        c.record_out_frame(&frame_with_body(99, vec![0u8; 1]), 14);

        let s = c.snapshot();
        assert_eq!(s.get_peers.out_count, 1);
        assert_eq!(s.get_peers.out_bytes, 9);
        assert_eq!(s.peers.out_count, 1);
        assert_eq!(s.peers.out_bytes, 23);
        assert_eq!(s.sync_info.out_count, 1);
        assert_eq!(s.sync_info.out_bytes, 45);

        let inv_header = s.inv_by_modifier.get(&1).copied().unwrap_or_default();
        assert_eq!(inv_header.out_count, 1);
        assert_eq!(inv_header.out_bytes, inv_wire);

        let req_tx = s.modifier_request_by_modifier.get(&2).copied().unwrap_or_default();
        assert_eq!(req_tx.out_count, 1);
        assert_eq!(req_tx.out_bytes, req_wire);

        let resp = s
            .modifier_response_by_modifier
            .get(&3)
            .copied()
            .unwrap_or_default();
        assert_eq!(resp.out_count, 1);
        assert_eq!(resp.out_bytes, resp_wire);

        let snap76 = s.snapshot_by_code.get(&76).copied().unwrap_or_default();
        assert_eq!(snap76.out_count, 1);
        assert_eq!(snap76.out_bytes, 14);

        assert_eq!(s.unknown.out_count, 1);
        assert_eq!(s.unknown.out_bytes, 14);
    }

    #[test]
    fn snapshot_since_is_populated() {
        let c = TrafficCounters::new();
        let s = c.snapshot();
        assert!(s.since_unix_seconds > 0);
    }

    #[test]
    fn snapshot_is_concurrent_safe() {
        use std::sync::Arc;
        use std::thread;

        let c = Arc::new(TrafficCounters::new());
        let mut handles = vec![];
        for _ in 0..4 {
            let c = c.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    c.record_in_message(&ProtocolMessage::GetPeers, 9);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = c.snapshot();
        assert_eq!(s.get_peers.in_count, 4 * 1000);
        assert_eq!(s.get_peers.in_bytes, 4 * 1000 * 9);
    }
}
