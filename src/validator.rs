use enr_chain::HeaderTracker;
use enr_p2p::routing::validator::{ModifierValidator, ModifierVerdict};

/// Modifier type ID for headers (NetworkObjectTypeId in JVM source).
const HEADER_TYPE_ID: u8 = 101;

/// Validates header modifiers via enr-chain before the router forwards them.
///
/// For modifier_type 101 (Header): parses the header, verifies PoW, and
/// tracks the best known height. Rejects unparseable or invalid-PoW headers.
///
/// All other modifier types pass through unconditionally.
pub struct HeaderValidator {
    tracker: HeaderTracker,
}

impl HeaderValidator {
    pub fn new() -> Self {
        Self {
            tracker: HeaderTracker::new(),
        }
    }

    /// Height of the highest valid header observed, if any.
    pub fn best_height(&self) -> Option<u32> {
        self.tracker.best_height()
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

        if let Err(e) = enr_chain::verify_pow(&header) {
            tracing::debug!("rejecting header at height {}: {e}", header.height);
            return ModifierVerdict::Reject;
        }

        self.tracker.observe(&header);
        tracing::debug!("accepted header at height {}", header.height);
        ModifierVerdict::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enr_chain::Header;
    use enr_p2p::routing::validator::ModifierVerdict;
    use sigma_ser::ScorexSerializable;

    fn valid_v2_header_bytes() -> (Header, Vec<u8>) {
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
        (header, bytes)
    }

    #[test]
    fn accepts_valid_header() {
        let mut v = HeaderValidator::new();
        let (_header, bytes) = valid_v2_header_bytes();
        let id = [0xaa; 32];

        let verdict = v.validate(HEADER_TYPE_ID, &id, &bytes);
        assert_eq!(verdict, ModifierVerdict::Accept);
        assert_eq!(v.best_height(), Some(614400));
    }

    #[test]
    fn rejects_unparseable_header() {
        let mut v = HeaderValidator::new();
        let verdict = v.validate(HEADER_TYPE_ID, &[0xaa; 32], &[0xff, 0x00, 0x01]);
        assert_eq!(verdict, ModifierVerdict::Reject);
        assert_eq!(v.best_height(), None);
    }

    #[test]
    fn passes_non_header_types_through() {
        let mut v = HeaderValidator::new();
        let verdict = v.validate(102, &[0xaa; 32], &[0xff; 100]);
        assert_eq!(verdict, ModifierVerdict::Accept);
        // No height tracked — this wasn't a header
        assert_eq!(v.best_height(), None);
    }

    #[test]
    fn tracks_height_across_multiple_headers() {
        let mut v = HeaderValidator::new();
        let (_header, bytes) = valid_v2_header_bytes();

        v.validate(HEADER_TYPE_ID, &[0xaa; 32], &bytes);
        assert_eq!(v.best_height(), Some(614400));

        // Same header again — height shouldn't change
        v.validate(HEADER_TYPE_ID, &[0xbb; 32], &bytes);
        assert_eq!(v.best_height(), Some(614400));
    }
}
