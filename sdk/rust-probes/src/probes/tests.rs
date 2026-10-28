//! The rings' life cycle, which Miri runs too: `just miri`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use super::*;

/// The rings and the control block are the process's, so one test at a
/// time uses them.
static SERIAL: Mutex<()> = Mutex::new(());

static STATE: AtomicU8 = AtomicU8::new(1);

/// Turns recording on, with the smallest rings, and holds the other tests
/// off. A test starts as a forked child would, taking no ring another
/// test made.
fn listen() -> std::sync::MutexGuard<'static, ()> {
    let serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    FORKS.fetch_add(1, Ordering::Relaxed);
    CONTROL
        .ring_size
        .store(abi::LEAST_RING_SIZE, Ordering::Relaxed);
    CONTROL.heartbeat.store(monotonic(), Ordering::Relaxed);
    CONTROL.timeout.store(u32::MAX, Ordering::Relaxed);
    CONTROL.enable.store(1, Ordering::Release);
    serial
}

/// The ring this thread writes to.
fn mine() -> &'static Ring {
    match RING.with(Cell::get) {
        Slot::Ring(ring) => ring,
        Slot::Unset | Slot::Nothing => panic!("the thread has no ring"),
    }
}

/// Whether the registry microscope reads lists `ring`.
fn registered(ring: &'static Ring) -> bool {
    let rings = CONTROL.rings.load(Ordering::Acquire) as usize;
    CONTROL.registry[..rings]
        .iter()
        .any(|slot| ptr::eq(slot.load(Ordering::Acquire), &ring.header))
}

/// An event as microscope decodes it.
#[derive(Debug)]
struct Event {
    site: u32,
    kind: u8,
    argument: u32,
    more: Vec<u8>,
}

/// Reads `ring`'s records from `tail` to its head, as microscope does, and
/// returns the events and the new tail.
fn read(ring: &Ring, tail: u64) -> (Vec<Event>, u64) {
    let head = ring.header.head.load(Ordering::Acquire);
    assert!(head - tail <= ring.records.len() as u64, "the ring wrapped");
    let record = |position: u64| {
        let words = &ring.records[position as usize & (ring.records.len() - 1)].0;
        let mut bytes = [0; abi::RECORD];
        bytes[..8].copy_from_slice(&words[0].load(Ordering::Relaxed).to_le_bytes());
        bytes[8..].copy_from_slice(&words[1].load(Ordering::Relaxed).to_le_bytes());
        bytes
    };

    let mut events = Vec::new();
    let mut position = tail;

    while position < head {
        let bytes = record(position);
        let word = u32::from_le_bytes(bytes[12..].try_into().unwrap());
        let kind = (word & 0xff) as u8;
        assert_ne!(kind, abi::CONTINUED, "an event starts at {position}");
        let mut event = Event {
            site: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            kind,
            argument: word >> 8,
            more: Vec::new(),
        };

        position += 1;
        while position < head && record(position)[12] == abi::CONTINUED {
            let bytes = record(position);
            event.more.extend_from_slice(&bytes[..12]);
            event.more.extend_from_slice(&bytes[13..]);
            position += 1;
        }

        events.push(event);
    }

    (events, position)
}

/// The walk that switches every site off reads each site's state offset
/// from its field and steps a whole site at a time, to the table's end.
#[test]
fn switching_off_zeroes_each_sites_state_byte() {
    const FIELD: usize = 24;

    #[repr(C, align(8))]
    struct Table {
        sites: [[u8; abi::SITE_SIZE]; 2],
        states: [AtomicU8; 3],
    }

    let mut table = Table {
        sites: [[0; abi::SITE_SIZE]; 2],
        states: [const { AtomicU8::new(1) }; 3],
    };

    // The first site's state is the first byte, the second site's the
    // third, and nothing names the second.
    let states = core::mem::offset_of!(Table, states);
    for (site, state) in [(0, states), (1, states + 2)] {
        let field = site * abi::SITE_SIZE + FIELD;
        let offset = i32::try_from(state).unwrap() - i32::try_from(field).unwrap();
        table.sites[site][FIELD..FIELD + 4].copy_from_slice(&offset.to_ne_bytes());
    }

    let start = ptr::from_ref(&table).cast::<u8>();

    // SAFETY: the table holds two whole sites, whose offsets name state
    // bytes of the same table, which outlives the call.
    unsafe { switch_all(start, start.wrapping_add(2 * abi::SITE_SIZE), 0) };

    let states = table
        .states
        .each_ref()
        .map(|state| state.load(Ordering::Relaxed));
    assert_eq!(states, [0, 1, 0]);
}

/// A message's text, by its length.
fn text(event: &Event) -> &[u8] {
    &event.more[..event.argument as usize]
}

#[test]
fn a_threads_events_are_whole_records_in_its_own_ring() {
    let _serial = listen();
    let (ring, thread) = std::thread::spawn(|| {
        drop(Zone::begin(&STATE));
        __message(&STATE, format_args!("a message of 38 bytes, in 3 records"));
        __plot(&STATE, 1.5);
        (mine(), gettid().cast_unsigned())
    })
    .join()
    .unwrap();

    assert!(registered(ring));
    let header = &ring.header;
    assert_eq!(header.magic, abi::RING_MAGIC);
    assert_eq!(u32::from(header.size), abi::LEAST_RING_SIZE);
    assert_eq!(header.thread.load(Ordering::Relaxed), thread);
    assert_eq!(header.state.load(Ordering::Acquire), abi::RING_RETIRED);

    let events = read(ring, 0).0;
    let kinds = events.iter().map(|event| event.kind).collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [abi::ZONE_BEGIN, abi::ZONE_END, abi::MESSAGE, abi::PLOT]
    );
    assert!(events.iter().all(|event| event.site == site(&STATE)));
    assert_eq!(text(&events[2]), b"a message of 38 bytes, in 3 records");
    assert_eq!(events[2].more.len(), 45, "three continued records");
    assert_eq!(events[3].more[..8], 1.5_f64.to_le_bytes());
    assert_eq!(header.head.load(Ordering::Relaxed), 1 + 1 + 4 + 2);
}

#[test]
fn a_ring_microscope_freed_is_taken_again_but_not_after_a_fork() {
    let _serial = listen();
    let emit = || {
        std::thread::spawn(|| {
            __event(&STATE, abi::TRIGGER);
            (mine(), gettid().cast_unsigned())
        })
        .join()
        .unwrap()
    };

    let (first, _) = emit();
    let (other, _) = emit();
    assert!(
        !ptr::eq(first, other),
        "a retired ring is microscope's to read"
    );

    first.header.state.store(abi::RING_FREE, Ordering::Release);
    let (again, thread) = emit();
    assert!(ptr::eq(first, again), "a freed ring is taken again");
    assert_eq!(again.header.thread.load(Ordering::Relaxed), thread);
    let head = again.header.head.load(Ordering::Relaxed);
    assert_eq!(
        read(again, head - 1).0[0].kind,
        abi::TRIGGER,
        "its events follow the last"
    );

    // A forked child's rings are its own: a ring made before the fork
    // stays free.
    again.header.state.store(abi::RING_FREE, Ordering::Release);
    FORKS.fetch_add(1, Ordering::Relaxed);
    let (after, _) = emit();
    assert!(!ptr::eq(again, after));
    assert_eq!(again.header.state.load(Ordering::Relaxed), abi::RING_FREE);
}

/// Threads come and go, writing messages, while a reader reads their rings
/// and frees those they left; every message arrives once, whole, in its
/// thread's order.
#[test]
fn concurrent_threads_reuse_the_rings_a_reader_frees() {
    const LANES: usize = 2;
    const THREADS: usize = 3;
    const MESSAGES: usize = 2;
    let _serial = listen();
    let made = || {
        let made = MADE_COUNT.load(Ordering::Acquire).min(abi::MOST_RINGS);
        MADE[..made].iter().filter_map(OnceLock::get).copied()
    };

    // Where microscope last read each ring that other tests made.
    let mut tails = made()
        .map(|ring| {
            (
                ptr::from_ref(ring).addr(),
                ring.header.head.load(Ordering::Acquire),
            )
        })
        .collect::<HashMap<_, _>>();
    let done = AtomicBool::new(false);
    let seen = std::thread::scope(|scope| {
        let lanes = (0..LANES)
            .map(|lane| {
                scope.spawn(move || {
                    for thread in 0..THREADS {
                        std::thread::spawn(move || {
                            for message in 0..MESSAGES {
                                __message(&STATE, format_args!("{lane}.{thread}.{message}:padded"));
                            }
                        })
                        .join()
                        .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();

        let done = &done;
        let reader = scope.spawn(move || {
            let mut seen = Vec::new();
            let mut sweep = || {
                // As microscope does, it reads rings as their threads write
                // them, and frees those their threads left.
                for ring in made() {
                    let state = ring.header.state.load(Ordering::Acquire);
                    if state == abi::RING_ACTIVE || state == abi::RING_RETIRED {
                        let tail = tails.entry(ptr::from_ref(ring).addr()).or_default();
                        let (events, next) = read(ring, *tail);
                        *tail = next;
                        seen.extend(events.iter().map(|event| text(event).to_vec()));
                    }

                    if state == abi::RING_RETIRED {
                        ring.header.state.store(abi::RING_FREE, Ordering::Release);
                    }
                }
            };

            while !done.load(Ordering::Acquire) {
                sweep();
                std::thread::yield_now();
            }

            sweep();
            seen
        });

        for lane in lanes {
            lane.join().unwrap();
        }

        done.store(true, Ordering::Release);
        reader.join().unwrap()
    });

    let mut expected = Vec::new();
    for lane in 0..LANES {
        for thread in 0..THREADS {
            for message in 0..MESSAGES {
                expected.push(format!("{lane}.{thread}.{message}:padded").into_bytes());
            }
        }
    }

    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(sorted, expected, "every message, once and whole");

    // Rings are read in any order, so only a thread's own are in order.
    for writer in expected.chunks(MESSAGES) {
        let prefix = &writer[0][..4];
        let order = seen
            .iter()
            .filter(|text| text.starts_with(prefix))
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            writer.iter().collect::<Vec<_>>(),
            "a thread's messages in order"
        );
    }
}

/// A run-time site is a record microscope can read where the control block
/// says, whole before it is counted; defined again it is the same site; it
/// writes only while on, by its key, only what its kind writes; and it
/// switches off with every other site.
#[test]
fn a_run_time_site_writes_by_its_key_only_while_on() {
    let _serial = listen();
    let zone = define(abi::SITE_ZONE, "dynamic.load", "app.py", 7).unwrap();
    let message = define(abi::SITE_MESSAGE, "dynamic.say", "app.py", 9).unwrap();
    assert!(ptr::eq(
        zone,
        define(abi::SITE_ZONE, "dynamic.load", "app.py", 7).unwrap()
    ));
    assert!(!ptr::eq(
        zone,
        define(abi::SITE_ZONE, "dynamic.load", "app.py", 8).unwrap()
    ));
    assert!(define(abi::SITE_NONE, "none", "app.py", 1).is_none());
    assert!(define(abi::SITE_TRIGGER + 1, "unknown", "app.py", 1).is_none());
    let long = "x".repeat(abi::MOST_SITE_TEXT as usize + 1);
    assert!(define(abi::SITE_ZONE, &long, "app.py", 1).is_none());

    // Where microscope finds it, as the ABI lays it out.
    let sites = CONTROL.sites.load(Ordering::Acquire);
    assert!(ptr::eq(sites, &SITES));
    assert_eq!(SITES.magic, abi::SITES_MAGIC);
    let count = SITES.count.load(Ordering::Acquire);
    let index = (0..count)
        .find(|&index| ptr::eq(&SITES.records[index as usize], zone))
        .unwrap();
    assert_eq!(zone.kind.load(Ordering::Relaxed), abi::SITE_ZONE);
    assert_eq!(zone.line.load(Ordering::Relaxed), 7);
    let (name, _, _) = *DEFINED
        .lock()
        .unwrap()
        .iter()
        .find(|(_, _, site)| ptr::eq(*site, zone))
        .unwrap();
    assert_eq!(name, "dynamic.load");
    assert_eq!(
        zone.name.load(Ordering::Relaxed),
        name.as_ptr().addr() as u64
    );
    assert_eq!(zone.name_length.load(Ordering::Relaxed), 12);
    let key = abi::RUNTIME_KEY | index;

    let ring = std::thread::spawn(move || {
        drop(zone.zone());
        zone.state.store(1, Ordering::Relaxed);
        message.state.store(1, Ordering::Relaxed);
        drop(zone.zone());
        zone.message(format_args!("not a zone's"));
        zone.plot(1.0);
        message.message(format_args!("said"));
        mine()
    })
    .join()
    .unwrap();
    let events = read(ring, 0).0;
    let written = events
        .iter()
        .map(|event| (event.site, event.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        written,
        [
            (key, abi::ZONE_BEGIN),
            (key, abi::ZONE_END),
            (message.key(), abi::MESSAGE)
        ]
    );
    assert_eq!(text(&events[2]), b"said");

    CONTROL
        .enable
        .store(abi::ENABLED | abi::ENABLE_DEFINED, Ordering::Release);
    let late = define(abi::SITE_FRAME, "dynamic.late", "app.py", 11).unwrap();
    assert!(
        late.on(),
        "on from its definition while every site is recorded"
    );

    disable();
    assert!(!zone.on() && !message.on() && !late.on());
}
