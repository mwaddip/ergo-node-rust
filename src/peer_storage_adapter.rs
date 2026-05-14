//! Glue between `enr_p2p::peer_db::PeerStorage` and `enr_store::ModifierStore`.
//!
//! The p2p crate owns the in-memory `PeerDb` and the `PeerRecord`
//! schema; the store crate stores opaque bytes keyed by `SocketAddr`.
//! This adapter is the encode/decode bridge.
//!
//! The on-disk record format is internal — it does not match any
//! wire format and is free to change. Bumping it just costs a one-
//! time `list_peers()` purge on startup (corrupt rows are skipped
//! with a warn log).

use enr_p2p::peer_db::{PeerRecord, PeerStorage, PeerStorageError};
use enr_store::{ModifierStore, RedbModifierStore};
use std::net::SocketAddr;
use std::sync::Arc;

pub struct PeerStorageAdapter {
    store: Arc<RedbModifierStore>,
}

impl PeerStorageAdapter {
    pub fn new(store: Arc<RedbModifierStore>) -> Self {
        Self { store }
    }
}

impl PeerStorage for PeerStorageAdapter {
    fn load_all(&self) -> Result<Vec<PeerRecord>, PeerStorageError> {
        let rows = self.store.list_peers().map_err(box_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for (addr, bytes) in rows {
            match decode_record(addr, &bytes) {
                Ok(rec) => out.push(rec),
                Err(e) => tracing::warn!(%addr, error = %e, "skipping undecodable peer record"),
            }
        }
        Ok(out)
    }

    fn put(&self, record: &PeerRecord) -> Result<(), PeerStorageError> {
        let bytes = encode_record(record);
        self.store.put_peer(record.address, &bytes).map_err(box_err)
    }

    fn delete(&self, addr: SocketAddr) -> Result<(), PeerStorageError> {
        self.store.delete_peer(addr).map_err(box_err)
    }
}

fn box_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> PeerStorageError {
    Box::new(e)
}

fn encode_record(r: &PeerRecord) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&r.last_seen_ms.to_le_bytes());
    encode_str(&mut buf, &r.agent_name);
    encode_str(&mut buf, &r.node_name);
    buf.push(r.version.0);
    buf.push(r.version.1);
    buf.push(r.version.2);
    buf.push(r.features.len().min(u8::MAX as usize) as u8);
    for (id, body) in r.features.iter().take(u8::MAX as usize) {
        buf.push(*id);
        encode_bytes(&mut buf, body);
    }
    buf
}

fn encode_str(buf: &mut Vec<u8>, s: &str) {
    encode_bytes(buf, s.as_bytes());
}

fn encode_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len().min(u16::MAX as usize) as u16;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&bytes[..len as usize]);
}

fn decode_record(address: SocketAddr, bytes: &[u8]) -> Result<PeerRecord, String> {
    let mut cur = Cursor::new(bytes);
    let last_seen_ms = cur.read_u64()?;
    let agent_name = cur.read_str()?;
    let node_name = cur.read_str()?;
    let v0 = cur.read_u8()?;
    let v1 = cur.read_u8()?;
    let v2 = cur.read_u8()?;
    let feat_count = cur.read_u8()? as usize;
    let mut features = Vec::with_capacity(feat_count);
    for _ in 0..feat_count {
        let id = cur.read_u8()?;
        let body = cur.read_bytes()?;
        features.push((id, body));
    }
    Ok(PeerRecord {
        address,
        last_seen_ms,
        agent_name,
        node_name,
        version: (v0, v1, v2),
        features,
    })
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self { Self { buf, pos: 0 } }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err(format!("short read: want {} have {}", n, self.buf.len() - self.pos));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, String> {
        let s = self.take(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }

    fn read_bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = self.read_u16()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn read_str(&mut self) -> Result<String, String> {
        let b = self.read_bytes()?;
        String::from_utf8(b).map_err(|e| format!("invalid utf8: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> PeerRecord {
        PeerRecord {
            address: "1.2.3.4:9030".parse().unwrap(),
            last_seen_ms: 1_700_000_000_000,
            agent_name: "ergoref".to_string(),
            node_name: "test-node".to_string(),
            version: (5, 0, 25),
            features: vec![
                (16, vec![0xaa, 0xbb]),
                (3, vec![0x01, 0x00, 0x02, 0x04]),
            ],
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let r = rec();
        let bytes = encode_record(&r);
        let back = decode_record(r.address, &bytes).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn decode_truncated_errors() {
        let bytes = encode_record(&rec());
        let r = decode_record("1.2.3.4:9030".parse().unwrap(), &bytes[..10]);
        assert!(r.is_err());
    }

    #[test]
    fn empty_features_roundtrip() {
        let mut r = rec();
        r.features.clear();
        let bytes = encode_record(&r);
        let back = decode_record(r.address, &bytes).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn empty_strings_roundtrip() {
        let mut r = rec();
        r.agent_name.clear();
        r.node_name.clear();
        let bytes = encode_record(&r);
        let back = decode_record(r.address, &bytes).unwrap();
        assert_eq!(back, r);
    }
}
