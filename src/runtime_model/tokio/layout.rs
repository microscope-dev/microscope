use std::sync::Arc;

use super::super::RuntimeImage;
use super::super::records::{self, Field, Missing, Sum};
use crate::{ThreadLocal, TypeReference};

pub const CONTEXT_SCOPE: &str = "tokio::runtime::context::CONTEXT";

pub fn thread_local_storage(
    image: &dyn RuntimeImage,
    scope: &str,
) -> Option<Result<ThreadLocal, Arc<str>>> {
    crate::rust_names::THREAD_LOCAL_STORAGE
        .iter()
        .find_map(|name| image.thread_local_within(scope, name))
}

#[derive(Debug)]
pub struct Storage {
    pub tls: ThreadLocal,
    pub value: Field,

    pub state_tls: ThreadLocal,
    pub state: u64,
    pub alive: u64,
}

impl Storage {
    fn bind(image: &dyn RuntimeImage, scope: &str, value: &str) -> Option<Result<Self, Missing>> {
        let tls = thread_local_storage(image, scope)?;
        Some(tls.and_then(|tls| Self::bind_at(image, scope, value, tls)))
    }

    fn bind_at(
        image: &dyn RuntimeImage,
        scope: &str,
        value: &str,
        tls: ThreadLocal,
    ) -> Result<Self, Missing> {
        let [newest, older] = crate::rust_names::EAGER_STORAGE
            .map(|storage| records::named(image, &format!("{storage}<{value}>")));
        let storage = match newest.or_else(|missing| older.map_err(|_| missing)) {
            Ok(storage) => storage,
            Err(missing) => {
                let Some(state_tls) =
                    image.thread_local_within(scope, crate::rust_names::STATE_STATIC)
                else {
                    return Err(missing);
                };

                return Ok(Self {
                    tls,
                    value: Field {
                        offset: 0,
                        ty: records::named(image, value)?,
                    },
                    state_tls: state_tls?,
                    state: 0,
                    alive: crate::rust_names::STATE_STATIC_ALIVE,
                });
            }
        };

        let state = records::field(image, storage, &["state", "value", "value"])?;
        let alive = records::enumerator(image, state.ty, "Alive")?;
        if records::size(image, state.ty)? != 1 {
            return Err("eager::Storage's state is not one byte".into());
        }

        Ok(Self {
            tls,
            value: records::field(image, storage, &["val", "value"])?,
            state_tls: tls,
            state: state.offset,
            alive,
        })
    }
}

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

const LOCAL_SCOPE: &str = "tokio::task::local::CURRENT";

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

#[derive(Debug)]
pub struct Layout {
    pub context: Result<Context, Missing>,
    pub tasks: Result<Tasks, Missing>,
    pub pool: Result<Pool, Missing>,
    pub spawns: Result<Spawns, Missing>,

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

#[derive(Debug)]
pub struct Locals {
    pub storage: Storage,

    pub context: u64,

    pub set: u64,

    pub value: u64,
    pub shared: u64,

    pub data: u64,

    pub id: u64,
    pub head: u64,
}

impl Locals {
    fn bind(image: &dyn RuntimeImage) -> Result<Option<Self>, Missing> {
        let Some(storage) = Storage::bind(image, LOCAL_SCOPE, "tokio::task::local::LocalData")
        else {
            return Ok(None);
        };

        let storage = storage?;
        let context = records::field(image, storage.value.ty, &["ctx", "inner", "__0", "value"])?;
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
            context: records::within(storage.value.offset, context.offset)?,
            storage,
            set: rc.offset,
            value: value.offset,
            shared: arc.offset,
            data: data.offset,
            id: owned(&["id"])?,
            head: owned(&["inner", "__0", "value", "list", "head"])?,
        }))
    }
}

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

#[derive(Debug)]
pub struct Context {
    pub storage: Storage,

    pub handle: Field,
    pub handle_option: Sum,
    pub handle_some: u64,
    pub flavors: Sum,

    pub runtimes: Vec<(Flavor, Arc<str>, Runtime)>,

    pub scheduler: u64,

    pub entered: Field,
    pub entered_state: Sum,

    pub task: Field,
    pub task_option: Sum,
    pub task_some: u64,
}

#[derive(Debug)]
pub struct Runtime {
    pub arc: u64,
    pub data: u64,
    pub owned: Owned,

    pub spawner: u64,
}

#[derive(Debug)]
pub struct Owned {
    pub at: u64,
    pub id: u64,
    pub shards: u64,
    pub count: u64,
    pub mask: u64,

    pub shard_size: u64,
    pub shard_lock: Lock,
    pub head: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lock {
    pub offset: u64,
    pub size: usize,
    pub held: u64,
}

impl Lock {
    pub const fn is_held(self, state: u64) -> bool {
        state & self.held != 0
    }
}

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
        let storage = Storage::bind(image, CONTEXT_SCOPE, "tokio::runtime::context::Context")
            .ok_or("the program has no tokio::runtime::context::CONTEXT")??;
        let context = storage.value;
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
            storage,
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

#[derive(Debug)]
pub struct Pool {
    pub queues: Queues,

    pub head: u64,
    pub len: u64,
    pub buffer: u64,
    pub capacity: u64,

    pub task_size: u64,
    pub task: u64,

    pub data: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guarded {
    pub lock: Lock,
    pub queue: u64,
}

impl Guarded {
    pub(super) fn shifted(self, by: u64) -> Result<Self, Missing> {
        Ok(Self {
            lock: Lock {
                offset: records::within(by, self.lock.offset)?,
                ..self.lock
            },
            queue: records::within(by, self.queue)?,
        })
    }
}

#[derive(Debug)]
pub enum Queues {
    One(Guarded),

    Chosen {
        choice: Sum,
        at: u64,
        locked: Guarded,
        sharded: Guarded,
        stride: u64,
        shards: u64,
    },
}

pub const MAX_SHARDS: u64 = 1024;

impl Pool {
    fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        let inner = records::named(image, "tokio::runtime::blocking::pool::Inner")?;

        let (queues, deque) = if records::field(image, inner, &["inner_impl"]).is_ok() {
            Self::chosen(image, inner)?
        } else {
            let (lock, shared) = mutex(image, inner, &["shared"], "the blocking pool")?;
            let (one, deque) = guarded(image, lock, shared)?;
            (Queues::One(one), deque)
        };

        let task = records::named(image, "tokio::runtime::blocking::pool::Task")?;
        let spawner = records::named(image, "tokio::runtime::blocking::pool::Spawner")?;
        let arc = records::field(image, spawner, &["inner", "ptr", "pointer"])?;
        let arc = records::target(image, arc.ty)?;
        let (head, len, buffer, capacity) = deque_words(image, deque)?;

        Ok(Self {
            queues,
            head,
            len,
            buffer,
            capacity,
            task_size: records::size(image, task)?,
            task: records::sized(image, task, &["task", "raw", "ptr", "pointer"], 8)?,
            data: records::field(image, arc, &["data"])?.offset,
        })
    }

    fn chosen(
        image: &dyn RuntimeImage,
        inner: TypeReference,
    ) -> Result<(Queues, TypeReference), Missing> {
        let field = records::field(image, inner, &["inner_impl"])?;
        let choice = records::sum(image, field.ty)?;

        let locked = choice.variant("Locked")?.payload;
        let (lock, data) = mutex(image, locked.ty, &["__0", "mutex"], "the blocking pool")?;
        let (one, deque) = guarded(image, lock, data)?;
        let locked = one.shifted(records::within(field.offset, locked.offset)?)?;

        let sharded = choice.variant("Sharded")?.payload;
        let shards = records::field(image, sharded.ty, &["__0", "shards"])?;
        let (shard, count) = records::array(image, shards.ty)?;
        if count == 0 || count > MAX_SHARDS {
            return Err(format!("the blocking pool has {count} shards").into());
        }

        let (lock, data) = mutex(image, shard, &[], "a blocking pool's shard")?;
        let (first, shard_deque) = guarded(image, lock, data)?;
        let first = first.shifted(records::within(
            records::within(field.offset, sharded.offset)?,
            shards.offset,
        )?)?;

        if deque_words(image, shard_deque)? != deque_words(image, deque)? {
            return Err("the blocking pool's queues differ in layout".into());
        }

        let at = field.offset;
        Ok((
            Queues::Chosen {
                choice,
                at,
                locked,
                sharded: first,
                stride: records::size(image, shard)?,
                shards: count,
            },
            deque,
        ))
    }
}

fn guarded(
    image: &dyn RuntimeImage,
    lock: Lock,
    data: Field,
) -> Result<(Guarded, TypeReference), Missing> {
    let queue = records::field(image, data.ty, &["queue"])?;
    Ok((
        Guarded {
            lock,
            queue: records::within(data.offset, queue.offset)?,
        },
        queue.ty,
    ))
}

fn deque_words(
    image: &dyn RuntimeImage,
    deque: TypeReference,
) -> Result<(u64, u64, u64, u64), Missing> {
    let sized = |path: &[&str]| records::sized(image, deque, path, 8);
    let raw = |name| sized(&["buf", "inner", name]).or_else(|_| sized(&["buf", name]));
    Ok((
        sized(&["head"])?,
        sized(&["len"])?,
        raw("ptr")?,
        raw("cap")?,
    ))
}

fn pointer_sized(image: &dyn RuntimeImage, ty: TypeReference, what: &str) -> Result<(), Missing> {
    if records::size(image, ty)? == 8 {
        Ok(())
    } else {
        Err(format!("{what} is not a word").into())
    }
}
