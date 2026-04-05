use ergo_sync::snapshot::protocol::{
    ProtocolError, SnapshotEntry, SnapshotMessage, GET_MANIFEST, GET_SNAPSHOTS_INFO,
    GET_UTXO_SNAPSHOT_CHUNK, MANIFEST, MAX_CHUNK_SIZE, MAX_MANIFEST_SIZE, MAX_SNAPSHOTS_INFO_SIZE,
    SNAPSHOTS_INFO, UTXO_SNAPSHOT_CHUNK,
};

/// Parse a SnapshotsInfo message containing 2 entries.
#[test]
fn parse_snapshots_info_two_entries() {
    let entry_a = SnapshotEntry {
        height: 800_000,
        manifest_id: [0xAAu8; 32],
    };
    let entry_b = SnapshotEntry {
        height: 900_000,
        manifest_id: [0xBBu8; 32],
    };

    // Encode then parse round-trip
    let msg = SnapshotMessage::SnapshotsInfo(vec![entry_a.clone(), entry_b.clone()]);
    let (code, body) = msg.encode();
    assert_eq!(code, SNAPSHOTS_INFO);

    let parsed = SnapshotMessage::parse(code, &body).unwrap();
    match parsed {
        SnapshotMessage::SnapshotsInfo(entries) => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0], entry_a);
            assert_eq!(entries[1], entry_b);
        }
        other => panic!("expected SnapshotsInfo, got {:?}", other),
    }
}

/// Build and parse GetSnapshotsInfo (empty body).
#[test]
fn build_get_snapshots_info() {
    let msg = SnapshotMessage::GetSnapshotsInfo;
    let (code, body) = msg.encode();
    assert_eq!(code, GET_SNAPSHOTS_INFO);
    assert!(body.is_empty());

    let parsed = SnapshotMessage::parse(code, &body).unwrap();
    assert_eq!(parsed, SnapshotMessage::GetSnapshotsInfo);
}

/// Build and parse GetManifest with a 32-byte id.
#[test]
fn build_get_manifest() {
    let id = [0xCCu8; 32];
    let msg = SnapshotMessage::GetManifest(id);
    let (code, body) = msg.encode();
    assert_eq!(code, GET_MANIFEST);
    assert_eq!(body.len(), 32);

    let parsed = SnapshotMessage::parse(code, &body).unwrap();
    assert_eq!(parsed, SnapshotMessage::GetManifest(id));
}

/// Build and parse GetUtxoSnapshotChunk with a 32-byte subtree id.
#[test]
fn build_get_utxo_snapshot_chunk() {
    let id = [0xDDu8; 32];
    let msg = SnapshotMessage::GetUtxoSnapshotChunk(id);
    let (code, body) = msg.encode();
    assert_eq!(code, GET_UTXO_SNAPSHOT_CHUNK);
    assert_eq!(body.len(), 32);

    let parsed = SnapshotMessage::parse(code, &body).unwrap();
    assert_eq!(parsed, SnapshotMessage::GetUtxoSnapshotChunk(id));
}

/// Parse a Manifest response (u32 len + bytes).
#[test]
fn parse_manifest_response() {
    let manifest_bytes = vec![0x42u8; 1024];
    let msg = SnapshotMessage::Manifest(manifest_bytes.clone());
    let (code, body) = msg.encode();
    assert_eq!(code, MANIFEST);
    // VLQ(1024) = 2 bytes, + 1024 data bytes
    assert_eq!(body.len(), 2 + 1024);

    let parsed = SnapshotMessage::parse(code, &body).unwrap();
    assert_eq!(parsed, SnapshotMessage::Manifest(manifest_bytes));
}

/// Parse a UtxoSnapshotChunk response (u32 len + bytes).
#[test]
fn parse_utxo_snapshot_chunk_response() {
    let chunk_bytes = vec![0x99u8; 2048];
    let msg = SnapshotMessage::UtxoSnapshotChunk(chunk_bytes.clone());
    let (code, body) = msg.encode();
    assert_eq!(code, UTXO_SNAPSHOT_CHUNK);
    // VLQ(2048) = 2 bytes, + 2048 data bytes
    assert_eq!(body.len(), 2 + 2048);

    let parsed = SnapshotMessage::parse(code, &body).unwrap();
    assert_eq!(parsed, SnapshotMessage::UtxoSnapshotChunk(chunk_bytes));
}

/// Reject oversized messages for all size-limited types.
#[test]
fn reject_oversized_messages() {
    // SnapshotsInfo over MAX_SNAPSHOTS_INFO_SIZE
    let oversized = vec![0u8; MAX_SNAPSHOTS_INFO_SIZE + 1];
    assert_eq!(
        SnapshotMessage::parse(SNAPSHOTS_INFO, &oversized),
        Err(ProtocolError::TooLarge(MAX_SNAPSHOTS_INFO_SIZE + 1))
    );

    // Manifest over MAX_MANIFEST_SIZE
    let oversized = vec![0u8; MAX_MANIFEST_SIZE + 1];
    assert_eq!(
        SnapshotMessage::parse(MANIFEST, &oversized),
        Err(ProtocolError::TooLarge(MAX_MANIFEST_SIZE + 1))
    );

    // UtxoSnapshotChunk over MAX_CHUNK_SIZE
    let oversized = vec![0u8; MAX_CHUNK_SIZE + 1];
    assert_eq!(
        SnapshotMessage::parse(UTXO_SNAPSHOT_CHUNK, &oversized),
        Err(ProtocolError::TooLarge(MAX_CHUNK_SIZE + 1))
    );
}

/// Parse real JVM wire bytes captured from testnet.
/// JVM sent: count=2, heights 268799 and 268927, VLQ+ZigZag encoded.
#[test]
fn parse_real_jvm_snapshots_info() {
    let body: Vec<u8> = vec![
        2, 254, 231, 32, 58, 66, 224, 79, 217, 75, 51, 233, 158, 23, 236, 50,
        7, 157, 130, 232, 149, 102, 39, 253, 72, 215, 26, 124, 86, 25, 119, 45,
        215, 94, 159, 154, 254, 233, 32, 207, 233, 138, 191, 11, 147, 60, 90, 220,
        89, 176, 29, 171, 60, 132, 45, 179, 136, 137, 176, 241, 183, 140, 231, 224,
        63, 174, 232, 234, 185, 200, 41,
    ];

    let parsed = SnapshotMessage::parse(SNAPSHOTS_INFO, &body).unwrap();
    match parsed {
        SnapshotMessage::SnapshotsInfo(entries) => {
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].height, 268799);
            assert_eq!(entries[1].height, 268927);
            // Manifest IDs from JVM REST API
            assert_eq!(
                hex::encode(&entries[0].manifest_id),
                "3a42e04fd94b33e99e17ec32079d82e8956627fd48d71a7c5619772dd75e9f9a"
            );
            assert_eq!(
                hex::encode(&entries[1].manifest_id),
                "cfe98abf0b933c5adc59b01dab3c842db38889b0f1b78ce7e03faee8eab9c829"
            );
        }
        other => panic!("expected SnapshotsInfo, got {:?}", other),
    }
}

/// Parse empty SnapshotsInfo (count=0, single byte).
#[test]
fn parse_empty_snapshots_info() {
    let body = vec![0u8]; // VLQ 0
    let parsed = SnapshotMessage::parse(SNAPSHOTS_INFO, &body).unwrap();
    match parsed {
        SnapshotMessage::SnapshotsInfo(entries) => assert!(entries.is_empty()),
        other => panic!("expected empty SnapshotsInfo, got {:?}", other),
    }
}

/// Verify is_snapshot_code for codes 75-82.
#[test]
fn is_snapshot_code_range() {
    assert!(!SnapshotMessage::is_snapshot_code(75), "75 should not be a snapshot code");
    assert!(SnapshotMessage::is_snapshot_code(76), "76 = GetSnapshotsInfo");
    assert!(SnapshotMessage::is_snapshot_code(77), "77 = SnapshotsInfo");
    assert!(SnapshotMessage::is_snapshot_code(78), "78 = GetManifest");
    assert!(SnapshotMessage::is_snapshot_code(79), "79 = Manifest");
    assert!(SnapshotMessage::is_snapshot_code(80), "80 = GetUtxoSnapshotChunk");
    assert!(SnapshotMessage::is_snapshot_code(81), "81 = UtxoSnapshotChunk");
    assert!(!SnapshotMessage::is_snapshot_code(82), "82 should not be a snapshot code");
}
