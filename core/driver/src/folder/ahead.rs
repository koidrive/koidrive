// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use koidrive_engine::commit::MAX_OPS;
use koidrive_proto::v1::{ChunkRef, UploadReceipt};

use crate::clock;

pub(super) const FILES: usize = MAX_OPS;
pub(super) const BATCH_FILES: usize = MAX_OPS / 4;
pub(super) const BATCHES: usize = 2;
const RECEIPT_LIFE: Duration = Duration::from_secs(30 * 60);

#[derive(Default)]
pub(super) struct Ahead {
    ready: HashMap<Vec<u8>, (UploadReceipt, Instant)>,
    running: BTreeMap<u64, HashSet<Vec<u8>>>,
    started: u64,
}

impl Ahead {
    pub fn start(&mut self, chunks: &[ChunkRef]) -> u64 {
        self.started += 1;
        let hashes = chunks.iter().map(|chunk| chunk.hash.clone()).collect();
        self.running.insert(self.started, hashes);
        self.started
    }

    pub fn running(&self) -> usize {
        self.running.len()
    }

    pub fn ended(&mut self, id: u64, receipts: Option<Vec<UploadReceipt>>) -> bool {
        if self.running.remove(&id).is_none() {
            return false;
        }
        let at = clock::instant();
        for receipt in receipts.into_iter().flatten() {
            self.ready.insert(receipt.hash.clone(), (receipt, at));
        }
        true
    }

    pub fn knows(&self, hash: &[u8]) -> bool {
        self.ready.contains_key(hash) || self.runs(hash)
    }

    pub fn awaits(&self, chunks: &[ChunkRef]) -> bool {
        chunks.iter().any(|chunk| self.runs(&chunk.hash))
    }

    fn runs(&self, hash: &[u8]) -> bool {
        self.running.values().any(|hashes| hashes.contains(hash))
    }

    pub fn take(&mut self, chunks: &mut Vec<ChunkRef>) -> Vec<UploadReceipt> {
        let now = clock::instant();
        (self.ready).retain(|_, (_, at)| now.saturating_duration_since(*at) < RECEIPT_LIFE);
        let mut taken = HashSet::new();
        let mut receipts = Vec::new();
        chunks.retain(|chunk| {
            if let Some((receipt, _)) = self.ready.remove(&chunk.hash) {
                taken.insert(chunk.hash.clone());
                receipts.push(receipt);
            }
            !taken.contains(&chunk.hash)
        });
        receipts
    }

    pub fn clear(&mut self) {
        self.ready.clear();
        self.running.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(byte: u8) -> ChunkRef {
        ChunkRef {
            hash: vec![byte; 32],
            size: 3,
            ..Default::default()
        }
    }

    fn receipt(byte: u8) -> UploadReceipt {
        UploadReceipt {
            hash: vec![byte; 32],
            size: 3,
            token: format!("token {byte}"),
        }
    }

    fn uploaded(bytes: &[u8]) -> Ahead {
        let mut ahead = Ahead::default();
        let chunks: Vec<ChunkRef> = bytes.iter().map(|byte| chunk(*byte)).collect();
        let id = ahead.start(&chunks);
        assert!(ahead.ended(id, Some(bytes.iter().map(|byte| receipt(*byte)).collect())));
        ahead
    }

    #[test]
    fn a_commit_takes_each_receipt_once_and_keeps_the_chunks_without_one() {
        let mut ahead = uploaded(&[1, 2]);
        let mut chunks = vec![chunk(1), chunk(3), chunk(1), chunk(2)];
        assert_eq!(ahead.take(&mut chunks), [receipt(1), receipt(2)]);
        assert_eq!(chunks, [chunk(3)]);
        assert!(!ahead.knows(&chunk(1).hash));
    }

    #[test]
    fn a_receipt_is_not_taken_once_it_is_half_as_old_as_the_server_lets_it_be() {
        let mut ahead = uploaded(&[1]);
        clock::advance_to(clock::instant() + RECEIPT_LIFE);
        let mut chunks = vec![chunk(1)];
        assert!(ahead.take(&mut chunks).is_empty());
        assert_eq!(chunks, [chunk(1)]);
    }

    #[test]
    fn a_failed_upload_or_one_that_ends_after_a_clear_leaves_nothing() {
        let mut ahead = Ahead::default();
        let failed = ahead.start(&[chunk(1)]);
        assert!(ahead.awaits(&[chunk(2), chunk(1)]));
        assert!(ahead.ended(failed, None));
        let cleared = ahead.start(&[chunk(2)]);
        ahead.clear();
        assert!(!ahead.ended(cleared, Some(vec![receipt(2)])));
        assert!(!ahead.awaits(&[chunk(1), chunk(2)]));
        assert!(!ahead.knows(&chunk(1).hash) && !ahead.knows(&chunk(2).hash));
    }
}
