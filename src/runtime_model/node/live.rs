//! The JavaScript frames of a thread that runs on, read from what V8
//! publishes of them in its isolate.
//!
//! JavaScript keeps no frame pointer anywhere but in a thread's rbp, which
//! only stopping the thread shows. When JavaScript calls out to native
//! code, V8 publishes where its frames begin, in the isolate the thread
//! runs: a call through `CEntry`, or into an API callback or accessor,
//! builds an exit frame and stores its frame pointer as `ThreadLocalTop`'s
//! `c_entry_fp_`; a fast C call, which builds none, stores its caller's
//! frame pointer and return address in `IsolateData`. `JSEntry` saves both
//! in its entry frame and clears them as it enters JavaScript again, and
//! keeps the outermost entry frame's pointer as `js_entry_sp_`, 0 while
//! the thread runs no JavaScript. A thread blocked in native code that
//! JavaScript called can therefore be read from memory alone, frame by
//! frame, as the stopped thread is, each frame described as
//! [`frames::describe`] describes it at a stop.
//!
//! The caller proves the thread did not run while it was read
//! ([`LiveThread`]). What this module proves before it believes a word:
//!
//! - `ThreadLocalTop`'s `isolate_` is the isolate it belongs to, which a
//!   wrong layout would not give;
//! - the thread entered its isolate from no other isolate, whose
//!   JavaScript would lie below the isolate's outermost entry, beyond what
//!   the isolate publishes ([`Constant::EntryStackItemPreviousIsolate`]);
//! - no frame pointer is published without an outermost entry, the stack
//!   can be walked (V8's deoptimizer clears `stack_is_iterable` while it
//!   rewrites a frame), and V8 is not collecting garbage;
//! - an exit frame's marker is an exit frame's, and every frame lies on
//!   the thread's own stack, above its stack pointer and up to the
//!   outermost entry, each above the one before.
//!
//! A thread whose isolate publishes frames on another thread's stack is a
//! thread that holds the isolate without having entered it, as V8's
//! helper threads do while they work for it (`LocalHeap::SetCurrent`), and
//! runs none of its JavaScript. V8 runs no JavaScript on such a thread:
//! only `Isolate::Enter` lets a thread run an isolate's code, and V8 makes
//! a `LocalHeap` set the current isolate only on threads that run its
//! background work. That holds while the isolate's heap is its own: with
//! V8's shared heap on (`--shared-heap`, `--shared-string-table`,
//! `--harmony-struct`), which Node leaves off unless asked, isolates share
//! work V8 does not promise to keep apart, and such a thread is left out
//! with that reason ([`SHARED_HEAP`]), read from the isolate's own
//! `uses_shared_heap_flag`.
//!
//! Anything else leaves the thread out with the reason: never a shorter
//! stack, nor a guessed one.

use super::Statics;
use super::frames;
use super::objects::{Problem, Reader};
use super::positions::Encoding;
use super::scripts::Table;
use crate::image::v8::Constant;
use crate::runtime_model::{
    CodeAt, CodeFrame, InterpretedFrame, LiveFrames, LiveThread, RuntimeStop,
};
use crate::unwind::{RBP, RegisterFile};
use crate::{AddressRange, CodeRole, ImageAddress, SourceLanguage, ThreadLocal, VirtualAddress};

/// The deepest stack read.
const MOST_FRAMES: usize = 4096;

/// The most entries of one isolate read.
const MOST_ENTRIES: usize = 64;

/// Why a thread's frames are not read, each named once so that a profile
/// counts the threads left out for each.
pub(super) const RUNNING: &str = "it runs JavaScript, whose frames V8 publishes only while \
                                  JavaScript calls out to native code";
pub(super) const COLLECTING: &str = "V8 is collecting its isolate's garbage";
pub(super) const NOT_ITERABLE: &str =
    "V8 is deoptimizing a frame of its isolate, which leaves its stack unwalkable";
pub(super) const NESTED: &str = "it entered its isolate while it ran another isolate's \
                                 JavaScript, whose frames that isolate does not publish";
pub(super) const STALE_EXIT: &str = "the exit frame its isolate names is gone";
pub(super) const OFF_STACK: &str = "a frame its isolate names is not on its stack";
pub(super) const OUTSIDE_STACK: &str = "its stack pointer is outside its stack";
pub(super) const SHARED_HEAP: &str = "it holds an isolate whose JavaScript runs on another \
     thread, which proves it runs none only while V8's shared heap is off, and it is on";
pub(super) const WEBASSEMBLY: &str = "it runs WebAssembly, whose frames a live read does not walk";
pub(super) const FAST_FROM_BUILTIN: &str = "it is in a fast C call from V8's own code in a \
     JavaScript frame, which saves no bytecode offset, so its line is not known";
pub(super) const UNPUBLISHED: &str =
    "its isolate publishes an exit frame or a fast C call but no entry into JavaScript";
pub(super) const FOREIGN_CODE: &str =
    "a frame its isolate names runs code that is neither V8's builtins nor code V8 generated";

/// What reading a thread needs of the module that carries V8.
pub(super) struct Context<'a> {
    pub table: &'a mut Table,
    pub encoding: Encoding,
    pub statics: &'a Statics,

    /// Where each thread keeps its current isolate.
    pub isolate: ThreadLocal,

    /// How far into an isolate its stack of entries is.
    pub entry_stack: u64,

    /// The embedded builtins, by the image's addresses its symbols give,
    /// and by the virtual addresses V8 stored for them.
    pub builtins: Option<AddressRange<ImageAddress>>,
    pub stored_builtins: Option<AddressRange<VirtualAddress>>,
}

/// The words an isolate publishes about its thread's frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Published {
    isolate: u64,
    c_entry_fp: u64,
    js_entry_sp: u64,
    fast_fp: u64,
    fast_pc: u64,
    stack_is_iterable: u8,
    uses_shared_heap: u8,
    vm_state: u16,
}

/// The offsets [`Published`] is read at, and the frames' slots.
struct Offsets {
    top: u64,
    top_isolate: u64,
    c_entry_fp: u64,
    js_entry_sp: u64,
    vm_state: u64,
    gc: u16,
    fast_fp: u64,
    fast_pc: u64,
    stack_is_iterable: u64,
    uses_shared_heap: u64,
    caller_fp: i64,
    caller_pc: i64,
    marker: i64,
    next_exit: i64,
    next_fast: Option<(i64, i64)>,
    previous_isolate: u64,
    previous_item: u64,
}

impl Offsets {
    fn of(reader: &Reader<'_>) -> Result<Self, Problem> {
        let constant = |constant: Constant| reader.constant(constant);
        let unsigned = |constant: Constant| -> Result<u64, Problem> {
            u64::try_from(reader.constant(constant)?)
                .map_err(|_| format!("{} is negative", constant.name()).into())
        };
        let fast = match (
            reader
                .layout
                .constant(Constant::EntryFrameNextFastCallFrameFpOffset),
            reader
                .layout
                .constant(Constant::EntryFrameNextFastCallFramePcOffset),
        ) {
            (Some(fp), Some(pc)) => Some((fp, pc)),
            (None, None) => None,
            _ => return Err("the entry frame saves a fast C call's fp or pc but not both".into()),
        };
        let gc = u16::try_from(constant(Constant::StateTagGc)?)
            .map_err(|_| Problem::from("StateTag::GC is no 16-bit state"))?;
        Ok(Self {
            top: unsigned(Constant::IsolateThreadLocalTopOffset)?,
            top_isolate: unsigned(Constant::ThreadLocalTopIsolate)?,
            c_entry_fp: unsigned(Constant::ThreadLocalTopCEntryFp)?,
            js_entry_sp: unsigned(Constant::ThreadLocalTopJsEntrySp)?,
            vm_state: unsigned(Constant::ThreadLocalTopCurrentVmState)?,
            gc,
            fast_fp: unsigned(Constant::IsolateFastCCallCallerFpOffset)?,
            fast_pc: unsigned(Constant::IsolateFastCCallCallerPcOffset)?,
            stack_is_iterable: unsigned(Constant::IsolateStackIsIterableOffset)?,
            uses_shared_heap: unsigned(Constant::IsolateUsesSharedHeapFlagOffset)?,
            caller_fp: constant(Constant::CommonFrameCallerFpOffset)?,
            caller_pc: constant(Constant::CommonFrameCallerPcOffset)?,
            marker: constant(Constant::CommonFrameContextOrFrameTypeOffset)?,
            next_exit: constant(Constant::EntryFrameNextExitFrameFpOffset)?,
            next_fast: fast,
            previous_isolate: unsigned(Constant::EntryStackItemPreviousIsolate)?,
            previous_item: unsigned(Constant::EntryStackItemPreviousItem)?,
        })
    }
}

/// The markers, each a frame type as a Smi, that tell V8's own frames
/// apart.
struct Markers {
    exits: Vec<u64>,
    entries: Vec<u64>,
    webassembly: Vec<u64>,
}

impl Markers {
    fn of(reader: &Reader<'_>) -> Result<Self, Problem> {
        let marker = |constant: Constant| {
            reader
                .layout
                .constant(constant)
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| value << 1)
        };
        let all = |constants: &[Constant]| constants.iter().copied().filter_map(marker).collect();
        let markers = Self {
            exits: all(&[
                Constant::FrameExit,
                Constant::FrameBuiltinExit,
                Constant::FrameApiCallbackExit,
                Constant::FrameApiAccessorExit,
                Constant::FrameApiNamedAccessorExit,
                Constant::FrameApiIndexedAccessorExit,
                Constant::FrameApiConstructExit,
            ]),
            entries: all(&[Constant::FrameEntry, Constant::FrameConstructEntry]),
            webassembly: all(&[
                Constant::FrameWasmExit,
                Constant::FrameWasm,
                Constant::FrameWasmToJs,
                Constant::FrameWasmToJsFunction,
                Constant::FrameJsToWasm,
                Constant::FrameCWasmEntry,
                Constant::FrameWasmDebugBreak,
                Constant::FrameWasmJspi,
                Constant::FrameWasmLiftoffSetup,
                Constant::FrameWasmSegmentStart,
                Constant::FrameWasmStackEntry,
                Constant::FrameWasmStackExit,
                Constant::FrameStackSwitch,
            ]),
        };
        if markers.entries.is_empty()
            || !markers
                .exits
                .contains(&marker(Constant::FrameExit).unwrap_or(1))
        {
            return Err("the layout names no entry or exit frame".into());
        }
        Ok(markers)
    }
}

/// The JavaScript frames of `thread`, read by `reader` through `context`.
pub(super) fn read(
    stop: &dyn RuntimeStop,
    reader: &Reader<'_>,
    context: &mut Context<'_>,
    thread: &LiveThread<'_>,
) -> LiveFrames {
    match frames_of(stop, reader, context, thread) {
        Ok(frames) => frames,
        Err(Unread::Named(reason)) => LiveFrames::Unread(reason.into()),
        Err(Unread::Problem(problem)) => {
            LiveFrames::Unread(format!("its frames cannot be read: {problem}").into())
        }
    }
}

/// Why a thread's frames are not read: one of this module's named reasons,
/// or what could not be read.
#[derive(Debug)]
enum Unread {
    Named(&'static str),
    Problem(Problem),
}

impl From<Problem> for Unread {
    fn from(problem: Problem) -> Self {
        Self::Problem(problem)
    }
}

fn frames_of(
    stop: &dyn RuntimeStop,
    reader: &Reader<'_>,
    context: &mut Context<'_>,
    thread: &LiveThread<'_>,
) -> Result<LiveFrames, Unread> {
    // Read with the reader's blocks, unlike `current_isolate`'s: the slot
    // lies in the thread's own pages, which a read before this one read.
    let cannot = |reason: &dyn std::fmt::Display| {
        Problem::from(format!("its isolate cannot be found: {reason}"))
    };
    let slot = stop
        .thread_local_address(thread.thread, context.isolate)
        .map_err(|reason| cannot(&reason))?;
    let isolate = reader
        .word(slot.get())
        .map_err(|_| cannot(&"the thread's current isolate is unreadable"))?;
    if isolate == 0 {
        return Ok(LiveFrames::None);
    }
    let offsets = Offsets::of(reader)?;
    let markers = Markers::of(reader)?;
    let published = publish(reader, &offsets, isolate)?;
    if published.isolate != isolate {
        return Err(Problem::from(format!(
            "the isolate at {isolate:#x} names {:#x} as its own, so its layout is not the one \
             microscope read or it is not set up yet",
            published.isolate
        ))
        .into());
    }
    let sp = thread.stack_pointer;
    if !thread.stack.contains(&sp) {
        return Err(Unread::Named(OUTSIDE_STACK));
    }
    if published.js_entry_sp == 0 {
        if published.c_entry_fp != 0 || published.fast_fp != 0 {
            return Err(Unread::Named(UNPUBLISHED));
        }
        entered_alone(reader, &offsets, isolate, context.entry_stack)?;
        return Ok(LiveFrames::None);
    }
    // The outermost entry on another thread's stack: the isolate's
    // JavaScript runs there, and this thread only holds the isolate, as
    // V8's helpers do, unless the heap is shared, which lets V8 set the
    // isolate where it otherwise would not.
    if !thread.stack.contains(&published.js_entry_sp) {
        if published.uses_shared_heap != 0 {
            return Err(Unread::Named(SHARED_HEAP));
        }
        return Ok(LiveFrames::None);
    }
    if published.js_entry_sp <= sp {
        return Err(Unread::Named(OFF_STACK));
    }
    entered_alone(reader, &offsets, isolate, context.entry_stack)?;
    if published.stack_is_iterable != 1 {
        return Err(Unread::Named(NOT_ITERABLE));
    }
    if published.vm_state == offsets.gc {
        return Err(Unread::Named(COLLECTING));
    }

    let walk = Walk {
        reader,
        offsets: &offsets,
        markers: &markers,
        sp,
        js_entry_sp: published.js_entry_sp,
    };
    let start = match (published.fast_fp, published.c_entry_fp) {
        (0, 0) => return Err(Unread::Named(RUNNING)),
        (0, exit) => Start::Exit(walk.exit_caller(exit)?),
        (fp, 0) => Start::Fast(fp, published.fast_pc),
        _ => {
            return Err(Problem::from(
                "its isolate publishes both an exit frame and a fast C call",
            )
            .into());
        }
    };
    walk.frames(context, thread, start)
}

/// The words `isolate` publishes, read at `offsets`.
fn publish(reader: &Reader<'_>, offsets: &Offsets, isolate: u64) -> Result<Published, Problem> {
    let top = isolate.wrapping_add(offsets.top);
    let word = |offset: u64| reader.word(top.wrapping_add(offset));
    let mut flag = [0; 1];
    reader.bytes_into(isolate.wrapping_add(offsets.stack_is_iterable), &mut flag)?;
    let mut shared = [0; 1];
    reader.bytes_into(isolate.wrapping_add(offsets.uses_shared_heap), &mut shared)?;
    let mut state = [0; 2];
    reader.bytes_into(top.wrapping_add(offsets.vm_state), &mut state)?;
    Ok(Published {
        isolate: word(offsets.top_isolate)?,
        c_entry_fp: word(offsets.c_entry_fp)?,
        js_entry_sp: word(offsets.js_entry_sp)?,
        fast_fp: reader.word(isolate.wrapping_add(offsets.fast_fp))?,
        fast_pc: reader.word(isolate.wrapping_add(offsets.fast_pc))?,
        stack_is_iterable: flag[0],
        uses_shared_heap: shared[0],
        vm_state: u16::from_le_bytes(state),
    })
}

/// Fails unless the thread that entered `isolate` had entered no other
/// isolate before it, whose JavaScript would lie below this one's.
fn entered_alone(
    reader: &Reader<'_>,
    offsets: &Offsets,
    isolate: u64,
    entry_stack: u64,
) -> Result<(), Unread> {
    let mut item = reader.word(isolate.wrapping_add(entry_stack))?;
    for _ in 0..MOST_ENTRIES {
        if item == 0 {
            return Ok(());
        }
        if reader.word(item.wrapping_add(offsets.previous_isolate))? != 0 {
            return Err(Unread::Named(NESTED));
        }
        item = reader.word(item.wrapping_add(offsets.previous_item))?;
    }
    Err(Problem::from(format!(
        "its isolate keeps more than {MOST_ENTRIES} entries"
    ))
    .into())
}

/// Where a walk begins: the frame that called out of JavaScript, by its
/// frame pointer and return address, through an exit frame or a fast C
/// call.
#[derive(Debug, Clone, Copy)]
enum Start {
    Exit((u64, u64)),
    Fast(u64, u64),
}

/// A walk of one thread's frames, from where its isolate says they begin
/// to its outermost entry.
struct Walk<'a, 'r> {
    reader: &'a Reader<'r>,
    offsets: &'a Offsets,
    markers: &'a Markers,
    sp: u64,
    js_entry_sp: u64,
}

impl Walk<'_, '_> {
    fn word(&self, fp: u64, offset: i64) -> Result<u64, Problem> {
        self.reader.word(fp.wrapping_add_signed(offset))
    }

    /// The caller of the exit frame at `fp`, by its frame pointer and
    /// return address.
    fn exit_caller(&self, fp: u64) -> Result<(u64, u64), Unread> {
        if fp <= self.sp || fp >= self.js_entry_sp {
            return Err(Unread::Named(OFF_STACK));
        }
        let marker = self.word(fp, self.offsets.marker)?;
        if self.markers.webassembly.contains(&marker) {
            return Err(Unread::Named(WEBASSEMBLY));
        }
        if !self.markers.exits.contains(&marker) {
            return Err(Unread::Named(STALE_EXIT));
        }
        Ok((
            self.word(fp, self.offsets.caller_fp)?,
            self.word(fp, self.offsets.caller_pc)?,
        ))
    }

    fn frames(
        &self,
        context: &mut Context<'_>,
        thread: &LiveThread<'_>,
        start: Start,
    ) -> Result<LiveFrames, Unread> {
        let (mut fp, mut pc, mut fast) = match start {
            Start::Exit((fp, pc)) => (fp, pc, false),
            Start::Fast(fp, pc) => (fp, pc, true),
        };
        let mut frames: Vec<InterpretedFrame> = Vec::new();
        let mut below = self.sp;
        for _ in 0..MOST_FRAMES {
            if fp <= below || fp > self.js_entry_sp {
                return Err(Unread::Named(OFF_STACK));
            }
            below = fp;
            let marker = self.word(fp, self.offsets.marker)?;
            if self.markers.entries.contains(&marker) {
                if fp == self.js_entry_sp {
                    if let Some(outermost) = frames.last_mut() {
                        outermost.outermost = true;
                    }
                    return Ok(LiveFrames::Frames(frames));
                }
                (fp, pc, fast) = self.inner_entry(fp)?;
                continue;
            }
            if self.markers.exits.contains(&marker) {
                return Err(Problem::from("an exit frame is a JavaScript frame's caller").into());
            }
            if self.markers.webassembly.contains(&marker) {
                return Err(Unread::Named(WEBASSEMBLY));
            }

            let generated = match (thread.code)(VirtualAddress::new(pc)) {
                CodeAt::Generated => true,
                CodeAt::Image(address) if is_builtin(self.reader, context, address) => false,
                CodeAt::Image(_) | CodeAt::Other => return Err(Unread::Named(FOREIGN_CODE)),
            };
            // A fast C call from V8's own code in a JavaScript frame, as a
            // bytecode handler's, saves no bytecode offset first
            // (`CodeAssembler::CallCFunction` runs no `CallPrologue`), so
            // the frame's offset may be an earlier call's. A frame of V8's
            // own keeps a Smi, its marker, where JavaScript keeps its
            // context.
            if std::mem::take(&mut fast) && !generated && marker & 1 != 0 {
                return Err(Unread::Named(FAST_FROM_BUILTIN));
            }
            self.describe(context, thread, (fp, pc, generated), &mut frames)?;
            (fp, pc) = (
                self.word(fp, self.offsets.caller_fp)?,
                self.word(fp, self.offsets.caller_pc)?,
            );
        }
        Err(Problem::from(format!("its stack is deeper than {MOST_FRAMES} frames")).into())
    }

    /// Where the walk goes on past the inner entry frame at `fp`: the
    /// frame that called out of JavaScript before it, which it saved.
    fn inner_entry(&self, fp: u64) -> Result<(u64, u64, bool), Unread> {
        if let Some((fast_fp, fast_pc)) = self.offsets.next_fast {
            let saved = self.word(fp, fast_fp)?;
            if saved != 0 {
                return Ok((saved, self.word(fp, fast_pc)?, true));
            }
        }
        let exit = self.word(fp, self.offsets.next_exit)?;
        if exit == 0 {
            return Err(Problem::from(
                "an inner entry frame saved no frame that called out of JavaScript",
            )
            .into());
        }
        let (caller, pc) = self.exit_caller(exit)?;
        Ok((caller, pc, false))
    }

    /// Adds what the frame at `fp`, at `pc`, runs to `frames`, as a stopped
    /// thread's frame is described.
    fn describe(
        &self,
        context: &mut Context<'_>,
        thread: &LiveThread<'_>,
        (fp, pc, generated): (u64, u64, bool),
        frames: &mut Vec<InterpretedFrame>,
    ) -> Result<(), Unread> {
        let registers = RegisterFile::new([(RBP, fp)]);
        let frame = CodeFrame {
            thread: thread.thread,
            instruction: VirtualAddress::new(pc),
            after_call: true,
            generated,
            image_instruction: None,
            entered: true,
            registers: &registers,
        };
        let described = frames::describe(
            self.reader,
            context.table,
            context.encoding,
            context.statics,
            &frame,
        )
        .ok_or_else(|| Problem::from("a frame its isolate names runs nothing V8 describes"))?;
        let role = described.role;
        for function in described.functions {
            if let Some(problem) = function.problem {
                return Err(
                    Problem::from(format!("a frame's place cannot be read: {problem}")).into(),
                );
            }
            frames.push(InterpretedFrame {
                frame: VirtualAddress::new(fp),
                language: SourceLanguage::JavaScript,
                function: Some(function.function),
                place: function.place,
                problem: None,
                first_line: function.first_line,
                outermost: false,
                role: if role == CodeRole::Interpreter {
                    CodeRole::RuntimeInternal
                } else {
                    role
                },
            });
        }
        if let Some(awaiting) = described.awaiting {
            if let Some(end) = awaiting.end {
                return Err(Problem::from(format!(
                    "the async functions that wait for a frame cannot all be read: {end}"
                ))
                .into());
            }
            for waiting in awaiting.frames {
                if let Some(problem) = &waiting.problem {
                    return Err(Problem::from(format!(
                        "an async function that waits for a frame cannot be read: {problem}"
                    ))
                    .into());
                }
                frames.push(waiting);
            }
        }
        Ok(())
    }
}

/// Whether the image's code at `address` is one of V8's embedded builtins:
/// within the bounds its symbols give, or else within those V8 stored, in
/// the module's own mapping.
fn is_builtin(reader: &Reader<'_>, context: &Context<'_>, address: ImageAddress) -> bool {
    if let Some(builtins) = context.builtins {
        return builtins.start <= address && address < builtins.end;
    }
    let bias = reader.stop.load_bias();
    context.stored_builtins.is_some_and(|stored| {
        bias.load(address)
            .is_some_and(|loaded| stored.start <= loaded && loaded < stored.end)
    })
}

#[cfg(test)]
mod tests {
    //! Each rule the reader proves before it believes a word, on a thread
    //! the tests write by hand on 14.6's generated layout: an isolate that
    //! publishes an exit frame whose caller is the outermost entry frame,
    //! which holds no JavaScript frame, then each lie or state a rule must
    //! catch.

    use std::collections::HashMap;

    use super::super::objects::Reader;
    use super::super::positions::Encoding;
    use super::super::scripts::Table;
    use super::super::{DispatchTable, Statics};
    use super::*;
    use crate::image::v8::{V8Layout, V8Version};
    use crate::{LoadBias, ThreadId};

    /// Memory the tests write, by address; what no test wrote is
    /// unreadable.
    #[derive(Default, Clone)]
    struct Memory(HashMap<u64, u8>);

    impl Memory {
        fn word(&mut self, address: u64, word: u64) {
            for (at, byte) in (address..).zip(word.to_le_bytes()) {
                self.0.insert(at, byte);
            }
        }

        fn byte(&mut self, address: u64, byte: u8) {
            self.0.insert(address, byte);
        }
    }

    impl RuntimeStop for Memory {
        fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
            for (at, byte) in (address.get()..).zip(bytes.iter_mut()) {
                let Some(read) = self.0.get(&at) else {
                    return false;
                };
                *byte = *read;
            }
            true
        }

        fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
            Some(THREAD_POINTER)
        }

        fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
            None
        }

        fn load_bias(&self) -> LoadBias {
            LoadBias::default()
        }

        fn threads(&self) -> Vec<ThreadId> {
            vec![THREAD]
        }
    }

    const VERSION: V8Version = V8Version {
        major: 14,
        minor: 6,
        build: 0,
        patch: 0,
    };

    const THREAD: ThreadId = ThreadId::new(7);
    const THREAD_POINTER: u64 = 0x7000_0000;
    /// Where the thread keeps its isolate, from its thread pointer.
    const SLOT: i64 = 0x10;
    const ISOLATE: u64 = 0x1000_0000;
    /// How far into an isolate its entries are, and the one entry.
    const ENTRY_STACK: u64 = 0x2000;
    const ITEM: u64 = 0x1100_0000;

    /// The thread's stack, its stack pointer, and its frames: the exit
    /// frame, an inner entry frame and the exit frame it saved, and the
    /// outermost entry frame.
    const STACK: std::ops::Range<u64> = 0x5000_0000..0x6000_0000;
    const SP: u64 = 0x5000_1000;
    const EXIT: u64 = 0x5000_2000;
    const INNER: u64 = 0x5000_3000;
    const SAVED_EXIT: u64 = 0x5000_4000;
    const ENTRY: u64 = 0x5000_8000;
    const OTHER_STACK: u64 = 0x6800_0000;

    /// The frame types the tests give the markers, chosen by hand: any
    /// distinct numbers serve.
    const ENTRY_TYPE: i64 = 1;
    const CONSTRUCT_ENTRY_TYPE: i64 = 2;
    const EXIT_TYPE: i64 = 3;
    const WASM_TYPE: i64 = 4;
    const STUB_TYPE: i64 = 5;

    const fn marker(frame_type: i64) -> u64 {
        frame_type.cast_unsigned() << 1
    }

    /// Code: the embedded builtins, and an address in no code V8 owns.
    const BUILTIN: u64 = 0x40_0000;
    const BUILTINS: AddressRange<ImageAddress> = AddressRange {
        start: ImageAddress::new(0x40_0000),
        end: ImageAddress::new(0x50_0000),
    };
    const FOREIGN: u64 = 0x9000_0000;

    fn layout() -> V8Layout {
        let mut layout = crate::debug_info::v8::generated_layout(14, 6).expect("a layout");
        for (constant, value) in [
            (Constant::FrameEntry, ENTRY_TYPE),
            (Constant::FrameConstructEntry, CONSTRUCT_ENTRY_TYPE),
            (Constant::FrameExit, EXIT_TYPE),
            (Constant::FrameWasm, WASM_TYPE),
        ] {
            layout.set_constant(constant, value);
        }
        layout
    }

    /// The offset `constant` gives in `layout`.
    fn at(layout: &V8Layout, constant: Constant) -> i64 {
        layout.constant(constant).expect("a generated offset")
    }

    /// The thread as the tests begin: blocked in native code that the
    /// outermost JavaScript entry's frame called through an exit frame.
    fn blocked(layout: &V8Layout) -> Memory {
        let offset = |constant| at(layout, constant).cast_unsigned();
        let top = ISOLATE + offset(Constant::IsolateThreadLocalTopOffset);
        let mut memory = Memory::default();
        memory.word(THREAD_POINTER.wrapping_add_signed(SLOT), ISOLATE);
        memory.word(top + offset(Constant::ThreadLocalTopIsolate), ISOLATE);
        memory.word(top + offset(Constant::ThreadLocalTopCEntryFp), EXIT);
        memory.word(top + offset(Constant::ThreadLocalTopJsEntrySp), ENTRY);
        // An external state, which is not the collector's.
        memory.word(top + offset(Constant::ThreadLocalTopCurrentVmState), 6);
        memory.word(
            ISOLATE + offset(Constant::IsolateFastCCallCallerFpOffset),
            0,
        );
        memory.word(
            ISOLATE + offset(Constant::IsolateFastCCallCallerPcOffset),
            0,
        );
        memory.byte(ISOLATE + offset(Constant::IsolateStackIsIterableOffset), 1);
        memory.byte(
            ISOLATE + offset(Constant::IsolateUsesSharedHeapFlagOffset),
            0,
        );
        memory.word(ISOLATE + ENTRY_STACK, ITEM);
        memory.word(ITEM + offset(Constant::EntryStackItemPreviousIsolate), 0);
        memory.word(ITEM + offset(Constant::EntryStackItemPreviousItem), 0);
        frame(
            layout,
            &mut memory,
            EXIT,
            marker(EXIT_TYPE),
            (ENTRY, BUILTIN),
        );
        frame(layout, &mut memory, ENTRY, marker(ENTRY_TYPE), (0, 0));
        memory
    }

    /// Writes the frame at `fp`: its marker, and its caller's frame
    /// pointer and return address.
    fn frame(layout: &V8Layout, memory: &mut Memory, fp: u64, marker: u64, caller: (u64, u64)) {
        let slot = |constant| fp.wrapping_add_signed(at(layout, constant));
        memory.word(slot(Constant::CommonFrameContextOrFrameTypeOffset), marker);
        memory.word(slot(Constant::CommonFrameCallerFpOffset), caller.0);
        memory.word(slot(Constant::CommonFrameCallerPcOffset), caller.1);
    }

    /// Writes `value` to the isolate's word at the generated `constant`,
    /// within its `ThreadLocalTop` if `top`.
    fn publish(layout: &V8Layout, memory: &mut Memory, constant: Constant, top: bool, value: u64) {
        let base = if top {
            ISOLATE + at(layout, Constant::IsolateThreadLocalTopOffset).cast_unsigned()
        } else {
            ISOLATE
        };
        memory.word(base + at(layout, constant).cast_unsigned(), value);
    }

    fn code_at(address: VirtualAddress) -> CodeAt {
        if (BUILTINS.start.get()..BUILTINS.end.get()).contains(&address.get()) {
            CodeAt::Image(ImageAddress::new(address.get()))
        } else {
            CodeAt::Other
        }
    }

    /// What the reader reads of `memory`'s thread, with its stack pointer
    /// at `sp`.
    fn read_at(layout: &V8Layout, memory: &Memory, sp: u64) -> LiveFrames {
        let reader = Reader {
            stop: memory,
            layout,
            version: VERSION,
            charge: None,
            blocks: None,
        };
        let statics = Statics {
            isolate: Some(Ok(ThreadLocal::Offset(SLOT))),
            dispatch_table: Ok(None::<DispatchTable>),
            handlers: Vec::new(),
            awaits: Err("no await chain is read".into()),
        };
        let mut table = Table::default();
        let mut context = Context {
            table: &mut table,
            encoding: Encoding::of(VERSION),
            statics: &statics,
            isolate: ThreadLocal::Offset(SLOT),
            entry_stack: ENTRY_STACK,
            builtins: Some(BUILTINS),
            stored_builtins: None,
        };
        let thread = LiveThread {
            thread: THREAD,
            stack_pointer: sp,
            stack: STACK,
            code: &code_at,
        };
        read(memory, &reader, &mut context, &thread)
    }

    fn read_of(layout: &V8Layout, memory: &Memory) -> LiveFrames {
        read_at(layout, memory, SP)
    }

    fn unread(reason: &str) -> LiveFrames {
        LiveFrames::Unread(reason.into())
    }

    /// Whether `frames` leaves the thread out with a problem that says
    /// `text`.
    fn problem(frames: &LiveFrames, text: &str) -> bool {
        matches!(frames, LiveFrames::Unread(reason)
            if reason.starts_with("its frames cannot be read") && reason.contains(text))
    }

    #[test]
    fn an_exit_frame_called_from_the_outermost_entry_holds_no_javascript() {
        let layout = layout();
        let memory = blocked(&layout);
        assert_eq!(read_of(&layout, &memory), LiveFrames::Frames(Vec::new()));
    }

    #[test]
    fn a_thread_in_no_isolate_or_outside_javascript_has_no_frames() {
        let layout = layout();
        let mut memory = blocked(&layout);
        memory.word(THREAD_POINTER.wrapping_add_signed(SLOT), 0);
        assert_eq!(read_of(&layout, &memory), LiveFrames::None, "no isolate");

        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopJsEntrySp,
            true,
            0,
        );
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopCEntryFp,
            true,
            0,
        );
        assert_eq!(read_of(&layout, &memory), LiveFrames::None, "no entry");

        // A helper thread that holds the isolate while JavaScript runs on
        // the thread that entered it.
        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopJsEntrySp,
            true,
            OTHER_STACK,
        );
        assert_eq!(read_of(&layout, &memory), LiveFrames::None, "another stack");

        // With the heap shared, holding the isolate proves nothing.
        let shared =
            ISOLATE + at(&layout, Constant::IsolateUsesSharedHeapFlagOffset).cast_unsigned();
        memory.byte(shared, 1);
        assert_eq!(
            read_of(&layout, &memory),
            unread(SHARED_HEAP),
            "shared heap"
        );

        // But a thread whose own stack holds the entry is read as ever.
        let mut memory = blocked(&layout);
        let frames = read_of(&layout, &memory);
        memory.byte(shared, 1);
        assert_eq!(read_of(&layout, &memory), frames, "its own entry");
        assert!(matches!(frames, LiveFrames::Frames(_)), "{frames:?}");
    }

    #[test]
    fn an_isolate_that_does_not_name_itself_is_not_believed() {
        let layout = layout();
        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopIsolate,
            true,
            ISOLATE + 8,
        );
        assert!(
            problem(&read_of(&layout, &memory), "names"),
            "{:?}",
            read_of(&layout, &memory)
        );
    }

    #[test]
    fn a_published_frame_without_an_entry_is_left_out() {
        let layout = layout();
        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopJsEntrySp,
            true,
            0,
        );
        assert_eq!(read_of(&layout, &memory), unread(UNPUBLISHED));

        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopJsEntrySp,
            true,
            0,
        );
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopCEntryFp,
            true,
            0,
        );
        publish(
            &layout,
            &mut memory,
            Constant::IsolateFastCCallCallerFpOffset,
            false,
            EXIT,
        );
        assert_eq!(
            read_of(&layout, &memory),
            unread(UNPUBLISHED),
            "a fast call"
        );
    }

    #[test]
    fn a_stack_pointer_off_its_stack_or_above_the_entry_is_left_out() {
        let layout = layout();
        let memory = blocked(&layout);
        assert_eq!(
            read_at(&layout, &memory, OTHER_STACK),
            unread(OUTSIDE_STACK)
        );
        assert_eq!(read_at(&layout, &memory, ENTRY + 8), unread(OFF_STACK));
        // The exit frame below the stack pointer is one that returned.
        assert_eq!(read_at(&layout, &memory, EXIT + 8), unread(OFF_STACK));
    }

    #[test]
    fn a_thread_that_entered_from_another_isolate_is_left_out() {
        let layout = layout();
        let mut memory = blocked(&layout);
        let previous = at(&layout, Constant::EntryStackItemPreviousIsolate).cast_unsigned();
        memory.word(ITEM + previous, ISOLATE + 0x100_0000);
        assert_eq!(read_of(&layout, &memory), unread(NESTED));

        // And so is one that left JavaScript for good, whose isolate's
        // entries say the same.
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopJsEntrySp,
            true,
            0,
        );
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopCEntryFp,
            true,
            0,
        );
        assert_eq!(read_of(&layout, &memory), unread(NESTED), "no entry");
    }

    /// The deoptimizer leaves an exit frame's pointer published while it
    /// rewrites the frames, and says so only by `stack_is_iterable`.
    #[test]
    fn a_stack_being_rewritten_or_collected_is_left_out() {
        let layout = layout();
        let mut memory = blocked(&layout);
        let iterable = at(&layout, Constant::IsolateStackIsIterableOffset).cast_unsigned();
        memory.byte(ISOLATE + iterable, 0);
        assert_eq!(read_of(&layout, &memory), unread(NOT_ITERABLE));

        let mut memory = blocked(&layout);
        let gc = at(&layout, Constant::StateTagGc).cast_unsigned();
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopCurrentVmState,
            true,
            gc,
        );
        assert_eq!(read_of(&layout, &memory), unread(COLLECTING));
    }

    #[test]
    fn a_thread_that_publishes_no_frame_runs_javascript() {
        let layout = layout();
        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopCEntryFp,
            true,
            0,
        );
        assert_eq!(read_of(&layout, &memory), unread(RUNNING));

        let mut memory = blocked(&layout);
        publish(
            &layout,
            &mut memory,
            Constant::IsolateFastCCallCallerFpOffset,
            false,
            EXIT,
        );
        assert!(problem(&read_of(&layout, &memory), "both"));
    }

    #[test]
    fn an_exit_frame_is_proven_by_its_marker() {
        let layout = layout();
        for (marker, expected) in [
            (marker(ENTRY_TYPE), unread(STALE_EXIT)),
            (marker(STUB_TYPE), unread(STALE_EXIT)),
            // A JavaScript frame's context, a heap object.
            (0x2000_0001, unread(STALE_EXIT)),
            (marker(WASM_TYPE), unread(WEBASSEMBLY)),
        ] {
            let mut memory = blocked(&layout);
            frame(&layout, &mut memory, EXIT, marker, (ENTRY, BUILTIN));
            assert_eq!(read_of(&layout, &memory), expected, "{marker:#x}");
        }
    }

    #[test]
    fn an_inner_entry_continues_at_the_exit_frame_it_saved() {
        let layout = layout();
        let mut memory = blocked(&layout);
        frame(
            &layout,
            &mut memory,
            EXIT,
            marker(EXIT_TYPE),
            (INNER, BUILTIN),
        );
        frame(&layout, &mut memory, INNER, marker(ENTRY_TYPE), (0, 0));
        let saved = at(&layout, Constant::EntryFrameNextExitFrameFpOffset);
        let fast = at(&layout, Constant::EntryFrameNextFastCallFrameFpOffset);
        memory.word(INNER.wrapping_add_signed(fast), 0);
        memory.word(INNER.wrapping_add_signed(saved), SAVED_EXIT);
        frame(
            &layout,
            &mut memory,
            SAVED_EXIT,
            marker(EXIT_TYPE),
            (ENTRY, BUILTIN),
        );
        assert_eq!(read_of(&layout, &memory), LiveFrames::Frames(Vec::new()));

        memory.word(INNER.wrapping_add_signed(saved), 0);
        assert!(problem(&read_of(&layout, &memory), "saved no frame"));

        // A saved exit frame below the inner entry's is not on the walk.
        memory.word(INNER.wrapping_add_signed(saved), EXIT);
        assert_eq!(read_of(&layout, &memory), unread(OFF_STACK));
    }

    #[test]
    fn a_frame_in_code_v8_does_not_own_is_left_out() {
        let layout = layout();
        let mut memory = blocked(&layout);
        let caller = 0x5000_5000;
        frame(
            &layout,
            &mut memory,
            EXIT,
            marker(EXIT_TYPE),
            (caller, FOREIGN),
        );
        frame(&layout, &mut memory, caller, 0x2000_0001, (ENTRY, BUILTIN));
        assert_eq!(read_of(&layout, &memory), unread(FOREIGN_CODE));
    }

    /// A bytecode handler's fast C call saves no bytecode offset first; it
    /// runs in the interpreter's frame, a JavaScript frame, which keeps its
    /// context, a heap object, where V8's own frames keep a marker.
    #[test]
    fn a_fast_call_from_a_builtins_frame_is_left_out() {
        let layout = layout();
        let mut memory = blocked(&layout);
        let caller = 0x5000_5000;
        publish(
            &layout,
            &mut memory,
            Constant::ThreadLocalTopCEntryFp,
            true,
            0,
        );
        publish(
            &layout,
            &mut memory,
            Constant::IsolateFastCCallCallerFpOffset,
            false,
            caller,
        );
        publish(
            &layout,
            &mut memory,
            Constant::IsolateFastCCallCallerPcOffset,
            false,
            BUILTIN,
        );
        frame(&layout, &mut memory, caller, 0x2000_0001, (ENTRY, BUILTIN));
        assert_eq!(read_of(&layout, &memory), unread(FAST_FROM_BUILTIN));
    }
}
