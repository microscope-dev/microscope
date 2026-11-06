//! The batches a recording drains, kept while records in them are held
//! back past a watermark: a held record stays where its drain put it,
//! rather than being copied after each drain it waits through.

use std::collections::VecDeque;

/// Batches kept with records held back in them, past which the records
/// left are copied into one, so that a record the watermark never passes
/// cannot keep every later batch.
const MOST_BATCHES: usize = 8;

/// Emptied batches kept to be filled again: as many as a drain every 10 ms
/// holds back for a watermark 20 ms behind, and one more.
const SPARE_BATCHES: usize = 4;

/// Where a record lies: the batch its drain filled, and its bytes there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecordAt {
    batch: u64,
    start: usize,
    length: usize,
}

impl RecordAt {
    pub(super) const fn new(batch: u64, start: usize, length: usize) -> Self {
        Self {
            batch,
            start,
            length,
        }
    }
}

/// The drained batches records are held back in, oldest first.
#[derive(Debug, Default)]
pub(super) struct Batches {
    kept: VecDeque<Vec<u8>>,

    /// The number of the oldest kept.
    first: u64,
    spare: Vec<Vec<u8>>,
}

impl Batches {
    /// Fills a new batch with what `drain` appends to an empty buffer, and
    /// returns its number.
    pub(super) fn fill(&mut self, drain: impl FnOnce(&mut Vec<u8>)) -> u64 {
        let mut bytes = self.take_spare();
        debug_assert!(bytes.is_empty(), "a spare batch holds records");
        drain(&mut bytes);
        self.kept.push_back(bytes);
        self.first + self.kept.len() as u64 - 1
    }

    /// The roomiest spare batch, so that one an empty drain left small is
    /// not grown again by the next busy one.
    fn take_spare(&mut self) -> Vec<u8> {
        let roomiest = (0..self.spare.len()).max_by_key(|&at| self.spare[at].capacity());
        roomiest.map_or_else(Vec::new, |at| self.spare.swap_remove(at))
    }

    /// The bytes of batch `batch`, which must be kept.
    pub(super) fn bytes(&self, batch: u64) -> &[u8] {
        debug_assert!(batch >= self.first, "batch {batch} was emptied");
        let at = batch
            .checked_sub(self.first)
            .and_then(|at| usize::try_from(at).ok())
            .unwrap_or(usize::MAX);
        self.kept.get(at).map_or(&[], Vec::as_slice)
    }

    /// The bytes of the record at `at`.
    pub(super) fn record(&self, at: RecordAt) -> &[u8] {
        let bytes = self.bytes(at.batch).get(at.start..at.start + at.length);
        debug_assert!(bytes.is_some(), "{at:?} lies past its batch");
        bytes.unwrap_or_default()
    }

    /// Keeps only the batches the records `held` lie in, emptying the rest
    /// to be filled again. Past [`MOST_BATCHES`], the records are copied
    /// into one batch, and `held` says where they are now.
    pub(super) fn release(&mut self, held: &mut [RecordAt]) {
        if self.kept.len() > MOST_BATCHES {
            let mut together = self.take_spare();
            for at in held.iter_mut() {
                let start = together.len();
                together.extend_from_slice(self.record(*at));
                *at = RecordAt::new(self.first + self.kept.len() as u64, start, at.length);
            }

            let kept = self.kept.len();
            self.drop_oldest(kept);
            self.kept.push_back(together);
            return;
        }

        let oldest = held
            .iter()
            .map(|at| at.batch)
            .min()
            .unwrap_or(self.first + self.kept.len() as u64);
        self.drop_oldest(usize::try_from(oldest - self.first).unwrap_or(usize::MAX));
    }

    /// Empties the oldest `count` batches kept, to be filled again.
    fn drop_oldest(&mut self, count: usize) {
        for _ in 0..count {
            let Some(mut emptied) = self.kept.pop_front() else {
                break;
            };
            self.first += 1;
            if self.spare.len() < SPARE_BATCHES {
                emptied.clear();
                self.spare.push(emptied);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record held back stays where its drain put it until no record is
    /// held in its batch or any before, and the batch's buffer is then filled
    /// again by a later drain; one the watermark never passes is copied out
    /// once too many batches are kept.
    #[test]
    fn a_held_record_keeps_its_batch_until_the_batch_is_copied_out() {
        let mut batches = Batches::default();
        let first = batches.fill(|bytes| bytes.extend_from_slice(b"abcdef"));
        let buffer = batches.bytes(first).as_ptr();
        let mut held = vec![RecordAt::new(first, 2, 3)];
        batches.release(&mut held);
        assert_eq!(batches.record(held[0]), b"cde");

        // The first batch's record is processed, and a record of the next
        // held: the first batch is emptied, and the third fills it again.
        let second = batches.fill(|bytes| bytes.extend_from_slice(b"ghij"));
        held = vec![RecordAt::new(second, 1, 2)];
        batches.release(&mut held);
        assert_eq!(batches.record(held[0]), b"hi");
        let third = batches.fill(|bytes| bytes.extend_from_slice(b"kl"));
        assert_eq!(batches.bytes(third).as_ptr(), buffer);
        assert_eq!(batches.bytes(third), b"kl");

        // A record held through more batches than are kept is copied into
        // one, and the rest emptied.
        for _ in 0..MOST_BATCHES {
            batches.fill(|bytes| bytes.extend_from_slice(b"mn"));
        }
        batches.release(&mut held);
        assert_eq!(batches.kept.len(), 1);
        assert_eq!(batches.record(held[0]), b"hi");

        // Once nothing is held, nothing is kept.
        batches.release(&mut []);
        assert!(batches.kept.is_empty());
        assert!(batches.spare.len() <= SPARE_BATCHES);
    }
}
