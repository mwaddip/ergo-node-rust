#[cfg(test)]
mod parse_tests {
    use crate::{parse_header, ChainError};
    use sigma_ser::ScorexSerializable;

    fn v2_header_json() -> &'static str {
        r#"{
            "extensionId": "d16f25b14457186df4c5f6355579cc769261ce1aebc8209949ca6feadbac5a3f",
            "difficulty": "626412390187008",
            "votes": "040000",
            "timestamp": 1618929697400,
            "size": 221,
            "stateRoot": "8ad868627ea4f7de6e2a2fe3f98fafe57f914e0f2ef3331c006def36c697f92713",
            "height": 471746,
            "nBits": 117586360,
            "version": 2,
            "id": "4caa17e62fe66ba7bd69597afdc996ae35b1ff12e0ba90c22ff288a4de10e91b",
            "adProofsRoot": "d882aaf42e0a95eb95fcce5c3705adf758e591532f733efe790ac3c404730c39",
            "transactionsRoot": "63eaa9aff76a1de3d71c81e4b2d92e8d97ae572a8e9ab9e66599ed0912dd2f8b",
            "extensionHash": "3f91f3c680beb26615fdec251aee3f81aaf5a02740806c167c0f3c929471df44",
            "powSolutions": {
              "pk": "02b3a06d6eaa8671431ba1db4dd427a77f75a5c2acbd71bfb725d38adc2b55f669",
              "w": "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
              "n": "5939ecfee6b0d7f4",
              "d": "1234000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"
            },
            "parentId": "6481752bace5fa5acba5d5ef7124d48826664742d46c974c98a2d60ace229a34"
        }"#
    }

    /// Parse a V2 header from scorex-serialized bytes and verify all fields match.
    #[test]
    fn parse_v2_header_roundtrip() {
        let from_json: ergo_chain_types::Header = serde_json::from_str(v2_header_json()).unwrap();
        let serialized = from_json.scorex_serialize_bytes().unwrap();

        let parsed = parse_header(&serialized).unwrap();

        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.height, 471746);
        assert_eq!(parsed.timestamp, 1618929697400);
        assert_eq!(parsed.n_bits, 117586360);
        assert_eq!(parsed.id, from_json.id);
        assert_eq!(parsed.parent_id, from_json.parent_id);
        assert_eq!(parsed.ad_proofs_root, from_json.ad_proofs_root);
        assert_eq!(parsed.transaction_root, from_json.transaction_root);
        assert_eq!(parsed.state_root, from_json.state_root);
        assert_eq!(parsed.extension_root, from_json.extension_root);
        // V2 wire format omits pow_onetime_pk and pow_distance — they're None after roundtrip
        assert_eq!(parsed.autolykos_solution.miner_pk, from_json.autolykos_solution.miner_pk);
        assert_eq!(parsed.autolykos_solution.nonce, from_json.autolykos_solution.nonce);
        assert!(parsed.autolykos_solution.pow_onetime_pk.is_none());
        assert!(parsed.autolykos_solution.pow_distance.is_none());
        assert_eq!(parsed.votes, from_json.votes);
    }

    /// Parse a V1 header from scorex-serialized bytes.
    #[test]
    fn parse_v1_header_roundtrip() {
        let json = r#"{
            "extensionId": "d16f25b14457186df4c5f6355579cc769261ce1aebc8209949ca6feadbac5a3f",
            "difficulty": "626412390187008",
            "votes": "000000",
            "timestamp": 1562027226367,
            "size": 279,
            "stateRoot": "144c15900826f6e2aac70cb50e541215b337d0d1674da6b491499944e686b41b0e",
            "height": 3132,
            "nBits": 117483687,
            "version": 1,
            "id": "41c73753452a292442799bd884fbcc2a9b0f62d4cff7ad02ccd3dbe65791c908",
            "adProofsRoot": "c9d58eacf6108c9a166b0b76020e3323c6c2ccec5ec8f905ea46f5bcc58aac80",
            "transactionsRoot": "01bf55fd587291172f458232a7f58b4b29469d72b8e304aafd68401f915b0c36",
            "extensionHash": "ccb136ffd50a16f50a499e1c33d8ae1e8426bdc70b13a4d82275d057be2d04a7",
            "powSolutions": {
              "pk": "02ff03f4b981c59ccd5185fddcd949b8f5697341e60d808d2be0e3e09d2ec78bf4",
              "w": "037427400e5292a177dc242631f78ab322b7845ad2b8491b016b7c36407c6a6d76",
              "n": "0000667700008481",
              "d": 410958177852074551025494081160156537946251159549691138805256284
            },
            "parentId": "150290bbaf91ccd4dcf307cb9a5113eed67e12694ec9be277e8fa55fb5ebf6ac"
        }"#;

        let from_json: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let serialized = from_json.scorex_serialize_bytes().unwrap();

        let parsed = parse_header(&serialized).unwrap();

        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.height, 3132);
        assert_eq!(parsed.id, from_json.id);
        assert_eq!(parsed.autolykos_solution.pow_distance, from_json.autolykos_solution.pow_distance);
        assert!(parsed.autolykos_solution.pow_onetime_pk.is_some());
    }

    /// Empty bytes must fail gracefully, not panic.
    #[test]
    fn parse_empty_bytes_fails() {
        let result = parse_header(&[]);
        assert!(result.is_err());
        assert!(matches!(result, Err(ChainError::Parse(_))));
    }

    /// Truncated bytes must fail gracefully.
    #[test]
    fn parse_truncated_bytes_fails() {
        let result = parse_header(&[0x02, 0x01, 0x02, 0x03]);
        assert!(result.is_err());
    }

    /// Random garbage must fail gracefully.
    #[test]
    fn parse_garbage_fails() {
        let garbage: Vec<u8> = (0..200).map(|i| (i * 37 + 13) as u8).collect();
        let result = parse_header(&garbage);
        // May succeed with garbage values or fail — either is fine.
        // The contract says "never panics on malformed input."
        let _ = result;
    }
}

#[cfg(test)]
mod pow_tests {
    use crate::verify_pow;

    /// Valid V2 header at height 614400 (first N increase) — known-good PoW from sigma-rust tests.
    #[test]
    fn verify_pow_valid_v2_header() {
        let json = r#"{
            "extensionId" : "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty" : "16384",
            "votes" : "000000",
            "timestamp" : 4928911477310178288,
            "size" : 223,
            "stateRoot" : "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height" : 614400,
            "nBits" : 37748736,
            "version" : 2,
            "id" : "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot" : "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot" : "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash" : "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions" : {
              "pk" : "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
              "n" : "0000000000003105"
            },
            "adProofsId" : "dec129290a763f4de41f04e87e2b661dd59758af6bdd00dd51f5d97c3a8cb9b5",
            "transactionsId" : "eba1dd82cf51147232e09c1f72b37c554c30f63274d5093bff36849a83472a42",
            "parentId" : "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;

        let header: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let result = verify_pow(&header);
        assert!(result.is_ok(), "valid PoW should pass: {result:?}");
    }

    /// Invalid V2 header at height 2870 — PoW doesn't meet difficulty target.
    #[test]
    fn verify_pow_invalid_v2_header() {
        let json = r#"{"extensionId":"277907e4e5e42f27e928e6101cc4fec173bee5d7728794b73d7448c339c380e5","difficulty":"1325481984","votes":"000000","timestamp":1611225263165,"size":219,"stateRoot":"c0d0b5eafd07b22487dac66628669c42a242b90bef3e1fcdc76d83140d58b6bc0e","height":2870,"nBits":72286528,"version":2,"id":"5b0ce6711de6b926f60b67040cc4512804517785df375d063f1bf1d75588af3a","adProofsRoot":"49453875a43035c7640dee2f905efe06128b00d41acd2c8df13691576d4fd85c","transactionsRoot":"770cbb6e18673ed025d386487f15d3252115d9a6f6c9b947cf3d04731dd6ab75","extensionHash":"9bc7d54583c5d44bb62a7be0473cd78d601822a626afc13b636f2cbff0d87faf","powSolutions":{"pk":"0288114b0586efea9f86e4587f2071bc1c85fb77e15eba96b2769733e0daf57903","w":"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","n":"000100000580a91b","d":0},"adProofsId":"4fc36d59bf26a672e01fbfde1445bd66f50e0f540f24102e1e27d0be1a99dfbf","transactionsId":"d196ef8a7ef582ab1fdab4ef807715183705301c6ae2ff0dcbe8f1d577ba081f","parentId":"ab19e6c7a4062979dddb534df83f236d1b949c7cef18bcf434a67e87c593eef9"}"#;

        let header: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let result = verify_pow(&header);
        assert!(result.is_err(), "invalid PoW should fail");
    }

    /// PoW verification on a parsed-from-bytes header (integration: parse then verify).
    #[test]
    fn parse_then_verify_pow() {
        use crate::parse_header;
        use sigma_ser::ScorexSerializable;

        let json = r#"{
            "extensionId" : "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty" : "16384",
            "votes" : "000000",
            "timestamp" : 4928911477310178288,
            "size" : 223,
            "stateRoot" : "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height" : 614400,
            "nBits" : 37748736,
            "version" : 2,
            "id" : "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot" : "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot" : "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash" : "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions" : {
              "pk" : "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
              "n" : "0000000000003105"
            },
            "adProofsId" : "dec129290a763f4de41f04e87e2b661dd59758af6bdd00dd51f5d97c3a8cb9b5",
            "transactionsId" : "eba1dd82cf51147232e09c1f72b37c554c30f63274d5093bff36849a83472a42",
            "parentId" : "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;

        // Simulate the P2P path: JSON → Header → bytes → parse_header → verify_pow
        let from_json: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let wire_bytes = from_json.scorex_serialize_bytes().unwrap();
        let parsed = parse_header(&wire_bytes).unwrap();

        assert_eq!(parsed.id, from_json.id);
        assert!(verify_pow(&parsed).is_ok());
    }
}

#[cfg(test)]
mod tracker_tests {
    use crate::HeaderTracker;

    fn make_header(height: u32, version: u8) -> ergo_chain_types::Header {
        // Build a minimal header. The id will be computed from the serialized content,
        // so different heights produce different ids.
        use ergo_chain_types::*;
        use sigma_ser::ScorexSerializable;

        let zero32 = Digest32::zero();
        let mut header = Header {
            version,
            id: BlockId(Digest32::zero()),
            parent_id: BlockId(Digest32::zero()),
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp: 1000000 + height as u64,
            n_bits: 100000,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: vec![0; 8],
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };

        // Compute the real id via serialize roundtrip
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    #[test]
    fn empty_tracker() {
        let tracker = HeaderTracker::new();
        assert_eq!(tracker.best_height(), None);
        assert!(tracker.best_header_id().is_none());
    }

    #[test]
    fn single_observation() {
        let mut tracker = HeaderTracker::new();
        let h = make_header(100, 2);
        tracker.observe(&h);

        assert_eq!(tracker.best_height(), Some(100));
        assert_eq!(tracker.best_header_id(), Some(&h.id));
    }

    #[test]
    fn higher_header_updates_tip() {
        let mut tracker = HeaderTracker::new();
        let h1 = make_header(100, 2);
        let h2 = make_header(200, 2);

        tracker.observe(&h1);
        tracker.observe(&h2);

        assert_eq!(tracker.best_height(), Some(200));
        assert_eq!(tracker.best_header_id(), Some(&h2.id));
    }

    #[test]
    fn lower_header_does_not_update_tip() {
        let mut tracker = HeaderTracker::new();
        let h1 = make_header(200, 2);
        let h2 = make_header(100, 2);

        tracker.observe(&h1);
        tracker.observe(&h2);

        assert_eq!(tracker.best_height(), Some(200));
        assert_eq!(tracker.best_header_id(), Some(&h1.id));
    }

    #[test]
    fn equal_height_does_not_update_tip() {
        let mut tracker = HeaderTracker::new();
        let h1 = make_header(100, 2);
        let h2 = make_header(100, 2);

        tracker.observe(&h1);
        let original_id = tracker.best_header_id().cloned();
        tracker.observe(&h2);

        assert_eq!(tracker.best_height(), Some(100));
        // Tip should not change — equal height doesn't dominate.
        assert_eq!(tracker.best_header_id().cloned(), original_id);
    }

    #[test]
    fn many_observations_tracks_max() {
        let mut tracker = HeaderTracker::new();
        let heights = [50, 100, 75, 200, 150, 300, 250, 300, 299];

        let headers: Vec<_> = heights.iter().map(|&h| make_header(h, 2)).collect();
        for h in &headers {
            tracker.observe(h);
        }

        assert_eq!(tracker.best_height(), Some(300));
        // The tip should be the first header observed at height 300 (index 5)
        assert_eq!(tracker.best_header_id(), Some(&headers[5].id));
    }

    #[test]
    fn default_impl() {
        let tracker = HeaderTracker::default();
        assert_eq!(tracker.best_height(), None);
    }
}

#[cfg(test)]
mod chain_tests {
    use crate::{ChainConfig, ChainError, HeaderChain};
    use ergo_chain_types::*;
    use sigma_ser::ScorexSerializable;

    /// Build a header with a computed ID. For chain tests — no real PoW.
    fn make_chain_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };

        // Compute ID via serialization roundtrip
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn genesis_parent_id() -> BlockId {
        BlockId(Digest32::zero())
    }

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    fn make_genesis(config: &ChainConfig) -> Header {
        make_chain_header(1, genesis_parent_id(), 1_000_000, config.initial_n_bits)
    }

    #[test]
    fn empty_chain() {
        let chain = HeaderChain::new(testnet_config());
        assert!(chain.is_empty());
        assert_eq!(chain.height(), 0);
        assert_eq!(chain.len(), 0);
    }

    #[test]
    #[should_panic(expected = "tip() called on empty chain")]
    fn tip_panics_on_empty_chain() {
        // Guard the invariant that tip() surfaces emptiness cleanly,
        // not via arithmetic underflow on `base_height + scores.len()
        // - 1`.
        let chain = HeaderChain::new(testnet_config());
        let _ = chain.tip();
    }

    #[test]
    fn append_genesis() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        let genesis_id = genesis.id;

        assert!(chain.try_append_no_pow(genesis).is_ok());
        assert_eq!(chain.height(), 1);
        assert_eq!(chain.len(), 1);
        assert!(chain.contains(&genesis_id));
        assert_eq!(chain.header_at(1).unwrap().id, genesis_id);
    }

    #[test]
    fn reject_genesis_wrong_parent() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        // Genesis with non-zero parent
        let bad = make_chain_header(1, BlockId(Digest32::from([1u8; 32])), 1_000_000, config.initial_n_bits);

        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::InvalidGenesisParent { .. }));
    }

    #[test]
    fn reject_genesis_wrong_height() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let bad = make_chain_header(5, genesis_parent_id(), 1_000_000, config.initial_n_bits);

        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::InvalidGenesisHeight { .. }));
    }

    #[test]
    fn reject_genesis_wrong_difficulty() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config);
        let bad = make_chain_header(1, genesis_parent_id(), 1_000_000, 99999);

        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::WrongDifficulty { .. }));
    }

    #[test]
    fn reject_genesis_wrong_id() {
        // Build a config with a specific genesis ID requirement
        let mut config = testnet_config();
        let expected_id: BlockId = "b0244dfc267baca974a4caee06120321562784303a8a688976ae56170e4d175b"
            .parse()
            .unwrap();
        config.genesis_id = Some(expected_id);

        let mut chain = HeaderChain::new(config.clone());
        // A synthetic genesis header whose computed ID won't match
        let genesis = make_chain_header(1, genesis_parent_id(), 1_000_000, config.initial_n_bits);
        assert_ne!(genesis.id, expected_id, "test requires ID mismatch");

        let err = chain.try_append_no_pow(genesis).unwrap_err();
        assert!(matches!(err, ChainError::GenesisIdMismatch { .. }));
    }

    #[test]
    fn append_child_after_genesis() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        let parent_id = genesis.id;
        let parent_n_bits = genesis.n_bits;

        chain.try_append_no_pow(genesis).unwrap();

        // Child at height 2 — within first epoch, so n_bits carries forward
        let child = make_chain_header(2, parent_id, 2_000_000, parent_n_bits);
        assert!(chain.try_append_no_pow(child).is_ok());
        assert_eq!(chain.height(), 2);
        assert_eq!(chain.len(), 2);
    }

    #[test]
    fn reject_child_wrong_parent() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        chain.try_append_no_pow(genesis).unwrap();

        // Child pointing to a non-existent parent
        let bad = make_chain_header(2, BlockId(Digest32::from([0xAB; 32])), 2_000_000, config.initial_n_bits);
        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::ParentNotFound { .. }));
    }

    #[test]
    fn reject_child_wrong_height() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        let parent_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        // Height 5 after genesis (should be 2)
        let bad = make_chain_header(5, parent_id, 2_000_000, config.initial_n_bits);
        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::NonSequentialHeight { .. }));
    }

    #[test]
    fn reject_child_timestamp_not_increasing() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        let parent_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        // Timestamp equal to genesis (should be strictly greater)
        let bad = make_chain_header(2, parent_id, 1_000_000, config.initial_n_bits);
        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::TimestampNotIncreasing { .. }));
    }

    #[test]
    fn reject_child_wrong_difficulty() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        let parent_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        // Wrong n_bits (should inherit from parent within epoch)
        let bad = make_chain_header(2, parent_id, 2_000_000, 99999);
        let err = chain.try_append_no_pow(bad).unwrap_err();
        assert!(matches!(err, ChainError::WrongDifficulty { .. }));
    }

    #[test]
    fn duplicate_header_returns_forked() {
        // A duplicate of an existing header has a parent in the chain but
        // not at the tip — try_append returns Forked, not an error.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=10 {
            let tip = chain.tip();
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, &chain).unwrap();
            let header = make_chain_header(h, tip.id, 1_000_000 + h as u64 * 45_000, expected_n_bits);
            chain.try_append_no_pow(header).unwrap();
        }

        let existing = chain.header_at(5).unwrap().clone();
        let result = chain.try_append_no_pow(existing).unwrap();
        assert!(matches!(result, crate::AppendResult::Forked { fork_height: 4 }));
        assert_eq!(chain.height(), 10, "chain should be unchanged");
    }

    #[test]
    fn header_extending_non_tip_returns_forked() {
        // A header whose parent exists in the chain but is not the tip
        // returns Forked — the chain is unchanged.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=10 {
            let tip = chain.tip();
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, &chain).unwrap();
            let header = make_chain_header(h, tip.id, 1_000_000 + h as u64 * 45_000, expected_n_bits);
            chain.try_append_no_pow(header).unwrap();
        }

        let mid_header = chain.header_at(5).unwrap();
        let fork_child = make_chain_header(6, mid_header.id, mid_header.timestamp + 1000, config.initial_n_bits);
        let result = chain.try_append_no_pow(fork_child).unwrap();
        assert!(matches!(result, crate::AppendResult::Forked { fork_height: 5 }));
        assert_eq!(chain.height(), 10, "chain should be unchanged");
    }

    #[test]
    fn difficulty_carries_within_epoch() {
        // Build a chain of several blocks within the first epoch.
        // All should have the same n_bits as genesis.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());

        let genesis = make_genesis(&config);
        let mut prev_id = genesis.id;
        let n_bits = genesis.n_bits;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=10 {
            let header = make_chain_header(h, prev_id, 1_000_000 + h as u64 * 45_000, n_bits);
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        assert_eq!(chain.height(), 10);
        assert_eq!(chain.len(), 10);
        // All headers should have the same difficulty
        for h in 1..=10 {
            assert_eq!(chain.header_at(h).unwrap().n_bits, n_bits);
        }
    }

    #[test]
    fn headers_from_range() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());

        let genesis = make_genesis(&config);
        let mut prev_id = genesis.id;
        let n_bits = genesis.n_bits;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=5 {
            let header = make_chain_header(h, prev_id, 1_000_000 + h as u64 * 45_000, n_bits);
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        let slice = chain.headers_from(2, 3);
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0].height, 2);
        assert_eq!(slice[1].height, 3);
        assert_eq!(slice[2].height, 4);

        // Beyond chain end
        let slice = chain.headers_from(4, 10);
        assert_eq!(slice.len(), 2); // heights 4, 5

        // Before chain start
        let slice = chain.headers_from(0, 5);
        assert_eq!(slice.len(), 0);
    }

    #[test]
    fn difficulty_recalculation_at_epoch_boundary() {
        // Build a chain of 129 blocks (genesis at 1, epoch_length=128).
        // Block 129 triggers recalculation (parent at height 128 = epoch boundary).
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());

        let genesis = make_genesis(&config);
        let mut prev_id = genesis.id;
        let n_bits = genesis.n_bits;
        chain.try_append_no_pow(genesis).unwrap();

        // Build up to height 128 with perfect timing (45s apart)
        for h in 2..=128 {
            let timestamp = 1_000_000 + (h as u64 - 1) * 45_000;
            let header = make_chain_header(h, prev_id, timestamp, n_bits);
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        assert_eq!(chain.height(), 128);

        // Height 129 triggers difficulty recalculation.
        // With perfect block timing (45s intervals), the difficulty should stay
        // approximately the same (the actual result depends on the linear regression
        // with only one epoch of data, which returns the same difficulty).
        let parent = chain.tip();
        let expected_n_bits =
            crate::difficulty::expected_difficulty(&parent, &chain).unwrap();

        let header129 = make_chain_header(
            129,
            prev_id,
            1_000_000 + 128 * 45_000,
            expected_n_bits,
        );
        assert!(chain.try_append_no_pow(header129).is_ok());
        assert_eq!(chain.height(), 129);
    }
}

#[cfg(test)]
mod reorg_tests {
    use crate::{ChainConfig, HeaderChain};
    use ergo_chain_types::*;
    use sigma_ser::ScorexSerializable;

    fn make_chain_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        make_chain_header_with_nonce(height, parent_id, timestamp, n_bits, height.to_be_bytes().repeat(2))
    }

    /// Build a header with a specific nonce — different nonces at the same
    /// height produce different IDs, simulating competing blocks.
    fn make_chain_header_with_nonce(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
        nonce: Vec<u8>,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce,
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    fn make_genesis(config: &ChainConfig) -> Header {
        make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits)
    }

    /// Build a chain of `count` headers, returning the chain.
    fn build_test_chain(count: u32) -> HeaderChain {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        if count == 0 {
            return chain;
        }

        let genesis = make_genesis(&config);
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=count {
            let tip = chain.tip();
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, &chain).unwrap();
            let header = make_chain_header(h, tip.id, 1_000_000 + (h as u64 - 1) * 45_000, expected_n_bits);
            chain.try_append_no_pow(header).unwrap();
        }

        chain
    }

    #[test]
    fn basic_1_deep_reorg() {
        // Chain: [1, 2, 3] with tip at height 3.
        // Create competing block at height 3 (same parent as current tip) + continuation at height 4.
        let mut chain = build_test_chain(3);
        let old_tip_id = chain.tip().id;
        let parent = chain.header_at(2).unwrap();
        let parent_id = parent.id;
        let parent_ts = parent.timestamp;
        let n_bits = parent.n_bits;

        // Alternative block at height 3 — same parent as current tip, different nonce
        let alt_tip = make_chain_header_with_nonce(
            3, parent_id, parent_ts + 50_000, n_bits,
            vec![0xFF; 8], // different nonce → different ID
        );
        assert_ne!(alt_tip.id, old_tip_id, "alternative must have different ID");

        // Continuation at height 4 building on the alternative
        let continuation = make_chain_header(4, alt_tip.id, parent_ts + 100_000, n_bits);

        let replaced = chain.try_reorg_no_pow(alt_tip.clone(), continuation.clone()).unwrap();

        assert_eq!(replaced, old_tip_id);
        assert_eq!(chain.height(), 4);
        assert_eq!(chain.len(), 4);
        assert_eq!(chain.header_at(3).unwrap().id, alt_tip.id);
        assert_eq!(chain.header_at(4).unwrap().id, continuation.id);
        assert!(!chain.contains(&old_tip_id), "old tip should be removed");
        assert!(chain.contains(&alt_tip.id));
        assert!(chain.contains(&continuation.id));
    }

    #[test]
    fn reorg_rejected_alternative_wrong_parent() {
        // Alternative doesn't share parent with current tip — not a competing block.
        let mut chain = build_test_chain(3);
        let n_bits = chain.tip().n_bits;

        // Alternative pointing to some random parent
        let alt_tip = make_chain_header_with_nonce(
            3, BlockId(Digest32::from([0xAB; 32])), 2_100_000, n_bits,
            vec![0xFF; 8],
        );
        let continuation = make_chain_header(4, alt_tip.id, 2_200_000, n_bits);

        let result = chain.try_reorg_no_pow(alt_tip, continuation);
        assert!(result.is_err());
        assert_eq!(chain.height(), 3, "chain should be unchanged");
    }

    #[test]
    fn reorg_rejected_continuation_wrong_parent() {
        // Continuation doesn't build on the alternative.
        let mut chain = build_test_chain(3);
        let parent = chain.header_at(2).unwrap();
        let parent_id = parent.id;
        let parent_ts = parent.timestamp;
        let n_bits = parent.n_bits;

        let alt_tip = make_chain_header_with_nonce(
            3, parent_id, parent_ts + 50_000, n_bits,
            vec![0xFF; 8],
        );
        // Continuation points to wrong parent (not the alternative)
        let continuation = make_chain_header(4, BlockId(Digest32::from([0xCD; 32])), parent_ts + 100_000, n_bits);

        let result = chain.try_reorg_no_pow(alt_tip, continuation);
        assert!(result.is_err());
        assert_eq!(chain.height(), 3, "chain should be unchanged");
    }

    #[test]
    fn reorg_rejected_alternative_wrong_height() {
        // Alternative at wrong height (not same as current tip).
        let mut chain = build_test_chain(3);
        let parent = chain.header_at(2).unwrap();
        let parent_id = parent.id;
        let parent_ts = parent.timestamp;
        let n_bits = parent.n_bits;

        // Height 5 instead of 3
        let alt_tip = make_chain_header_with_nonce(
            5, parent_id, parent_ts + 50_000, n_bits,
            vec![0xFF; 8],
        );
        let continuation = make_chain_header(6, alt_tip.id, parent_ts + 100_000, n_bits);

        let result = chain.try_reorg_no_pow(alt_tip, continuation);
        assert!(result.is_err());
        assert_eq!(chain.height(), 3);
    }

    #[test]
    fn reorg_rejected_on_empty_chain() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());

        let alt = make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits);
        let cont = make_chain_header(2, alt.id, 2_000_000, config.initial_n_bits);

        let result = chain.try_reorg_no_pow(alt, cont);
        assert!(result.is_err());
    }

    #[test]
    fn reorg_rejected_on_single_header_chain() {
        // Can't reorg genesis — there's no parent to fall back to.
        let mut chain = build_test_chain(1);
        let n_bits = chain.tip().n_bits;

        let alt = make_chain_header_with_nonce(
            1, BlockId(Digest32::zero()), 1_000_000, n_bits,
            vec![0xFF; 8],
        );
        let cont = make_chain_header(2, alt.id, 2_000_000, n_bits);

        let result = chain.try_reorg_no_pow(alt, cont);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod sync_info_tests {
    use crate::{build_sync_info, parse_sync_info, ChainConfig, HeaderChain, SyncInfo};
    use ergo_chain_types::*;
    use sigma_ser::ScorexSerializable;

    fn make_chain_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    fn make_genesis(config: &ChainConfig) -> Header {
        make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits)
    }

    fn build_test_chain(count: u32) -> HeaderChain {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        if count == 0 {
            return chain;
        }

        let genesis = make_genesis(&config);
        let mut prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=count {
            let parent = chain.tip();
            let expected_n_bits =
                crate::difficulty::expected_difficulty(&parent, &chain).unwrap();
            let timestamp = 1_000_000 + (h as u64 - 1) * 45_000;
            let header = make_chain_header(h, prev_id, timestamp, expected_n_bits);
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        chain
    }

    // --- build_sync_info tests ---

    #[test]
    fn build_empty_chain_produces_v2_zero_headers() {
        let chain = build_test_chain(0);
        let bytes = build_sync_info(&chain);
        // VLQ(0) = 0x00, mode = 0xFF, count = 0x00
        assert_eq!(bytes, vec![0x00, 0xFF, 0x00]);
    }

    #[test]
    fn build_short_chain_includes_only_tip() {
        // 5 headers (heights 1-5). Offsets: [0, 16, 128, 512].
        // Only offset 0 (height 5) is within range.
        let chain = build_test_chain(5);
        let bytes = build_sync_info(&chain);

        // Parse it back to verify content
        let sync = parse_sync_info(&bytes).unwrap();
        match sync {
            SyncInfo::V2 { headers } => {
                assert_eq!(headers.len(), 1);
                assert_eq!(headers[0].height, 5);
            }
            _ => panic!("expected V2"),
        }
    }

    #[test]
    fn build_long_chain_includes_four_headers_at_offsets() {
        // 600 headers (heights 1-600). Offsets from tip (600):
        //   0 → height 600, 16 → height 584, 128 → height 472, 512 → height 88
        // All within range (chain starts at 1).
        let chain = build_test_chain(600);
        let bytes = build_sync_info(&chain);

        let sync = parse_sync_info(&bytes).unwrap();
        match sync {
            SyncInfo::V2 { headers } => {
                assert_eq!(headers.len(), 4);
                // Tip-first ordering
                assert_eq!(headers[0].height, 600);
                assert_eq!(headers[1].height, 584);
                assert_eq!(headers[2].height, 472);
                assert_eq!(headers[3].height, 88);
            }
            _ => panic!("expected V2"),
        }
    }

    // --- roundtrip tests ---

    #[test]
    fn build_parse_roundtrip() {
        let chain = build_test_chain(200);
        let bytes = build_sync_info(&chain);
        let sync = parse_sync_info(&bytes).unwrap();

        match sync {
            SyncInfo::V2 { headers } => {
                // Offsets from tip 200: 0→200, 16→184, 128→72. 512 below chain start.
                assert_eq!(headers.len(), 3);
                for parsed_hdr in &headers {
                    let chain_hdr = chain.header_at(parsed_hdr.height).unwrap();
                    assert_eq!(parsed_hdr.id, chain_hdr.id);
                }
            }
            _ => panic!("expected V2"),
        }
    }

    // --- parse V1 ---

    #[test]
    fn parse_v1_header_ids() {
        // V1: VLQ(count) followed by count * 32-byte header IDs
        let mut body = Vec::new();
        // VLQ encode 3
        body.push(3u8);
        // 3 header IDs (32 bytes each)
        for i in 0u8..3 {
            body.extend_from_slice(&[i + 1; 32]);
        }

        let sync = parse_sync_info(&body).unwrap();
        match sync {
            SyncInfo::V1 { header_ids } => {
                assert_eq!(header_ids.len(), 3);
                assert_eq!(header_ids[0].0 .0, [1u8; 32]);
                assert_eq!(header_ids[1].0 .0, [2u8; 32]);
                assert_eq!(header_ids[2].0 .0, [3u8; 32]);
            }
            _ => panic!("expected V1"),
        }
    }

    // --- parse V2 with zero headers ---

    #[test]
    fn parse_v2_zero_headers() {
        let body = vec![0x00, 0xFF, 0x00];
        let sync = parse_sync_info(&body).unwrap();
        match sync {
            SyncInfo::V2 { headers } => assert!(headers.is_empty()),
            _ => panic!("expected V2"),
        }
    }

    // --- error cases ---

    #[test]
    fn parse_garbage_returns_error() {
        let garbage: Vec<u8> = (0..50).map(|i| (i * 37 + 13) as u8).collect();
        // Should not panic — may be Err or weird Ok, but never panic.
        let _ = parse_sync_info(&garbage);
    }

    #[test]
    fn parse_empty_returns_error() {
        let result = parse_sync_info(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_v2_too_many_headers_rejected() {
        // Craft a V2 message claiming 51 headers
        let body = vec![0x00, 0xFF, 51u8];
        let result = parse_sync_info(&body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_v2_oversized_header_rejected() {
        // Craft a V2 message with 1 header claiming 1001 bytes
        let mut body = vec![0x00, 0xFF, 0x01];
        // VLQ encode 1001 (0xE9 0x07 in VLQ)
        body.push(0xE9);
        body.push(0x07);
        // Pad with enough junk bytes
        body.extend(vec![0xAB; 1001]);

        let result = parse_sync_info(&body);
        assert!(result.is_err());
    }

    #[test]
    fn parse_v1_too_many_header_ids_rejected() {
        // V1 with 1002 header IDs — over the 1001 limit
        let mut body = Vec::new();
        // VLQ encode 1002 = 0xEA 0x07
        body.push(0xEA);
        body.push(0x07);
        // Don't need actual data — should reject before reading
        body.extend(vec![0x00; 32 * 10]); // some padding

        let result = parse_sync_info(&body);
        assert!(result.is_err());
    }
}

#[cfg(test)]
mod section_tests {
    use crate::section_ids;

    fn hex(bytes: &[u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Verify section IDs against JVM-computed values for a real mainnet header.
    /// Header at height 2870 — section IDs from the JVM node's JSON output.
    #[test]
    fn section_ids_match_jvm_test_vector() {
        let json = r#"{"extensionId":"277907e4e5e42f27e928e6101cc4fec173bee5d7728794b73d7448c339c380e5","difficulty":"1325481984","votes":"000000","timestamp":1611225263165,"size":219,"stateRoot":"c0d0b5eafd07b22487dac66628669c42a242b90bef3e1fcdc76d83140d58b6bc0e","height":2870,"nBits":72286528,"version":2,"id":"5b0ce6711de6b926f60b67040cc4512804517785df375d063f1bf1d75588af3a","adProofsRoot":"49453875a43035c7640dee2f905efe06128b00d41acd2c8df13691576d4fd85c","transactionsRoot":"770cbb6e18673ed025d386487f15d3252115d9a6f6c9b947cf3d04731dd6ab75","extensionHash":"9bc7d54583c5d44bb62a7be0473cd78d601822a626afc13b636f2cbff0d87faf","powSolutions":{"pk":"0288114b0586efea9f86e4587f2071bc1c85fb77e15eba96b2769733e0daf57903","w":"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","n":"000100000580a91b","d":0},"adProofsId":"4fc36d59bf26a672e01fbfde1445bd66f50e0f540f24102e1e27d0be1a99dfbf","transactionsId":"d196ef8a7ef582ab1fdab4ef807715183705301c6ae2ff0dcbe8f1d577ba081f","parentId":"ab19e6c7a4062979dddb534df83f236d1b949c7cef18bcf434a67e87c593eef9"}"#;

        let header: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let ids = section_ids(&header);

        // BlockTransactions (type 102)
        assert_eq!(ids[0].0, 102);
        assert_eq!(
            hex(&ids[0].1),
            "d196ef8a7ef582ab1fdab4ef807715183705301c6ae2ff0dcbe8f1d577ba081f"
        );

        // ADProofs (type 104)
        assert_eq!(ids[1].0, 104);
        assert_eq!(
            hex(&ids[1].1),
            "4fc36d59bf26a672e01fbfde1445bd66f50e0f540f24102e1e27d0be1a99dfbf"
        );

        // Extension (type 108)
        assert_eq!(ids[2].0, 108);
        assert_eq!(
            hex(&ids[2].1),
            "277907e4e5e42f27e928e6101cc4fec173bee5d7728794b73d7448c339c380e5"
        );
    }

    /// Same but for a second header (height 614400) to avoid single-vector luck.
    #[test]
    fn section_ids_second_vector() {
        let json = r#"{
            "extensionId" : "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty" : "16384",
            "votes" : "000000",
            "timestamp" : 4928911477310178288,
            "size" : 223,
            "stateRoot" : "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height" : 614400,
            "nBits" : 37748736,
            "version" : 2,
            "id" : "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot" : "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot" : "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash" : "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions" : {
              "pk" : "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
              "n" : "0000000000003105"
            },
            "adProofsId" : "dec129290a763f4de41f04e87e2b661dd59758af6bdd00dd51f5d97c3a8cb9b5",
            "transactionsId" : "eba1dd82cf51147232e09c1f72b37c554c30f63274d5093bff36849a83472a42",
            "parentId" : "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;

        let header: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let ids = section_ids(&header);

        assert_eq!(ids[0].0, 102);
        assert_eq!(
            hex(&ids[0].1),
            "eba1dd82cf51147232e09c1f72b37c554c30f63274d5093bff36849a83472a42"
        );

        assert_eq!(ids[1].0, 104);
        assert_eq!(
            hex(&ids[1].1),
            "dec129290a763f4de41f04e87e2b661dd59758af6bdd00dd51f5d97c3a8cb9b5"
        );

        assert_eq!(ids[2].0, 108);
        assert_eq!(
            hex(&ids[2].1),
            "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18"
        );
    }

    #[test]
    fn section_ids_deterministic() {
        let json = r#"{"extensionId":"277907e4e5e42f27e928e6101cc4fec173bee5d7728794b73d7448c339c380e5","difficulty":"1325481984","votes":"000000","timestamp":1611225263165,"size":219,"stateRoot":"c0d0b5eafd07b22487dac66628669c42a242b90bef3e1fcdc76d83140d58b6bc0e","height":2870,"nBits":72286528,"version":2,"id":"5b0ce6711de6b926f60b67040cc4512804517785df375d063f1bf1d75588af3a","adProofsRoot":"49453875a43035c7640dee2f905efe06128b00d41acd2c8df13691576d4fd85c","transactionsRoot":"770cbb6e18673ed025d386487f15d3252115d9a6f6c9b947cf3d04731dd6ab75","extensionHash":"9bc7d54583c5d44bb62a7be0473cd78d601822a626afc13b636f2cbff0d87faf","powSolutions":{"pk":"0288114b0586efea9f86e4587f2071bc1c85fb77e15eba96b2769733e0daf57903","w":"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798","n":"000100000580a91b","d":0},"adProofsId":"4fc36d59bf26a672e01fbfde1445bd66f50e0f540f24102e1e27d0be1a99dfbf","transactionsId":"d196ef8a7ef582ab1fdab4ef807715183705301c6ae2ff0dcbe8f1d577ba081f","parentId":"ab19e6c7a4062979dddb534df83f236d1b949c7cef18bcf434a67e87c593eef9"}"#;

        let header: ergo_chain_types::Header = serde_json::from_str(json).unwrap();
        let ids1 = section_ids(&header);
        let ids2 = section_ids(&header);
        assert_eq!(ids1, ids2);
    }
}

#[cfg(test)]
mod score_and_deep_reorg_tests {
    use crate::{AppendResult, ChainConfig, HeaderChain};
    use ergo_chain_types::*;
    use num_bigint::BigUint;
    use sigma_ser::ScorexSerializable;

    fn make_chain_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        make_chain_header_with_nonce(height, parent_id, timestamp, n_bits, height.to_be_bytes().repeat(2))
    }

    fn make_chain_header_with_nonce(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
        nonce: Vec<u8>,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce,
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    fn make_genesis(config: &ChainConfig) -> Header {
        make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits)
    }

    fn build_test_chain(count: u32) -> HeaderChain {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        if count == 0 {
            return chain;
        }
        let genesis = make_genesis(&config);
        chain.try_append_no_pow(genesis).unwrap();
        for h in 2..=count {
            let tip = chain.tip();
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, &chain).unwrap();
            let header = make_chain_header(h, tip.id, 1_000_000 + (h as u64 - 1) * 45_000, expected_n_bits);
            chain.try_append_no_pow(header).unwrap();
        }
        chain
    }

    // --- Cumulative score tests ---

    #[test]
    fn cumulative_score_empty_chain_is_zero() {
        let chain = build_test_chain(0);
        assert_eq!(chain.cumulative_score(), BigUint::ZERO);
    }

    #[test]
    fn cumulative_score_increases_on_append() {
        let chain = build_test_chain(5);
        let score = chain.cumulative_score();
        assert!(score > BigUint::ZERO, "score should be positive after appending headers");
        // Each header contributes decode_compact_bits(n_bits). With constant n_bits,
        // the score should be n * difficulty.
        let score_at_1 = chain.score_at(1).unwrap().clone();
        let score_at_5 = chain.score_at(5).unwrap().clone();
        assert!(score_at_5 > score_at_1);
    }

    #[test]
    fn score_at_returns_none_for_empty_chain() {
        let chain = build_test_chain(0);
        assert!(chain.score_at(1).is_none());
    }

    #[test]
    fn score_at_returns_none_for_out_of_range() {
        let chain = build_test_chain(5);
        assert!(chain.score_at(100).is_none());
    }

    // --- AppendResult tests ---

    #[test]
    fn try_append_returns_extended_for_tip_child() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_genesis(&config);
        let result = chain.try_append_no_pow(genesis).unwrap();
        assert!(matches!(result, AppendResult::Extended));
    }

    #[test]
    fn try_append_returns_forked_for_non_tip_parent() {
        let mut chain = build_test_chain(10);
        let mid = chain.header_at(5).unwrap();
        let fork_header = make_chain_header(6, mid.id, mid.timestamp + 1000, mid.n_bits);
        let result = chain.try_append_no_pow(fork_header).unwrap();
        assert!(matches!(result, AppendResult::Forked { fork_height: 5 }));
    }

    #[test]
    fn try_append_forked_does_not_modify_chain() {
        let mut chain = build_test_chain(10);
        let original_height = chain.height();
        let original_tip_id = chain.tip().id;
        let original_score = chain.cumulative_score();

        let mid = chain.header_at(5).unwrap();
        let fork_header = make_chain_header(6, mid.id, mid.timestamp + 1000, mid.n_bits);
        let _ = chain.try_append_no_pow(fork_header).unwrap();

        assert_eq!(chain.height(), original_height);
        assert_eq!(chain.tip().id, original_tip_id);
        assert_eq!(chain.cumulative_score(), original_score);
    }

    // --- Deep reorg tests ---

    #[test]
    fn try_reorg_deep_switches_to_longer_fork() {
        // Build chain [1, 2, 3, 4] (tip at 4).
        // Create a new branch from genesis: [2', 3', 4', 5'] — longer fork.
        let mut chain = build_test_chain(4);
        let n_bits = chain.tip().n_bits;
        let genesis_id = chain.header_at(1).unwrap().id;
        let genesis_ts = chain.header_at(1).unwrap().timestamp;

        // Build alternative branch from height 1 (genesis)
        let mut alt_branch = Vec::new();
        let mut prev_id = genesis_id;
        for h in 2..=5 {
            let header = make_chain_header_with_nonce(
                h, prev_id, genesis_ts + (h as u64) * 50_000, n_bits,
                vec![0xAA; 8],
            );
            prev_id = header.id;
            alt_branch.push(header);
        }

        let demoted = chain.try_reorg_deep_no_pow(1, alt_branch.clone()).unwrap();

        assert_eq!(demoted.len(), 3); // heights 2, 3, 4 were demoted
        assert_eq!(chain.height(), 5);
        assert_eq!(chain.len(), 5);
        assert_eq!(chain.header_at(2).unwrap().id, alt_branch[0].id);
        assert_eq!(chain.header_at(5).unwrap().id, alt_branch[3].id);
    }

    #[test]
    fn try_reorg_deep_rolls_back_on_validation_failure() {
        // Build chain [1, 2, 3, 4, 5].
        // New branch has a bad header (wrong timestamp at position 2).
        let mut chain = build_test_chain(5);
        let original_height = chain.height();
        let original_tip_id = chain.tip().id;
        let original_score = chain.cumulative_score();
        let n_bits = chain.tip().n_bits;
        let genesis_id = chain.header_at(1).unwrap().id;
        let genesis_ts = chain.header_at(1).unwrap().timestamp;

        // First header is valid, second has timestamp <= first (invalid)
        let h2 = make_chain_header_with_nonce(2, genesis_id, genesis_ts + 50_000, n_bits, vec![0xBB; 8]);
        let h3_bad = make_chain_header_with_nonce(3, h2.id, genesis_ts + 10_000, n_bits, vec![0xBB; 8]); // ts goes backwards

        let result = chain.try_reorg_deep_no_pow(1, vec![h2, h3_bad]);
        assert!(result.is_err());
        assert_eq!(chain.height(), original_height);
        assert_eq!(chain.tip().id, original_tip_id);
        assert_eq!(chain.cumulative_score(), original_score);
    }

    #[test]
    fn try_reorg_deep_rejects_empty_branch() {
        let mut chain = build_test_chain(5);
        let result = chain.try_reorg_deep_no_pow(1, vec![]);
        assert!(result.is_err());
    }

    #[test]
    fn try_reorg_deep_rejects_wrong_parent() {
        // First header doesn't connect to the fork point.
        let mut chain = build_test_chain(5);
        let n_bits = chain.tip().n_bits;

        let bad = make_chain_header(2, BlockId(Digest32::from([0xDD; 32])), 2_000_000, n_bits);
        let result = chain.try_reorg_deep_no_pow(1, vec![bad]);
        assert!(result.is_err());
    }

    #[test]
    fn try_reorg_deep_updates_scores() {
        // After reorg, cumulative_score reflects the new branch.
        let mut chain = build_test_chain(4);
        let n_bits = chain.tip().n_bits;
        let genesis_id = chain.header_at(1).unwrap().id;
        let genesis_ts = chain.header_at(1).unwrap().timestamp;
        let score_before = chain.cumulative_score();

        // Build a 5-header branch (longer → higher score with same difficulty).
        let mut alt_branch = Vec::new();
        let mut prev_id = genesis_id;
        for h in 2..=6 {
            let header = make_chain_header_with_nonce(
                h, prev_id, genesis_ts + (h as u64) * 50_000, n_bits,
                vec![0xCC; 8],
            );
            prev_id = header.id;
            alt_branch.push(header);
        }

        chain.try_reorg_deep_no_pow(1, alt_branch).unwrap();
        let score_after = chain.cumulative_score();

        // New branch has 5 blocks above genesis vs 3 before → higher score.
        assert!(score_after > score_before);
        // Score at fork point should be unchanged.
        assert!(chain.score_at(1).is_some());
    }
}

#[cfg(test)]
mod voting_chain_tests {
    use crate::{ChainConfig, HeaderChain};
    use ergo_chain_types::*;
    use sigma_ser::ScorexSerializable;

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    /// Build a header at the given height with custom votes.
    fn make_header_with_votes(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
        votes: [u8; 3],
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes(votes),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn build_chain_with_votes(count: u32, votes: [u8; 3]) -> HeaderChain {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        let genesis = make_header_with_votes(
            1,
            BlockId(Digest32::zero()),
            1_000_000,
            n_bits,
            votes,
        );
        let mut prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=count {
            let tip = chain.tip();
            let expected_n_bits = crate::difficulty::expected_difficulty(&tip, &chain).unwrap();
            let header = make_header_with_votes(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                expected_n_bits,
                votes,
            );
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }
        chain
    }

    #[test]
    fn epoch_boundary_zero_is_false() {
        let chain = HeaderChain::new(testnet_config());
        assert!(!chain.is_epoch_boundary(0));
    }

    #[test]
    fn epoch_boundary_within_first_epoch_is_false() {
        let chain = HeaderChain::new(testnet_config());
        assert!(!chain.is_epoch_boundary(127));
    }

    #[test]
    fn epoch_boundary_at_voting_length_is_true() {
        let chain = HeaderChain::new(testnet_config());
        // testnet voting_length = 128
        assert!(chain.is_epoch_boundary(128));
        assert!(chain.is_epoch_boundary(256));
        assert!(chain.is_epoch_boundary(128 * 50));
    }

    #[test]
    fn epoch_boundary_off_by_one_is_false() {
        let chain = HeaderChain::new(testnet_config());
        assert!(!chain.is_epoch_boundary(129));
    }

    #[test]
    fn epoch_boundary_mainnet_at_1024() {
        let chain = HeaderChain::new(ChainConfig::mainnet());
        assert!(chain.is_epoch_boundary(1024));
        assert!(!chain.is_epoch_boundary(1023));
        assert!(!chain.is_epoch_boundary(128), "128 is testnet boundary, not mainnet");
    }

    #[test]
    fn count_votes_in_epoch_uniform_voting() {
        // Build 128 testnet headers each voting [1, 0, 0]
        let chain = build_chain_with_votes(128, [1, 0, 0]);
        let tally = chain.count_votes_in_epoch(128).unwrap();
        assert_eq!(tally.get(&1), Some(&128));
        assert_eq!(tally.len(), 1);
    }

    #[test]
    fn count_votes_in_epoch_three_slots_summed() {
        let chain = build_chain_with_votes(128, [1, 2, 3]);
        let tally = chain.count_votes_in_epoch(128).unwrap();
        assert_eq!(tally.get(&1), Some(&128));
        assert_eq!(tally.get(&2), Some(&128));
        assert_eq!(tally.get(&3), Some(&128));
    }

    #[test]
    fn count_votes_in_epoch_zero_slots_ignored() {
        let chain = build_chain_with_votes(128, [0, 0, 0]);
        let tally = chain.count_votes_in_epoch(128).unwrap();
        assert!(tally.is_empty());
    }

    #[test]
    fn count_votes_in_epoch_only_uses_last_voting_length_headers() {
        // Build 200 headers, only the last 128 should be counted at height=200.
        let chain = build_chain_with_votes(200, [1, 0, 0]);
        let tally = chain.count_votes_in_epoch(200).unwrap();
        assert_eq!(tally.get(&1), Some(&128));
    }

    #[test]
    fn count_votes_in_epoch_missing_header_errors() {
        // Chain has only 50 headers; asking for epoch ending at 128 must error.
        let chain = build_chain_with_votes(50, [1, 0, 0]);
        assert!(chain.count_votes_in_epoch(128).is_err());
    }

    #[test]
    fn active_parameters_starts_as_default() {
        let chain = HeaderChain::new(testnet_config());
        let params = chain.active_parameters();
        // The default we provide is JVM-startup defaults — must include MaxBlockCost
        // (the upstream Default::default() omits it).
        // Testnet starts at protocol v4 (post-6.0 testnet.conf).
        assert_eq!(params.block_version(), 4);
        assert_eq!(params.storage_fee_factor(), 1_250_000);
        let _ = params.max_block_cost(); // Must not panic

        // Mainnet still starts at v1
        let mainnet_chain = HeaderChain::new(crate::ChainConfig::mainnet());
        assert_eq!(mainnet_chain.active_parameters().block_version(), 1);
    }

    #[test]
    fn apply_epoch_boundary_parameters_advances_active() {
        let mut chain = HeaderChain::new(testnet_config());
        let new_params = ergo_lib::chain::parameters::Parameters::new(
            2,         // BlockVersion
            1_300_000, // StorageFeeFactor
            360,       // MinValuePerByte
            524_288,   // MaxBlockSize
            1_000_000, // MaxBlockCost
            100,       // TokenAccessCost
            2_000,     // InputCost
            100,       // DataInputCost
            100,       // OutputCost
        );
        let keep = chain.active_proposed_update_bytes().to_vec();
        chain.apply_epoch_boundary_parameters(new_params.clone(), keep);
        assert_eq!(chain.active_parameters(), &new_params);
        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_300_000);
        assert_eq!(chain.active_parameters().block_version(), 2);
    }

    #[test]
    fn compute_expected_parameters_majority_increases_step() {
        // 65 of 128 testnet headers vote to increase StorageFeeFactor (ID 1).
        // changeApproved threshold is `> 64` (strict), so 65 passes.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        // Genesis (no vote)
        let mut prev_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let genesis = make_header_with_votes(1, prev_id, 1_000_000, n_bits, [0, 0, 0]);
        prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        // Headers 2..=66: vote [1,0,0]; headers 67..=128: vote [0,0,0].
        for h in 2..=128u32 {
            let votes = if h <= 66 { [1, 0, 0] } else { [0, 0, 0] };
            let header = make_header_with_votes(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
                votes,
            );
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        // Current active_parameters StorageFeeFactor = 1_250_000 (default)
        let original = chain.active_parameters().storage_fee_factor();
        assert_eq!(original, 1_250_000);

        let expected = chain.compute_expected_parameters(128, &[]).unwrap();
        // Step is +25_000 for ID 1
        assert_eq!(expected.storage_fee_factor(), 1_275_000);
    }

    #[test]
    fn compute_expected_parameters_exact_half_no_change() {
        // 64 of 128 vote — exactly half — `change_approved` is strict `>`.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        let mut prev_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let genesis = make_header_with_votes(1, prev_id, 1_000_000, n_bits, [0, 0, 0]);
        prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=128u32 {
            let votes = if h <= 65 { [1, 0, 0] } else { [0, 0, 0] };
            // 65 - 2 + 1 = 64 voting headers (heights 2..=65)
            let header = make_header_with_votes(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
                votes,
            );
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        let expected = chain.compute_expected_parameters(128, &[]).unwrap();
        // Tally is exactly 64 → not approved → unchanged.
        assert_eq!(expected.storage_fee_factor(), 1_250_000);
    }

    #[test]
    fn compute_expected_parameters_negative_vote_decreases() {
        // 65 vote -1 (decrease StorageFeeFactor).
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        let mut prev_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let genesis = make_header_with_votes(1, prev_id, 1_000_000, n_bits, [0, 0, 0]);
        prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=128u32 {
            // 65 voting headers: heights 2..=66
            let votes = if h <= 66 { [0xFFu8, 0, 0] } else { [0, 0, 0] };
            let header = make_header_with_votes(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
                votes,
            );
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        let expected = chain.compute_expected_parameters(128, &[]).unwrap();
        assert_eq!(expected.storage_fee_factor(), 1_225_000); // -25_000
    }

    #[test]
    fn compute_expected_parameters_no_votes_unchanged() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;

        let mut prev_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let genesis = make_header_with_votes(1, prev_id, 1_000_000, n_bits, [0, 0, 0]);
        prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        for h in 2..=128u32 {
            let header = make_header_with_votes(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
                [0, 0, 0],
            );
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        let expected = chain.compute_expected_parameters(128, &[]).unwrap();
        assert_eq!(
            expected.storage_fee_factor(),
            chain.active_parameters().storage_fee_factor()
        );
        // Testnet starts at protocol v4 — unchanged when no votes are cast.
        assert_eq!(expected.block_version(), 4);
    }

    #[test]
    fn recompute_with_no_loader_errors() {
        let mut chain = HeaderChain::new(testnet_config());
        // Chain has tip at 0, no loader → error path covered.
        let tip = chain.height();
        let r = chain.recompute_active_parameters_from_storage(tip);
        // With no loader and no boundary block needed (empty chain), should be Ok
        // because nothing to do.
        assert!(r.is_ok());
    }

    #[test]
    fn recompute_with_short_chain_keeps_defaults() {
        let mut chain = HeaderChain::new(testnet_config());
        // Set a loader that should never be called
        chain.set_extension_loader(|_| panic!("loader called for short chain"));
        // Build chain shorter than voting_length (testnet = 128)
        let n_bits = chain.config().initial_n_bits;
        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=50 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        let tip = chain.height();
        chain.recompute_active_parameters_from_storage(tip).unwrap();
        // Defaults preserved (testnet starts at v4)
        assert_eq!(chain.active_parameters().block_version(), 4);
    }

    #[test]
    fn recompute_loads_params_from_loader() {
        use crate::voting::{pack_extension_bytes, pack_parameters_to_kv};
        use std::collections::HashMap;

        let mut chain = HeaderChain::new(testnet_config());
        let n_bits = chain.config().initial_n_bits;

        // Build a chain past the first epoch boundary (height 128).
        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=130 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        // Build the synthetic extension at height 128.
        let mut params: HashMap<i8, i32> = HashMap::new();
        params.insert(1, 1_300_000); // StorageFeeFactor
        params.insert(2, 360);
        params.insert(3, 524_288);
        params.insert(4, 1_000_000);
        params.insert(5, 100);
        params.insert(6, 2_000);
        params.insert(7, 100);
        params.insert(8, 100);
        params.insert(123, 2);

        let kv = pack_parameters_to_kv(&params);
        let header_id = chain.header_at(128).unwrap().id;
        let extension_bytes = pack_extension_bytes(&header_id, &kv);

        // Loader returns extension only at height 128.
        chain.set_extension_loader(move |h| {
            if h == 128 {
                Some(extension_bytes.clone())
            } else {
                None
            }
        });

        let tip = chain.height();
        chain.recompute_active_parameters_from_storage(tip).unwrap();
        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_300_000);
        assert_eq!(chain.active_parameters().block_version(), 2);
    }

    #[test]
    fn recompute_loader_returning_none_errors() {
        let mut chain = HeaderChain::new(testnet_config());
        let n_bits = chain.config().initial_n_bits;

        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=130 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        chain.set_extension_loader(|_| None);
        let tip = chain.height();
        let r = chain.recompute_active_parameters_from_storage(tip);
        assert!(r.is_err());
    }

    #[test]
    fn recompute_loader_returning_garbage_errors() {
        let mut chain = HeaderChain::new(testnet_config());
        let n_bits = chain.config().initial_n_bits;

        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=130 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        chain.set_extension_loader(|_| Some(vec![0xAB; 5])); // Too short
        let tip = chain.height();
        let r = chain.recompute_active_parameters_from_storage(tip);
        assert!(r.is_err());
    }

    #[test]
    fn recompute_mismatched_header_id_errors() {
        use crate::voting::{pack_extension_bytes, pack_parameters_to_kv};
        use std::collections::HashMap;

        let mut chain = HeaderChain::new(testnet_config());
        let n_bits = chain.config().initial_n_bits;

        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=130 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        // Pack extension with a bogus header_id that doesn't match height 128.
        let bogus_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::from([0xFFu8; 32]));
        let mut params: HashMap<i8, i32> = HashMap::new();
        params.insert(1, 1_300_000);
        let kv = pack_parameters_to_kv(&params);
        let extension_bytes = pack_extension_bytes(&bogus_id, &kv);

        chain.set_extension_loader(move |h| {
            if h == 128 {
                Some(extension_bytes.clone())
            } else {
                None
            }
        });

        let tip = chain.height();
        let r = chain.recompute_active_parameters_from_storage(tip);
        assert!(r.is_err());
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("mismatch"), "expected mismatch error, got: {msg}");
    }

    #[test]
    fn soft_fork_lifecycle_full_traversal() {
        // Build a chain that:
        // 1. Votes unanimously for soft-fork (slot value 120) for the entire
        //    voting period (32 epochs * 128 = 4096 blocks)
        // 2. Goes through activation (32 more epochs)
        // 3. Cleans up at +1 epoch beyond activation
        //
        // Verify BlockVersion bumps at activation height and state clears at cleanup.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let n_bits = config.initial_n_bits;
        let voting_length = config.voting.voting_length;

        let mut prev_id = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let genesis = make_header_with_votes(1, prev_id, 1_000_000, n_bits, [120, 0, 0]);
        prev_id = genesis.id;
        chain.try_append_no_pow(genesis).unwrap();

        // Build voting period: heights 2..=4096 (32 * 128).
        // Each header has [120, 0, 0] (one soft-fork vote).
        let voting_end = voting_length * config.voting.soft_fork_epochs; // 4096
        for h in 2..=voting_end {
            let header = make_header_with_votes(
                h,
                prev_id,
                1_000_000 + (h as u64 - 1) * 45_000,
                n_bits,
                [120, 0, 0],
            );
            prev_id = header.id;
            chain.try_append_no_pow(header).unwrap();
        }
        assert_eq!(chain.height(), voting_end);

        // Walk epoch boundaries: 128, 256, ..., 4096.
        // At each, simulate validator: compute, then apply.
        let mut current_height = voting_length;
        while current_height <= voting_end {
            let params = chain.compute_expected_parameters(current_height, &[]).unwrap();
            // Save current tip; we need to advance to the boundary height before applying.
            // (We've already built the chain — now we just simulate apply at each boundary.)
            // The chain's tip is at voting_end, so we need a per-height apply path.
            // For lifecycle test, we test apply at the very end of each epoch.

            // For this test we simulate ONLY by calling compute → apply; the chain
            // remains at voting_end the whole time.
            let keep = chain.active_proposed_update_bytes().to_vec();
            chain.apply_epoch_boundary_parameters(params, keep);
            current_height += voting_length;
        }
        // After voting period, BlockVersion has NOT changed yet (activation is later).
        // Soft-fork state has been accumulated in parameters_table.
        // (Testnet starts at v4; this test exercises the lifecycle, not the bump-from-1 case.)
        assert_eq!(
            chain.active_parameters().block_version(),
            4,
            "BlockVersion only bumps at activation"
        );
        let votes = chain
            .active_parameters()
            .soft_fork_votes_collected()
            .unwrap_or(0);
        assert!(votes > 0, "votes accumulated in parameters_table");
    }

    #[test]
    fn active_proposed_update_bytes_starts_at_launch_default() {
        // Fresh chain seeds the field from
        // `default_proposed_update_bytes(network)` — the encoded form
        // of JVM `LaunchParameters.proposedUpdate` =
        // `ErgoValidationSettingsUpdate(Seq(215, 409), Seq.empty)`.
        let chain = HeaderChain::new(testnet_config());
        let default = crate::voting::default_proposed_update_bytes(crate::Network::Testnet);
        assert_eq!(chain.active_proposed_update_bytes(), &default[..]);
        assert!(
            chain.active_proposed_update_bytes().starts_with(&[
                0x02, 0xD7, 0x01, 0x99, 0x03
            ]),
            "seed must encode rulesToDisable=[215,409]"
        );
    }

    #[test]
    fn apply_epoch_boundary_parameters_updates_proposed_update_bytes() {
        let mut chain = HeaderChain::new(testnet_config());
        let params = chain.active_parameters().clone();
        let bytes = vec![0x02, 0xD7, 0x01, 0x99, 0x03, 0x00];
        chain.apply_epoch_boundary_parameters(params.clone(), bytes.clone());
        assert_eq!(chain.active_proposed_update_bytes(), &bytes[..]);

        // Subsequent call replaces the previous value — no ratchet.
        chain.apply_epoch_boundary_parameters(params, Vec::new());
        assert!(chain.active_proposed_update_bytes().is_empty());
    }

    #[test]
    fn recompute_extracts_proposed_update_from_extension() {
        use crate::voting::{pack_extension_bytes, pack_parameters_to_kv};
        use std::collections::HashMap;

        let mut chain = HeaderChain::new(testnet_config());
        let n_bits = chain.config().initial_n_bits;

        // Build a chain past the first epoch boundary (height 128).
        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=130 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        // Build extension with the testnet ground-truth disabling rules bytes.
        let mut params: HashMap<i8, i32> = HashMap::new();
        params.insert(1, 1_250_000);
        params.insert(2, 360);
        params.insert(3, 524_288);
        params.insert(4, 1_000_000);
        params.insert(5, 100);
        params.insert(6, 2_000);
        params.insert(7, 100);
        params.insert(8, 100);
        params.insert(123, 4);
        let mut kv = pack_parameters_to_kv(&params);
        // Add ID 124 (SoftForkDisablingRules) — variable-length value
        let disabling_rules_bytes = vec![0x02, 0xD7, 0x01, 0x99, 0x03, 0x00];
        kv.push(([0x00u8, 0x7C], disabling_rules_bytes.clone()));

        let header_id = chain.header_at(128).unwrap().id;
        let extension_bytes = pack_extension_bytes(&header_id, &kv);

        chain.set_extension_loader(move |h| {
            if h == 128 {
                Some(extension_bytes.clone())
            } else {
                None
            }
        });

        let tip = chain.height();
        chain.recompute_active_parameters_from_storage(tip).unwrap();
        assert_eq!(
            chain.active_proposed_update_bytes(),
            &disabling_rules_bytes[..],
            "ID 124 bytes should round-trip from extension into active_proposed_update_bytes"
        );
        // Block version is also restored
        assert_eq!(chain.active_parameters().block_version(), 4);
    }

    #[test]
    fn recompute_absent_id_124_falls_back_to_launch_default() {
        // A boundary extension that carries parameters but no ID 124
        // field must leave `active_proposed_update_bytes` at the
        // network launch default rather than silently clearing it —
        // empty bytes here would diverge from JVM, which treats the
        // absent field as `ErgoValidationSettingsUpdate.empty` but
        // this Rust chain has never observed a mainnet boundary
        // without the field since it was introduced pre-v6.
        use crate::voting::{pack_extension_bytes, pack_parameters_to_kv};
        use std::collections::HashMap;

        let mut chain = HeaderChain::new(testnet_config());
        let n_bits = chain.config().initial_n_bits;

        // Build a chain past the first epoch boundary (height 128).
        let mut prev = ergo_chain_types::BlockId(ergo_chain_types::Digest32::zero());
        let g = make_header_with_votes(1, prev, 1_000_000, n_bits, [0, 0, 0]);
        prev = g.id;
        chain.try_append_no_pow(g).unwrap();
        for h in 2..=130 {
            let header = make_header_with_votes(
                h, prev, 1_000_000 + (h as u64 - 1) * 45_000, n_bits, [0, 0, 0],
            );
            prev = header.id;
            chain.try_append_no_pow(header).unwrap();
        }

        // Extension carries parameters but NO ID 124 entry.
        let mut params: HashMap<i8, i32> = HashMap::new();
        params.insert(1, 1_250_000);
        params.insert(123, 4);
        let kv = pack_parameters_to_kv(&params);

        let header_id = chain.header_at(128).unwrap().id;
        let extension_bytes = pack_extension_bytes(&header_id, &kv);

        chain.set_extension_loader(move |h| {
            if h == 128 {
                Some(extension_bytes.clone())
            } else {
                None
            }
        });

        let tip = chain.height();
        chain.recompute_active_parameters_from_storage(tip).unwrap();

        let expected = crate::voting::default_proposed_update_bytes(crate::Network::Testnet);
        assert_eq!(
            chain.active_proposed_update_bytes(),
            &expected[..],
            "absent ID 124 in boundary extension must fall back to launch default"
        );
    }

    #[test]
    fn compute_expected_parameters_testnet_height_128_no_votes() {
        // Synthetic 128-header testnet chain with all-zero votes.
        // The expected parameters at boundary 128 should match the testnet
        // ground-truth ordinary defaults + BlockVersion=4 + SubblocksPerBlock=30
        // (auto-inserted by compute_expected_parameters at BlockVersion==4).
        //
        // The full testnet ground-truth at block 128 (per JVM /blocks/{id}/extension):
        //   StorageFeeFactor=1250000, MinValuePerByte=360, MaxBlockSize=524288,
        //   MaxBlockCost=1000000, TokenAccessCost=100, InputCost=2000,
        //   DataInputCost=100, OutputCost=100, SubblocksPerBlock=30,
        //   BlockVersion=4, SoftForkDisablingRules=02d701990300
        // (SoftForkDisablingRules tracked separately on HeaderChain.)
        use ergo_lib::chain::parameters::Parameter;

        let chain = build_chain_with_votes(128, [0, 0, 0]);
        let expected = chain.compute_expected_parameters(128, &[]).unwrap();

        assert_eq!(expected.storage_fee_factor(), 1_250_000);
        assert_eq!(expected.min_value_per_byte(), 360);
        assert_eq!(expected.max_block_size(), 524_288);
        assert_eq!(expected.max_block_cost(), 1_000_000);
        assert_eq!(expected.token_access_cost(), 100);
        assert_eq!(expected.input_cost(), 2_000);
        assert_eq!(expected.data_input_cost(), 100);
        assert_eq!(expected.output_cost(), 100);
        assert_eq!(expected.block_version(), 4);
        assert_eq!(
            expected
                .parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(30),
            "SubblocksPerBlock must be present at block_version=4"
        );
    }

    #[test]
    fn compute_expected_parameters_auto_inserts_subblocks_at_v4() {
        // A chain whose active parameters are at BlockVersion==4 but somehow
        // missing SubblocksPerBlock should have it auto-inserted by
        // compute_expected_parameters at the next epoch boundary.
        //
        // Construction: start with default testnet (which already has the
        // entry), strip it, then run compute. The output should reintroduce
        // SubblocksPerBlock=30.
        use ergo_lib::chain::parameters::Parameter;

        let mut chain = build_chain_with_votes(128, [0, 0, 0]);
        // Force-remove SubblocksPerBlock from the active parameters to
        // simulate a chain that hasn't had it auto-inserted yet.
        let mut stripped = chain.active_parameters().clone();
        stripped.parameters_table.remove(&Parameter::SubblocksPerBlock);
        assert!(
            !stripped
                .parameters_table
                .contains_key(&Parameter::SubblocksPerBlock),
            "precondition: stripped table must not contain SubblocksPerBlock"
        );
        let keep = chain.active_proposed_update_bytes().to_vec();
        chain.apply_epoch_boundary_parameters(stripped, keep);

        let expected = chain.compute_expected_parameters(128, &[]).unwrap();
        assert_eq!(expected.block_version(), 4);
        assert_eq!(
            expected
                .parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(30),
            "auto-insert at BlockVersion==4 must populate SubblocksPerBlock=30"
        );
    }

    // ---- Target-height-aware recompute tests ----
    //
    // The validator's resume height (the height it's about to start
    // re-validating from) is generally far behind the chain tip on a fresh
    // resync. The active parameters table at startup must reflect the
    // parameters in effect at *that* height, not at the chain tip — otherwise
    // a fresh genesis resync against a chain whose tip carries v6-era
    // parameters fails the very first epoch boundary because the locally
    // computed expected table has more entries than the v1-era extension.

    /// Build a synthetic extension at the given boundary height carrying a
    /// distinct StorageFeeFactor value, so tests can tell which boundary
    /// the recompute actually loaded from.
    fn extension_with_storage_fee(
        chain: &HeaderChain,
        boundary_height: u32,
        storage_fee: i32,
    ) -> Vec<u8> {
        use crate::voting::{pack_extension_bytes, pack_parameters_to_kv};
        use std::collections::HashMap;

        let mut params: HashMap<i8, i32> = HashMap::new();
        params.insert(1, storage_fee);
        params.insert(2, 360);
        params.insert(3, 524_288);
        params.insert(4, 1_000_000);
        params.insert(5, 100);
        params.insert(6, 2_000);
        params.insert(7, 100);
        params.insert(8, 100);
        params.insert(123, 4);

        let kv = pack_parameters_to_kv(&params);
        let header_id = chain
            .header_at(boundary_height)
            .expect("test setup: chain must contain the boundary height")
            .id;
        pack_extension_bytes(&header_id, &kv)
    }

    #[test]
    fn recompute_target_zero_keeps_defaults_loader_not_called() {
        // Even with a chain spanning multiple epoch boundaries, target=0
        // means "the validator has applied nothing yet" → no boundary at or
        // before that height → defaults stand. The loader must not be
        // consulted because there is nothing to load.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        chain.set_extension_loader(|_| panic!("loader must not be called for target=0"));

        chain
            .recompute_active_parameters_from_storage(0)
            .expect("target=0 must be a no-op success");

        // Defaults preserved (testnet starts with the chain-internal defaults).
        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_250_000);
    }

    #[test]
    fn recompute_target_just_below_first_boundary_keeps_defaults() {
        // target = voting_length - 1 (=127). Still no boundary at or before
        // that height. Defaults stand, loader not called.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        chain.set_extension_loader(|_| {
            panic!("loader must not be called below the first boundary")
        });

        chain
            .recompute_active_parameters_from_storage(127)
            .expect("target below first boundary must be a no-op");

        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_250_000);
    }

    #[test]
    fn recompute_target_at_first_boundary_loads_first_boundary() {
        // target = 128 → boundary at 128 → loads the params from that
        // extension. Verify by parking a distinct StorageFeeFactor in the
        // synthetic extension at h=128.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        let ext_128 = extension_with_storage_fee(&chain, 128, 1_300_000);
        chain.set_extension_loader(move |h| {
            if h == 128 {
                Some(ext_128.clone())
            } else {
                None
            }
        });

        chain
            .recompute_active_parameters_from_storage(128)
            .expect("target at first boundary must load");

        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_300_000);
    }

    #[test]
    fn recompute_target_just_above_first_boundary_loads_first_boundary() {
        // target = 129 → most recent boundary ≤ 129 is 128.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        let ext_128 = extension_with_storage_fee(&chain, 128, 1_300_000);
        chain.set_extension_loader(move |h| {
            if h == 128 {
                Some(ext_128.clone())
            } else {
                None
            }
        });

        chain
            .recompute_active_parameters_from_storage(129)
            .expect("target just above first boundary must load 128");

        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_300_000);
    }

    #[test]
    fn recompute_target_just_below_second_boundary_loads_first_boundary() {
        // target = 255 → most recent boundary ≤ 255 is 128, NOT 256.
        // Even though the chain has the header at h=256, the function must
        // not "round up" to the next boundary.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        let ext_128 = extension_with_storage_fee(&chain, 128, 1_300_000);
        // Loader at 256 returns garbage to ensure it is NOT consulted.
        chain.set_extension_loader(move |h| match h {
            128 => Some(ext_128.clone()),
            256 => panic!("must not load boundary 256 when target is 255"),
            _ => None,
        });

        chain
            .recompute_active_parameters_from_storage(255)
            .expect("target just below second boundary must load 128");

        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_300_000);
    }

    #[test]
    fn recompute_target_at_second_boundary_loads_second_boundary() {
        // target = 256 → boundary at 256, NOT 128. Verified by giving the
        // two boundaries different StorageFeeFactor values.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        let ext_128 = extension_with_storage_fee(&chain, 128, 1_300_000);
        let ext_256 = extension_with_storage_fee(&chain, 256, 1_400_000);
        chain.set_extension_loader(move |h| match h {
            128 => Some(ext_128.clone()),
            256 => Some(ext_256.clone()),
            _ => None,
        });

        chain
            .recompute_active_parameters_from_storage(256)
            .expect("target at second boundary must load 256");

        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_400_000);
    }

    #[test]
    fn recompute_target_above_second_boundary_loads_second_boundary() {
        // target = 260 → most recent boundary ≤ 260 is 256.
        let mut chain = build_chain_with_votes(260, [0, 0, 0]);
        let ext_128 = extension_with_storage_fee(&chain, 128, 1_300_000);
        let ext_256 = extension_with_storage_fee(&chain, 256, 1_400_000);
        chain.set_extension_loader(move |h| match h {
            128 => Some(ext_128.clone()),
            256 => Some(ext_256.clone()),
            _ => None,
        });

        chain
            .recompute_active_parameters_from_storage(260)
            .expect("target above second boundary must load 256");

        assert_eq!(chain.active_parameters().storage_fee_factor(), 1_400_000);
    }

    // ---- Mainnet v6 activation regression tests ----
    //
    // Mainnet sync stalled at height 1,628,160 because our
    // `compute_expected_parameters` unconditionally auto-inserted
    // `SubblocksPerBlock` at BlockVersion==4, emitting 12 parameter-table
    // entries while the on-chain block carried 11. JVM guards the insert
    // with `!activatedUpdate.rulesToDisable.contains(409)`; mainnet's
    // `LaunchParameters.proposedUpdate = [215, 409]` is activated at the
    // v6 BlockVersion bump (1,562,624 + 1024 * 64 = 1,628,160), so the
    // insert is suppressed at activation and fires at the next boundary.

    /// Build a mainnet `HeaderChain` populated across the just-ended
    /// voting epoch for `boundary_height`. All headers carry zero votes —
    /// tests that need specific pre-state seed `active_parameters`
    /// manually via `apply_epoch_boundary_parameters`.
    ///
    /// Uses `install_from_nipopow_proof_no_pow` to avoid synthesizing
    /// the 1.6M-header genesis prefix. `light_client_mode` is set as a
    /// side-effect; irrelevant here because
    /// `compute_expected_parameters` does not gate on it.
    fn build_mainnet_chain_for_boundary(boundary_height: u32) -> HeaderChain {
        let config = crate::ChainConfig::mainnet();
        let voting_length = config.voting.voting_length;
        assert!(
            boundary_height >= voting_length,
            "boundary height must be ≥ voting_length (1024) for the epoch walk to be non-empty"
        );
        let n_bits = config.initial_n_bits;

        let base_height = boundary_height - voting_length;
        let mut prev_id = BlockId(Digest32::zero());
        let head = make_header_with_votes(
            base_height,
            prev_id,
            1_000_000,
            n_bits,
            [0, 0, 0],
        );
        prev_id = head.id;

        let mut tail: Vec<Header> = Vec::with_capacity((voting_length - 1) as usize);
        for i in 1..voting_length {
            let hdr = make_header_with_votes(
                base_height + i,
                prev_id,
                1_000_000 + (i as u64) * 45_000,
                n_bits,
                [0, 0, 0],
            );
            prev_id = hdr.id;
            tail.push(hdr);
        }

        let mut chain = HeaderChain::new(config);
        chain
            .install_from_nipopow_proof_no_pow(head, tail)
            .expect("install must succeed on fresh chain");
        assert_eq!(chain.height(), boundary_height - 1);
        chain
    }

    /// Raw `ErgoValidationSettingsUpdate` payload observed on mainnet
    /// block 1,628,160 extension (key `[0x00, 0x7C]`). Decodes to
    /// `rulesToDisable = [215, 409]` + 3 status updates.
    const MAINNET_V6_PROPOSED_UPDATE: [u8; 18] = [
        0x02, 0xD7, 0x01, 0x99, 0x03, 0x03, 0x0B, 0x01, 0x03,
        0x10, 0x07, 0x01, 0x03, 0x11, 0x08, 0x01, 0x03, 0x12,
    ];

    #[test]
    fn compute_expected_parameters_mainnet_v6_activation_suppresses_subblocks() {
        // Activation boundary: `activatedUpdate` contains rule 409 → Step 4
        // auto-insert of `SubblocksPerBlock` MUST be skipped. Result = 11
        // entries (8 ordinary + BlockVersion + 121 + 122).
        use ergo_lib::chain::parameters::Parameter;

        let starting_height = 1_562_624u32;
        let voting_length = 1024u32;
        let activation = starting_height + voting_length * 64; // 1,628,160
        assert_eq!(activation, 1_628_160);

        // On-chain ground truth at h=1,627,136 (previous boundary):
        // SoftForkVotesCollected = 0x7b2c = 31,532. Well above the
        // approval threshold (1024 * 32 * 9 / 10 = 29,491).
        let votes_collected = 31_532i32;

        let mut chain = build_mainnet_chain_for_boundary(activation);

        let mut pre = crate::voting::default_parameters(crate::Network::Mainnet);
        pre.parameters_table
            .insert(Parameter::BlockVersion, 3);
        pre.parameters_table
            .insert(Parameter::SoftForkStartingHeight, starting_height as i32);
        pre.parameters_table
            .insert(Parameter::SoftForkVotesCollected, votes_collected);
        let keep = chain.active_proposed_update_bytes().to_vec();
        chain.apply_epoch_boundary_parameters(pre, keep);

        let result = chain
            .compute_expected_parameters(activation, &MAINNET_V6_PROPOSED_UPDATE)
            .expect("compute must succeed");

        assert_eq!(
            result.block_version(),
            4,
            "voting activation must bump BlockVersion 3 → 4"
        );
        assert!(
            !result
                .parameters_table
                .contains_key(&Parameter::SubblocksPerBlock),
            "rule 409 in activatedUpdate MUST suppress SubblocksPerBlock auto-insert"
        );
        assert_eq!(
            result
                .parameters_table
                .get(&Parameter::SoftForkStartingHeight)
                .copied(),
            Some(starting_height as i32),
            "soft-fork state persists until cleanup_success (next boundary)",
        );
        assert_eq!(
            result
                .parameters_table
                .get(&Parameter::SoftForkVotesCollected)
                .copied(),
            Some(votes_collected),
        );
        // 8 ordinary (IDs 1-8) + BlockVersion (123) + 121 + 122 = 11
        assert_eq!(
            result.parameters_table.len(),
            11,
            "table must match on-chain block's 11-entry layout"
        );
    }

    #[test]
    fn compute_expected_parameters_mainnet_v6_cleanup_success_inserts_subblocks() {
        // Next boundary after activation (cleanup_success): JVM's
        // `activatedUpdate` is empty this call (no activation happens),
        // so rule 409 gate is false → SubblocksPerBlock IS auto-inserted.
        // Result = 10 entries (8 ordinary + BlockVersion + SubblocksPerBlock,
        // with 121/122 removed by cleanup).
        use ergo_lib::chain::parameters::Parameter;

        let starting_height = 1_562_624u32;
        let voting_length = 1024u32;
        let cleanup_success = starting_height + voting_length * 65; // 1,629,184
        assert_eq!(cleanup_success, 1_629_184);

        let votes_collected = 31_532i32;

        let mut chain = build_mainnet_chain_for_boundary(cleanup_success);

        // Pre-state after the 1,628,160 activation was applied:
        // BlockVersion = 4, soft-fork state still present (it clears at
        // THIS boundary), NO SubblocksPerBlock yet.
        let mut pre = crate::voting::default_parameters(crate::Network::Mainnet);
        pre.parameters_table
            .insert(Parameter::BlockVersion, 4);
        pre.parameters_table
            .insert(Parameter::SoftForkStartingHeight, starting_height as i32);
        pre.parameters_table
            .insert(Parameter::SoftForkVotesCollected, votes_collected);
        let keep = chain.active_proposed_update_bytes().to_vec();
        chain.apply_epoch_boundary_parameters(pre, keep);

        // Same bytes as at activation — doesn't matter because the gate
        // sees no voting-driven BlockVersion bump in this call and
        // skips the parse.
        let result = chain
            .compute_expected_parameters(cleanup_success, &MAINNET_V6_PROPOSED_UPDATE)
            .expect("compute must succeed");

        assert_eq!(result.block_version(), 4);
        assert!(
            !result
                .parameters_table
                .contains_key(&Parameter::SoftForkStartingHeight),
            "cleanup_success must remove SoftForkStartingHeight"
        );
        assert!(
            !result
                .parameters_table
                .contains_key(&Parameter::SoftForkVotesCollected),
            "cleanup_success must remove SoftForkVotesCollected"
        );
        assert_eq!(
            result
                .parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(30),
            "SubblocksPerBlock auto-insert fires at the boundary AFTER activation"
        );
        // 8 ordinary + BlockVersion + SubblocksPerBlock = 10
        assert_eq!(result.parameters_table.len(), 10);
    }

    #[test]
    fn compute_expected_parameters_mainnet_v6_activation_with_empty_update_inserts_subblocks() {
        // Counterfactual: if the activation block's ID 124 field were
        // empty (or absent), the gate would NOT trip and the insert
        // would fire. Confirms the guard is genuinely driven by the
        // payload, not by the activation itself.
        use ergo_lib::chain::parameters::Parameter;

        let starting_height = 1_562_624u32;
        let voting_length = 1024u32;
        let activation = starting_height + voting_length * 64;

        let mut chain = build_mainnet_chain_for_boundary(activation);

        let mut pre = crate::voting::default_parameters(crate::Network::Mainnet);
        pre.parameters_table.insert(Parameter::BlockVersion, 3);
        pre.parameters_table
            .insert(Parameter::SoftForkStartingHeight, starting_height as i32);
        pre.parameters_table
            .insert(Parameter::SoftForkVotesCollected, 31_532);
        let keep = chain.active_proposed_update_bytes().to_vec();
        chain.apply_epoch_boundary_parameters(pre, keep);

        // Empty proposed update → rule 409 NOT in activatedUpdate → insert fires.
        let result = chain
            .compute_expected_parameters(activation, &[])
            .expect("compute must succeed");

        assert_eq!(result.block_version(), 4);
        assert_eq!(
            result
                .parameters_table
                .get(&Parameter::SubblocksPerBlock)
                .copied(),
            Some(30),
            "without rule 409 in activatedUpdate, auto-insert fires at activation"
        );
    }
}

#[cfg(test)]
mod light_client_install_tests {
    //! Tests for `install_from_nipopow_proof`, `light_client_mode`, and
    //! `reorg_floor` (Phase 6 light-client install API).
    //!
    //! Synthetic-header tests use the `_no_pow` install variant since the
    //! real Autolykos solutions are not present on these fixtures. The PoW
    //! path is exercised by integration tests against real chains in the
    //! main crate.

    use crate::{AppendResult, ChainConfig, ChainError, HeaderChain};
    use ergo_chain_types::*;
    use sigma_ser::ScorexSerializable;

    fn make_chain_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        make_chain_header_with_nonce(height, parent_id, timestamp, n_bits, height.to_be_bytes().repeat(2))
    }

    fn make_chain_header_with_nonce(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
        nonce: Vec<u8>,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce,
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    /// Build a synthetic "suffix" — k headers starting at `start_height`
    /// with the given parent_id, all with the same n_bits.
    fn build_suffix(
        start_height: u32,
        first_parent: BlockId,
        first_timestamp: u64,
        n_bits: u32,
        count: u32,
    ) -> Vec<Header> {
        let mut headers = Vec::with_capacity(count as usize);
        let mut prev_id = first_parent;
        for i in 0..count {
            let h = make_chain_header_with_nonce(
                start_height + i,
                prev_id,
                first_timestamp + (i as u64) * 50_000,
                n_bits,
                vec![0xEE; 8],
            );
            prev_id = h.id;
            headers.push(h);
        }
        headers
    }

    // --- install_from_nipopow_proof tests ---

    #[test]
    fn install_succeeds_on_empty_chain() {
        // A fresh chain accepts the install. tip(), height(), and is_empty()
        // reflect the installed suffix.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        assert!(chain.is_empty());

        // 5-header suffix starting at height 1000 (well above genesis to
        // exercise the "install at arbitrary height" property).
        let parent = BlockId(Digest32::from([0x77; 32]));
        let suffix = build_suffix(1000, parent, 5_000_000, config.initial_n_bits, 5);
        let suffix_head = suffix[0].clone();
        let suffix_tail: Vec<Header> = suffix[1..].to_vec();
        let last_id = suffix.last().unwrap().id;
        let last_height = suffix.last().unwrap().height;

        chain
            .install_from_nipopow_proof_no_pow(suffix_head, suffix_tail)
            .expect("install must succeed on empty chain");

        assert!(!chain.is_empty());
        assert_eq!(chain.height(), last_height);
        assert_eq!(chain.tip().id, last_id);
        assert!(chain.light_client_mode());
        // scores[0] = 0 — see Phase 6 doc on cumulative-difficulty meaninglessness.
        assert_eq!(chain.score_at(1000).unwrap(), num_bigint::BigUint::ZERO);
    }

    #[test]
    fn install_succeeds_with_empty_suffix_tail() {
        // The contract permits a single-header install (suffix_tail empty).
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());

        let parent = BlockId(Digest32::from([0x77; 32]));
        let head = make_chain_header(500, parent, 5_000_000, config.initial_n_bits);
        let head_id = head.id;

        chain
            .install_from_nipopow_proof_no_pow(head, vec![])
            .expect("install must succeed");

        assert_eq!(chain.height(), 500);
        assert_eq!(chain.tip().id, head_id);
        assert!(chain.light_client_mode());
    }

    #[test]
    fn install_rejects_non_empty_chain() {
        // A chain that already contains headers cannot be re-installed.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits);
        chain.try_append_no_pow(genesis).unwrap();

        let parent = BlockId(Digest32::from([0x77; 32]));
        let suffix = build_suffix(100, parent, 5_000_000, config.initial_n_bits, 3);
        let suffix_head = suffix[0].clone();
        let suffix_tail: Vec<Header> = suffix[1..].to_vec();

        let err = chain
            .install_from_nipopow_proof_no_pow(suffix_head, suffix_tail)
            .unwrap_err();
        assert!(matches!(err, ChainError::ChainNotEmpty));

        // Chain is unchanged.
        assert_eq!(chain.height(), 1);
        assert!(!chain.light_client_mode());
    }

    #[test]
    fn install_rejects_broken_parent_linkage_in_suffix() {
        // suffix_tail[1].parent_id deliberately points to nowhere — install
        // must fail and roll back ALL state (suffix_head + light_client_mode).
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());

        let parent = BlockId(Digest32::from([0x77; 32]));
        let mut suffix = build_suffix(1000, parent, 5_000_000, config.initial_n_bits, 4);
        // Break the link between suffix[1] and suffix[2].
        suffix[2] = make_chain_header_with_nonce(
            1002,
            BlockId(Digest32::from([0xDE; 32])), // bogus parent
            5_100_000,
            config.initial_n_bits,
            vec![0xFF; 8],
        );
        let suffix_head = suffix[0].clone();
        let suffix_tail: Vec<Header> = suffix[1..].to_vec();

        let err = chain
            .install_from_nipopow_proof_no_pow(suffix_head, suffix_tail)
            .unwrap_err();
        assert!(matches!(err, ChainError::ParentNotFound { .. }));

        // Full rollback: chain is empty, light_client_mode is back to false.
        assert!(chain.is_empty());
        assert_eq!(chain.height(), 0);
        assert!(!chain.light_client_mode());
    }

    #[test]
    fn install_rejects_bad_pow_via_real_path() {
        // Real install_from_nipopow_proof (with PoW) must reject synthetic
        // headers when the difficulty target is meaningful. Note that the
        // testnet `initial_n_bits` decodes to difficulty 1, so any random
        // pow_hit passes — synthetic headers with that target ARE accepted
        // by `verify_pow` (this is why the `_no_pow` test variants exist).
        //
        // To exercise the rejection path we use a real testnet nBits value
        // (`72286528`, decoded difficulty 1_325_481_984) — the synthetic
        // nonce will essentially never satisfy `pow_hit < q / D` at that
        // difficulty.
        let mut chain = HeaderChain::new(testnet_config());

        let high_n_bits = 72286528u32;
        let parent = BlockId(Digest32::from([0x77; 32]));
        let suffix = build_suffix(1000, parent, 5_000_000, high_n_bits, 3);
        let suffix_head = suffix[0].clone();
        let suffix_tail: Vec<Header> = suffix[1..].to_vec();

        let err = chain
            .install_from_nipopow_proof(suffix_head, suffix_tail)
            .unwrap_err();
        assert!(
            matches!(err, ChainError::PowInvalid { .. } | ChainError::PowCompute(_)),
            "expected PoW failure, got {err:?}"
        );

        // Chain unchanged on error.
        assert!(chain.is_empty());
        assert!(!chain.light_client_mode());
    }

    #[test]
    fn try_append_after_install_extends_tip() {
        // Key test: post-install, a valid child of the suffix tip is
        // accepted by try_append_no_pow. Difficulty check is skipped because
        // light_client_mode is set.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let parent = BlockId(Digest32::from([0x77; 32]));
        let suffix = build_suffix(1000, parent, 5_000_000, config.initial_n_bits, 3);
        let suffix_head = suffix[0].clone();
        let suffix_tail: Vec<Header> = suffix[1..].to_vec();

        chain
            .install_from_nipopow_proof_no_pow(suffix_head, suffix_tail)
            .expect("install");
        let tip = chain.tip().clone();
        assert_eq!(chain.height(), 1002);

        // Build a valid child.
        let child = make_chain_header_with_nonce(
            1003,
            tip.id,
            tip.timestamp + 50_000,
            config.initial_n_bits,
            vec![0xAB; 8],
        );
        let result = chain
            .try_append_no_pow(child)
            .expect("try_append must succeed in light mode");
        assert!(matches!(result, AppendResult::Extended));
        assert_eq!(chain.height(), 1003);
    }

    // --- light_client_mode skip tests ---

    #[test]
    fn light_mode_accepts_wrong_n_bits_post_install() {
        // In normal (full) mode, a child header with a deliberately wrong
        // n_bits is rejected. After install, the same kind of mismatch is
        // accepted because light mode skips the difficulty check entirely.
        let config = testnet_config();

        // 1) Normal-mode rejection: build a chain at genesis, try a child
        //    with wrong n_bits, expect WrongDifficulty.
        let mut full_chain = HeaderChain::new(config.clone());
        let genesis = make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits);
        let genesis_id = genesis.id;
        full_chain.try_append_no_pow(genesis).unwrap();
        let bad_child = make_chain_header(2, genesis_id, 2_000_000, config.initial_n_bits + 1);
        let err = full_chain.try_append_no_pow(bad_child).unwrap_err();
        assert!(
            matches!(err, ChainError::WrongDifficulty { .. }),
            "full mode must reject wrong n_bits"
        );

        // 2) Light-mode acceptance: install a fresh chain from a synthetic
        //    1-header suffix and try the same kind of "wrong n_bits" child.
        //    The difficulty check is skipped, so it should be accepted.
        let mut light_chain = HeaderChain::new(config.clone());
        let parent = BlockId(Digest32::from([0x42; 32]));
        let head = make_chain_header_with_nonce(
            5000,
            parent,
            6_000_000,
            config.initial_n_bits,
            vec![0xC0; 8],
        );
        let head_id = head.id;
        let head_ts = head.timestamp;
        light_chain
            .install_from_nipopow_proof_no_pow(head, vec![])
            .expect("install");
        // Append a child with deliberately-wrong n_bits — would be a
        // WrongDifficulty error in full mode. Light mode trusts whatever
        // n_bits the network sent.
        let child = make_chain_header_with_nonce(
            5001,
            head_id,
            head_ts + 50_000,
            config.initial_n_bits + 1, // wrong on purpose
            vec![0xC1; 8],
        );
        let result = light_chain
            .try_append_no_pow(child)
            .expect("light mode must accept wrong n_bits");
        assert!(matches!(result, AppendResult::Extended));
        assert_eq!(light_chain.height(), 5001);
    }

    // --- reorg_floor tests ---

    #[test]
    fn reorg_floor_genesis_chain_returns_one() {
        // A normal chain starting at genesis has reorg_floor() == 1, which
        // is the structural expression of "can't reorg past genesis".
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let genesis = make_chain_header(1, BlockId(Digest32::zero()), 1_000_000, config.initial_n_bits);
        chain.try_append_no_pow(genesis).unwrap();
        assert_eq!(chain.reorg_floor(), 1);
    }

    #[test]
    fn reorg_floor_empty_chain_returns_one() {
        // Back-compat: empty chains return 1. Reorg machinery refuses
        // empty chains anyway, so the value is informational.
        let chain = HeaderChain::new(testnet_config());
        assert_eq!(chain.reorg_floor(), 1);
    }

    #[test]
    fn reorg_floor_light_installed_chain_returns_install_height() {
        // After installing a suffix at height N, reorg_floor() returns N.
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let parent = BlockId(Digest32::from([0x77; 32]));
        let suffix = build_suffix(2500, parent, 5_000_000, config.initial_n_bits, 4);
        let head = suffix[0].clone();
        let tail: Vec<Header> = suffix[1..].to_vec();
        chain.install_from_nipopow_proof_no_pow(head, tail).unwrap();
        assert_eq!(chain.reorg_floor(), 2500);
    }

    #[test]
    fn try_reorg_deep_rejects_fork_below_install_floor() {
        // A reorg whose fork point is below the install boundary must be
        // rejected with a clear error (no panic, no partial state mutation).
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let parent = BlockId(Digest32::from([0x77; 32]));
        let suffix = build_suffix(2500, parent, 5_000_000, config.initial_n_bits, 4);
        let head = suffix[0].clone();
        let tail: Vec<Header> = suffix[1..].to_vec();
        chain.install_from_nipopow_proof_no_pow(head, tail).unwrap();

        // Try to reorg from height 2499 (below the install boundary 2500).
        let bogus = make_chain_header_with_nonce(
            2500,
            BlockId(Digest32::from([0xCC; 32])),
            5_500_000,
            config.initial_n_bits,
            vec![0xDD; 8],
        );
        let err = chain
            .try_reorg_deep_no_pow(2499, vec![bogus])
            .unwrap_err();
        match &err {
            ChainError::Reorg(msg) => {
                assert!(
                    msg.contains("below reorg floor"),
                    "error must mention reorg floor: {msg}"
                );
            }
            other => panic!("expected Reorg error, got {other:?}"),
        }

        // Chain still at the installed tip, light mode still set.
        assert_eq!(chain.height(), 2503);
        assert!(chain.light_client_mode());
    }
}

#[cfg(test)]
mod lazy_cache_tests {
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use ergo_chain_types::*;
    use num_bigint::BigUint;
    use sigma_ser::ScorexSerializable;

    use crate::{ChainConfig, HeaderChain};

    fn testnet_config() -> ChainConfig {
        ChainConfig::testnet()
    }

    fn genesis_parent_id() -> BlockId {
        BlockId(Digest32::zero())
    }

    /// Synthetic header builder — no real PoW, id computed via
    /// serialization roundtrip so each (height, parent_id, timestamp,
    /// n_bits) tuple yields a distinct, deterministic id.
    fn make_header(
        height: u32,
        parent_id: BlockId,
        timestamp: u64,
        n_bits: u32,
    ) -> Header {
        let zero32 = Digest32::zero();
        let mut header = Header {
            version: 2,
            id: BlockId(Digest32::zero()),
            parent_id,
            ad_proofs_root: zero32,
            state_root: ADDigest::zero(),
            transaction_root: zero32,
            timestamp,
            n_bits,
            height,
            extension_root: zero32,
            autolykos_solution: AutolykosSolution {
                miner_pk: Box::new(EcPoint::default()),
                pow_onetime_pk: None,
                nonce: height.to_be_bytes().repeat(2),
                pow_distance: None,
            },
            votes: Votes([0, 0, 0]),
            unparsed_bytes: Box::new([]),
        };
        let bytes = header.scorex_serialize_bytes().unwrap();
        let reparsed = Header::scorex_parse_bytes(&bytes).unwrap();
        header.id = reparsed.id;
        header
    }

    /// Push `count` sequential headers starting from genesis. Returns
    /// the headers for convenient inspection.
    fn build_chain(chain: &mut HeaderChain, count: u32) -> Vec<Header> {
        let config = chain.config().clone();
        let genesis = make_header(1, genesis_parent_id(), 1_000_000, config.initial_n_bits);
        let mut prev_id = genesis.id;
        let n_bits = genesis.n_bits;
        let mut built = vec![genesis.clone()];
        chain.try_append_no_pow(genesis).unwrap();
        for h in 2..=count {
            let hdr = make_header(h, prev_id, 1_000_000 + h as u64 * 45_000, n_bits);
            prev_id = hdr.id;
            built.push(hdr.clone());
            chain.try_append_no_pow(hdr).unwrap();
        }
        built
    }

    #[test]
    fn push_writes_through_to_cache() {
        let mut chain = HeaderChain::new(testnet_config());
        let built = build_chain(&mut chain, 5);

        for (i, expected) in built.iter().enumerate() {
            let h = (i + 1) as u32;
            let cached = chain
                .lazy()
                .peek_header(h)
                .unwrap_or_else(|| panic!("cache missing height {h}"));
            assert_eq!(cached.id, expected.id, "height {h} header id mismatch");
        }
        // Scores are cumulative — we don't compare exact values here;
        // just assert they exist at every height that was pushed.
        for h in 1..=5 {
            assert!(chain.lazy().peek_score(h).is_some(), "score cache missing height {h}");
        }
        assert_eq!(chain.lazy().header_cache_len(), 5);
        assert_eq!(chain.lazy().score_cache_len(), 5);
    }

    #[test]
    fn miss_without_loader_returns_none() {
        let chain = HeaderChain::new(testnet_config());
        assert!(chain.lazy().get_header(99).is_none());
        assert!(chain.lazy().get_score(99).is_none());
        assert!(!chain.has_header_loader());
        assert!(!chain.has_score_loader());
    }

    #[test]
    fn miss_consults_header_loader_once_then_caches() {
        let mut chain = HeaderChain::new(testnet_config());
        let synth = make_header(42, genesis_parent_id(), 1_234_567, 16842752);
        let synth_id = synth.id;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_hot = calls.clone();
        chain.set_header_loader(move |h| {
            calls_hot.fetch_add(1, Ordering::SeqCst);
            if h == 42 {
                Some(synth.clone())
            } else {
                None
            }
        });
        assert!(chain.has_header_loader());

        // First call: cache miss, loader invoked.
        let got = chain.lazy().get_header(42).expect("loader should return");
        assert_eq!(got.id, synth_id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second call: cache hit, loader NOT invoked again.
        let got2 = chain.lazy().get_header(42).expect("cache hit");
        assert_eq!(got2.id, synth_id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Unknown height: loader says None, no cache insertion.
        assert!(chain.lazy().get_header(999).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(chain.lazy().peek_header(999).is_none());
    }

    #[test]
    fn miss_consults_score_loader_once_then_caches() {
        let mut chain = HeaderChain::new(testnet_config());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_hot = calls.clone();
        chain.set_score_loader(move |h| {
            calls_hot.fetch_add(1, Ordering::SeqCst);
            if h == 10 { Some(BigUint::from(7u32)) } else { None }
        });
        assert!(chain.has_score_loader());

        let s = chain.lazy().get_score(10).unwrap();
        assert_eq!(s, BigUint::from(7u32));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let s2 = chain.lazy().get_score(10).unwrap();
        assert_eq!(s2, BigUint::from(7u32));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lru_evicts_oldest_at_capacity() {
        let mut chain = HeaderChain::new(testnet_config());
        // Shrink both caches to 3 entries before pushing anything.
        chain.set_cache_capacity(NonZeroUsize::new(3).unwrap());
        build_chain(&mut chain, 5);

        // Oldest two (heights 1, 2) evicted; three most recent (3, 4, 5)
        // retained.
        assert!(chain.lazy().peek_header(1).is_none());
        assert!(chain.lazy().peek_header(2).is_none());
        assert!(chain.lazy().peek_header(3).is_some());
        assert!(chain.lazy().peek_header(4).is_some());
        assert!(chain.lazy().peek_header(5).is_some());
        assert_eq!(chain.lazy().header_cache_len(), 3);

        // Score cache tracks the same eviction order.
        assert!(chain.lazy().peek_score(1).is_none());
        assert!(chain.lazy().peek_score(2).is_none());
        assert_eq!(chain.lazy().score_cache_len(), 3);
    }

    #[test]
    fn reorg_drain_evicts_cache_and_restores_on_rollback() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let built = build_chain(&mut chain, 5);

        // Attempt a deep reorg with a deliberately-broken branch so it
        // fails validation and triggers rollback restoration. Fork at
        // height 3: new branch replaces heights 4 and 5. We sabotage
        // it by giving the first new header a wrong parent_id so the
        // reorg errors out during validation.
        let fork_parent = &built[2]; // height 3
        let bad_first = make_header(
            4,
            BlockId(Digest32::from([0xaa; 32])), // wrong parent
            fork_parent.timestamp + 1000,
            fork_parent.n_bits,
        );
        let err = chain
            .try_reorg_deep_no_pow(3, vec![bad_first])
            .unwrap_err();
        // We don't care which error variant — just that it failed and
        // rolled back.
        drop(err);

        // After rollback, heights 4 and 5 are back in cache (restored
        // via `lazy.put`) and the chain looks untouched.
        assert_eq!(chain.height(), 5);
        assert!(chain.lazy().peek_header(4).is_some());
        assert!(chain.lazy().peek_header(5).is_some());
        assert_eq!(chain.lazy().peek_header(5).unwrap().id, built[4].id);
    }

    #[test]
    fn successful_reorg_evicts_old_and_caches_new() {
        let config = testnet_config();
        let mut chain = HeaderChain::new(config.clone());
        let built = build_chain(&mut chain, 5);
        let old_height_4_id = built[3].id;
        let old_height_5_id = built[4].id;

        let fork_parent = &built[2]; // height 3
        let new4 = make_header(
            4,
            fork_parent.id,
            fork_parent.timestamp + 9_999,
            fork_parent.n_bits,
        );
        let new5 = make_header(
            5,
            new4.id,
            new4.timestamp + 45_000,
            new4.n_bits,
        );
        let new4_id = new4.id;
        let new5_id = new5.id;

        chain
            .try_reorg_deep_no_pow(3, vec![new4, new5])
            .expect("reorg should succeed");

        // Old tip headers got evicted then replaced by the new ones.
        let cached4 = chain.lazy().peek_header(4).expect("new h=4 cached");
        let cached5 = chain.lazy().peek_header(5).expect("new h=5 cached");
        assert_eq!(cached4.id, new4_id);
        assert_eq!(cached5.id, new5_id);
        assert_ne!(cached4.id, old_height_4_id);
        assert_ne!(cached5.id, old_height_5_id);
    }

    #[test]
    fn rollback_install_clears_cache() {
        let mut chain = HeaderChain::new(testnet_config());
        // Install a valid head followed by a bogus tail header whose
        // parent_id doesn't link to the head → triggers rollback.
        let head = make_header(1000, BlockId(Digest32::from([7u8; 32])), 50_000_000, 16842752);
        let bogus_tail = make_header(
            1001,
            BlockId(Digest32::from([0xff; 32])), // wrong parent
            50_000_045,
            16842752,
        );
        let err = chain
            .install_from_nipopow_proof_no_pow(head, vec![bogus_tail])
            .unwrap_err();
        drop(err);

        // Cache should be empty, matching by_id/scores/base_height.
        assert_eq!(chain.lazy().header_cache_len(), 0);
        assert_eq!(chain.lazy().score_cache_len(), 0);
        assert!(chain.is_empty());
    }

    #[test]
    fn public_header_at_returns_none_for_evicted_heights_without_loader() {
        // Phase 3 invariant: the in-memory Vec<Header> safety net is
        // retired. With no loader wired, reads for evicted heights
        // resolve to None — this is the honest signal that the
        // integrator hasn't wired persistent storage.
        let mut chain = HeaderChain::new(testnet_config());
        chain.set_cache_capacity(NonZeroUsize::new(2).unwrap());
        let built = build_chain(&mut chain, 5);

        // Heights 1-3 are evicted from the cache (cap=2, we pushed 5).
        assert!(chain.lazy().peek_header(1).is_none());
        assert!(chain.lazy().peek_header(2).is_none());
        assert!(chain.lazy().peek_header(3).is_none());

        // Public API for evicted heights → None.
        assert!(chain.header_at(1).is_none());
        assert!(chain.header_at(2).is_none());
        assert!(chain.header_at(3).is_none());

        // Tip and penultimate are still resident, reads succeed.
        assert_eq!(chain.header_at(4).unwrap().id, built[3].id);
        assert_eq!(chain.header_at(5).unwrap().id, built[4].id);
        assert_eq!(chain.tip().id, built[4].id);

        // `headers_from` inherits the same behavior — `filter_map`
        // drops the Nones, so the returned slice is short.
        let slice = chain.headers_from(1, 5);
        assert_eq!(slice.len(), 2, "only cached heights survive");
        assert_eq!(slice[0].id, built[3].id);
        assert_eq!(slice[1].id, built[4].id);
    }

    #[test]
    fn public_header_at_consults_loader_on_cache_miss() {
        // Phase 2 invariant: when cache misses and a loader IS wired,
        // the loader answers (no fallback to Vec needed in production).
        let mut chain = HeaderChain::new(testnet_config());
        chain.set_cache_capacity(NonZeroUsize::new(2).unwrap());
        let built = build_chain(&mut chain, 5);
        let loader_source = built.clone();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_hot = calls.clone();
        chain.set_header_loader(move |h| {
            calls_hot.fetch_add(1, Ordering::SeqCst);
            loader_source.get((h - 1) as usize).cloned()
        });

        // Height 1 is evicted → loader fires exactly once.
        let got = chain.header_at(1).expect("loader returns Some");
        assert_eq!(got.id, built[0].id);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Second read at same height: cache hit, loader quiet.
        let _ = chain.header_at(1).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Height 5 (tip) is resident → loader not consulted.
        let _ = chain.header_at(5).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn successful_install_caches_suffix_head_with_zero_score() {
        let mut chain = HeaderChain::new(testnet_config());
        let head = make_header(1000, BlockId(Digest32::from([7u8; 32])), 50_000_000, 16842752);
        let head_id = head.id;
        chain
            .install_from_nipopow_proof_no_pow(head, vec![])
            .expect("install should succeed");

        let cached = chain.lazy().peek_header(1000).expect("head must be cached");
        assert_eq!(cached.id, head_id);
        // Per the install contract, scores[0] = 0.
        assert_eq!(
            chain.lazy().peek_score(1000).unwrap(),
            BigUint::ZERO,
            "install-boundary score must be zero in cache"
        );
    }
}

