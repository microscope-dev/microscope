//! Pure x86-64 debug-register planning for hardware watchpoints.
//!
//! The planner maps watched byte ranges onto the four address slots of
//! DR0-DR3 and their DR7 control bits, and decodes DR6 hit bits back to the
//! owning watchpoints. It performs no ptrace operations; the Linux edge applies
//! [`DebugRegisterPlan::programming_sequence`] to every traced thread.

use std::collections::BTreeSet;

use crate::WatchpointId;

/// Address slots provided by DR0-DR3.
pub const SLOT_COUNT: usize = 4;

/// The widest aligned span one slot can cover on x86-64.
pub const MAX_SLOT_BYTES: u64 = 8;

/// The debug status register.
pub const STATUS_REGISTER: usize = 6;

/// The debug control register.
pub const CONTROL_REGISTER: usize = 7;

/// The architectural DR6 value with no recorded condition, which the kernel
/// stores as an empty virtual status.
pub const STATUS_IDLE: u64 = 0xffff_0ff0;

/// DR6 B0-B3: which slots reported a hit.
const STATUS_HITS: u64 = 0xf;

// Each slot's R/W and LEN fields take four bits of DR7 from bit 16.
const _: () = assert!(16 + 4 * SLOT_COUNT <= 32, "DR7 has no room for the slots");

// Clearing DR6 must not read as a hit.
const _: () = assert!(STATUS_IDLE & STATUS_HITS == 0, "idle DR6 reports a hit");

/// The first address Linux refuses for user hardware breakpoints
/// (`TASK_SIZE_MAX` with 4-level paging). A watched span must end at or
/// before it.
const USER_ADDRESS_LIMIT: u64 = 0x7fff_ffff_f000;

/// The memory accesses one slot reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotAccess {
    /// Data writes only.
    Write,

    /// Data reads or writes; x86 has no read-only condition.
    ReadWrite,
}

impl SlotAccess {
    const fn control_bits(self) -> u64 {
        match self {
            Self::Write => 0b01,
            Self::ReadWrite => 0b11,
        }
    }
}

/// One naturally aligned 1, 2, 4, or 8 byte span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chunk {
    pub address: u64,
    pub length: ChunkLength,
}

/// The lengths a debug register watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChunkLength {
    One = 1,
    Two = 2,
    Four = 4,
    Eight = 8,
}

impl ChunkLength {
    pub const fn bytes(self) -> u64 {
        self as u64
    }
}

impl Chunk {
    pub const fn end(self) -> u64 {
        self.address + self.length.bytes()
    }

    const fn length_bits(self) -> u64 {
        match self.length {
            ChunkLength::One => 0b00,
            ChunkLength::Two => 0b01,
            ChunkLength::Four => 0b11,
            ChunkLength::Eight => 0b10,
        }
    }
}

/// Why a byte range cannot be covered by debug-register slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// The range contains no bytes.
    Empty,

    /// The range wraps the address space.
    Overflow,

    /// The range reaches memory the kernel refuses to watch.
    OutsideUserSpace,

    /// Covering the range exactly needs more slots than a thread has.
    TooLarge { required: u64 },
}

/// Splits `[address, address + length)` into the minimal exact set of
/// naturally aligned chunks.
///
/// Taking the widest aligned chunk at each position is optimal: any cover must
/// split at every alignment boundary the greedy choice splits at.
pub fn split_range(address: u64, length: u64) -> Result<Vec<Chunk>, RangeError> {
    if length == 0 {
        return Err(RangeError::Empty);
    }

    let end = address.checked_add(length).ok_or(RangeError::Overflow)?;
    if end > USER_ADDRESS_LIMIT {
        return Err(RangeError::OutsideUserSpace);
    }

    let required = chunk_count(address, end);
    if required > SLOT_COUNT as u64 {
        return Err(RangeError::TooLarge { required });
    }

    let mut chunks = Vec::new();
    let mut cursor = address;
    while cursor < end {
        let length = widest_chunk(cursor, end);
        chunks.push(Chunk {
            address: cursor,
            length,
        });
        cursor += length.bytes();
    }

    if cfg!(debug_assertions) {
        check_exact_cover(address, length, &chunks);
    }

    Ok(chunks)
}

/// The widest aligned chunk at `cursor`. Callers only ask while `cursor` is
/// below `end`, so a single byte always fits.
fn widest_chunk(cursor: u64, end: u64) -> ChunkLength {
    [ChunkLength::Eight, ChunkLength::Four, ChunkLength::Two]
        .into_iter()
        .find(|length| cursor.is_multiple_of(length.bytes()) && end - cursor >= length.bytes())
        .unwrap_or(ChunkLength::One)
}

/// Counts greedy chunks without materializing an arbitrarily large range.
fn chunk_count(address: u64, end: u64) -> u64 {
    let mut count = 0;
    let mut cursor = address;
    while cursor < end && !cursor.is_multiple_of(MAX_SLOT_BYTES) {
        cursor += widest_chunk(cursor, end).bytes();
        count += 1;
    }

    let aligned_end = end & !(MAX_SLOT_BYTES - 1);
    if cursor < aligned_end {
        count += (aligned_end - cursor) / MAX_SLOT_BYTES;
        cursor = aligned_end;
    }

    while cursor < end {
        cursor += widest_chunk(cursor, end).bytes();
        count += 1;
    }

    count
}

/// One programmed slot and the watchpoints sharing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub chunk: Chunk,
    pub access: SlotAccess,
    pub owners: BTreeSet<WatchpointId>,
}

/// A watchpoint needs more free slots than remain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityError {
    pub required: usize,
    pub available: usize,
}

/// DR6 reported a hit in a slot this plan does not program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSlots {
    pub status: u64,
}

/// The debug-register state every traced thread must carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebugRegisterPlan {
    slots: [Option<Slot>; SLOT_COUNT],
}

impl DebugRegisterPlan {
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }

    pub const fn slots(&self) -> &[Option<Slot>; SLOT_COUNT] {
        &self.slots
    }

    pub fn free_slots(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_none()).count()
    }

    /// Returns a plan that also arms `chunks` for `id`, sharing identical
    /// slots and otherwise filling the lowest free indices. Writing an address
    /// makes the kernel reserve that slot for the thread, so reusing low
    /// indices keeps the reservation footprint minimal.
    pub fn with_watchpoint(
        &self,
        id: WatchpointId,
        chunks: &[Chunk],
        access: SlotAccess,
    ) -> Result<Self, CapacityError> {
        debug_assert!(
            !self
                .slots
                .iter()
                .flatten()
                .any(|slot| slot.owners.contains(&id)),
            "watchpoint {id} is armed while it already holds a slot"
        );
        let mut next = self.clone();
        let mut required = 0;
        for chunk in chunks {
            let shared = next.slots.iter_mut().flatten().find(|slot| {
                slot.chunk == *chunk && slot.access == access && !slot.owners.contains(&id)
            });
            if let Some(slot) = shared {
                slot.owners.insert(id);
                continue;
            }

            required += 1;
            if let Some(free) = next.slots.iter_mut().find(|slot| slot.is_none()) {
                *free = Some(Slot {
                    chunk: *chunk,
                    access,
                    owners: BTreeSet::from([id]),
                });
            }
        }

        let available = self.free_slots();
        if required > available {
            return Err(CapacityError {
                required,
                available,
            });
        }

        if cfg!(debug_assertions) {
            next.check_invariants();
        }

        Ok(next)
    }

    /// Returns a plan without `id`, freeing every slot it no longer shares.
    pub fn without_watchpoint(&self, id: WatchpointId) -> Self {
        let mut next = self.clone();
        for entry in &mut next.slots {
            if let Some(slot) = entry {
                slot.owners.remove(&id);
                if slot.owners.is_empty() {
                    *entry = None;
                }
            }
        }

        if cfg!(debug_assertions) {
            next.check_invariants();
        }

        next
    }

    /// Panics unless every slot is owned, aligned, and encoded in DR7 as
    /// planned, and every free slot is clear there.
    fn check_invariants(&self) {
        let control = self.control();
        for (index, slot) in self.slots.iter().enumerate() {
            let enabled = control >> (2 * index) & 1 == 1;
            let fields = control >> (16 + 4 * index) & 0xf;
            if let Some(slot) = slot {
                debug_assert!(!slot.owners.is_empty(), "slot {index} has no owner");
                debug_assert!(
                    slot.chunk.address.is_multiple_of(slot.chunk.length.bytes()),
                    "slot {index} watches {:#x}, not aligned to its {} bytes",
                    slot.chunk.address,
                    slot.chunk.length.bytes()
                );
                debug_assert!(enabled, "slot {index} is not enabled in DR7");
                debug_assert_eq!(
                    fields,
                    slot.access.control_bits() | slot.chunk.length_bits() << 2,
                    "slot {index}'s access and length fields in DR7 are wrong"
                );
            } else {
                debug_assert!(!enabled, "free slot {index} is enabled in DR7");
                debug_assert_eq!(fields, 0, "free slot {index} has fields in DR7");
            }
        }

        debug_assert_eq!(
            control & !0xffff_00ff,
            0,
            "DR7 sets bits outside the enables and slot fields"
        );
    }

    /// The DR7 value arming every planned slot with a local enable bit.
    pub fn control(&self) -> u64 {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, slot)))
            .fold(0, |control, (index, slot)| {
                let shift = 16 + 4 * index;
                control
                    | 1 << (2 * index)
                    | slot.access.control_bits() << shift
                    | slot.chunk.length_bits() << (shift + 2)
            })
    }

    /// Ordered `(register, value)` writes that install this plan on a thread.
    ///
    /// DR7 is cleared first: the kernel validates a new address against the
    /// slot's existing length bits even while the slot is disabled, so a
    /// misaligned address after a wider previous use would be refused.
    /// Addresses follow, then the final control word.
    pub fn programming_sequence(&self) -> Vec<(usize, u64)> {
        let mut writes = vec![(CONTROL_REGISTER, 0)];
        writes.extend(
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(index, slot)| slot.as_ref().map(|slot| (index, slot.chunk.address))),
        );
        let control = self.control();
        if control != 0 {
            writes.push((CONTROL_REGISTER, control));
        }

        writes
    }

    /// The watchpoints whose slots DR6 reports as hit. A hit in an
    /// unprogrammed slot is never attributed to a guessed owner.
    pub fn owners_for_status(&self, status: u64) -> Result<BTreeSet<WatchpointId>, UnknownSlots> {
        let mut owners = BTreeSet::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if status & (1 << index) == 0 {
                continue;
            }

            let slot = slot.as_ref().ok_or(UnknownSlots { status })?;
            owners.extend(slot.owners.iter().copied());
        }

        Ok(owners)
    }
}

/// Whether DR6 records any slot hit.
pub const fn status_has_hits(status: u64) -> bool {
    status & STATUS_HITS != 0
}

#[cfg(feature = "fuzzing")]
pub fn fuzz(data: &[u8]) {
    let mut plan = DebugRegisterPlan::default();
    let mut armed: std::collections::BTreeMap<WatchpointId, (Vec<Chunk>, SlotAccess)> =
        std::collections::BTreeMap::new();
    for operation in data.chunks(12) {
        let [kind, id, rest @ ..] = operation else {
            break;
        };

        let id = WatchpointId::new(u64::from(*id % 8));
        match kind % 3 {
            0 => {
                let mut address = [0_u8; 8];
                let available = rest.len().min(7);
                address[..available].copy_from_slice(&rest[..available]);
                let address = u64::from_le_bytes(address) % (USER_ADDRESS_LIMIT + 64);
                let length = u64::from(rest.get(7).copied().unwrap_or(1)) % 40;
                let access = if kind & 0x80 == 0 {
                    SlotAccess::Write
                } else {
                    SlotAccess::ReadWrite
                };

                if armed.contains_key(&id) {
                    continue;
                }

                let Ok(chunks) = split_range(address, length) else {
                    continue;
                };

                match plan.with_watchpoint(id, &chunks, access) {
                    Ok(next) => {
                        plan = next;
                        armed.insert(id, (chunks, access));
                    }
                    Err(error) => assert!(error.required > error.available),
                }
            }
            1 => {
                plan = plan.without_watchpoint(id);
                armed.remove(&id);
            }
            _ => {
                let status = u64::from(rest.first().copied().unwrap_or(0)) | STATUS_IDLE;
                if let Ok(owners) = plan.owners_for_status(status) {
                    assert!(owners.iter().all(|owner| armed.contains_key(owner)));
                }
            }
        }

        assert_plan_invariants(&plan, &armed);
    }

    for id in std::mem::take(&mut armed).into_keys() {
        plan = plan.without_watchpoint(id);
    }

    assert!(plan.is_empty());
    assert_eq!(plan.control(), 0);
}

/// Panics unless `chunks` cover `[address, address + length)` exactly, in
/// order, each naturally aligned.
fn check_exact_cover(address: u64, length: u64, chunks: &[Chunk]) {
    let mut cursor = address;
    for chunk in chunks {
        debug_assert_eq!(chunk.address, cursor, "chunks are contiguous");
        debug_assert!(
            chunk.address.is_multiple_of(chunk.length.bytes()),
            "the chunk at {:#x} is not aligned to its {} bytes",
            chunk.address,
            chunk.length.bytes()
        );
        cursor = chunk.end();
    }

    debug_assert_eq!(
        cursor,
        address + length,
        "chunks cover {address:#x} for {length} bytes exactly"
    );
}

#[cfg(any(test, feature = "fuzzing"))]
#[allow(
    clippy::expect_used,
    reason = "a fuzz harness reports a bug by panicking"
)]
fn assert_plan_invariants(
    plan: &DebugRegisterPlan,
    armed: &std::collections::BTreeMap<WatchpointId, (Vec<Chunk>, SlotAccess)>,
) {
    for slot in plan.slots.iter().flatten() {
        assert!(!slot.owners.is_empty(), "an empty slot is freed");
        for owner in &slot.owners {
            let (chunks, access) = armed.get(owner).expect("slot owner is armed");
            assert!(chunks.contains(&slot.chunk) && *access == slot.access);
        }
    }

    for (id, (chunks, access)) in armed {
        for chunk in chunks {
            assert_eq!(
                plan.slots
                    .iter()
                    .flatten()
                    .filter(|slot| slot.chunk == *chunk
                        && slot.access == *access
                        && slot.owners.contains(id))
                    .count(),
                1,
                "each armed chunk is owned by exactly one slot"
            );
        }
    }

    plan.check_invariants();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force minimal cover over aligned power-of-two chunks.
    fn optimal_chunks(address: u64, length: u64) -> u64 {
        let length = usize::try_from(length).expect("small test length");
        let mut best = vec![u64::MAX; length + 1];
        best[0] = 0;

        for covered in 0..length {
            if best[covered] == u64::MAX {
                continue;
            }

            let cursor = address + covered as u64;
            for chunk in [1_u64, 2, 4, 8] {
                let width = usize::try_from(chunk).expect("chunk width fits usize");
                if cursor.is_multiple_of(chunk) && covered + width <= length {
                    let next = covered + width;
                    best[next] = best[next].min(best[covered] + 1);
                }
            }
        }

        best[length]
    }

    #[test]
    fn ranges_split_into_exact_minimal_aligned_covers() {
        for offset in 0..32 {
            let address = 0x40_1000 + offset;
            for length in 1..=48 {
                let optimal = optimal_chunks(address, length);
                match split_range(address, length) {
                    Ok(chunks) => {
                        check_exact_cover(address, length, &chunks);
                        assert_eq!(chunks.len() as u64, optimal, "{address:#x}+{length}");
                    }
                    Err(RangeError::TooLarge { required }) => {
                        assert!(optimal > SLOT_COUNT as u64);
                        assert_eq!(required, optimal, "{address:#x}+{length}");
                    }
                    Err(error) => panic!("unexpected {error:?} for {address:#x}+{length}"),
                }
            }
        }

        assert_eq!(
            split_range(0x1003, 8),
            Ok(vec![
                Chunk {
                    address: 0x1003,
                    length: ChunkLength::One
                },
                Chunk {
                    address: 0x1004,
                    length: ChunkLength::Four
                },
                Chunk {
                    address: 0x1008,
                    length: ChunkLength::Two
                },
                Chunk {
                    address: 0x100a,
                    length: ChunkLength::One
                },
            ])
        );
    }

    #[test]
    fn range_boundaries_are_typed_failures() {
        assert_eq!(split_range(0x1000, 0), Err(RangeError::Empty));
        assert_eq!(split_range(u64::MAX - 3, 8), Err(RangeError::Overflow));
        assert!(split_range(USER_ADDRESS_LIMIT - 8, 8).is_ok());
        assert_eq!(
            split_range(USER_ADDRESS_LIMIT - 4, 8),
            Err(RangeError::OutsideUserSpace)
        );
        assert_eq!(
            split_range(USER_ADDRESS_LIMIT, 1),
            Err(RangeError::OutsideUserSpace)
        );
        assert_eq!(
            split_range(0xffff_ffff_8100_0000, 8),
            Err(RangeError::OutsideUserSpace)
        );
        assert_eq!(
            split_range(0x1000, 1 << 40),
            Err(RangeError::TooLarge {
                required: (1 << 40) / 8
            })
        );
    }

    #[test]
    fn control_words_match_values_the_kernel_accepted() {
        let chunk = |address| Chunk {
            address,
            length: ChunkLength::Eight,
        };

        let mut plan = DebugRegisterPlan::default();
        plan = plan
            .with_watchpoint(WatchpointId::new(1), &[chunk(0x1000)], SlotAccess::Write)
            .unwrap();

        // Observed DR7 readbacks after arming one and four 8-byte write slots.
        assert_eq!(plan.control(), 0x9_0001);
        for (id, address) in [(2, 0x1008), (3, 0x1010), (4, 0x1018)] {
            plan = plan
                .with_watchpoint(WatchpointId::new(id), &[chunk(address)], SlotAccess::Write)
                .unwrap();
        }

        assert_eq!(plan.control(), 0x9999_0055);

        let one_byte = DebugRegisterPlan::default()
            .with_watchpoint(
                WatchpointId::new(1),
                &[Chunk {
                    address: 0x1003,
                    length: ChunkLength::One,
                }],
                SlotAccess::ReadWrite,
            )
            .unwrap();
        assert_eq!(one_byte.control(), 0x3_0001);

        let four_bytes_slot_one = DebugRegisterPlan::default()
            .with_watchpoint(
                WatchpointId::new(1),
                &[
                    Chunk {
                        address: 0x1000,
                        length: ChunkLength::Two,
                    },
                    Chunk {
                        address: 0x1004,
                        length: ChunkLength::Four,
                    },
                ],
                SlotAccess::Write,
            )
            .unwrap();
        assert_eq!(four_bytes_slot_one.control(), 0xd5_0005);
    }

    #[test]
    fn identical_chunks_share_slots_until_their_last_owner_is_removed() {
        let first = WatchpointId::new(1);
        let second = WatchpointId::new(2);
        let chunks = split_range(0x2000, 16).unwrap();
        let plan = DebugRegisterPlan::default()
            .with_watchpoint(first, &chunks, SlotAccess::Write)
            .unwrap()
            .with_watchpoint(second, &chunks, SlotAccess::Write)
            .unwrap();
        assert_eq!(plan.free_slots(), 2);

        let read_write = plan
            .with_watchpoint(WatchpointId::new(3), &chunks, SlotAccess::ReadWrite)
            .unwrap();
        assert_eq!(
            read_write.free_slots(),
            0,
            "access kinds never share a slot"
        );

        let after_first = plan.without_watchpoint(first);
        assert_eq!(after_first.free_slots(), 2);
        assert_eq!(after_first.control(), plan.control());
        assert!(after_first.without_watchpoint(second).is_empty());
    }

    #[test]
    fn exhausted_capacity_reports_requirements_and_leaves_the_plan_unchanged() {
        let plan = DebugRegisterPlan::default()
            .with_watchpoint(
                WatchpointId::new(1),
                &split_range(0x3000, 24).unwrap(),
                SlotAccess::Write,
            )
            .unwrap();
        assert_eq!(
            plan.with_watchpoint(
                WatchpointId::new(2),
                &split_range(0x4000, 16).unwrap(),
                SlotAccess::Write
            ),
            Err(CapacityError {
                required: 2,
                available: 1
            })
        );
    }

    #[test]
    fn freed_slots_are_reused_from_the_lowest_index() {
        let chunk = |address| {
            [Chunk {
                address,
                length: ChunkLength::Eight,
            }]
        };

        let mut plan = DebugRegisterPlan::default();

        for id in 1..=4 {
            plan = plan
                .with_watchpoint(
                    WatchpointId::new(id),
                    &chunk(0x1000 * id),
                    SlotAccess::Write,
                )
                .unwrap();
        }

        plan = plan
            .without_watchpoint(WatchpointId::new(2))
            .without_watchpoint(WatchpointId::new(4));
        plan = plan
            .with_watchpoint(WatchpointId::new(5), &chunk(0x9000), SlotAccess::Write)
            .unwrap();
        assert_eq!(
            plan.slots()[1].as_ref().map(|slot| slot.chunk.address),
            Some(0x9000)
        );
        assert!(plan.slots()[3].is_none());
    }

    #[test]
    fn status_decoding_attributes_every_hit_and_refuses_unknown_slots() {
        let first = WatchpointId::new(1);
        let second = WatchpointId::new(2);
        let shared = WatchpointId::new(3);
        let plan = DebugRegisterPlan::default()
            .with_watchpoint(first, &split_range(0x5000, 8).unwrap(), SlotAccess::Write)
            .unwrap()
            .with_watchpoint(second, &split_range(0x5008, 8).unwrap(), SlotAccess::Write)
            .unwrap()
            .with_watchpoint(shared, &split_range(0x5008, 8).unwrap(), SlotAccess::Write)
            .unwrap();

        assert_eq!(
            plan.owners_for_status(STATUS_IDLE | 0b11),
            Ok(BTreeSet::from([first, second, shared]))
        );
        assert_eq!(
            plan.owners_for_status(STATUS_IDLE | 1 << 14 | 0b01),
            Ok(BTreeSet::from([first]))
        );
        assert_eq!(plan.owners_for_status(STATUS_IDLE), Ok(BTreeSet::new()));
        assert_eq!(
            plan.owners_for_status(STATUS_IDLE | 0b101),
            Err(UnknownSlots {
                status: STATUS_IDLE | 0b101
            })
        );
        assert!(status_has_hits(0b1000));
        assert!(!status_has_hits(STATUS_IDLE | 1 << 14));
    }

    /// Applies a long deterministic pseudo-random sequence of arm, disarm,
    /// and decode operations, checking every invariant after each one.
    #[test]
    fn random_operation_sequences_preserve_slot_ownership_invariants() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut plan = DebugRegisterPlan::default();
        let mut armed = std::collections::BTreeMap::new();
        let mut capacity_errors = 0;

        for _ in 0..20_000 {
            let id = WatchpointId::new(next() % 6);
            match next() % 4 {
                0 | 1 => {
                    if armed.contains_key(&id) {
                        continue;
                    }

                    // Few distinct addresses so identical chunks are shared.
                    let address = 0x1000 + (next() % 24);
                    let length = 1 + next() % 12;
                    let access = if next() % 3 == 0 {
                        SlotAccess::ReadWrite
                    } else {
                        SlotAccess::Write
                    };

                    let Ok(chunks) = split_range(address, length) else {
                        continue;
                    };

                    match plan.with_watchpoint(id, &chunks, access) {
                        Ok(next_plan) => {
                            plan = next_plan;
                            armed.insert(id, (chunks, access));
                        }
                        Err(error) => {
                            assert!(error.required > error.available);
                            capacity_errors += 1;
                        }
                    }
                }
                2 => {
                    plan = plan.without_watchpoint(id);
                    armed.remove(&id);
                }
                _ => {
                    let status = STATUS_IDLE | (next() % 16);
                    if let Ok(owners) = plan.owners_for_status(status) {
                        for owner in &owners {
                            assert!(armed.contains_key(owner));
                        }
                    }
                }
            }

            assert_plan_invariants(&plan, &armed);
        }

        assert!(capacity_errors > 0, "the sequence exercised exhaustion");
    }
}
