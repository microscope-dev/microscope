use std::cell::RefCell;

use nix::errno::Errno;
use proptest::prelude::*;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest, prop_state_machine};

use super::*;
use crate::protocol::BreakpointId;
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
    expected: Option<&(Vec<u8>, MemoryReadCompletion)>,
) {
    let address = VirtualAddress::new(address);
    let words = read_logical_memory_with(address, size, sites, |word| memory.memory_word(word));
    let block = read_logical_block(&Target(memory), Pid::from_raw(1), sites, address, size);

    for (path, read) in [("word", words), ("block", block)] {
        match (expected, read) {
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
                    trap: if installed {
                        Trap::Installed
                    } else {
                        Trap::Lifted(Lift::Repair)
                    },
                    owners: BTreeSet::new(),
                };
                (VirtualAddress::new(memory.base + offset as u64), site)
            })
            .collect::<BTreeMap<_, _>>();
        for (address, site) in &sites {
            if site.trap == Trap::Installed && memory.get(address.get()).is_some() {
                memory.set(address.get(), BREAKPOINT_OPCODE);
            }
        }

        let user = |address, byte| match sites.get(&VirtualAddress::new(address)) {
            Some(site) if site.trap == Trap::Installed => site.original_byte,
            _ => byte,
        };
        let expected = expected_read(&memory, user, address, size);
        check_reads(&memory, &sites, address, size, expected.as_ref());
    }
}

/// The user's view of memory, the owners of each site, and why each
/// lifted site's trap is out, for a live process: readability changes
/// only between words.
#[derive(Clone, Debug)]
struct World {
    user: Bytes,
    sites: BTreeMap<u64, BTreeSet<BreakpointOwner>>,
    lifted: BTreeMap<u64, Lift>,
}

impl World {
    fn physical(&self) -> Bytes {
        let mut physical = self.user.clone();
        for &address in self.sites.keys() {
            if !self.lifted.contains_key(&address) {
                physical.set(address, BREAKPOINT_OPCODE);
            }
        }
        physical
    }

    fn trap(&self, address: u64) -> Trap {
        self.lifted
            .get(&address)
            .map_or(Trap::Installed, |&lift| Trap::Lifted(lift))
    }

    /// Whether a trap can go at `address`: any readable byte takes one.
    fn install(&mut self, address: u64, owner: BreakpointOwner) -> bool {
        if self.user.get(address).is_none() {
            return false;
        }

        self.sites.entry(address).or_default().insert(owner);
        true
    }

    fn remove(&mut self, address: u64, owner: BreakpointOwner) -> bool {
        let owners = self.sites.get_mut(&address).expect("a site with the owner");
        assert!(
            owners.remove(&owner),
            "{owner:?} owns the site at {address:#x}"
        );
        let last = owners.is_empty();
        if last {
            self.sites.remove(&address);
            self.lifted.remove(&address);
        }
        last
    }

    /// Writes the bytes before the first unreadable one and returns how
    /// many, or `None` when the write would pass the end of the address
    /// space.
    fn write(&mut self, address: u64, bytes: &[u8]) -> Option<u64> {
        address.checked_add(bytes.len() as u64)?;
        let mut written = 0;
        for (offset, &byte) in bytes.iter().enumerate() {
            let target = address + offset as u64;
            if self.user.get(target).is_none() {
                break;
            }
            self.user.set(target, byte);
            written += 1;
        }
        Some(written)
    }

    /// How far a read of the whole model goes: the last byte of the address
    /// space cannot be read, as a read's end would overflow.
    fn whole(&self) -> (u64, usize) {
        let length = self.user.bytes.len();
        let end = self.user.base.checked_add(length as u64);
        (self.user.base, length - usize::from(end.is_none()))
    }
}

#[derive(Clone, Debug)]
enum Step {
    Install(u64, BreakpointOwner),
    RemoveOwner(u64, BreakpointOwner),
    Write(u64, Vec<u8>),
    Read(u64, usize),

    /// Lifts a site's trap, keeping the site. A lifted trap stays out for
    /// its first reason.
    Lift(u64, Lift),
    Reinstall(u64),
}

fn owner() -> impl Strategy<Value = BreakpointOwner> {
    prop_oneof![
        (1..=2_u64).prop_map(|id| BreakpointOwner::User(BreakpointId::new(id))),
        Just(BreakpointOwner::Loader),
    ]
}

struct Breakpoints;

impl ReferenceStateMachine for Breakpoints {
    type State = World;
    type Transition = Step;

    fn init_state() -> BoxedStrategy<World> {
        memory(prop_oneof![3 => Just(WORD), 1 => Just(0)])
            .prop_map(|user| World {
                user,
                sites: BTreeMap::new(),
                lifted: BTreeMap::new(),
            })
            .boxed()
    }

    fn transitions(world: &World) -> BoxedStrategy<Step> {
        let length = world.user.bytes.len();
        let base = world.user.base;
        let address = move || (0..length).prop_map(move |offset| base + offset as u64);
        let install =
            (address(), owner()).prop_map(|(address, owner)| Step::Install(address, owner));
        let write = (address(), prop::collection::vec(any::<u8>(), 0..=2 * WORD))
            .prop_map(|(address, bytes)| Step::Write(address, bytes));
        let read = read_in(&world.user).prop_map(|(address, size)| Step::Read(address, size));

        let owned = world
            .sites
            .iter()
            .flat_map(|(&address, owners)| owners.iter().map(move |&owner| (address, owner)))
            .collect::<Vec<_>>();
        if owned.is_empty() {
            return prop_oneof![3 => install, 2 => write, 1 => read].boxed();
        }

        let addresses = world.sites.keys().copied().collect::<Vec<_>>();
        let lift = prop_oneof![Just(Lift::Repair), Just(Lift::Vfork)];
        prop_oneof![
            3 => install,
            2 => prop::sample::select(owned)
                .prop_map(|(address, owner)| Step::RemoveOwner(address, owner)),
            2 => write,
            1 => read,
            1 => (prop::sample::select(addresses.clone()), lift)
                .prop_map(|(address, lift)| Step::Lift(address, lift)),
            1 => prop::sample::select(addresses).prop_map(Step::Reinstall),
        ]
        .boxed()
    }

    fn preconditions(world: &World, step: &Step) -> bool {
        match step {
            Step::RemoveOwner(address, owner) => world
                .sites
                .get(address)
                .is_some_and(|owners| owners.contains(owner)),
            Step::Lift(address, _) | Step::Reinstall(address) => world.sites.contains_key(address),
            Step::Install(..) | Step::Write(..) | Step::Read(..) => true,
        }
    }

    fn apply(mut world: World, step: &Step) -> World {
        match step {
            Step::Install(address, owner) => {
                world.install(*address, *owner);
            }
            Step::RemoveOwner(address, owner) => {
                world.remove(*address, *owner);
            }
            Step::Write(address, bytes) => {
                world.write(*address, bytes);
            }
            Step::Lift(address, lift) => {
                world.lifted.entry(*address).or_insert(*lift);
            }
            Step::Reinstall(address) => {
                world.lifted.remove(address);
            }
            Step::Read(..) => {}
        }
        world
    }
}

/// Physical memory and the sites the production code keeps, with the world
/// before the latest step, which says what that step must report.
struct Process {
    memory: RefCell<Bytes>,
    sites: BTreeMap<VirtualAddress, BreakpointSite>,
    before: World,
}

fn peek(memory: &RefCell<Bytes>) -> impl Fn(u64) -> Result<u64> + '_ {
    |word| memory.borrow().peek(word)
}

fn poke(memory: &RefCell<Bytes>) -> impl Fn(u64, u64) -> Result<()> + '_ {
    |word, value| memory.borrow_mut().poke(word, value)
}

impl StateMachineTest for Process {
    type SystemUnderTest = Self;
    type Reference = Breakpoints;

    fn init_test(world: &World) -> Self {
        Self {
            memory: RefCell::new(world.physical()),
            sites: BTreeMap::new(),
            before: world.clone(),
        }
    }

    fn apply(mut process: Self, world: &World, step: Step) -> Self {
        let mut expected = process.before.clone();
        match step {
            Step::Install(address, owner) => {
                let installs = expected.install(address, owner);
                let memory = &process.memory;
                let result = install_site(
                    &mut process.sites,
                    VirtualAddress::new(address),
                    owner,
                    peek(memory),
                    poke(memory),
                );
                assert_eq!(
                    result.is_ok(),
                    installs,
                    "installing at {address:#x}: {result:?}"
                );
            }
            Step::RemoveOwner(address, owner) => {
                let last = expected.remove(address, owner);
                let memory = &process.memory;
                let removed = remove_site_owner(
                    &mut process.sites,
                    VirtualAddress::new(address),
                    owner,
                    |sites| {
                        remove_site(
                            sites,
                            VirtualAddress::new(address),
                            peek(memory),
                            poke(memory),
                        )
                    },
                )
                .expect("an owner's removal");
                assert_eq!(
                    removed.is_some(),
                    last,
                    "removing {owner:?} at {address:#x}"
                );
            }
            Step::Write(address, bytes) => {
                let written = expected.write(address, &bytes);
                let memory = &process.memory;
                let result = write_logical_memory_with(
                    &mut process.sites,
                    VirtualAddress::new(address),
                    &bytes,
                    peek(memory),
                    poke(memory),
                );
                match (written, result) {
                    (None, Err(Error::AddressOverflow)) => {}
                    (Some(0), Err(Error::MemoryNotWritable(_))) if !bytes.is_empty() => {}
                    (Some(written), Ok(count)) if count == written => {}
                    (written, result) => panic!(
                        "writing {} bytes at {address:#x}: expected {written:?}, got {result:?}",
                        bytes.len()
                    ),
                }
            }
            Step::Read(address, size) => {
                check_reads(
                    &process.memory.borrow(),
                    &process.sites,
                    address,
                    size,
                    expected_read(&expected.user, |_, byte| byte, address, size).as_ref(),
                );
            }
            Step::Lift(address, lift) => {
                lift_site(
                    &mut process.sites,
                    VirtualAddress::new(address),
                    lift,
                    peek(&process.memory),
                    poke(&process.memory),
                )
                .expect("lifting a trap");
            }
            Step::Reinstall(address) => {
                reinstall_site(
                    &mut process.sites,
                    VirtualAddress::new(address),
                    peek(&process.memory),
                    poke(&process.memory),
                )
                .expect("reinstalling a trap");
            }
        }

        process.before = world.clone();
        process
    }

    fn check_invariants(process: &Self, world: &World) {
        let memory = process.memory.borrow();
        let physical = world.physical();
        assert_eq!(
            memory.bytes, physical.bytes,
            "physical memory holds traps only at sites"
        );
        if world.sites.is_empty() {
            assert_eq!(
                memory.bytes, world.user.bytes,
                "with no sites, memory is the user's"
            );
        }

        let kept = process
            .sites
            .iter()
            .map(|(address, site)| {
                assert_eq!(
                    site.trap,
                    world.trap(address.get()),
                    "the site at {address} is installed or lifted as it was left"
                );
                assert_eq!(
                    Some(site.original_byte),
                    world.user.get(address.get()),
                    "the site at {address} saves the user's byte"
                );
                (address.get(), site.owners.clone())
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(kept, world.sites, "sites and their owners");

        let (address, size) = world.whole();
        let expected = expected_read(&world.user, |_, byte| byte, address, size);
        check_reads(&memory, &process.sites, address, size, expected.as_ref());
    }
}

prop_state_machine! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Installs, removals, lifts, reinstalls, and writes keep traps exactly
    /// at the sites whose traps are in, and every read shows the bytes the
    /// user wrote.
    #[test]
    fn traps_and_writes_keep_the_users_view(sequential 1..24 => Process);
}
