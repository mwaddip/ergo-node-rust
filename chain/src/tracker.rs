use ergo_chain_types::{BlockId, Header};

/// Stateless observer that tracks the best known header from the network.
///
/// Does not validate chain linkage — just remembers the highest header seen.
/// Used to know "what height is the network at" before we have a validated chain.
pub struct HeaderTracker {
    best: Option<TrackedTip>,
}

struct TrackedTip {
    height: u32,
    id: BlockId,
}

impl HeaderTracker {
    pub fn new() -> Self {
        Self { best: None }
    }

    /// Record a header observation. Updates tip if this header is higher.
    pub fn observe(&mut self, header: &Header) {
        let dominated = self
            .best
            .as_ref()
            .is_none_or(|tip| header.height > tip.height);

        if dominated {
            self.best = Some(TrackedTip {
                height: header.height,
                id: header.id,
            });
        }
    }

    /// Height of the highest header observed, if any.
    pub fn best_height(&self) -> Option<u32> {
        self.best.as_ref().map(|tip| tip.height)
    }

    /// ID of the highest header observed, if any.
    pub fn best_header_id(&self) -> Option<&BlockId> {
        self.best.as_ref().map(|tip| &tip.id)
    }
}

impl Default for HeaderTracker {
    fn default() -> Self {
        Self::new()
    }
}
