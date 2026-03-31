use std::collections::HashMap;
use std::sync::Arc;

use enr_chain::{BlockId, ChainError, Header, HeaderChain, HeaderTracker};
use enr_p2p::routing::validator::{ModifierValidator, ModifierVerdict};
use tokio::sync::Mutex;

/// Modifier type ID for headers (NetworkObjectTypeId in JVM source).
const HEADER_TYPE_ID: u8 = 101;

/// Max pending headers before we start dropping old entries.
const MAX_PENDING: usize = 10_000;

/// Validates header modifiers via enr-chain before the router forwards them.
///
/// Uses two levels of validation:
/// - **Chain validation**: if the header extends the validated chain (parent exists,
///   timestamps correct, difficulty correct, PoW valid), it's appended to the chain.
/// - **PoW-only with buffering**: if chain validation fails because the parent is
///   missing (out-of-order delivery), the header is PoW-verified and buffered.
///   When a header IS chained, the buffer is drained: any buffered header whose
///   parent is now the new tip gets chained too, iteratively.
///
/// Headers that fail PoW verification are always rejected.
/// All other modifier types pass through unconditionally.
pub struct HeaderValidator {
    chain: Arc<Mutex<HeaderChain>>,
    tracker: HeaderTracker,
    /// Headers that passed PoW but couldn't chain yet (parent not in chain).
    /// Keyed by parent_id — the ID they need to chain after.
    pending: HashMap<BlockId, Header>,
}

impl HeaderValidator {
    pub fn new(chain: Arc<Mutex<HeaderChain>>) -> Self {
        Self {
            chain,
            tracker: HeaderTracker::new(),
            pending: HashMap::new(),
        }
    }
}

impl ModifierValidator for HeaderValidator {
    fn validate(&mut self, modifier_type: u8, _id: &[u8; 32], data: &[u8]) -> ModifierVerdict {
        if modifier_type != HEADER_TYPE_ID {
            return ModifierVerdict::Accept;
        }

        let header = match enr_chain::parse_header(data) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!("rejecting header: parse failed: {e}");
                return ModifierVerdict::Reject;
            }
        };

        let height = header.height;

        // Try full chain validation first.
        // Use try_lock since we're called from a sync context inside the async
        // event loop. If the lock is held (sync machine reading), fall through
        // to PoW-only validation + buffering.
        if let Ok(mut chain) = self.chain.try_lock() {
            match chain.try_append(header.clone()) {
                Ok(()) => {
                    self.tracker.observe(&header);
                    tracing::debug!("chain-validated header at height {height}");

                    // Drain pending buffer: chain any buffered headers whose
                    // parent is now in the chain.
                    let mut next_parent = header.id;
                    while let Some(buffered) = self.pending.remove(&next_parent) {
                        let bh = buffered.height;
                        let bid = buffered.id;
                        match chain.try_append(buffered.clone()) {
                            Ok(()) => {
                                self.tracker.observe(&buffered);
                                next_parent = bid;
                            }
                            Err(e) => {
                                tracing::debug!("buffered header at height {bh} failed: {e}");
                                break;
                            }
                        }
                    }

                    let chain_height = chain.height();
                    drop(chain);

                    if chain_height > height {
                        tracing::info!(
                            chain_height,
                            "drained pending buffer from height {height}"
                        );
                    }

                    return ModifierVerdict::Accept;
                }
                Err(ChainError::ParentNotFound { .. })
                | Err(ChainError::InvalidGenesisParent { .. })
                | Err(ChainError::InvalidGenesisHeight { .. }) => {
                    drop(chain);
                    // Can't place this header in our chain — fall back to PoW + buffer.
                }
                Err(e) => {
                    drop(chain);
                    tracing::debug!("rejecting header at height {height}: {e}");
                    return ModifierVerdict::Reject;
                }
            }
        }

        // Fallback: PoW-only validation + buffer for later
        if let Err(e) = enr_chain::verify_pow(&header) {
            tracing::debug!("rejecting unchained header at height {height}: {e}");
            return ModifierVerdict::Reject;
        }

        self.tracker.observe(&header);

        // Buffer for when the parent arrives
        if self.pending.len() < MAX_PENDING {
            self.pending.insert(header.parent_id, header);
        }

        ModifierVerdict::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enr_chain::ChainConfig;
    use enr_p2p::routing::validator::ModifierVerdict;

    fn testnet_validator() -> HeaderValidator {
        let chain = Arc::new(Mutex::new(HeaderChain::new(ChainConfig::testnet())));
        HeaderValidator::new(chain)
    }

    #[test]
    fn rejects_unparseable_header() {
        let mut v = testnet_validator();
        let verdict = v.validate(HEADER_TYPE_ID, &[0xaa; 32], &[0xff, 0x00, 0x01]);
        assert_eq!(verdict, ModifierVerdict::Reject);
    }

    #[test]
    fn passes_non_header_types_through() {
        let mut v = testnet_validator();
        let verdict = v.validate(102, &[0xaa; 32], &[0xff; 100]);
        assert_eq!(verdict, ModifierVerdict::Accept);
    }

    #[test]
    fn accepts_valid_pow_header_without_chain() {
        use enr_chain::Header;
        use sigma_ser::ScorexSerializable;

        let json = r#"{
            "extensionId": "00cce45975d87414e8bdd8146bc88815be59cd9fe37a125b5021101e05675a18",
            "difficulty": "16384",
            "votes": "000000",
            "timestamp": 4928911477310178288,
            "size": 223,
            "stateRoot": "5c8c00b8403d3701557181c8df800001b6d5009e2201c6ff807d71808c00019780",
            "height": 614400,
            "nBits": 37748736,
            "version": 2,
            "id": "5603a937ec1988220fc44fb5022fb82d5565b961f005ebb55d85bd5a9e6f801f",
            "adProofsRoot": "5d3f80dcff7f5e7f59007294c180808d0158d1ff6ba10000f901c7f0ef87dcff",
            "transactionsRoot": "f17fffacb6ff7f7f1180d2ff7f1e24ffffe1ff937f807f0797b9ff6ebdae007e",
            "extensionHash": "1480887f80007f4b01cf7f013ff1ffff564a0000b9a54f00770e807f41ff88c0",
            "powSolutions": {
                "pk": "03bedaee069ff4829500b3c07c4d5fe6b3ea3d3bf76c5c28c1d4dcdb1bed0ade0c",
                "n": "0000000000003105"
            },
            "parentId": "ac2101807f0000ca01ff0119db227f202201007f62000177a080005d440896d0"
        }"#;
        let header: Header = serde_json::from_str(json).unwrap();
        let bytes = header.scorex_serialize_bytes().unwrap();

        let mut v = testnet_validator();
        let verdict = v.validate(HEADER_TYPE_ID, &[0xaa; 32], &bytes);
        assert_eq!(verdict, ModifierVerdict::Accept);
    }
}
