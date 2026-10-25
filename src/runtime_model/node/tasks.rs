//! Node's event loop's work, as tasks: each JavaScript thread's
//! `Environment`, reached through its isolate's current context, keeps the
//! handles and requests it waits on in two lists (`env.h`), whose offsets
//! Node's `nodedbg_` variables give; its timers are in `internal/timers`'
//! lists ([`super::timers`]); and libuv's thread pool runs its jobs on
//! threads of its own ([`super::pool`]).
//!
//! A handle or request runs no code until Node calls its callback, so it
//! has no frames. A thread-pool job runs on its pool thread, whose frames
//! are its own.
//!
//! Numbers are Node's async ids: the main thread's `Environment`'s as Node
//! gives them, a worker's above `2^32`, by the worker's thread, so that no
//! two collide and each keeps its number while it lives.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::objects::{Object, Problem, Reader};
use super::scripts::Table;
use crate::image::v8::{Constant, Field, NodeTasks, ScopeInfoSlice, scope_info_slice};
use crate::runtime_model::{RuntimeImage, RuntimeTask, TaskCallback};
use crate::{TaskState, ThreadId, ThreadLocal};

/// The bit that marks a thread-pool job's number, which is its thread's
/// with this bit set.
const POOL_JOB: u64 = 1 << 62;

/// Where a worker's thread sits in the numbers of its `Environment`'s
/// work.
const WORKER_SHIFT: u32 = 32;

/// The most entries of one list read: a process waits on far fewer.
const MOST_ENTRIES: usize = 1 << 16;

/// The libuv handle and request a wrapper owns, past its list node, as
/// `HandleWrap` (`handle_wrap.h`) declares `handle_` and `ReqWrap`
/// (`req_wrap.h`) its inline `req_`. Each is proven by the libuv
/// structure's `data`, which Node points back at the wrapper.
const HANDLE_AFTER_NODE: u64 = 16;
const REQUEST_AFTER_NODE: u64 = 24;

/// libuv's public layout (`uv.h`): every handle's and request's `data`
/// first; a handle's type after its loop, and its flags after
/// `UV_HANDLE_FIELDS` (`unix.h`'s `next_closing`, then `flags`); a
/// request's type after `data`; and a file request's operation and path
/// after `UV_REQ_FIELDS`.
const UV_HANDLE_TYPE: u64 = 16;
const UV_HANDLE_FLAGS: u64 = 88;
const UV_REQUEST_TYPE: u64 = 8;
const UV_FS_TYPE: u64 = 64;
const UV_FS_PATH: u64 = 104;

/// libuv's handle flags (`uv-common.h`).
const UV_HANDLE_CLOSING: u32 = 0x1;
const UV_HANDLE_ACTIVE: u32 = 0x4;
const UV_HANDLE_REF: u32 = 0x8;

/// libuv's handle types, from `UV_UNKNOWN_HANDLE` (`uv.h`'s
/// `UV_HANDLE_TYPE_MAP`, then `UV_FILE`).
const UV_HANDLE_TYPES: [&str; 18] = [
    "unknown",
    "async",
    "check",
    "fs_event",
    "fs_poll",
    "handle",
    "idle",
    "named_pipe",
    "poll",
    "prepare",
    "process",
    "stream",
    "tcp",
    "timer",
    "tty",
    "udp",
    "signal",
    "file",
];

/// libuv's request types, from `UV_UNKNOWN_REQ` (`UV_REQ_TYPE_MAP`).
const UV_REQUEST_TYPES: [&str; 11] = [
    "unknown",
    "req",
    "connect",
    "write",
    "shutdown",
    "udp_send",
    "fs",
    "work",
    "getaddrinfo",
    "getnameinfo",
    "random",
];
const UV_FS: u32 = 6;

/// libuv's file operations, from `UV_FS_CUSTOM` (`uv_fs_type`).
const UV_FS_OPERATIONS: [&str; 37] = [
    "custom",
    "open",
    "close",
    "read",
    "write",
    "sendfile",
    "stat",
    "lstat",
    "fstat",
    "ftruncate",
    "utime",
    "futime",
    "access",
    "chmod",
    "fchmod",
    "fsync",
    "fdatasync",
    "unlink",
    "rmdir",
    "mkdir",
    "mkdtemp",
    "rename",
    "scandir",
    "link",
    "symlink",
    "readlink",
    "chown",
    "fchown",
    "realpath",
    "copyfile",
    "lchown",
    "opendir",
    "readdir",
    "closedir",
    "statfs",
    "mkstemp",
    "lutime",
];

/// V8's `FUNCTION_SCOPE` (`globals.h`'s `ScopeType`), the scope of every
/// function's own `ScopeInfo`, which keeps the function's positions.
const FUNCTION_SCOPE: i64 = 2;

/// Whose `Environment` a task is in: the main thread's, or the worker's
/// that runs on a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    Main,
    Worker(ThreadId),
}

impl Owner {
    /// The number of the work Node gave `async_id` in this `Environment`.
    pub(super) fn number(self, async_id: f64) -> Result<u64, Problem> {
        let limit = f64::from(u32::MAX);
        if !(1.0..=limit).contains(&async_id) || async_id.fract() != 0.0 {
            return Err(format!("the async id {async_id} is no id microscope numbers").into());
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a whole number from 1 to 2^32 - 1"
        )]
        let id = async_id as u64;
        match self {
            Self::Main => Ok(id),
            Self::Worker(thread) => {
                // Linux's thread ids are below 2^22.
                if thread.get() >= POOL_JOB >> WORKER_SHIFT {
                    return Err(format!("thread {thread}'s id is too large to number").into());
                }
                Ok((thread.get() << WORKER_SHIFT) | id)
            }
        }
    }
}

/// The number of the thread-pool job `thread` runs.
pub(super) const fn pool_job(thread: ThreadId) -> u64 {
    POOL_JOB | thread.get()
}

/// The pool thread whose job `number` is, if it is a job's.
pub(super) const fn pool_thread(number: u64) -> Option<ThreadId> {
    if number & POOL_JOB == 0 {
        None
    } else {
        Some(ThreadId::new(number & !POOL_JOB))
    }
}

/// One JavaScript thread's `Environment`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Environment {
    pub(super) address: u64,
    pub(super) owner: Owner,
}

/// What the walk needs at a stop besides memory.
pub(super) struct Walk<'a> {
    pub(super) reader: Reader<'a>,
    pub(super) layout: &'a NodeTasks,
    pub(super) image: &'a (dyn RuntimeImage + Send + Sync),
    pub(super) table: &'a mut Table,
}

/// Every task of every JavaScript thread's `Environment`, by number, with
/// why some could not be read.
pub(super) fn listed(
    walk: &mut Walk<'_>,
    isolate: ThreadLocal,
) -> (Vec<RuntimeTask>, Vec<Problem>) {
    let mut gaps = Vec::new();
    let mut tasks = Vec::new();
    for environment in environments(&walk.reader, walk.layout, isolate, &mut gaps) {
        let context = match environment.owner {
            Owner::Main => "the main thread".to_owned(),
            Owner::Worker(thread) => format!("worker thread {thread}"),
        };
        match lists(walk, environment, &mut tasks) {
            Ok(()) => {}
            Err(problem) => gaps.push(
                format!("the handles and requests of {context} cannot all be read: {problem}")
                    .into(),
            ),
        }
        if let Err(problem) = super::timers::listed(walk, environment, &mut tasks) {
            gaps.push(format!("the timers of {context} cannot all be read: {problem}").into());
        }
    }
    tasks.sort_by_key(|task| task.number);
    // Numbers are unique by Node's async ids; two tasks with one number
    // mean memory the walk misread.
    if tasks
        .windows(2)
        .any(|pair| pair[0].number == pair[1].number)
    {
        gaps.push("two of Node's tasks have one async id, so the list was misread".into());
        tasks.dedup_by_key(|task| task.number);
    }
    (tasks, gaps)
}

/// The `Environment` of each isolate the process's threads hold, with
/// why any cannot be found.
fn environments(
    reader: &Reader<'_>,
    layout: &NodeTasks,
    isolate: ThreadLocal,
    gaps: &mut Vec<Problem>,
) -> Vec<Environment> {
    let stop = reader.stop;
    let mut holders: BTreeMap<u64, Vec<ThreadId>> = BTreeMap::new();
    for thread in stop.threads() {
        match super::current_isolate(stop, thread, isolate) {
            Ok(0) => {}
            Ok(isolate) => holders.entry(isolate).or_default().push(thread),
            Err(problem) => gaps.push(
                format!(
                    "thread {thread}'s isolate, and so its Node tasks, cannot be read: {problem}"
                )
                .into(),
            ),
        }
    }

    let main = stop.main_thread();
    let mut found = Vec::new();
    for (isolate, threads) in holders {
        // From V8 13.6 a helper thread holds an isolate while it compiles
        // for it. The main thread is known by its id; a worker's thread is
        // known only where no helper holds its isolate too.
        let (owner, thread) = match (main, threads.as_slice()) {
            (Some(main), _) if threads.contains(&main) => (Owner::Main, main),
            (_, [thread]) => (Owner::Worker(*thread), *thread),
            _ => {
                let threads = threads
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                gaps.push(
                    format!(
                        "threads {threads} hold the isolate at {isolate:#x}, so which runs its \
                         event loop, and how its tasks are numbered, is unknown; they are left out"
                    )
                    .into(),
                );
                continue;
            }
        };
        match environment(reader, layout, isolate) {
            Ok(address) => found.push(Environment { address, owner }),
            Err(problem) => gaps.push(
                format!("thread {thread}'s Node environment cannot be found: {problem}").into(),
            ),
        }
    }
    found
}

/// The `Environment` of the context `isolate` entered, as
/// `Environment::GetCurrent` finds it: the context's embedder data holds
/// it beside Node's context tag.
fn environment(reader: &Reader<'_>, layout: &NodeTasks, isolate: u64) -> Result<u64, Problem> {
    let context = reader.word(isolate + layout.current_context)?;
    let context = reader
        .object(context)?
        .ok_or("its isolate has entered no context")?;
    let native = super::access::native_context(reader, context)?;
    let data = super::access::slot(reader, native, Constant::ContextEmbedderDataIndex)?;
    let data = reader
        .object(data)?
        .filter(|data| {
            reader
                .is(*data, Constant::EmbedderDataArrayType)
                .unwrap_or(false)
        })
        .ok_or("its context keeps no embedder data")?;
    let tag = reader.word(
        reader
            .stop
            .load_bias()
            .load(layout.context_tag)
            .ok_or("Node's context tag is outside the address space")?
            .get(),
    )?;
    if reader.word(data.0 + layout.tag_slot)? != tag {
        return Err("its context is not one of Node's".into());
    }
    let environment = reader.word(data.0 + layout.environment_slot)?;
    if environment == 0 {
        return Err("its context has no environment yet".into());
    }
    Ok(environment)
}

/// The value of the `index`th of Node's `nodedbg_` variables at this stop.
fn nodedbg(reader: &Reader<'_>, layout: &NodeTasks, index: usize) -> Result<u64, Problem> {
    let address = layout
        .nodedbg
        .get(index)
        .and_then(|address| reader.stop.load_bias().load(*address))
        .ok_or("a nodedbg_ variable is outside the address space")?;
    reader.word(address.get())
}

/// One of an `Environment`'s two lists: where it is in the
/// `Environment`, where its head is in it, where an entry's node is in
/// the entry, and where a node keeps its neighbours.
#[derive(Debug, Clone, Copy)]
struct List {
    queue: u64,
    head: u64,
    node: u64,
    next: u64,
    previous: u64,
}

/// The two lists' layouts and where a `BaseObject` keeps its object, from
/// Node's `nodedbg_` variables, which 22 and 24 set as they start.
fn layouts(reader: &Reader<'_>, layout: &NodeTasks) -> Result<(List, List, u64), Problem> {
    let value = |index| nodedbg(reader, layout, index);
    let handles = List {
        queue: value(2)?,
        head: value(4)?,
        node: value(6)?,
        next: value(8)?,
        previous: value(10)?,
    };
    let requests = List {
        queue: value(3)?,
        head: value(5)?,
        node: value(7)?,
        next: value(9)?,
        previous: value(11)?,
    };
    let persistent = value(1)?;
    // An offset into the Environment or a wrapper is never 0, and a node's
    // two pointers are apart: all zero is a process that has not yet set
    // them.
    if handles.queue == 0 || requests.queue == 0 || handles.next == handles.previous {
        return Err("Node has not set its nodedbg_ variables yet".into());
    }
    Ok((handles, requests, persistent))
}

/// The entries of the list `list` of `environment`, innermost first: each
/// entry's address, proven by its neighbours.
fn entries(reader: &Reader<'_>, environment: u64, list: List) -> Result<Vec<u64>, Problem> {
    let head = environment + list.queue + list.head;
    let mut found = Vec::new();
    let mut previous = head;
    let mut node = reader.word(head + list.next)?;
    while node != head {
        if found.len() == MOST_ENTRIES {
            return Err(format!("a list holds more than {MOST_ENTRIES} entries").into());
        }
        if node == 0 || reader.word(node + list.previous)? != previous {
            return Err(
                format!("the list's node at {node:#x} is not linked to its neighbour").into(),
            );
        }
        found.push(node);
        previous = node;
        node = reader.word(node + list.next)?;
    }
    Ok(found)
}

/// The tasks of `environment`'s handles and requests, appended to `tasks`.
fn lists(
    walk: &mut Walk<'_>,
    environment: Environment,
    tasks: &mut Vec<RuntimeTask>,
) -> Result<(), Problem> {
    let (handles, requests, persistent) = layouts(&walk.reader, walk.layout)?;
    let mut problems = Vec::new();
    for node in entries(&walk.reader, environment.address, handles)? {
        let wrap = node - handles.node;
        match entry(
            walk,
            environment,
            wrap,
            persistent,
            Kind::Handle(node + HANDLE_AFTER_NODE),
        ) {
            Ok(task) => tasks.push(task),
            Err(problem) => problems.push(format!("the handle at {wrap:#x}: {problem}")),
        }
    }
    for node in entries(&walk.reader, environment.address, requests)? {
        let wrap = node - requests.node;
        match entry(
            walk,
            environment,
            wrap,
            persistent,
            Kind::Request(node + REQUEST_AFTER_NODE),
        ) {
            Ok(task) => tasks.push(task),
            Err(problem) => problems.push(format!("the request at {wrap:#x}: {problem}")),
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; ").into())
    }
}

/// Whether an entry is a handle, with where its wrapper keeps the pointer
/// to its libuv handle, or a request, with where its libuv request is.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Handle(u64),
    Request(u64),
}

fn u32_at(reader: &Reader<'_>, address: u64) -> Result<u32, Problem> {
    let mut bytes = [0; 4];
    reader.bytes_into(address, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

/// The C string at `address`, of at most `limit` bytes.
fn c_string(reader: &Reader<'_>, address: u64, limit: usize) -> Result<String, Problem> {
    let mut text = Vec::new();
    // The stop's block cache makes each byte's read cheap.
    for at in 0..limit as u64 {
        let mut byte = [0];
        reader.bytes_into(address + at, &mut byte)?;
        if byte[0] == 0 {
            return Ok(String::from_utf8_lossy(&text).into_owned());
        }
        text.push(byte[0]);
    }
    Err(format!("the string at {address:#x} is longer than {limit} bytes").into())
}

/// The name of the class whose vtable `vtable` points into, as its
/// symbol, `vtable for node::TCPWrap`, gives it: Itanium's vtable pointer
/// is two words past the vtable's start.
fn class(walk: &Walk<'_>, vtable: u64) -> Option<String> {
    let pointer = walk
        .reader
        .stop
        .load_bias()
        .unload(crate::VirtualAddress::new(vtable))?;
    let (class, start) = walk.image.vtable_class(pointer)?;
    (start.get().checked_add(16) == Some(pointer.get())).then_some(class)
}

/// The name of Node's async provider `index`, from its table of names.
fn provider(reader: &Reader<'_>, layout: &NodeTasks, index: u32) -> Result<String, Problem> {
    // Node has fewer than a hundred providers.
    if index >= 256 {
        return Err(format!("the provider {index} is out of range").into());
    }
    let table = reader
        .stop
        .load_bias()
        .load(layout.provider_names)
        .ok_or("the providers' names are outside the address space")?;
    let name = c_string(reader, reader.word(table.get() + 8 * u64::from(index))?, 64)?;
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(format!("the provider {index} has no name").into());
    }
    Ok(name)
}

/// A wrapper's async id and its trigger's.
fn async_ids(reader: &Reader<'_>, layout: &NodeTasks, wrap: u64) -> Result<(f64, f64), Problem> {
    Ok((
        f64::from_bits(reader.word(wrap + layout.async_id)?),
        f64::from_bits(reader.word(wrap + layout.async_id + 8)?),
    ))
}

/// The JavaScript object of the `BaseObject` at `wrap`, proven by the
/// object's own pointer back to it; `None` for a wrapper whose object is
/// gone.
fn object(
    reader: &Reader<'_>,
    layout: &NodeTasks,
    wrap: u64,
    persistent: u64,
) -> Result<Option<Object>, Problem> {
    let slot = reader.word(wrap + persistent)?;
    if slot == 0 {
        return Ok(None);
    }
    let Some(object) = reader.object(reader.word(slot)?)? else {
        return Ok(None);
    };
    let tag = reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
    if reader.word(object.0 - tag + layout.object_wrapper)? != wrap {
        return Err("its JavaScript object does not point back at it".into());
    }
    Ok(Some(object))
}

/// What Node calls a handle of `provider` for, in its own words.
fn handle_noun(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "TCPSERVERWRAP" => "TCP server",
        "TCPWRAP" => "TCP socket",
        "PIPESERVERWRAP" => "pipe server",
        "PIPEWRAP" => "pipe",
        "TTYWRAP" => "TTY",
        "UDPWRAP" => "UDP socket",
        "PROCESSWRAP" => "child process",
        "SIGNALWRAP" => "signal watcher",
        "FSEVENTWRAP" => "file watcher",
        "STATWATCHER" => "file poller",
        "MESSAGEPORT" => "message port",
        "TIMERWRAP" => "timer",
        _ => return None,
    })
}

fn request_noun(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "GETADDRINFOREQWRAP" => "DNS lookup",
        "GETNAMEINFOREQWRAP" => "DNS reverse lookup",
        "TCPCONNECTWRAP" => "TCP connect",
        "PIPECONNECTWRAP" => "pipe connect",
        "WRITEWRAP" => "write",
        "SHUTDOWNWRAP" => "shutdown",
        "UDPSENDWRAP" => "UDP send",
        "FILEHANDLECLOSEREQ" => "file handle close",
        _ => return None,
    })
}

/// The property Node calls on a handle's or request's object when its
/// work is done (`env_properties.h`'s strings): `None` for a provider
/// whose callback Node keeps elsewhere.
fn callback_property(provider: &str, request: bool) -> Option<&'static str> {
    if request {
        return (provider != "FSREQPROMISE").then_some("oncomplete");
    }
    Some(match provider {
        "TCPSERVERWRAP" | "PIPESERVERWRAP" => "onconnection",
        "PROCESSWRAP" => "onexit",
        "SIGNALWRAP" => "onsignal",
        "UDPWRAP" => "onmessage",
        "FSEVENTWRAP" | "STATWATCHER" => "onchange",
        _ => return None,
    })
}

/// The task of the wrapper at `wrap`.
fn entry(
    walk: &mut Walk<'_>,
    environment: Environment,
    wrap: u64,
    persistent: u64,
    kind: Kind,
) -> Result<RuntimeTask, Problem> {
    let reader = walk.reader;
    let layout = walk.layout;
    let (async_id, trigger) = async_ids(&reader, layout, wrap)?;
    let number = environment.owner.number(async_id)?;
    let class = class(walk, reader.word(wrap)?);
    let provider = provider(
        &reader,
        layout,
        u32_at(&reader, wrap + layout.provider_type)?,
    )?;

    let (state, detail) = match kind {
        Kind::Handle(pointer) => handle_state(&reader, wrap, reader.word(pointer)?, &provider)?,
        Kind::Request(request) => request_state(&reader, wrap, request, &provider)?,
    };

    let mut problems = Vec::new();
    let callback = callback_property(&provider, matches!(kind, Kind::Request(_)))
        .map(|property| -> Result<Option<TaskCallback>, Problem> {
            let Some(object) = object(&reader, layout, wrap, persistent)? else {
                return Ok(None);
            };
            match own_data(&reader, object, property)? {
                Some(function) => callback(&reader, walk.table, function),
                None => Ok(None),
            }
        })
        .transpose()
        .unwrap_or_else(|problem| {
            problems.push(format!("its callback cannot be read: {problem}"));
            None
        })
        .flatten();

    let detail = match problems.as_slice() {
        [] => detail,
        _ => format!("{detail} ({})", problems.join("; ")),
    };
    Ok(RuntimeTask {
        number,
        locator: wrap,
        state,
        detail: Some(detail.into()),
        thread: None,
        resume: None,
        creation: None,
        spawned: None,
        entry: None,
        parent: environment.owner.number(trigger).ok(),
        internal: false,
        // async_hooks' name for the resource, and the wrapper's class.
        labels: std::iter::once((Arc::from("type"), Arc::from(provider)))
            .chain(class.map(|class| (Arc::from("class"), Arc::from(class))))
            .collect(),
        callback,
        stackless: true,
    })
}

/// A handle's state and what it is, from its libuv handle at `handle`,
/// which must point back at its wrapper.
fn handle_state(
    reader: &Reader<'_>,
    wrap: u64,
    handle: u64,
    provider: &str,
) -> Result<(TaskState, String), Problem> {
    let noun = handle_noun(provider).map_or_else(|| provider.to_owned(), str::to_owned);
    if handle == 0 || reader.word(handle)? != wrap {
        return Ok((
            TaskState::Unknown("its libuv handle does not point back at it".into()),
            noun,
        ));
    }
    let ty = u32_at(reader, handle + UV_HANDLE_TYPE)?;
    let flags = u32_at(reader, handle + UV_HANDLE_FLAGS)?;
    let Some(uv) = usize::try_from(ty)
        .ok()
        .and_then(|ty| UV_HANDLE_TYPES.get(ty))
    else {
        return Ok((
            TaskState::Unknown(format!("its libuv handle has the unknown type {ty}").into()),
            noun,
        ));
    };
    let server = matches!(provider, "TCPSERVERWRAP" | "PIPESERVERWRAP");
    let (state, status) = if flags & UV_HANDLE_CLOSING != 0 {
        (TaskState::Exited, "closing")
    } else if flags & UV_HANDLE_ACTIVE == 0 {
        (TaskState::Blocked, "inactive")
    } else if server {
        // A server's handle is active once it listens (`uv_listen`).
        (TaskState::Blocked, "listening")
    } else {
        (TaskState::Blocked, "active")
    };
    let unreferenced = if flags & UV_HANDLE_REF == 0 {
        ", unref'd"
    } else {
        ""
    };
    let noun = if handle_noun(provider).is_some() {
        noun
    } else {
        format!("{noun} ({uv})")
    };
    Ok((state, format!("{noun} — {status}{unreferenced}")))
}

/// A request's state and what it is, from its libuv request at `request`,
/// which must point back at its wrapper.
fn request_state(
    reader: &Reader<'_>,
    wrap: u64,
    request: u64,
    provider: &str,
) -> Result<(TaskState, String), Problem> {
    if reader.word(request)? != wrap {
        return Ok((
            TaskState::Unknown("its libuv request does not point back at it".into()),
            request_noun(provider).unwrap_or(provider).to_owned(),
        ));
    }
    let ty = u32_at(reader, request + UV_REQUEST_TYPE)?;
    let noun = if ty == UV_FS {
        let operation = u32_at(reader, request + UV_FS_TYPE)?;
        let operation = usize::try_from(operation)
            .ok()
            .and_then(|operation| UV_FS_OPERATIONS.get(operation))
            .map_or_else(
                || format!("operation {operation}"),
                |name| (*name).to_owned(),
            );
        let path = reader.word(request + UV_FS_PATH)?;
        let path = if path == 0 {
            String::new()
        } else {
            format!(" {}", c_string(reader, path, 4096)?)
        };
        let promise = if provider == "FSREQPROMISE" {
            " (promise)"
        } else {
            ""
        };
        format!("fs {operation}{path}{promise}")
    } else {
        match (
            request_noun(provider),
            usize::try_from(ty)
                .ok()
                .and_then(|ty| UV_REQUEST_TYPES.get(ty)),
        ) {
            (Some(noun), _) => noun.to_owned(),
            (None, Some(uv)) => format!("{provider} ({uv})"),
            (None, None) => {
                return Ok((
                    TaskState::Unknown(
                        format!("its libuv request has the unknown type {ty}").into(),
                    ),
                    provider.to_owned(),
                ));
            }
        }
    };
    Ok((TaskState::Blocked, format!("{noun} — pending")))
}

/// The own data property `name` of `object`, as a tagged word; `None`
/// where it has none. An accessor is refused: Node's callbacks are data.
pub(super) fn own_data(
    reader: &Reader<'_>,
    object: Object,
    name: &str,
) -> Result<Option<u64>, Problem> {
    for property in super::own::named(reader, object)? {
        if property.symbol || !super::properties::is_named(reader, property.key.0, name)? {
            continue;
        }
        return match property.held {
            super::own::Held::Data(word) => Ok(Some(word)),
            super::own::Held::Double { .. } => Err(format!("{name} is a number").into()),
            super::own::Held::Accessor(_) => Err(format!("{name} is an accessor").into()),
        };
    }
    Ok(None)
}

/// The function `word` holds, if it holds one.
pub(super) fn function(reader: &Reader<'_>, word: u64) -> Result<Option<Object>, Problem> {
    let Some(object) = reader.object(word)? else {
        return Ok(None);
    };
    let ty = reader.instance_type(object)?;
    let functions = reader.constant(Constant::FirstJSFunctionType)?
        ..=reader.constant(Constant::LastJSFunctionType)?;
    Ok(functions.contains(&ty).then_some(object))
}

pub(super) fn shared(reader: &Reader<'_>, function: Object) -> Result<Object, Problem> {
    reader.typed_field(
        function,
        Field::JSFunctionSharedFunctionInfo,
        Constant::SharedFunctionInfoType,
    )
}

/// The path of the script `function` was compiled from, if it has one.
pub(super) fn script_path(
    reader: &Reader<'_>,
    table: &mut Table,
    function: Object,
) -> Result<Option<(Object, Arc<str>)>, Problem> {
    let Some(script) = super::frames::script(reader, shared(reader, function)?)? else {
        return Ok(None);
    };
    Ok(table.path(reader, script)?.map(|path| (script, path)))
}

/// Whether `path` is one of Node's own scripts, as `node:net`.
fn is_nodes(path: &str) -> bool {
    path.starts_with("node:")
}

/// The callback Node will call through the function `word` holds: the
/// function itself, or where it is a wrapper of Node's, as `fs`'s
/// `makeCallback` makes, that captures exactly one function of the
/// program's, that function.
pub(super) fn callback(
    reader: &Reader<'_>,
    table: &mut Table,
    word: u64,
) -> Result<Option<TaskCallback>, Problem> {
    let Some(called) = function(reader, word)? else {
        return Ok(None);
    };
    let mut shown = called;
    if script_path(reader, table, called)?.is_some_and(|(_, path)| is_nodes(&path)) {
        let context = reader
            .object(reader.field(called, Field::JSFunctionContext)?)?
            .ok_or("a function's context is not an object")?;
        let mut captured = Vec::new();
        for (_, value) in super::variables::locals(reader, context)? {
            let Ok(value) = value else { continue };
            if let Some(inner) = function(reader, value)?
                && script_path(reader, table, inner)?.is_some_and(|(_, path)| !is_nodes(&path))
            {
                captured.push(inner);
            }
        }
        if let [inner] = captured.as_slice() {
            shown = *inner;
        }
    }
    describe(reader, table, shown).map(Some)
}

/// The function `function` as its callback: its name, and where its
/// source begins.
fn describe(
    reader: &Reader<'_>,
    table: &mut Table,
    function: Object,
) -> Result<TaskCallback, Problem> {
    let shared = shared(reader, function)?;
    let name = super::names::debug_name(reader, function, shared)?;
    let name: Arc<str> = if name.is_empty() {
        "<anonymous>".into()
    } else {
        name.into()
    };
    let place = match (
        script_path(reader, table, function)?,
        start_position(reader, shared)?,
    ) {
        (Some((script, path)), Some(offset)) => {
            let (line, column) = table.line_column(reader, script, offset)?;
            Some(crate::RecordedPlace { path, line, column })
        }
        _ => None,
    };
    Ok(TaskCallback {
        function: name,
        place,
    })
}

/// Where the source of the function `shared` describes begins, as
/// `SharedFunctionInfo::StartPosition` gives it: from its scope once it
/// is compiled, else from its uncompiled data; `None` for one with none.
fn start_position(reader: &Reader<'_>, shared: Object) -> Result<Option<u64>, Problem> {
    let held = reader.field(shared, Field::SharedFunctionInfoNameOrScopeInfo)?;
    if let Some(scope) = reader.object(held)?
        && reader.is(scope, Constant::ScopeInfoType)?
    {
        let flags = super::names::scope_flags(reader, scope)?;
        let ty = (flags & reader.constant(Constant::ScopeTypeMask)?)
            >> reader.constant(Constant::ScopeTypeShift)?;
        if ty == FUNCTION_SCOPE {
            let start = if reader.layout.has(Field::ScopeInfoPositionInfo) {
                Some(reader.field_address(scope, Field::ScopeInfoPositionInfo)?)
            } else {
                let untagged = scope.0 - reader.constant(Constant::HeapObjectTag)?.cast_unsigned();
                scope_info_slice(
                    reader.version,
                    ScopeInfoSlice::PositionInfo,
                    &mut |offset| reader.word(untagged + offset).ok(),
                )?
                .filter(|slice| slice.length > 0)
                .map(|slice| untagged + slice.offset)
            };
            if let Some(start) = start
                && let Some(position) = reader.smi(reader.word(start)?)?
                && let Ok(position) = u64::try_from(position)
            {
                return Ok(Some(position));
            }
        }
    }
    let Some(data) = super::names::function_data(reader, shared)? else {
        return Ok(None);
    };
    let ty = reader.instance_type(data)?;
    let uncompiled = [
        Constant::UncompiledDataWithPreparseDataType,
        Constant::UncompiledDataWithoutPreparseDataType,
    ]
    .iter()
    .any(|kind| reader.constant(*kind).ok() == Some(ty));
    if !uncompiled {
        return Ok(None);
    }
    let position = reader.int32_field(data, Field::UncompiledDataStartPosition)?;
    Ok(u64::try_from(position).ok())
}
