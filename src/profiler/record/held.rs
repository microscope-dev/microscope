//! The blocks a recording drains, kept while records in them are held
//! back past a watermark: a held record stays where the reader put it,
//! rather than being copied after each drain it waits through.

use std::collections::VecDeque;

use crate::backend::perf::Blocks;

/// Blocks kept, emptied ones among them, past which the records held in
/// the older half are copied into one, so that a record the watermark
/// never passes cannot keep a block, and a place for every later one.
const MOST_KEPT: usize = 64;

/// Where a record lies: the block it was drained into, and its bytes there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecordAt {
    block: u64,
    start: usize,
    length: usize,
}

impl RecordAt {
    pub(super) const fn new(block: u64, start: usize, length: usize) -> Self {
        Self {
            block,
            start,
            length,
        }
    }
}

/// The drained blocks records are held back in, oldest first, each by its
/// number. A block no held record lies in is emptied for the reader to
/// fill again.
#[derive(Debug, Default)]
pub(super) struct Batches {
    kept: VecDeque<Option<Vec<u8>>>,

    /// The number of the oldest kept.
    first: u64,

    /// The blocks a drain hands on and takes back, kept between drains.
    blocks: Blocks,

    /// Which kept blocks a held record lies in.
    pinned: Vec<bool>,
}

impl Batches {
    /// Keeps the blocks `drain` fills, giving it the blocks emptied since
    /// the last, and returns their numbers.
    pub(super) fn fill(&mut self, drain: impl FnOnce(&mut Blocks)) -> std::ops::Range<u64> {
        drain(&mut self.blocks);
        let from = self.end();
        self.kept.extend(self.blocks.filled.drain(..).map(Some));
        from..self.end()
    }

    /// Keeps a block of a copy of `bytes`, and returns its number.
    pub(super) fn copy(&mut self, bytes: &[u8]) -> std::ops::Range<u64> {
        if bytes.is_empty() {
            return self.end()..self.end();
        }

        // The roomiest emptied block, so that one a small copy left small is
        // not grown again by the next large one.
        let emptied = &mut self.blocks.emptied;
        let roomiest = (0..emptied.len()).max_by_key(|&at| emptied[at].capacity());
        let mut block = roomiest.map_or_else(Vec::new, |at| emptied.swap_remove(at));
        block.clear();
        block.extend_from_slice(bytes);
        let from = self.end();
        self.kept.push_back(Some(block));
        from..self.end()
    }

    /// The number the next block kept takes.
    fn end(&self) -> u64 {
        self.first + self.kept.len() as u64
    }

    /// The bytes of block `block`, which must be kept.
    pub(super) fn bytes(&self, block: u64) -> &[u8] {
        let bytes = block
            .checked_sub(self.first)
            .and_then(|at| usize::try_from(at).ok())
            .and_then(|at| self.kept.get(at))
            .and_then(Option::as_deref);
        debug_assert!(bytes.is_some(), "block {block} was emptied");
        bytes.unwrap_or_default()
    }

    /// The bytes of the record at `at`.
    pub(super) fn record(&self, at: RecordAt) -> &[u8] {
        let bytes = self.bytes(at.block).get(at.start..at.start + at.length);
        debug_assert!(bytes.is_some(), "{at:?} lies past its block");
        bytes.unwrap_or_default()
    }

    /// Keeps only the blocks the records `held` lie in, emptying the rest
    /// to be filled again. Past [`MOST_KEPT`], the records held in the
    /// older half are copied into a new block, and `held` says where they
    /// are now.
    pub(super) fn release(&mut self, held: &mut [RecordAt]) {
        if self.kept.len() > MOST_KEPT {
            let older = self.first + (self.kept.len() / 2) as u64;
            let mut together = self.blocks.emptied.pop().unwrap_or_default();
            together.clear();
            let number = self.end();
            for at in held.iter_mut().filter(|at| at.block < older) {
                let start = together.len();
                together.extend_from_slice(self.record(*at));
                *at = RecordAt::new(number, start, at.length);
            }
            self.kept.push_back(Some(together));
        }

        self.pinned.clear();
        self.pinned.resize(self.kept.len(), false);
        for at in held.iter() {
            if let Some(pinned) = at
                .block
                .checked_sub(self.first)
                .and_then(|at| usize::try_from(at).ok())
                .and_then(|at| self.pinned.get_mut(at))
            {
                *pinned = true;
            }
        }

        for (kept, &pinned) in self.kept.iter_mut().zip(&self.pinned) {
            if !pinned && let Some(block) = kept.take() {
                self.blocks.emptied.push(block);
            }
        }
        while self.kept.front().is_some_and(Option::is_none) {
            self.kept.pop_front();
            self.first += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(batches: &mut Batches, records: &[&[u8]]) -> std::ops::Range<u64> {
        batches.fill(|blocks| {
            for bytes in records {
                let mut block = blocks.emptied.pop().unwrap_or_default();
                block.extend_from_slice(bytes);
                blocks.filled.push(block);
            }
        })
    }

    /// A held record stays where the reader put it until it is no longer
    /// held, and its block is then given back to be filled again; the
    /// blocks around it are given back at once.
    #[test]
    fn a_held_record_keeps_only_its_own_block() {
        let mut batches = Batches::default();
        let blocks = fill(&mut batches, &[b"abcdef", b"ghij", b"kl"]);
        let middle = blocks.start + 1;
        let buffer = batches.bytes(middle).as_ptr();
        let mut held = vec![RecordAt::new(middle, 1, 2)];
        batches.release(&mut held);
        assert_eq!(batches.record(held[0]), b"hi");
        assert_eq!(batches.blocks.emptied.len(), 2, "the blocks around it");

        // Later drains are given the emptied blocks, and the held record
        // stays put.
        fill(&mut batches, &[b"mn"]);
        batches.release(&mut held);
        assert_eq!(batches.record(held[0]), b"hi");
        assert_eq!(batches.bytes(middle).as_ptr(), buffer);

        // The later drain filled an emptied block, not a new one.
        batches.release(&mut []);
        assert!(batches.kept.is_empty());
        assert_eq!(batches.blocks.emptied.len(), 3);
    }

    /// A record held while more than [`MOST_KEPT`] blocks go by is copied
    /// out once, and its block is given back.
    #[test]
    fn a_record_held_too_long_is_copied_out_of_its_block() {
        let mut batches = Batches::default();
        let first = fill(&mut batches, &[b"abc"]).start;
        let mut held = vec![RecordAt::new(first, 0, 3)];
        for _ in 0..MOST_KEPT {
            fill(&mut batches, &[b"xyz"]);
            batches.release(&mut held);
            assert_eq!(batches.record(held[0]), b"abc");
        }

        assert_ne!(held[0], RecordAt::new(first, 0, 3), "copied out");
        assert!(
            batches.kept.len() <= MOST_KEPT / 2 + 2,
            "{}",
            batches.kept.len()
        );
        batches.release(&mut []);
        assert!(batches.kept.is_empty());
    }
}
