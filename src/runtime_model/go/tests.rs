//! The Go model on a real runtime's layout, its static data from the
//! executable's file, and goroutines the tests write: states no program
//! shows on demand, such as a goroutine caught mid-scan, and what a damaged
//! or incomplete runtime gives.

use std::collections::BTreeMap;
use std::sync::Arc;

use object::{Object, ObjectSection};

use super::super::{ImageSymbol, Member, RuntimeImage, RuntimeModel, RuntimeStop, ThreadActivity};
use crate::{
    ImageAddress, IntegerValue, ModuleImage, StackSegment, TaskState, ThreadId, ThreadLocal,
    VirtualAddress,
};

/// A Go executable linked by Go's own linker and loaded where it was
/// linked, so its image addresses are its virtual ones.
const FIXTURE: &str = "steps-go-o2";

/// Where the tests place the goroutines and threads they write, which the
/// executable's file leaves unmapped.
const HEAP: u64 = 0x7000_0000_0000;

fn fixture_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/test-programs")
        .join(FIXTURE)
}

/// The fixture's image, with some of the runtime's names hidden, as a
/// runtime that lacks them would be.
#[derive(Debug)]
struct Image {
    module: Arc<ModuleImage>,
    hidden: Vec<&'static str>,

    /// Where `runtime.tlsg` says the goroutine is kept, as an external link
    /// would place it.
    tlsg: Option<ThreadLocal>,
}

impl Image {
    fn load() -> Self {
        let module = crate::debug_info::load_module(
            &fixture_path(),
            crate::ModuleImageId::new(0),
            &crate::debug_info::DebugFileSearch::default(),
        )
        .expect("run `just build-test-programs`")
        .image;
        Self {
            module,
            hidden: Vec::new(),
            tlsg: None,
        }
    }

    fn hiding(mut self, name: &'static str) -> Self {
        self.hidden.push(name);
        self
    }

    fn shows(&self, name: &str) -> bool {
        !self.hidden.contains(&name)
    }
}

impl RuntimeImage for Image {
    fn producers(&self) -> Vec<&str> {
        self.module.producers().collect()
    }

    fn constant(&self, name: &str) -> Option<IntegerValue> {
        self.shows(name)
            .then(|| RuntimeImage::constant(self.module.as_ref(), name))?
    }

    fn symbol(&self, name: &str) -> Option<ImageSymbol> {
        self.shows(name)
            .then(|| RuntimeImage::symbol(self.module.as_ref(), name))?
    }

    fn has_function(&self, name: &str) -> bool {
        self.shows(name) && RuntimeImage::has_function(self.module.as_ref(), name)
    }

    fn function_answering(&self, name: &str) -> Option<ImageSymbol> {
        self.shows(name)
            .then(|| RuntimeImage::function_answering(self.module.as_ref(), name))?
    }

    fn symbol_at(&self, address: ImageAddress) -> Option<Arc<str>> {
        RuntimeImage::symbol_at(self.module.as_ref(), address)
    }

    fn function_body(&self, name: &str) -> Option<ImageAddress> {
        self.shows(name)
            .then(|| RuntimeImage::function_body(self.module.as_ref(), name))?
    }

    fn member(&self, type_name: &str, path: &[&str]) -> Option<Member> {
        let name = format!("{type_name}.{}", path.join("."));
        self.shows(&name)
            .then(|| RuntimeImage::member(self.module.as_ref(), type_name, path))?
    }

    fn function_name(&self, address: ImageAddress) -> Option<Arc<str>> {
        RuntimeImage::function_name(self.module.as_ref(), address)
    }

    fn thread_local(&self, name: &str) -> Option<Result<ThreadLocal, Arc<str>>> {
        match self.tlsg {
            Some(place) if name == "runtime.tlsg" => Some(Ok(place)),
            _ => RuntimeImage::thread_local(self.module.as_ref(), name),
        }
    }
}

/// Memory as the executable's file lays it out, under what a test writes,
/// with the threads a test stops.
struct Memory {
    file: Vec<(u64, Vec<u8>)>,
    written: BTreeMap<u64, u8>,
    threads: BTreeMap<ThreadId, u64>,
}

impl Memory {
    fn load() -> Self {
        let bytes = std::fs::read(fixture_path()).expect("run `just build-test-programs`");
        let object = object::File::parse(bytes.as_slice()).expect("an executable");
        let file = object
            .sections()
            .filter(|section| section.address() != 0)
            .filter_map(|section| Some((section.address(), section.data().ok()?.to_vec())))
            .filter(|(_, data)| !data.is_empty())
            .collect();
        Self {
            file,
            written: BTreeMap::new(),
            threads: BTreeMap::new(),
        }
    }

    fn write(&mut self, address: u64, bytes: &[u8]) {
        for (at, byte) in (address..).zip(bytes) {
            self.written.insert(at, *byte);
        }
    }

    fn word(&mut self, address: u64, value: u64) {
        self.write(address, &value.to_le_bytes());
    }

    fn byte(&self, address: u64) -> Option<u8> {
        self.written.get(&address).copied().or_else(|| {
            self.file.iter().find_map(|(start, data)| {
                let index = usize::try_from(address.checked_sub(*start)?).ok()?;
                data.get(index).copied()
            })
        })
    }
}

impl RuntimeStop for Memory {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        for (at, byte) in (address.get()..).zip(bytes.iter_mut()) {
            let Some(read) = self.byte(at) else {
                return false;
            };
            *byte = read;
        }

        true
    }

    fn thread_pointer(&self, thread: ThreadId) -> Option<u64> {
        self.threads.get(&thread).copied()
    }

    fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
        None
    }

    fn load_bias(&self) -> crate::LoadBias {
        crate::LoadBias::default()
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.threads.keys().copied().collect()
    }
}

/// What a test writes of a goroutine.
#[derive(Clone, Copy)]
struct G {
    goid: u64,
    status: u64,
    wait_reason: u64,

    /// The m running it, or 0.
    m: u64,
}

/// Writes the runtime's goroutines as `allgs` lists them, and returns
/// where each g is, or the address given in place of one.
struct World<'a> {
    image: &'a Image,
    memory: Memory,
}

impl<'a> World<'a> {
    fn new(image: &'a Image) -> Self {
        Self {
            image,
            memory: Memory::load(),
        }
    }

    fn offset(&self, ty: &str, path: &[&str]) -> u64 {
        RuntimeImage::member(self.image.module.as_ref(), ty, path)
            .unwrap_or_else(|| panic!("{ty}.{}", path.join(".")))
            .offset
    }

    fn status(&self, name: &str) -> u64 {
        match RuntimeImage::constant(self.image.module.as_ref(), &format!("runtime.{name}")) {
            Some(IntegerValue::Signed(value)) => u64::try_from(value).expect("a status"),
            Some(IntegerValue::Unsigned(value)) => u64::try_from(value).expect("a status"),
            None => panic!("no runtime.{name}"),
        }
    }

    fn symbol(&self, name: &str) -> u64 {
        RuntimeImage::symbol(self.image.module.as_ref(), name)
            .unwrap_or_else(|| panic!("no {name}"))
            .address
            .get()
    }

    /// Writes `g` at `at`, a goroutine that began in `main.main`, with
    /// every field it does not name zero.
    fn goroutine(&mut self, at: u64, g: G) {
        let size = RuntimeImage::member(self.image.module.as_ref(), "runtime.g", &[])
            .expect("runtime.g")
            .size;
        self.memory
            .write(at, &vec![0; usize::try_from(size).expect("a g's size")]);
        let main = self.symbol("main.main");
        for (path, value) in [
            (&["goid"][..], g.goid),
            (&["m"], g.m),
            (&["startpc"], main),
            (&["gopc"], main),
        ] {
            let offset = self.offset("runtime.g", path);
            self.memory.word(at + offset, value);
        }

        let status = self.offset("runtime.g", &["atomicstatus"]);
        self.memory.write(
            at + status,
            &u32::try_from(g.status).expect("a status").to_le_bytes(),
        );
        let reason = self.offset("runtime.g", &["waitreason"]);
        self.memory.write(
            at + reason,
            &[u8::try_from(g.wait_reason).expect("a reason")],
        );
    }

    /// Publishes `gs` as `allgs`, with `published` of them in `allglen`.
    fn allgs(&mut self, gs: &[u64], published: u64) {
        let array = HEAP + 0x10_0000;
        for (index, g) in (0..).zip(gs) {
            self.memory.word(array + 8 * index, *g);
        }

        let allgs = self.symbol("runtime.allgs");
        self.memory.word(allgs, array);
        self.memory.word(allgs + 8, gs.len() as u64);
        self.memory.word(allgs + 16, gs.len() as u64);
        self.memory.word(self.symbol("runtime.allglen"), published);
    }

    /// Writes goroutines at consecutive places and lists them all.
    fn goroutines(&mut self, gs: &[G]) -> Vec<u64> {
        let places = (0..)
            .map(|index| HEAP + 0x1000 * index)
            .take(gs.len())
            .collect::<Vec<_>>();
        for (place, g) in places.iter().zip(gs) {
            self.goroutine(*place, *g);
        }

        self.allgs(&places, places.len() as u64);
        places
    }

    fn model(&self) -> Arc<dyn RuntimeModel> {
        let image = Image {
            module: Arc::clone(&self.image.module),
            hidden: self.image.hidden.clone(),
            tlsg: self.image.tlsg,
        };

        super::detect(Arc::new(image))
            .expect("a Go runtime")
            .expect("the runtime binds")
    }
}

/// A listed task's number, state, and the runtime's words for it.
type Listed = (u64, TaskState, Option<String>);

/// What a page of every task says of each, and its gaps.
fn listed(model: &dyn RuntimeModel, stop: &Memory) -> (Vec<Listed>, Vec<String>) {
    let page = model.tasks(stop, 0, 64, false);
    let tasks = page
        .value
        .tasks
        .iter()
        .map(|task| {
            (
                task.number,
                task.state.clone(),
                task.detail.as_deref().map(str::to_owned),
            )
        })
        .collect();
    let gaps = page.gaps.iter().map(ToString::to_string).collect();
    (tasks, gaps)
}

/// A goroutine whose stack the collector is scanning has the scan bit set
/// in its status, over the status it returns to, which is what it shows.
#[test]
fn a_goroutine_being_scanned_shows_the_status_it_returns_to() {
    let image = Image::load();
    let mut world = World::new(&image);
    let scan = world.status("_Gscan");
    let waiting = world.status("_Gwaiting");
    let runnable = world.status("_Grunnable");
    let dead = world.status("_Gdead");
    let receive = world.status("waitReasonChanReceive");
    let gs = |bits: u64| {
        [
            G {
                goid: 1,
                status: waiting | bits,
                wait_reason: receive,
                m: 0,
            },
            G {
                goid: 2,
                status: runnable | bits,
                wait_reason: 0,
                m: 0,
            },
            G {
                goid: 3,
                status: dead | bits,
                wait_reason: 0,
                m: 0,
            },
        ]
    };

    world.goroutines(&gs(0));
    let model = world.model();
    let (resting, gaps) = listed(model.as_ref(), &world.memory);
    assert_eq!(
        resting,
        [
            (1, TaskState::Blocked, Some("chan receive".to_owned())),
            (2, TaskState::Runnable, Some("runnable".to_owned())),
        ]
    );
    assert!(gaps.is_empty(), "{gaps:?}");

    world.goroutines(&gs(scan));
    let (scanned, gaps) = listed(world.model().as_ref(), &world.memory);
    assert_eq!(scanned, resting);
    assert!(gaps.is_empty(), "{gaps:?}");
}

/// A goroutine the list names but memory does not hold is a gap that says
/// where it is, and every other goroutine is listed; a list longer than
/// its slice is refused whole.
#[test]
fn an_unreadable_goroutine_is_a_gap_among_the_others() {
    let image = Image::load();
    let mut world = World::new(&image);
    let runnable = world.status("_Grunnable");
    let g = |goid| G {
        goid,
        status: runnable,
        wait_reason: 0,
        m: 0,
    };

    let places = world.goroutines(&[g(1), g(3)]);
    let missing = HEAP + 0xdead_0000;
    world.allgs(&[places[0], missing, places[1]], 3);
    let (tasks, gaps) = listed(world.model().as_ref(), &world.memory);
    let numbers = tasks.iter().map(|task| task.0).collect::<Vec<_>>();
    assert_eq!(numbers, [1, 3]);
    assert_eq!(gaps, [format!("goroutine at {missing:#x} is unreadable")]);

    world.allgs(&places, 5);
    let (tasks, gaps) = listed(world.model().as_ref(), &world.memory);
    assert!(tasks.is_empty(), "{tasks:?}");
    assert_eq!(
        gaps,
        ["runtime.allglen is 5, beyond what runtime.allgs holds"]
    );
}

/// The goroutine a thread runs is kept at a place relative to its thread
/// pointer: in the last word below it, as Go's own linker places it, or
/// at an offset a slot holds, as an external link's initial-exec model
/// does. A thread with no goroutine there runs none.
#[test]
fn a_threads_goroutine_is_found_by_each_way_of_keeping_it() {
    let thread = ThreadId::new(4242);
    let pointer = HEAP + 0x50_0000;
    let slot = HEAP + 0x60_0000;

    // An external link's thread-local block puts it further below.
    let below = 0x40_u64;
    for (tlsg, place) in [
        (None, pointer - 8),
        (
            Some(ThreadLocal::Slot(ImageAddress::new(slot))),
            pointer - below,
        ),
    ] {
        let mut image = Image::load();
        image.tlsg = tlsg;
        let mut world = World::new(&image);
        let running = world.status("_Grunning");
        let m = HEAP + 0x40_0000;
        let places = world.goroutines(&[G {
            goid: 7,
            status: running,
            wait_reason: 0,
            m,
        }]);
        let g0 = HEAP + 0x41_0000;

        for (path, value) in [
            ("procid", 4242),
            ("curg", places[0]),
            ("g0", g0),
            ("gsignal", HEAP + 0x42_0000),
        ] {
            let offset = world.offset("runtime.m", &[path]);
            world.memory.word(m + offset, value);
        }

        world.memory.word(slot, below.wrapping_neg());
        world.memory.threads.insert(thread, pointer);
        let model = world.model();
        let activity = |memory: &Memory| model.thread_activity(memory, thread);

        world.memory.word(place, places[0]);
        assert_eq!(
            activity(&world.memory),
            ThreadActivity::Task {
                number: 7,
                stack: StackSegment::Task
            },
            "{tlsg:?}"
        );

        // On its m's system stack, the thread still runs the goroutine.
        world.memory.word(g0 + world.offset("runtime.g", &["m"]), m);
        world.memory.word(place, g0);
        assert_eq!(
            activity(&world.memory),
            ThreadActivity::Task {
                number: 7,
                stack: StackSegment::System
            },
            "{tlsg:?}"
        );
        world.memory.word(place, 0);
        assert_eq!(activity(&world.memory), ThreadActivity::Idle, "{tlsg:?}");
    }
}

/// Each part of the contract binds on its own: a runtime that lacks a name
/// makes only what reads it unavailable, with the name in the reason.
#[test]
fn a_missing_name_makes_only_what_needs_it_unavailable() {
    let thread = ThreadId::new(4242);
    let unavailable = |hidden: &'static str| {
        let image = Image::load().hiding(hidden);
        let mut world = World::new(&image);
        let runnable = world.status("_Grunnable");
        world.goroutines(&[G {
            goid: 1,
            status: runnable,
            wait_reason: 0,
            m: 0,
        }]);
        world.memory.threads.insert(thread, HEAP + 0x50_0000);
        world.memory.word(HEAP + 0x50_0000 - 8, 0);
        let model = world.model();
        let (tasks, gaps) = listed(model.as_ref(), &world.memory);
        let activity = model.thread_activity(&world.memory, thread);

        (tasks.len(), gaps, activity)
    };

    // Without profiler labels, goroutines are listed and say what is
    // missing.
    let (tasks, gaps, activity) = unavailable("runtime.g.labels");
    assert_eq!(tasks, 1);
    assert_eq!(
        gaps,
        ["profiler labels are not read: the runtime has no member runtime.g.labels"]
    );
    assert_eq!(activity, ThreadActivity::Idle);

    // Without a goroutine's stack, threads are unknown, and goroutines are
    // listed.
    let (tasks, gaps, activity) = unavailable("runtime.g.stack.lo");
    assert_eq!(tasks, 1);
    assert!(gaps.is_empty(), "{gaps:?}");
    assert_eq!(
        activity,
        ThreadActivity::Unknown("the runtime has no member runtime.g.stack.lo".into())
    );

    // Without goroutine ids, nothing about goroutines can be read.
    let (tasks, gaps, activity) = unavailable("runtime.g.goid");
    assert_eq!(tasks, 0);
    assert_eq!(gaps, ["the runtime has no member runtime.g.goid"]);
    assert_eq!(
        activity,
        ThreadActivity::Unknown("the runtime has no member runtime.g.goid".into())
    );
}
