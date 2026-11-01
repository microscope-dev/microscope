use std::cell::RefCell;

use nix::errno::Errno;
use proptest::prelude::*;

use super::*;
use crate::test_strategies::u64_edges;

const WORD: usize = 8;

/// Memory as bytes from `base` on, each readable or not. Bytes outside
/// are unreadable. A readable byte is writable too.
#[derive(Clone, Debug)]
struct Bytes {
    base: u64,
    bytes: Vec<Option<u8>>,
}

impl Bytes {
    fn get(&self, address: u64) -> Option<u8> {
        let offset = usize::try_from(address.checked_sub(self.base)?).ok()?;
        self.bytes.get(offset).copied().flatten()
    }

    fn set(&mut self, address: u64, byte: u8) {
        let offset = usize::try_from(address - self.base).expect("an offset in the model");
        let slot = self.bytes[offset].as_mut().expect("a readable byte");
        *slot = byte;
    }

    /// The bytes of a word at any address, or `None` unless all are
    /// readable, as `PTRACE_PEEKDATA` reads them.
    fn word_bytes(&self, address: u64) -> Option<[u8; WORD]> {
        let mut word = [0; WORD];
        for (offset, byte) in word.iter_mut().enumerate() {
            *byte = self.get(address.checked_add(offset as u64)?)?;
        }
        Some(word)
    }

    fn peek(&self, address: u64) -> Result<u64> {
        self.word_bytes(address)
            .map(u64::from_le_bytes)
            .ok_or_else(|| backend_error(LinuxError::System(Errno::EIO)))
    }

    fn poke(&mut self, address: u64, value: u64) -> Result<()> {
        self.word_bytes(address)
            .ok_or_else(|| backend_error(LinuxError::System(Errno::EIO)))?;
        for (offset, byte) in value.to_le_bytes().into_iter().enumerate() {
            self.set(address + offset as u64, byte);
        }
        Ok(())
    }

    /// An aligned word's readable leading bytes, as a core dump's word
    /// reader reports them.
    fn memory_word(&self, address: u64) -> std::result::Result<u64, MemoryAccessError> {
        let mut word = [0; WORD];
        let mut readable = 0;
        while readable < WORD {
            let Some(byte) = address
                .checked_add(readable as u64)
                .and_then(|address| self.get(address))
            else {
                break;
            };
            word[readable] = byte;
            readable += 1;
        }

        match readable {
            WORD => Ok(u64::from_le_bytes(word)),
            0 => Err(MemoryAccessError::Inaccessible),
            _ => Err(MemoryAccessError::Partial {
                word: u64::from_le_bytes(word),
                readable,
            }),
        }
    }
}

/// A target whose memory is the model: block reads succeed only when every
/// byte is readable.
struct Target<'a>(&'a Bytes);

impl InspectionOps for Target<'_> {
    fn read_word(&self, _pid: Pid, address: u64) -> Result<u64> {
        self.0.peek(address)
    }

    fn read_memory_word(
        &self,
        _pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        self.0.memory_word(address)
    }

    fn read_block(&self, _pid: Pid, address: u64, size: usize) -> Option<Vec<u8>> {
        (0..size as u64)
            .map(|offset| self.0.get(address.checked_add(offset)?))
            .collect()
    }

    fn registers(&self, _pid: Pid) -> Result<nix::libc::user_regs_struct> {
        Err(Error::NotStopped)
    }
}

/// What a read of `size` bytes at `address` must return: each byte as
/// `user` shows it, up to the first unreadable one.
fn expected_read(
    memory: &Bytes,
    user: impl Fn(u64, u8) -> u8,
    address: u64,
    size: usize,
) -> Option<(Vec<u8>, MemoryReadCompletion)> {
    let end = address.checked_add(size as u64)?;
    let mut bytes = Vec::new();
    for current in address..end {
        let Some(byte) = memory.get(current) else {
            let completion = MemoryReadCompletion::Incomplete {
                next_address: VirtualAddress::new(current),
                reason: MemoryReadUnavailableReason::Inaccessible,
            };
            return Some((bytes, completion));
        };
        bytes.push(user(current, byte));
    }

    Some((bytes, MemoryReadCompletion::Complete))
}

/// Reads through both paths, the word reader's and the block reader's, and
/// checks that each returns `expected`.
fn check_reads(
    memory: &Bytes,
    sites: &BTreeMap<VirtualAddress, BreakpointSite>,
    address: u64,
    size: usize,
    expected: Option<(Vec<u8>, MemoryReadCompletion)>,
) {
    let address = VirtualAddress::new(address);
    let words = read_logical_memory_with(address, size, sites, |word| memory.memory_word(word));
    let block = read_logical_block(&Target(memory), Pid::from_raw(1), sites, address, size);

    for (path, read) in [("word", words), ("block", block)] {
        match (&expected, read) {
            (Some((bytes, completion)), Ok(read)) => {
                assert_eq!(&read.bytes, bytes, "{path} read of {size} at {address}");
                assert_eq!(
                    &read.completion, completion,
                    "{path} read of {size} at {address}"
                );
            }
            (None, Err(Error::AddressOverflow)) => {}
            (expected, read) => panic!(
                "{path} read of {size} at {address}: expected {expected:?}, got {:?}",
                read.map(|read| (read.bytes, read.completion))
            ),
        }
    }
}

/// A base address for `words` words, drawn toward the edges of the address
/// space, so that the last word may end at `u64::MAX`.
fn base_for(words: usize) -> impl Strategy<Value = u64> {
    u64_edges().prop_map(move |edge| {
        (edge & !(WORD as u64 - 1)).min(((words * WORD) as u64).wrapping_neg())
    })
}

/// Memory of 1 to 6 words, each with this many readable leading bytes.
fn memory(readable: impl Strategy<Value = usize> + Clone) -> impl Strategy<Value = Bytes> {
    (1..=6_usize)
        .prop_flat_map(move |words| {
            (
                base_for(words),
                prop::collection::vec((readable.clone(), any::<[u8; WORD]>()), words),
            )
        })
        .prop_map(|(base, words)| Bytes {
            base,
            bytes: words
                .into_iter()
                .flat_map(|(readable, word)| {
                    word.into_iter()
                        .enumerate()
                        .map(move |(offset, byte)| (offset < readable).then_some(byte))
                })
                .collect(),
        })
}

/// A read inside the model's bytes, of any size up to past their end.
fn read_in(memory: &Bytes) -> impl Strategy<Value = (u64, usize)> + use<> {
    let length = memory.bytes.len();
    let base = memory.base;
    (0..length, 0..=length + WORD).prop_map(move |(offset, size)| (base + offset as u64, size))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A read hides each installed trap and stops at the first byte it
    /// cannot read, alike a word at a time and in one block.
    #[test]
    fn reads_hide_traps_and_stop_at_the_first_unreadable_byte(
        (mut memory, sites, (address, size)) in memory(prop_oneof![
            3 => Just(WORD),
            1 => Just(0),
            1 => 1..WORD,
        ])
        .prop_flat_map(|memory| {
            let sites = prop::collection::vec(
                (0..memory.bytes.len(), any::<u8>(), any::<bool>()),
                0..6,
            );
            let read = read_in(&memory);
            (Just(memory), sites, read)
        })
    ) {
        let sites = sites
            .into_iter()
            .map(|(offset, original_byte, installed)| {
                let site = BreakpointSite {
                    original_byte,
                    installed,
                    owners: BTreeSet::new(),
                };
                (VirtualAddress::new(memory.base + offset as u64), site)
            })
            .collect::<BTreeMap<_, _>>();
        for (address, site) in &sites {
            if site.installed && memory.get(address.get()).is_some() {
                memory.set(address.get(), BREAKPOINT_OPCODE);
            }
        }

        let user = |address, byte| match sites.get(&VirtualAddress::new(address)) {
            Some(site) if site.installed => site.original_byte,
            _ => byte,
        };
        let expected = expected_read(&memory, user, address, size);
        check_reads(&memory, &sites, address, size, expected);
    }
}

fn peek(memory: &RefCell<Bytes>) -> impl Fn(u64) -> Result<u64> + '_ {
    |word| memory.borrow().peek(word)
}

fn poke(memory: &RefCell<Bytes>) -> impl Fn(u64, u64) -> Result<()> + '_ {
    |word, value| memory.borrow_mut().poke(word, value)
}

/// A write to the last word of the address space ends with it.
#[test]
fn a_write_to_the_last_word_of_the_address_space_ends_with_it() {
    let memory = RefCell::new(Bytes {
        base: u64::MAX - 7,
        bytes: vec![Some(0); WORD],
    });
    let written = write_logical_memory_with(
        &mut BTreeMap::new(),
        VirtualAddress::new(u64::MAX - 7),
        &[1],
        peek(&memory),
        poke(&memory),
    );
    assert_eq!(written.expect("a write"), 1);
    assert_eq!(memory.borrow().get(u64::MAX - 7), Some(1));
}
