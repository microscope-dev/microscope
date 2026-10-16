//! The contract: every name the tokio model reads, bound against the
//! program's debug information.
//!
//! Offsets, sizes, and tags come from DWARF by name; a name the program
//! lacks makes only the features that read it unavailable, with a reason
//! that names it.

use std::sync::Arc;

use super::super::RuntimeImage;
use super::super::records::{self, Field, Missing, Sum};
use crate::{ThreadLocal, TypeReference};

/// Where the thread-local `CONTEXT` is, by its scope and the name std's
/// `thread_local!` gives its storage.
const CONTEXT_SCOPE: &str = "tokio::runtime::context::CONTEXT";
const CONTEXT_STORAGE: &str = "__RUST_STD_INTERNAL_VAL";

/// Each runtime flavor, as `scheduler::Handle` names its variant, with
/// the type its handle's `Arc` holds.
const FLAVORS: [(Flavor, &str, &str); 2] = [
    (
        Flavor::MultiThread,
        "MultiThread",
        "tokio::runtime::scheduler::multi_thread::handle::Handle",
    ),
    (
        Flavor::CurrentThread,
        "CurrentThread",
        "tokio::runtime::scheduler::current_thread::Handle",
    ),
];

/// Where the thread-local that names the `LocalSet` a thread runs is.
const LOCAL_SCOPE: &str = "tokio::task::local::CURRENT";

/// A runtime's scheduler, or a `LocalSet`, which keeps tasks of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Flavor {
    MultiThread,
    CurrentThread,
    Local,
}

impl Flavor {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::MultiThread => "multi-thread runtime",
            Self::CurrentThread => "current-thread runtime",
            Self::Local => "local set",
        }
    }
}

/// Everything the model reads, each part bound on its own.
#[derive(Debug)]
pub struct Layout {
    pub context: Result<Context, Missing>,
    pub tasks: Result<Tasks, Missing>,
    pub pool: Result<Pool, Missing>,
    pub spawns: Result<Spawns, Missing>,

    /// `None` for a program that never runs a `LocalSet`.
    pub locals: Result<Option<Locals>, Missing>,
}

impl Layout {
    pub fn bind(image: &dyn RuntimeImage) -> Self {
        let context = Context::bind(image);
        let tasks = Tasks::bind(image);
        let pool = Pool::bind(image);
        let spawns = Spawns::bind(image);
        let locals = Locals::bind(image);
        Self {
            context,
            tasks,
            pool,
            spawns,
            locals,
        }
    }
}

/// A `LocalSet`'s tasks, which no runtime lists: the set's `Shared` holds
/// them in a list of its own, which a thread's `CURRENT` reaches while
/// the thread runs the set, and the set itself reaches always.
#[derive(Debug)]
pub struct Locals {
    pub tls: ThreadLocal,

    /// std's state of the thread's storage, a byte, and the value saying
    /// the thread made it.
    pub state: u64,
    pub alive: u64,

    /// The `Option<Rc<Context>>` the storage holds, a pointer or null.
    pub context: u64,

    /// The `LocalSet`'s own `Rc<Context>` pointer.
    pub set: u64,

    /// The `Context` within its `Rc`'s allocation, and the `Arc<Shared>`
    /// pointer within the `Context`.
    pub value: u64,
    pub shared: u64,

    /// The `Shared` within its `Arc`'s allocation.
    pub data: u64,

    /// The list's id and head within the `Shared`.
    pub id: u64,
    pub head: u64,
}

impl Locals {
    fn bind(image: &dyn RuntimeImage) -> Result<Option<Self>, Missing> {
        let Some(tls) = image.thread_local_within(LOCAL_SCOPE, CONTEXT_STORAGE) else {
            return Ok(None);
        };

        let tls = tls?;
        let storage = records::named(
            image,
            "std::sys::thread_local::native::eager::Storage<tokio::task::local::LocalData>",
        )?;
        let state = records::field(image, storage, &["state", "value", "value"])?;
        let alive = records::enumerator(image, state.ty, "Alive")?;
        if records::size(image, state.ty)? != 1 {
            return Err("eager::Storage's state is not one byte".into());
        }

        let context = records::field(
            image,
            storage,
            &["val", "value", "ctx", "inner", "__0", "value"],
        )?;
        pointer_sized(image, context.ty, "a LocalSet's context")?;
        let set = records::named(image, "tokio::task::local::LocalSet")?;
        let rc = records::field(image, set, &["context", "ptr", "pointer"])?;
        let inner = records::target(image, rc.ty)?;
        let value = records::field(image, inner, &["value"])?;
        let arc = records::field(image, value.ty, &["shared", "ptr", "pointer"])?;
        let shared = records::target(image, arc.ty)?;
        let data = records::field(image, shared, &["data"])?;
        let owned = |path: &[&str]| {
            let mut whole = vec!["local_state", "owned"];
            whole.extend_from_slice(path);
            records::sized(image, data.ty, &whole, 8)
        };

        Ok(Some(Self {
            tls,
            state: state.offset,
            alive,
            context: context.offset,
            set: rc.offset,
            value: value.offset,
            shared: arc.offset,
            data: data.offset,
            id: owned(&["id"])?,
            head: owned(&["inner", "__0", "value", "list", "head"])?,
        }))
    }
}

/// Where each task was spawned, which tokio records only in a build with
/// `tokio_unstable`: the offset in a task's vtable of the offset from its
/// header to its `&'static Location`, and where a `Location` keeps its
/// file's name, its length, its line, and its column.
#[derive(Debug)]
pub struct Spawns {
    pub offset: u64,
    pub file: u64,
    pub length: u64,
    pub line: u64,
    pub column: u64,
}

impl Spawns {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let vtable = records::named(image, "tokio::runtime::task::raw::Vtable")?;
        let offset = records::sized(image, vtable, &["spawn_location_offset"], 8)
            .map_err(|_| "the program was built without tokio_unstable")?;
        let location = records::named(image, "core::panic::location::Location")?;

        // The file's name is a `NonNull<str>` in recent releases of Rust,
        // and was a `&str` before; either is a slice's pointer and length.
        let name = records::field(image, location, &["filename", "pointer"])
            .or_else(|_| records::field(image, location, &["file"]))?;
        records::slice_element(image, name.ty)?;
        Ok(Self {
            offset,
            file: name.offset,
            length: records::within(name.offset, 8)?,
            line: records::sized(image, location, &["line"], 4)?,
            column: records::sized(image, location, &["col"], 4)?,
        })
    }
}

/// A thread's `CONTEXT`: where it is, whether the thread made it, the
/// runtime the thread entered, its scheduler, and the task it polls.
#[derive(Debug)]
pub struct Context {
    pub tls: ThreadLocal,

    /// std's state of the thread's storage, a byte, and the value saying
    /// the thread made it.
    pub state: u64,
    pub alive: u64,

    /// `Option<scheduler::Handle>`, and the handle within its `Some`.
    pub handle: Field,
    pub handle_option: Sum,
    pub handle_some: u64,
    pub flavors: Sum,

    /// What each flavor's handle reaches.
    pub runtimes: Vec<(Flavor, Arc<str>, Runtime)>,

    /// The scheduler's context pointer, null outside a worker.
    pub scheduler: u64,

    /// Whether the thread entered its runtime, as a worker or a thread
    /// blocking on it does, and a pool thread does not.
    pub entered: Field,
    pub entered_state: Sum,

    /// `Option<task::Id>`, and the id within its `Some`.
    pub task: Field,
    pub task_option: Sum,
    pub task_some: u64,
}

/// What a runtime's handle holds: where its `Arc`'s pointer is within the
/// flavor's variant, where the handle is within what that points to, and
/// where its task list and blocking pool are within the handle.
#[derive(Debug)]
pub struct Runtime {
    pub arc: u64,
    pub data: u64,
    pub owned: Owned,

    /// The blocking pool's `Arc<Inner>` pointer within the handle.
    pub spawner: u64,
}

/// An `OwnedTasks`: its id, its shards, and its count.
#[derive(Debug)]
pub struct Owned {
    pub at: u64,
    pub id: u64,
    pub shards: u64,
    pub count: u64,
    pub mask: u64,

    /// Each shard's size, its lock, and its list's head.
    pub shard_size: u64,
    pub shard_lock: Lock,
    pub head: u64,
}

/// The lock of one of tokio's own mutexes, which wraps std's, or with
/// tokio's `parking_lot` feature, the `parking_lot` crate's: where its
/// state is, its size, and the bits of the state that say it is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lock {
    pub offset: u64,
    pub size: usize,
    pub held: u64,
}

impl Lock {
    /// Whether a lock in this state is held.
    pub const fn is_held(self, state: u64) -> bool {
        state & self.held != 0
    }
}

/// The lock and the data of `what`'s tokio mutex at `path` within `ty`,
/// both placed within `ty`. tokio's `Mutex<T>` is
/// `Mutex(std::sync::Mutex<T>)`, whose lock is a futex word that is zero
/// when free, or with its `parking_lot` feature
/// `Mutex(PhantomData<…>, parking_lot::Mutex<T>)`, whose lock is a byte
/// that also says whether threads are parked on it.
fn mutex(
    image: &dyn RuntimeImage,
    ty: TypeReference,
    path: &[&str],
    what: &str,
) -> Result<(Lock, Field), Missing> {
    let mutex = records::field(image, ty, path)?;
    let parking_lot = records::field(image, mutex.ty, &["__1"]).is_ok();
    let (state, data): (&[&str], &[&str]) = if parking_lot {
        (&["__1", "raw", "state"], &["__1", "data", "value"])
    } else {
        (&["__0", "inner"], &["__0", "data", "value"])
    };

    // The `parking_lot` crate's lock is held while its `LOCKED_BIT` is
    // set; its `PARKED_BIT` may be set while it is free.
    let (size, held, kind) = if parking_lot {
        (1, 0b1, "byte")
    } else {
        (4, u64::from(u32::MAX), "futex word")
    };

    let lock = records::field(image, mutex.ty, state)?;
    if records::size(image, lock.ty)? != size as u64 {
        return Err(format!("{what}'s lock is not a {kind}").into());
    }

    let data = records::field(image, mutex.ty, data)?;
    Ok((
        Lock {
            offset: records::within(mutex.offset, lock.offset)?,
            size,
            held,
        },
        Field {
            offset: records::within(mutex.offset, data.offset)?,
            ty: data.ty,
        },
    ))
}

impl Context {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let tls = image
            .thread_local_within(CONTEXT_SCOPE, CONTEXT_STORAGE)
            .ok_or("the program has no tokio::runtime::context::CONTEXT")??;
        let storage = records::named(
            image,
            "std::sys::thread_local::native::eager::Storage<tokio::runtime::context::Context>",
        )?;
        let state = records::field(image, storage, &["state", "value", "value"])?;
        let alive = records::enumerator(image, state.ty, "Alive")?;
        if records::size(image, state.ty)? != 1 {
            return Err("eager::Storage's state is not one byte".into());
        }

        let context = records::field(image, storage, &["val", "value"])?;
        let field = |path: &[&str]| {
            records::field(image, context.ty, path).and_then(|found| {
                Ok(Field {
                    offset: records::within(context.offset, found.offset)?,
                    ty: found.ty,
                })
            })
        };

        let handle = field(&["current", "handle", "value", "value"])?;
        let handle_option = records::sum(image, handle.ty)?;
        let some = handle_option.variant("Some")?.payload;
        let inner = records::field(image, some.ty, &["__0"])?;
        let flavors = records::sum(image, inner.ty)?;
        let runtimes = FLAVORS
            .iter()
            .map(|&(flavor, variant, handle)| {
                Ok((
                    flavor,
                    Arc::from(variant),
                    Runtime::bind(image, &flavors, variant, handle)?,
                ))
            })
            .collect::<Result<Vec<_>, Missing>>()?;
        let scheduler = field(&["scheduler", "inner", "value", "value"])?;
        pointer_sized(image, scheduler.ty, "Context.scheduler")?;
        let entered = field(&["runtime", "value", "value"])?;
        let entered_state = records::sum(image, entered.ty)?;
        entered_state.variant("Entered")?;
        entered_state.variant("NotEntered")?;
        let task = field(&["current_task_id", "value", "value"])?;
        let task_option = records::sum(image, task.ty)?;
        let task_some = task_option.variant("Some")?.payload;
        let id = records::field(image, task_some.ty, &["__0", "__0"])?;
        pointer_sized(image, id.ty, "task::Id")?;
        Ok(Self {
            tls,
            state: state.offset,
            alive,
            handle,
            handle_option,
            handle_some: records::within(some.offset, inner.offset)?,
            flavors,
            runtimes,
            scheduler: scheduler.offset,
            entered,
            entered_state,
            task,
            task_option,
            task_some: records::within(task_some.offset, id.offset)?,
        })
    }
}

impl Runtime {
    fn bind(
        image: &dyn RuntimeImage,
        flavors: &Sum,
        variant: &str,
        handle: &str,
    ) -> Result<Self, Missing> {
        let payload = flavors.variant(variant)?.payload;
        let arc = records::field(image, payload.ty, &["__0", "ptr", "pointer"])?;
        let inner = records::target(image, arc.ty)?;
        let data = records::field(image, inner, &["data"])?;
        let ty = records::named(image, handle)?;
        if !image.same_type(data.ty, ty) {
            return Err(format!("the {variant} handle does not hold a {handle}").into());
        }

        let spawner = records::field(image, ty, &["blocking_spawner", "inner", "ptr", "pointer"])?;
        Ok(Self {
            arc: records::within(payload.offset, arc.offset)?,
            data: data.offset,
            owned: Owned::bind(image, records::field(image, ty, &["shared", "owned"])?)?,
            spawner: spawner.offset,
        })
    }
}

impl Owned {
    fn bind(image: &dyn RuntimeImage, owned: Field) -> Result<Self, Missing> {
        let field = |path: &[&str]| records::field(image, owned.ty, path);
        let sized = |path: &[&str]| records::sized(image, owned.ty, path, 8);
        let shards = field(&["list", "lists"])?;
        let shard = records::slice_element(image, shards.ty)?;
        let (lock, list) = mutex(image, shard, &[], "a shard")?;
        let head = records::field(image, list.ty, &["head"])?;
        pointer_sized(image, head.ty, "a shard's head")?;
        Ok(Self {
            at: owned.offset,
            id: sized(&["id"])?,
            shards: shards.offset,
            count: sized(&["list", "count"])?,
            mask: sized(&["list", "shard_mask"])?,
            shard_size: records::size(image, shard)?,
            shard_lock: lock,
            head: records::within(list.offset, head.offset)?,
        })
    }
}

/// A task's header, its vtable, and its trailer's links.
#[derive(Debug)]
pub struct Tasks {
    pub state: u64,
    pub vtable: u64,
    pub owner: u64,
    pub poll: u64,
    pub shutdown: u64,
    pub trailer_offset: u64,
    pub id_offset: u64,
    pub prev: u64,
    pub next: u64,
}

impl Tasks {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let header = records::named(image, "tokio::runtime::task::core::Header")?;
        let vtable = records::named(image, "tokio::runtime::task::raw::Vtable")?;
        let trailer = records::named(image, "tokio::runtime::task::core::Trailer")?;
        let header_field = |path: &[&str]| records::sized(image, header, path, 8);
        let vtable_field = |name: &str| records::sized(image, vtable, &[name], 8);
        let link =
            |name: &str| records::sized(image, trailer, &["owned", "inner", "value", name], 8);
        Ok(Self {
            state: header_field(&["state"])?,
            vtable: header_field(&["vtable"])?,
            owner: header_field(&["owner_id"])?,
            poll: vtable_field("poll")?,
            shutdown: vtable_field("shutdown")?,
            trailer_offset: vtable_field("trailer_offset")?,
            id_offset: vtable_field("id_offset")?,
            prev: link("prev")?,
            next: link("next")?,
        })
    }
}

/// The blocking pool's queue: a `VecDeque<Task>` behind a mutex.
#[derive(Debug)]
pub struct Pool {
    /// The mutex's lock within the pool's `Inner`.
    pub lock: Lock,

    /// The queue's head index, length, buffer, and capacity.
    pub head: u64,
    pub len: u64,
    pub buffer: u64,
    pub capacity: u64,

    /// Each queued task's size, and its header pointer within it.
    pub task_size: u64,
    pub task: u64,

    /// Where the pool's `Inner` is within its `Arc`'s allocation.
    pub data: u64,
}

impl Pool {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let inner = records::named(image, "tokio::runtime::blocking::pool::Inner")?;
        let (lock, shared) = mutex(image, inner, &["shared"], "the blocking pool")?;
        let queue = records::field(image, shared.ty, &["queue"])?;
        let queue_at = records::within(shared.offset, queue.offset)?;
        let sized = |path: &[&str]| {
            records::sized(image, queue.ty, path, 8)
                .and_then(|offset| records::within(queue_at, offset))
        };

        let task = records::named(image, "tokio::runtime::blocking::pool::Task")?;
        let spawner = records::named(image, "tokio::runtime::blocking::pool::Spawner")?;
        let arc = records::field(image, spawner, &["inner", "ptr", "pointer"])?;
        let arc = records::target(image, arc.ty)?;
        Ok(Self {
            lock,
            head: sized(&["head"])?,
            len: sized(&["len"])?,
            // Each a word wrapped in single-member records whose names std
            // changes: `Unique` and `Cap` once, `NonNull` and
            // `UsizeNoHighBit` now. The word begins each wrapper.
            buffer: sized(&["buf", "inner", "ptr"])?,
            capacity: sized(&["buf", "inner", "cap"])?,
            task_size: records::size(image, task)?,
            task: records::sized(image, task, &["task", "raw", "ptr", "pointer"], 8)?,
            data: records::field(image, arc, &["data"])?.offset,
        })
    }
}

fn pointer_sized(image: &dyn RuntimeImage, ty: TypeReference, what: &str) -> Result<(), Missing> {
    if records::size(image, ty)? == 8 {
        Ok(())
    } else {
        Err(format!("{what} is not a word").into())
    }
}
