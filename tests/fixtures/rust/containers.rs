





use std::cell::{Cell, OnceCell, RefCell, UnsafeCell};
use std::cmp::Reverse;
use std::convert::Infallible;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet, LinkedList, VecDeque};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt::Debug;
use std::hint::black_box;
use std::mem::ManuallyDrop;
use std::num::{NonZero, Saturating};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU32};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};


#[expect(dead_code, reason = "the debugger reads it")]
enum Shape {
    Square { side: u32 },
    Circle(u32),
}

#[derive(Debug)]
#[expect(dead_code, reason = "the debugger reads it")]
struct Point {
    x: i32,
    y: i32,
}


#[expect(dead_code, reason = "the debugger reads it")]
struct Meters(u32);



#[repr(C)]
struct Reading {
    seconds: i64,
    nanoseconds: u32,
}

#[inline(never)]
fn barrier(fixture: *const u8) {
    black_box(fixture);
}

fn main() {
    let text = String::from("hello, world"); // VIEW: text => "hello, world"
    let empty_text = String::new(); // VIEW: empty_text => ""
    let long_text = "y".repeat(300); // VIEW: long_text => "{y*256}"... (300 bytes)
    let boxed_text: Box<str> = Box::from("boxed"); // VIEW: boxed_text => "boxed"
    let path = PathBuf::from("/tmp/microscope"); // VIEW: path => "/tmp/microscope"
    let os_text = OsString::from("os text"); // VIEW: os_text => "os text"
    let c_text = CString::new("c text").expect("no NUL"); // VIEW: c_text => "c text"
    let ints: Vec<i32> = vec![1, 2, 3]; // VIEW: ints => len=3 [1, 2, 3]
    let no_ints: Vec<u64> = Vec::new(); // VIEW: no_ints => len=0 []
    let words = vec![String::from("one"), String::from("two")]; // VIEW: words => len=2 ["one", "two"]
    let many: Vec<u32> = (0..300).collect(); // VIEW: many => len=300 [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]

    let mut ring: VecDeque<i32> = VecDeque::with_capacity(4); // VIEW: ring => len=4 [1, 2, 3, 4]
    ring.push_back(3);
    ring.push_back(4);
    ring.push_front(2);
    ring.push_front(1);

    let units: Vec<()> = vec![(); 3]; // VIEW: units => len=3 [(), (), ()]
    let mut unit_ring: VecDeque<()> = VecDeque::new(); // VIEW: unit_ring => len=2 [(), ()]
    unit_ring.push_back(());
    unit_ring.push_front(());
    let mut hashed: HashMap<i32, i32> = HashMap::new(); // VIEW: hashed => len=2 {1: 10, 2: 20} (any order)
    hashed.insert(1, 10);
    hashed.insert(2, 20);
    let mut named: HashMap<String, u64> = HashMap::new(); // VIEW: named => len=1 {"one": 1}
    named.insert(String::from("one"), 1);
    let no_hashed: HashMap<i32, i32> = HashMap::new(); // VIEW: no_hashed => len=0 {}
    let many_hashed: HashMap<u32, u32> = (0..300).map(|key| (key, key * 2)).collect(); // VIEW: many_hashed => count: 300
    let set: HashSet<u8> = HashSet::from([7, 9]); // VIEW: set => len=2 [7, 9] (any order)
    let no_set: HashSet<u8> = HashSet::new(); // VIEW: no_set => len=0 []
    let mut tree: BTreeMap<i32, i32> = BTreeMap::new(); // VIEW: tree => len=3 {1: 10, 2: 20, 3: 30}
    tree.insert(2, 20);
    tree.insert(3, 30);
    tree.insert(1, 10);
    let no_tree: BTreeMap<u8, u8> = BTreeMap::new(); // VIEW: no_tree => len=0 {}
    let mut emptied: BTreeMap<u8, u8> = BTreeMap::from([(1, 2)]); // VIEW: emptied => len=0 {}
    emptied.remove(&1);
    let tall: BTreeMap<u32, u64> = (0..300).map(|key| (key, u64::from(key) * 3)).collect(); // VIEW: tall => count: 300
    let titles = BTreeMap::from([(String::from("b"), vec![]), (String::from("a"), vec![1_u8])]); // VIEW: titles => len=2 {"a": len=1 [1], "b": len=0 []}
    let tree_set: BTreeSet<u16> = BTreeSet::from([5, 1, 3]); // VIEW: tree_set => len=3 [1, 3, 5]
    let no_tree_set: BTreeSet<u16> = BTreeSet::new(); // VIEW: no_tree_set => len=0 []

    let lost_text = 0x10 as *const String; // VIEW: lost_text => problem: inaccessible
    let no_text: *const String = std::ptr::null(); // VIEW: no_text => stored



    let mut four: Vec<i32> = Vec::with_capacity(8);
    four.extend([10, 11, 12, 13]);
    // SAFETY: a Vec<i32> is three words, and the result is never read or
    // dropped; the debugger reads it.
    let mut fields: [usize; 3] = unsafe { std::mem::transmute(ManuallyDrop::new(four)) };
    let length = fields.iter().position(|field| *field == 4).expect("a length field");
    fields[length] = 9;
    // SAFETY: as above.
    let past_capacity: ManuallyDrop<Vec<i32>> = unsafe { std::mem::transmute(fields) }; // VIEW: past_capacity.value.0 => problem: check


    let three = BTreeMap::from([(1_i32, 1_i32), (2, 2), (3, 3)]);
    // SAFETY: a BTreeMap is three words, and the result is never read or
    // dropped; the debugger reads it.
    let mut fields: [usize; 3] = unsafe { std::mem::transmute(ManuallyDrop::new(three)) };
    let length = fields.iter().position(|field| *field == 3).expect("a length field");
    fields[length] = 5;
    // SAFETY: as above.
    let overcounted: ManuallyDrop<BTreeMap<i32, i32>> = unsafe { std::mem::transmute(fields) }; // VIEW: overcounted.value.0 => problem: declares 5 elements and generates 3

    // SAFETY: the vector is never read or dropped; the debugger reads it.
    let dangling = ManuallyDrop::new(unsafe { Vec::from_raw_parts(0x10 as *mut i32, 2, 2) }); // VIEW: dangling.value.0 => len=2 [<unavailable>, …]


    let boxed: Box<i32> = Box::new(42); // VIEW: boxed => 42
    let rc = Rc::new(7_u64); // VIEW: rc => 7
    let rc_too = Rc::clone(&rc); // VIEW: rc_too => children: strong = 2, weak = 1, [raw]
    let rc_weak = Rc::downgrade(&rc); // VIEW: rc_weak => 7
    let no_weak: Weak<u64> = Weak::new(); // VIEW: no_weak => dangling
    let dropped = Rc::downgrade(&Rc::new(1_u8)); // VIEW: dropped => dropped
    let arc = Arc::new(String::from("shared")); // VIEW: arc => "shared"
    let arc_weak = Arc::downgrade(&arc); // VIEW: arc_weak => children: capacity = 6, strong = 1, weak = 1, [raw]


    // VIEW: *arc => "shared"
    let cell = Cell::new(5_i32); // VIEW: cell => 5
    let unsafe_cell = UnsafeCell::new(3_u16); // VIEW: unsafe_cell => 3
    let ref_cell = RefCell::new(vec![1, 2]); // VIEW: ref_cell => children: [0] = 1, [1] = 2, borrow = 1, [raw]
    let borrowed = ref_cell.borrow();
    let mutex = Mutex::new(9_u8); // VIEW: mutex => children: locked = false, poisoned = false, [raw]
    let some: Option<i32> = Some(4); // VIEW: some => Some(4)
    let nothing: Option<i32> = None; // VIEW: nothing => None
    let ok: Result<u32, String> = Ok(7); // VIEW: ok => Ok(7)
    let failed: Result<u32, String> = Err(String::from("no")); // VIEW: failed => Err("no")

    let infallible: Result<u8, Infallible> = Ok(3); // VIEW: infallible => Ok(3)
    let never_ok: Result<Infallible, ()> = Err(()); // VIEW: never_ok => Err
    let shape = Shape::Square { side: 4 }; // VIEW: shape => Square {side: 4}
    let dynamic: Box<dyn Debug> = Box::new(Point { x: 1, y: 2 }); // VIEW: dynamic => Point {x: 1, y: 2}


    let pair = (1, "two", 3.5); // VIEW: pair => (1, "two", 3.5)
    // VIEW: pair => children: __0 = 1, __1 = "two", __2 = 3.5, [raw]
    let meters = Meters(7); // VIEW: meters => Meters(7)
    let nested = ((1_u8, 2_u8), Meters(3)); // VIEW: nested => ((1, 2), Meters(3))
    let mut pin_target = vec![1_u8];
    let wrapping = std::num::Wrapping(5_u8); // VIEW: wrapping => Wrapping(5)
    let saturating = Saturating(6_i8); // VIEW: saturating => Saturating(6)
    let reversed = Reverse(3_u16); // VIEW: reversed => Reverse(3)
    let elapsed = Duration::new(90, 500_000_000); // VIEW: elapsed => 1m30.5s
    let tiny = Duration::from_nanos(42); // VIEW: tiny => 42ns
    let no_time = Duration::ZERO; // VIEW: no_time => 0s
    let instant = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000); // VIEW: instant => 2023-11-14 22:13:20 +0000 UTC
    let before_epoch = SystemTime::UNIX_EPOCH - Duration::from_millis(1500); // VIEW: before_epoch => 1969-12-31 23:59:58.5 +0000 UTC
    // SAFETY: an Instant is a monotonic reading of this layout on Linux.
    let uptime: Instant = unsafe { std::mem::transmute(Reading { seconds: 5, nanoseconds: 0 }) }; // VIEW: uptime => 5s
    let counter = AtomicU32::new(5); // VIEW: counter => 5
    let signed_counter = AtomicI64::new(-9); // VIEW: signed_counter => -9
    let ready = AtomicBool::new(true); // VIEW: ready => true
    let slot: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut()); // VIEW: slot => 0x0
    let nonzero = NonZero::new(7_u32).expect("nonzero"); // VIEW: nonzero => 7
    let maybe_nonzero = NonZero::new(8_u64); // VIEW: maybe_nonzero => Some(8)
    let non_null: NonNull<u64> = NonNull::dangling(); // VIEW: non_null => 0x8
    let once = OnceCell::new(); // VIEW: once => Some(3)
    once.set(3_i32).expect("unset");
    let no_once: OnceCell<i32> = OnceCell::new(); // VIEW: no_once => None
    let once_lock = OnceLock::new(); // VIEW: once_lock => "set"
    once_lock.set(String::from("set")).expect("unset");
    let no_once_lock: OnceLock<String> = OnceLock::new(); // VIEW: no_once_lock => uninitialized
    let rw_lock = RwLock::new(4_u8); // VIEW: rw_lock => children: readers = 1, writer = false, poisoned = false, [raw]
    let reading = rw_lock.read().expect("unpoisoned");
    let written = RwLock::new(5_u8); // VIEW: written => children: readers = 0, writer = true, poisoned = false, [raw]
    let writing = written.write().expect("unpoisoned");
    let linked = LinkedList::from([1, 2, 3]); // VIEW: linked => len=3 [1, 2, 3]
    let no_linked: LinkedList<u8> = LinkedList::new(); // VIEW: no_linked => len=0 []
    let heap = BinaryHeap::from([1, 3, 2]); // VIEW: heap => len=3 [3, 1, 2]
    let shared_text: Rc<str> = Rc::from("shared"); // VIEW: shared_text => "shared"
    let atomic_text: Arc<str> = Arc::from("atomic"); // VIEW: atomic_text => "atomic"
    let shared_slice: Rc<[i32]> = Rc::from([1, 2, 3]); // VIEW: shared_slice => stored
    let path_ref = Path::new("/etc/hosts"); // VIEW: path_ref => "/etc/hosts"
    let os_ref = OsStr::new("os"); // VIEW: os_ref => "os"
    let c_ref = c"c ref"; // VIEW: c_ref => "c ref"
    let c_ref_too: &CStr = c_text.as_c_str(); // VIEW: c_ref_too => "c text"
    let pinned = Box::pin(11_i32); // VIEW: pinned => 11
    // VIEW: *pinned => 11
    let pinned_ref: Pin<&mut Vec<u8>> = Pin::new(&mut pin_target);

    black_box((
        &text,
        &empty_text,
        &long_text,
        &boxed_text,
        &path,
        &os_text,
        &c_text,
        &ints,
        &no_ints,
        &words,
        &many,
        &ring,
        &units,
        &unit_ring,
        &hashed,
        &named,
        &no_hashed,
        &many_hashed,
        &set,
        &no_set,
        &tree,
        &no_tree,
        &emptied,
        &tall,
        &titles,
        &tree_set,
        &no_tree_set,
        &lost_text,
        &no_text,
        &past_capacity,
        &overcounted,
        &dangling,
        &boxed,
        &rc,
        &rc_too,
        &rc_weak,
        &no_weak,
        &dropped,
        &arc,
        &arc_weak,
        &cell,
        &ref_cell,
        &borrowed,
        &mutex,
        &some,
        &nothing,
        &ok,
        &failed,
        &shape,
        &dynamic,
    ));
    black_box((&pair, &meters, &nested, &wrapping, &saturating, &reversed));
    black_box((&elapsed, &tiny, &no_time, &instant, &before_epoch, &uptime));
    black_box((&counter, &signed_counter, &ready, &slot, &nonzero, &maybe_nonzero));
    black_box((&non_null, &once, &no_once, &once_lock, &no_once_lock, &rw_lock));
    black_box((&reading, &linked, &no_linked, &heap, &shared_text, &atomic_text));
    black_box((&shared_slice, &path_ref, &os_ref, &c_ref, &c_ref_too, &pinned));
    black_box((&pinned_ref, &writing, &unsafe_cell, &infallible, &never_ok));
    barrier(std::ptr::from_ref(&text).cast());
    std::process::exit(i32::from(ints.len() + many.len() != 303));
}
