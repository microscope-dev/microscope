//! The runtime the macros call: the control block, each thread's ring, and
//! the paths that write them.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{
    AtomicPtr, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering, compiler_fence,
};

use std::sync::{Once, OnceLock};

use crate::abi;

/// The control block microscope finds in the program's file and writes.
#[repr(C, align(64))]
pub struct Control {
    magic: [u8; 8],
    abi: u16,
    clock: AtomicU16,
    enable: AtomicU32,
    ring_size: AtomicU32,
    rings: AtomicU32,
    heartbeat: AtomicU64,
    dropped: AtomicU64,
    capacity: u32,
    exiting: AtomicU32,
    flushed: AtomicU32,
    timeout: AtomicU32,
    sites: AtomicPtr<Sites>,
    registry: [AtomicPtr<Header>; abi::MOST_RINGS],
}

#[unsafe(link_section = "microscope_probe_control")]
#[used]
static CONTROL: Control = Control {
    magic: abi::MAGIC,
    abi: abi::ABI,
    clock: AtomicU16::new(abi::CLOCK_MONOTONIC),
    enable: AtomicU32::new(0),
    ring_size: AtomicU32::new(abi::DEFAULT_RING_SIZE),
    rings: AtomicU32::new(0),
    heartbeat: AtomicU64::new(0),
    dropped: AtomicU64::new(0),

    #[expect(clippy::cast_possible_truncation, reason = "the registry is small")]
    capacity: abi::MOST_RINGS as u32,
    exiting: AtomicU32::new(0),
    flushed: AtomicU32::new(0),
    timeout: AtomicU32::new(0),
    sites: AtomicPtr::new(ptr::null_mut()),
    registry: [const { AtomicPtr::new(ptr::null_mut()) }; abi::MOST_RINGS],
};

const _: () = assert!(core::mem::offset_of!(Control, registry) == abi::CONTROL_SIZE);
const _: () = assert!(core::mem::offset_of!(Control, sites) == 56);

/// The run-time sites: [`abi::SITES_HEADER`]'s header, then the records.
#[repr(C, align(16))]
struct Sites {
    magic: u32,
    version: u8,
    reserved: [u8; 3],
    capacity: u32,
    count: AtomicU32,
    records: [Site; abi::MOST_RUNTIME_SITES],
}

/// A site a program defines as it runs, which [`define`] gives.
#[repr(C)]
pub struct Site {
    kind: AtomicU8,
    pub(crate) state: AtomicU8,
    flags: AtomicU16,
    line: AtomicU32,
    name: AtomicU64,
    name_length: AtomicU32,
    file_length: AtomicU32,
    file: AtomicU64,

    /// Its key, which the ABI leaves out.
    key: AtomicU32,
    reserved: [AtomicU32; 3],
}

const _: () = assert!(core::mem::size_of::<Site>() == abi::RUNTIME_SITE_SIZE);
const _: () = assert!(core::mem::offset_of!(Sites, records) == abi::SITES_HEADER);

static SITES: Sites = Sites {
    magic: abi::SITES_MAGIC,
    version: abi::SITES_VERSION,
    reserved: [0; 3],
    #[expect(clippy::cast_possible_truncation, reason = "the table is small")]
    capacity: abi::MOST_RUNTIME_SITES as u32,
    count: AtomicU32::new(0),
    records: [const {
        Site {
            kind: AtomicU8::new(abi::SITE_NONE),
            state: AtomicU8::new(0),
            flags: AtomicU16::new(0),
            line: AtomicU32::new(0),
            name: AtomicU64::new(0),
            name_length: AtomicU32::new(0),
            file_length: AtomicU32::new(0),
            file: AtomicU64::new(0),
            key: AtomicU32::new(0),
            reserved: [const { AtomicU32::new(0) }; 3],
        }
    }; abi::MOST_RUNTIME_SITES],
};

/// The run-time sites defined so far, with their name and file, which also
/// keeps one definition at a time.
type Definitions = Vec<(&'static str, &'static str, &'static Site)>;
static DEFINED: std::sync::Mutex<Definitions> = std::sync::Mutex::new(Vec::new());

/// The run-time site of `kind`, a `SITE_` constant other than
/// [`abi::SITE_NONE`], named `name` at `file`'s `line`: the one defined
/// before with all four, or a new one. None when the kind is not one,
/// either text is longer than [`abi::MOST_SITE_TEXT`], or every site is
/// taken. It is off until microscope switches it on, as a static site is.
pub fn define(kind: u8, name: &str, file: &str, line: u32) -> Option<&'static Site> {
    let longest = abi::MOST_SITE_TEXT as usize;
    if !(abi::SITE_ZONE..=abi::SITE_TRIGGER).contains(&kind)
        || name.len() > longest
        || file.len() > longest
    {
        return None;
    }

    let mut defined = DEFINED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Few sites are defined, each once, so a search costs little.
    if let Some(&(_, _, site)) = defined.iter().find(|(named, at, site)| {
        *named == name && *at == file && site.is(kind) && site.line.load(Ordering::Relaxed) == line
    }) {
        return Some(site);
    }

    let index = SITES.count.load(Ordering::Relaxed);
    let site = SITES.records.get(index as usize)?;

    // Its texts live as long as the program, as a static site's do.
    let name: &'static str = Box::leak(name.to_owned().into_boxed_str());
    let file: &'static str = Box::leak(file.to_owned().into_boxed_str());
    let address = |text: &str| ptr::from_ref(text).addr() as u64;
    site.kind.store(kind, Ordering::Relaxed);
    site.line.store(line, Ordering::Relaxed);
    site.name.store(address(name), Ordering::Relaxed);
    site.file.store(address(file), Ordering::Relaxed);
    #[expect(clippy::cast_possible_truncation, reason = "a text is bounded")]
    {
        site.name_length.store(name.len() as u32, Ordering::Relaxed);
        site.file_length.store(file.len() as u32, Ordering::Relaxed);
    }
    site.key.store(abi::RUNTIME_KEY | index, Ordering::Relaxed);

    let switched = CONTROL.enable.load(Ordering::SeqCst) & abi::ENABLE_DEFINED != 0;
    if switched {
        site.state.store(1, Ordering::Relaxed);
    }

    // Whole before it is counted, and counted before microscope is told
    // where to look.
    SITES.count.store(index + 1, Ordering::SeqCst);

    // Whoever switches every site off clears `enable` and then reads the
    // count. If that read came before this site was counted, `enable`
    // was cleared first, and this load sees it.
    if switched && CONTROL.enable.load(Ordering::SeqCst) & abi::ENABLE_DEFINED == 0 {
        site.state.store(0, Ordering::Relaxed);
    }
    CONTROL
        .sites
        .store(ptr::from_ref(&SITES).cast_mut(), Ordering::Release);

    defined.push((name, file, site));
    Some(site)
}

impl Site {
    /// Whether microscope switched the site on.
    #[must_use]
    #[inline]
    pub fn on(&self) -> bool {
        self.state.load(Ordering::Relaxed) != 0
    }

    pub(crate) fn key(&self) -> u32 {
        self.key.load(Ordering::Relaxed)
    }

    fn is(&self, kind: u8) -> bool {
        self.kind.load(Ordering::Relaxed) == kind
    }

    /// A span of the thread's time, from here until the guard drops, at a
    /// zone's site.
    pub fn zone(&self) -> Zone {
        let began =
            self.is(abi::SITE_ZONE) && self.on() && write(self.key(), abi::ZONE_BEGIN, 0, &[]);
        Zone(began.then(|| self.key()), PhantomData)
    }

    /// The end of a frame, at a frame's site, or a trigger, at a trigger's.
    pub fn event(&self) {
        let kind = if self.is(abi::SITE_FRAME) {
            abi::FRAME
        } else if self.is(abi::SITE_TRIGGER) {
            abi::TRIGGER
        } else {
            return;
        };
        if self.on() {
            write(self.key(), kind, 0, &[]);
        }
    }

    /// A value of the series, at a plot's site.
    pub fn plot(&self, value: f64) {
        if self.is(abi::SITE_PLOT) && self.on() {
            write(self.key(), abi::PLOT, 0, &value.to_le_bytes());
        }
    }

    /// A message on the thread's timeline, at a message's site, cut to 256
    /// bytes.
    pub fn message(&self, message: fmt::Arguments<'_>) {
        if self.is(abi::SITE_MESSAGE) && self.on() {
            text(self.key(), abi::MESSAGE, message);
        }
    }

    /// Labels the thread's time `name=value` until the guard drops, at a
    /// label's site.
    pub fn label(&self, value: &dyn fmt::Display) -> Label {
        let began = self.is(abi::SITE_LABEL)
            && self.on()
            && text(self.key(), abi::LABEL_BEGIN, format_args!("{value}"));
        Label(began.then(|| self.key()), PhantomData)
    }
}

/// A ring's header; its records follow it.
#[repr(C, align(128))]
pub struct Header {
    magic: u32,
    version: u8,
    size: u8,
    clock: AtomicU8,
    reserved: u8,
    thread: AtomicU32,
    state: AtomicU32,
    signal_drops: AtomicU64,

    /// The forks the process had made when the ring was made, which the
    /// ABI leaves out: a forked child takes none of its parent's rings.
    generation: AtomicU32,
    line_0: [u8; 36],
    head: AtomicU64,

    /// Whether the thread is writing a record, which a signal handler that
    /// interrupts it must not.
    writing: AtomicU8,
    line_1: [u8; 55],
}

const _: () = assert!(core::mem::size_of::<Header>() == abi::RING_HEADER);
const _: () = assert!(core::mem::offset_of!(Header, head) == 64);

/// A record, written as two words, which microscope reads as its 16 bytes.
#[repr(C)]
struct Record([AtomicU64; 2]);

const _: () = assert!(core::mem::size_of::<Record>() == abi::RECORD);

/// A ring: its header, then its records, a power of two of them. A ring,
/// once made, is never freed.
#[repr(C)]
struct Ring {
    header: Header,
    records: [Record],
}

/// The ring a thread writes to.
#[derive(Clone, Copy)]
enum Slot {
    /// None yet: the thread's first event takes one.
    Unset,

    /// None: the thread is exiting, or none was left.
    Nothing,
    Ring(&'static Ring),
}

thread_local! {
    static RING: Cell<Slot> = const { Cell::new(Slot::Unset) };
    static RETIRE: Retire = const { Retire };
}

/// Retires the thread's ring when the thread exits.
struct Retire;

impl Drop for Retire {
    fn drop(&mut self) {
        if let Slot::Ring(ring) = RING.with(|slot| slot.replace(Slot::Nothing)) {
            ring.header
                .state
                .store(abi::RING_RETIRED, Ordering::Release);
        }
    }
}

/// Every ring this process made, so that a thread can take one another
/// left: the ABI's registry holds their addresses for microscope, and this
/// the rings themselves.
static MADE: [OnceLock<&'static Ring>; abi::MOST_RINGS] =
    [const { OnceLock::new() }; abi::MOST_RINGS];
static MADE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The forks the process has made, by [`Header::generation`].
static FORKS: AtomicU32 = AtomicU32::new(0);

/// Whether microscope is recording the program's instrumentation: every site
/// it chose is on.
#[must_use]
pub fn listening() -> bool {
    CONTROL.enable.load(Ordering::Acquire) != 0
}

/// Whether the site whose state byte is `state` is on.
#[doc(hidden)]
#[inline(always)]
pub fn __on(state: &AtomicU8) -> bool {
    state.load(Ordering::Relaxed) != 0
}

/// What keeps a guard on the thread it began on, whose ring it ends in.
type ThreadBound = PhantomData<*const ()>;

/// The zone [`zone!`](crate::zone) opens, which ends when it drops.
#[must_use = "a zone ends when its guard drops"]
pub struct Zone(Option<u32>, ThreadBound);

impl Zone {
    #[doc(hidden)]
    #[inline(always)]
    pub fn begin(state: &'static AtomicU8) -> Self {
        let began = __on(state) && write(site(state), abi::ZONE_BEGIN, 0, &[]);
        Self(began.then(|| site(state)), PhantomData)
    }
}

impl Zone {
    /// Whether the zone began, leaving it to end as a separate call: for a
    /// caller across a language boundary, which ends it itself.
    #[cfg(feature = "c")]
    pub(crate) fn began(self) -> bool {
        let began = self.0.is_some();
        core::mem::forget(self);
        began
    }
}

impl Drop for Zone {
    #[inline(always)]
    fn drop(&mut self) {
        // A zone that began ends, whether or not its site is still on.
        if let Some(key) = self.0 {
            end(key, abi::ZONE_END);
        }
    }
}

/// The label [`label!`](crate::label) opens, which ends when it drops.
#[must_use = "a label ends when its guard drops"]
pub struct Label(Option<u32>, ThreadBound);

impl Label {
    #[doc(hidden)]
    #[inline(always)]
    pub fn begin(state: &'static AtomicU8, value: &dyn fmt::Display) -> Self {
        let began = __on(state) && text(site(state), abi::LABEL_BEGIN, format_args!("{value}"));
        Self(began.then(|| site(state)), PhantomData)
    }
}

impl Label {
    /// Whether the label began, leaving it to end as a separate call.
    #[cfg(feature = "c")]
    pub(crate) fn began(self) -> bool {
        let began = self.0.is_some();
        core::mem::forget(self);
        began
    }
}

impl Drop for Label {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(key) = self.0 {
            end(key, abi::LABEL_END);
        }
    }
}

/// Ends a zone or label out of line, so that a guard whose site was off
/// drops to one test, which the compiler then inlines.
#[inline(never)]
pub(crate) fn end(key: u32, kind: u8) {
    write(key, kind, 0, &[]);
}

#[doc(hidden)]
#[inline(always)]
pub fn __event(state: &'static AtomicU8, kind: u8) {
    if __on(state) {
        write(site(state), kind, 0, &[]);
    }
}

#[doc(hidden)]
#[inline]
pub fn __plot(state: &'static AtomicU8, value: f64) {
    write(site(state), abi::PLOT, 0, &value.to_le_bytes());
}

#[doc(hidden)]
#[inline(never)]
pub fn __message(state: &'static AtomicU8, message: fmt::Arguments<'_>) {
    text(site(state), abi::MESSAGE, message);
}

/// Writes an event whose argument is `text`, cut to its longest.
#[inline(never)]
fn text(key: u32, kind: u8, text: fmt::Arguments<'_>) -> bool {
    let mut cut = Cut {
        bytes: [0; abi::MOST_TEXT],
        length: 0,
    };

    // A cut text is the bytes that fit, as a full one would be.
    let _ = fmt::write(&mut cut, text);
    let text = &cut.bytes[..cut.length];
    #[expect(clippy::cast_possible_truncation, reason = "a text is short")]
    write(key, kind, text.len() as u32, text)
}

/// A text, cut where it would pass its longest, at a character's start.
struct Cut {
    bytes: [u8; abi::MOST_TEXT],
    length: usize,
}

impl fmt::Write for Cut {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = abi::MOST_TEXT - self.length;
        let mut take = text.len().min(room);
        while !text.is_char_boundary(take) {
            take -= 1;
        }

        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        if take < text.len() {
            Err(fmt::Error)
        } else {
            Ok(())
        }
    }
}

/// Writes one event of `kind` from the site whose key is `site` to the
/// thread's ring: a record, and `more` in the records after it. Whether
/// the ring took it.
#[inline(always)]
fn write(site: u32, kind: u8, argument: u32, more: &[u8]) -> bool {
    let mut slot = RING.with(Cell::get);
    if let Slot::Unset = slot {
        slot = acquire();
    }

    let Slot::Ring(ring) = slot else {
        CONTROL.dropped.fetch_add(1, Ordering::Relaxed);
        return false;
    };

    let header = &ring.header;
    if header.writing.load(Ordering::Relaxed) != 0 {
        header.signal_drops.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    header.writing.store(1, Ordering::Relaxed);
    compiler_fence(Ordering::SeqCst);
    let time = now(header.clock.load(Ordering::Relaxed));
    let head = header.head.load(Ordering::Relaxed);
    let mut record = [0; abi::RECORD];
    record[..8].copy_from_slice(&time.to_le_bytes());
    record[8..12].copy_from_slice(&site.to_le_bytes());
    record[12..].copy_from_slice(&(u32::from(kind) | argument << 8).to_le_bytes());
    ring.put(head, &record);

    let mut position = head + 1;
    for chunk in more.chunks(15) {
        let mut record = [0; abi::RECORD];
        let (low, high) = chunk.split_at(chunk.len().min(12));
        record[..low.len()].copy_from_slice(low);
        record[12] = abi::CONTINUED;
        record[13..13 + high.len()].copy_from_slice(high);
        ring.put(position, &record);
        position += 1;
    }

    header.head.store(position, Ordering::Release);
    compiler_fence(Ordering::SeqCst);
    header.writing.store(0, Ordering::Relaxed);

    // Every so often, the thread asks whether microscope is still there.
    if (head ^ position) >> 10 != 0 {
        check_heartbeat();
    }

    true
}

impl Ring {
    /// Writes `record` at `position`, as the ring wraps.
    #[inline(always)]
    fn put(&self, position: u64, record: &[u8; abi::RECORD]) {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a position is masked to the ring"
        )]
        let index = position as usize & (self.records.len() - 1);
        let (low, high) = record.split_at(8);
        let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().unwrap_or_default());
        let slot = &self.records[index].0;
        slot[0].store(word(low), Ordering::Relaxed);
        slot[1].store(word(high), Ordering::Relaxed);
    }
}

/// A static site's key: its state byte's offset from the control block.
#[inline(always)]
fn site(state: &'static AtomicU8) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a module is under 1 GiB, as ABI 2 requires"
    )]
    let offset =
        (ptr::from_ref(state).addr() as i64 - ptr::from_ref(&CONTROL).addr() as i64) as i32;
    offset.cast_unsigned()
}

/// The time now, by `clock`.
#[inline(always)]
fn now(clock: u8) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if u16::from(clock) == abi::CLOCK_TSC {
        // SAFETY: RDTSC reads no memory. Every x86-64 processor has it; a
        // process that `PR_SET_TSC` forbade it faults, which microscope
        // does not ask for and is not undefined behavior.
        return unsafe { core::arch::x86_64::_rdtsc() };
    }

    let _ = clock;
    monotonic()
}

#[repr(C)]
struct Timespec {
    seconds: i64,
    nanoseconds: i64,
}

// SAFETY: each declaration matches its C function's. `clock_gettime`
// writes only through its pointer, which a `&mut` makes valid, and the
// handlers are functions, which live for the program.
unsafe extern "C" {
    safe fn clock_gettime(clock: i32, time: &mut Timespec) -> i32;
    safe fn gettid() -> i32;
    #[cfg(not(miri))]
    safe fn atexit(handler: extern "C" fn()) -> i32;
    #[cfg(not(miri))]
    safe fn pthread_atfork(
        prepare: Option<extern "C" fn()>,
        parent: Option<extern "C" fn()>,
        child: Option<extern "C" fn()>,
    ) -> i32;
}

/// Miri runs neither, and its tests never fork or exit.
#[cfg(miri)]
fn atexit(_: extern "C" fn()) -> i32 {
    0
}

#[cfg(miri)]
fn pthread_atfork(
    _: Option<extern "C" fn()>,
    _: Option<extern "C" fn()>,
    _: Option<extern "C" fn()>,
) -> i32 {
    0
}

/// Nanoseconds of `CLOCK_MONOTONIC`.
fn monotonic() -> u64 {
    const CLOCK_MONOTONIC: i32 = 1;
    let mut time = Timespec {
        seconds: 0,
        nanoseconds: 0,
    };

    clock_gettime(CLOCK_MONOTONIC, &mut time);
    (time.seconds.cast_unsigned())
        .wrapping_mul(1_000_000_000)
        .wrapping_add(time.nanoseconds.cast_unsigned())
}

/// Switches every site off once microscope has gone quiet.
#[cold]
fn check_heartbeat() {
    let heartbeat = CONTROL.heartbeat.load(Ordering::Relaxed);
    let timeout = match CONTROL.timeout.load(Ordering::Relaxed) {
        0 => abi::STALE_NANOSECONDS,
        milliseconds => u64::from(milliseconds) * 1_000_000,
    };

    if CONTROL.enable.load(Ordering::Relaxed) != 0
        && monotonic().saturating_sub(heartbeat) > timeout
    {
        disable();
    }
}

#[cfg(not(miri))]
unsafe extern "C" {
    static __start_microscope_sites: [u8; 0];
    static __stop_microscope_sites: [u8; 0];
}

// The SDK's own site, so that the section is always there.
static SITE: &AtomicU8 = crate::__site!(crate::abi::SITE_NONE, "microscope_probes");

/// Switches every site off.
fn disable() {
    // Cleared before the count is read: see `define`.
    CONTROL.enable.store(0, Ordering::SeqCst);
    switch_sites_off();
    let defined = SITES.count.load(Ordering::SeqCst) as usize;
    for site in SITES.records.iter().take(defined) {
        site.state.store(0, Ordering::Relaxed);
    }
    let _ = SITE;
}

/// Writes zero to every site's state byte. Miri has no section to walk.
#[cfg(miri)]
fn switch_sites_off() {}

#[cfg(not(miri))]
fn switch_sites_off() {
    let start = (&raw const __start_microscope_sites).cast::<u8>();
    let stop = (&raw const __stop_microscope_sites).cast::<u8>();

    // SAFETY: the linker gathers every site into the section, whole, and
    // each site's state offset names an `AtomicU8` static in its module.
    unsafe { switch_off(start, stop) };
}

/// Writes zero to the state byte of each site in the table from `start` to
/// `stop`.
///
/// The table and the state bytes are separate statics, which Rust's model
/// does not let one pointer step between, so the walk steps with wrapping
/// arithmetic, which promises the compiler nothing about where it lands.
///
/// # Safety
///
/// `start..stop` holds whole sites, and each site's state offset, from its
/// field, names an `AtomicU8` that lives through the call.
unsafe fn switch_off(start: *const u8, stop: *const u8) {
    /// Where a site's state offset lies in it.
    const STATE_FIELD: usize = 24;

    let mut site = start;
    while site < stop {
        let field = site.wrapping_add(STATE_FIELD);

        // SAFETY: the caller guarantees a whole site here, whose state
        // offset is four bytes at any alignment, and the `AtomicU8` it
        // names.
        unsafe {
            let offset = field.cast::<i32>().read_unaligned();
            let state = &*field.wrapping_offset(offset as isize).cast::<AtomicU8>();
            state.store(0, Ordering::Relaxed);
        }

        site = site.wrapping_add(abi::SITE_SIZE);
    }
}

static ATFORK: Once = Once::new();

/// A child of `fork` records nothing until microscope says so, and never into
/// its parent's rings.
extern "C" fn forked() {
    disable();
    FORKS.fetch_add(1, Ordering::Relaxed);
    for ring in &CONTROL.registry {
        ring.store(ptr::null_mut(), Ordering::Relaxed);
    }

    CONTROL.rings.store(0, Ordering::Relaxed);
    let _ = RING.try_with(|slot| slot.set(Slot::Unset));
}

/// A program that exits while microscope reads it waits a moment for microscope
/// to read what its threads wrote last.
extern "C" fn exiting() {
    let fresh = |now: u64| {
        now.saturating_sub(CONTROL.heartbeat.load(Ordering::Relaxed)) < abi::FRESH_NANOSECONDS
    };

    let start = monotonic();
    if CONTROL.enable.load(Ordering::Relaxed) == 0 || !fresh(start) {
        return;
    }

    CONTROL.exiting.store(1, Ordering::Release);

    while CONTROL.flushed.load(Ordering::Acquire) == 0
        && monotonic().saturating_sub(start) < abi::EXIT_WAIT_NANOSECONDS
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// The thread's ring, the first time it writes one: a free one, or a new
/// one, or none.
#[cold]
#[inline(never)]
fn acquire() -> Slot {
    if CONTROL.enable.load(Ordering::Acquire) == 0 {
        return Slot::Nothing;
    }

    ATFORK.call_once(|| {
        // The handlers touch only atomics, the thread's own slot, and the
        // clock.
        pthread_atfork(None, None, Some(forked));
        atexit(exiting);
    });

    // A thread that is exiting keeps no ring.
    if RETIRE.try_with(|_| ()).is_err() {
        return Slot::Nothing;
    }

    let slot = match reuse().or_else(allocate) {
        Some(ring) => {
            let header = &ring.header;
            header
                .thread
                .store(gettid().cast_unsigned(), Ordering::Relaxed);
            #[expect(clippy::cast_possible_truncation, reason = "a clock is a small number")]
            header.clock.store(
                CONTROL.clock.load(Ordering::Relaxed) as u8,
                Ordering::Relaxed,
            );
            header.state.store(abi::RING_ACTIVE, Ordering::Release);
            Slot::Ring(ring)
        }
        None => Slot::Nothing,
    };

    RING.with(|cell| cell.set(slot));
    slot
}

/// A ring of this process that a thread that exited left, which microscope
/// has read.
fn reuse() -> Option<&'static Ring> {
    let generation = FORKS.load(Ordering::Relaxed);
    let made = MADE_COUNT.load(Ordering::Acquire).min(abi::MOST_RINGS);
    MADE[..made]
        .iter()
        .filter_map(OnceLock::get)
        .copied()
        .find(|ring| {
            ring.header.generation.load(Ordering::Relaxed) == generation
                && ring
                    .header
                    .state
                    .compare_exchange(
                        abi::RING_FREE,
                        abi::RING_CLAIMED,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
        })
}

/// A new ring of the size microscope chose, in both registries, or none.
fn allocate() -> Option<&'static Ring> {
    let mut index = CONTROL.rings.load(Ordering::Relaxed);
    loop {
        if index as usize >= abi::MOST_RINGS {
            return None;
        }

        match CONTROL.rings.compare_exchange_weak(
            index,
            index + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(now) => index = now,
        }
    }

    let size = CONTROL
        .ring_size
        .load(Ordering::Relaxed)
        .clamp(abi::LEAST_RING_SIZE, abi::MOST_RING_SIZE);
    let ring = make_ring(size)?;
    let made = MADE_COUNT.fetch_add(1, Ordering::AcqRel);
    if let Some(slot) = MADE.get(made) {
        let _ = slot.set(ring);
    }

    CONTROL.registry[index as usize]
        .store(ptr::from_ref(&ring.header).cast_mut(), Ordering::Release);
    Some(ring)
}

/// A ring of `2^size` records, claimed, which lives for the program.
fn make_ring(size: u32) -> Option<&'static Ring> {
    let records = 1_usize << size;
    let layout = std::alloc::Layout::from_size_align(
        abi::RING_HEADER + abi::RECORD * records,
        abi::RING_HEADER,
    )
    .ok()?;

    // SAFETY: the layout has a size.
    let block = unsafe { std::alloc::alloc_zeroed(layout) };
    if block.is_null() {
        return None;
    }

    // A ring's records are its slice, so a pointer to one carries their
    // count as its metadata.
    let ring = ptr::slice_from_raw_parts_mut(block, records) as *mut Ring;

    // SAFETY: the block is the layout of a ring of `records` records,
    // zeroed, which is a valid ring, and no other thread sees it before
    // the registries hold it. Its header's plain fields are written before
    // a shared reference to it exists, and it is never freed.
    unsafe {
        (&raw mut (*ring).header.magic).write(abi::RING_MAGIC);
        (&raw mut (*ring).header.version).write(abi::RING_VERSION);
        #[expect(clippy::cast_possible_truncation, reason = "a size is clamped")]
        (&raw mut (*ring).header.size).write(size as u8);
        let ring = &*ring;
        ring.header
            .generation
            .store(FORKS.load(Ordering::Relaxed), Ordering::Relaxed);
        ring.header
            .state
            .store(abi::RING_CLAIMED, Ordering::Relaxed);
        Some(ring)
    }
}

#[cfg(test)]
mod tests;
