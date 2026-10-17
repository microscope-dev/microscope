//! The walk of Python's frames and thread states over process memory the
//! tests synthesize. The layout's offsets are the tests' own, not any
//! CPython's, and the walk reads only what the layout names, so a
//! structure is written whole and its fields are placed by the builders
//! below.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::code::CodeTable;
use super::frames::Walk;
use super::linetable::CODE_UNIT;
use super::objects::{self, Reader};
use super::threads::{self, ThreadState};
use crate::image::python::{Constant, Field, PythonLayout};
use crate::inspection::InspectionBudget;
use crate::runtime_model::{InterpretedFrame, RuntimeStop, SampledFrame};
use crate::{InspectionLimits, LoadBias, ThreadId, VirtualAddress};

/// Where the builders allocate objects first.
const HEAP: u64 = 0x7000_0000_0000;

/// An address no builder allocates, for pointers to unreadable memory.
const UNMAPPED: u64 = 0x6000_0000_0000;

/// The source file every code object names.
const FILE: &str = "app.py";

/// The sizes of the synthetic structures the layout gives.
const FRAME_SIZE: u64 = 96;
const CODE_SIZE: u64 = 128;
const THREAD_SIZE: u64 = 64;
const STRING_SIZE: u64 = 48;

/// A runtime or an interpreter, whose fields all lie in its first bytes.
const SMALL: u64 = 32;

/// How far past the start of each allocation memory is mapped: as far as
/// the model reads an object's head.
const MAPPED_PAST: u64 = 64;

/// What a walk of one thread returns: its frames and why they end early.
type Walked = (Vec<InterpretedFrame>, Option<Arc<str>>);

/// The fields and constants of the objects the object model reads: classes
/// and their dicts, instances' attributes, slots, sets, and the static
/// types' dicts in the interpreter's state.
fn object_model(layout: &mut PythonLayout) {
    let fields = [
        (Field::CodeName, 64),
        (Field::TypeBasicsize, 48),
        (Field::TypeBase, 56),
        (Field::TypeDict, 64),
        (Field::TypeMro, 72),
        (Field::TypeSubclasses, 80),
        (Field::TypeGetattro, 88),
        (Field::TypeDescrGet, 96),
        (Field::TypeDescrSet, 104),
        (Field::HeapTypeQualname, 112),
        (Field::HeapTypeCachedKeys, 120),
        (Field::HeapTypeSlots, 128),
        (Field::MemberDefType, 8),
        (Field::MemberDefOffset, 16),
        (Field::MemberDescrMember, 24),
        (Field::WrapperDescrWrapped, 24),
        (Field::SetUsed, 24),
        (Field::SetMask, 32),
        (Field::SetTable, 40),
        (Field::SetEntryKey, 0),
        (Field::SetEntryHash, 8),
        (Field::SetEntrySize, 16),
        (Field::ModuleDict, 24),
        (Field::FunctionQualname, 24),
        (Field::BuiltinMethod, 24),
        (Field::BuiltinSelf, 32),
        (Field::MethodDefName, 0),
        (Field::InterpreterStaticTypes, 32),
        (Field::InterpreterStaticTypesEnd, 64),
        (Field::StaticTypeStateSize, 16),
        (Field::StaticTypeStateDict, 8),
        (Field::StaticTypeStateType, 0),
        (Field::InterpreterStaticExtensionTypes, 64),
        (Field::InterpreterStaticExtensionTypesEnd, 96),
    ];
    for (field, offset) in fields {
        layout.set_field(field, offset);
    }

    layout.set_constant(Constant::TypeHeapBit, 9);
    layout.set_constant(Constant::TypeImmutableBit, 8);
    layout.set_constant(Constant::TypeStaticBuiltinBit, 1);
    layout.set_constant(Constant::TypeTypeSubclassBit, 31);
    layout.set_constant(Constant::TypeManagedDictBit, 4);
    layout.set_constant(Constant::TypeInlineValuesBit, 2);
    layout.set_constant(Constant::ManagedDictWords, 3);
    layout.set_constant(Constant::ManagedDictTagged, 1);
    layout.set_constant(Constant::MemberObjectEx, 16);
}

/// The layout the tests read. Each structure's fields are at offsets the
/// tests choose, within the structure's size, and the constants are the
/// values the tests' owners and string states use.
fn layout() -> PythonLayout {
    let mut layout = PythonLayout::default();
    let fields = [
        (Field::FrameSize, FRAME_SIZE),
        (Field::FramePrevious, 0),
        (Field::FrameExecutable, 8),
        (Field::FrameInstruction, 16),
        (Field::FrameOwner, 24),
        (Field::CodeSize, CODE_SIZE),
        (Field::ObjectType, 0),
        (Field::ObjectSize, 8),
        (Field::VarObjectSize, 8),
        (Field::CodeQualname, 16),
        (Field::CodeFilename, 24),
        (Field::CodeLinetable, 32),
        (Field::CodeFirstlineno, 40),
        (Field::CodeLocalsplusnames, 48),
        (Field::CodeLocalspluskinds, 56),
        (Field::CodeArgcount, 80),
        (Field::CodeKwonlyargcount, 84),
        (Field::CodeFlags, 88),
        (Field::CodeCodeAdaptive, 96),
        (Field::UnicodeCompactSize, STRING_SIZE),
        (Field::UnicodeAsciiSize, 40),
        (Field::UnicodeLength, 16),
        (Field::UnicodeHash, 24),
        (Field::UnicodeState, 32),
        (Field::BytesSize, 16),
        (Field::BytesData, 32),
        (Field::TupleSize, 16),
        (Field::TupleItems, 24),
        (Field::RuntimeInterpretersHead, 0),
        (Field::InterpreterId, 0),
        (Field::InterpreterNext, 8),
        (Field::InterpreterThreadsHead, 16),
        (Field::ThreadSize, THREAD_SIZE),
        (Field::ThreadNext, 0),
        (Field::ThreadCurrentFrame, 8),
        (Field::ThreadNativeId, 16),
        (Field::TypeName, 24),
        (Field::TypeFlags, 32),
        (Field::TypeDictoffset, 40),
        (Field::LongTag, 16),
        (Field::LongDigits, 24),
        (Field::FloatValue, 16),
        (Field::ListSize, 16),
        (Field::ListItems, 24),
        (Field::DictUsed, 16),
        (Field::DictValues, 24),
        (Field::DictKeys, 32),
        (Field::DictKeysLog2Size, 0),
        (Field::DictKeysLog2IndexBytes, 1),
        (Field::DictKeysKind, 2),
        (Field::DictKeysNentries, 8),
        (Field::DictKeysIndices, 16),
        (Field::DictEntrySize, 24),
        (Field::DictEntryHash, 0),
        (Field::DictEntryKey, 8),
        (Field::DictEntryValue, 16),
        (Field::DictUnicodeEntrySize, 16),
        (Field::DictUnicodeEntryKey, 0),
        (Field::DictUnicodeEntryValue, 8),
        (Field::FrameGlobals, 32),
        (Field::FrameBuiltins, 40),
        (Field::FrameLocalsplus, 48),
        (Field::CellRef, 16),
    ];
    for (field, offset) in fields {
        layout.set_field(field, offset);
    }

    layout.set_constant(Constant::OwnerThread, 0);
    layout.set_constant(Constant::OwnerGenerator, 1);
    layout.set_constant(Constant::OwnerFrameObject, 2);
    layout.set_constant(Constant::OwnerEntry, 3);
    layout.set_constant(Constant::StackRefs, 0);
    layout.set_constant(Constant::UnicodeKindShift, 2);
    layout.set_constant(Constant::UnicodeCompactShift, 5);
    layout.set_constant(Constant::UnicodeAsciiShift, 6);
    layout.set_constant(Constant::LongShift, 30);
    layout.set_constant(Constant::LongNonSizeBits, 3);
    layout.set_constant(Constant::FastHidden, 0x10);
    layout.set_constant(Constant::FastLocal, 0x20);
    layout.set_constant(Constant::FastCell, 0x40);
    layout.set_constant(Constant::FastFree, 0x80);
    layout.set_constant(Constant::VarArgs, 4);
    layout.set_constant(Constant::VarKeywords, 8);
    layout.set_constant(Constant::DictKeysGeneral, 0);
    layout.set_constant(Constant::DictKeysUnicode, 1);
    layout.set_constant(Constant::DictKeysSplit, 2);
    object_model(&mut layout);
    layout
}

/// The layout of a free-threaded build, which also keeps copies of each
/// code object's bytecode in a `_PyCodeArray`.
fn free_threaded_layout() -> PythonLayout {
    let mut layout = layout();
    layout.set_field(Field::FrameTlbcIndex, 28);
    layout.set_field(Field::CodeTlbc, 72);
    layout.set_field(Field::CodeArraySize, 0);
    layout.set_field(Field::CodeArrayEntries, 8);
    layout
}

/// The line table entry of `units` code units, at the line `delta` after
/// the code's first line. A run of up to eight units at a delta of 0 to 2
/// is the one-line form, which CPython 3.11 and later write as code 10 to
/// 12; its two column bytes are zero, as the walk does not read them.
fn one_run(units: u8, delta: u8) -> [u8; 3] {
    assert!(
        (1..=8).contains(&units) && delta <= 2,
        "a one-line entry covers up to eight units, at a delta of up to two"
    );
    [0x80 | ((10 + delta) << 3) | (units - 1), 0, 0]
}

/// The bytes of the process the tests build, and the threads it stops.
#[derive(Default)]
struct Memory {
    bytes: BTreeMap<u64, u8>,
    threads: Vec<ThreadId>,
}

impl Memory {
    /// Writes the low `size` bytes of `value`, little-endian, at `address`.
    fn write(&mut self, address: u64, value: u64, size: usize) {
        self.fill(address, &value.to_le_bytes()[..size]);
    }

    /// Writes `bytes` at `address`.
    fn fill(&mut self, address: u64, bytes: &[u8]) {
        for (index, byte) in (0..).zip(bytes) {
            self.bytes.insert(address + index, *byte);
        }
    }
}

impl RuntimeStop for Memory {
    fn read(&self, address: VirtualAddress, bytes: &mut [u8]) -> bool {
        for (index, byte) in (0..).zip(bytes.iter_mut()) {
            let Some(read) = address
                .get()
                .checked_add(index)
                .and_then(|at| self.bytes.get(&at))
            else {
                return false;
            };
            *byte = *read;
        }

        true
    }

    fn thread_pointer(&self, _thread: ThreadId) -> Option<u64> {
        None
    }

    fn instruction(&self, _thread: ThreadId) -> Option<VirtualAddress> {
        None
    }

    fn load_bias(&self) -> LoadBias {
        LoadBias::default()
    }

    fn threads(&self) -> Vec<ThreadId> {
        self.threads.clone()
    }
}

/// A process the tests build: its memory, the layout that reads it, the
/// address of its `PyCode_Type`, where the next object goes, and what it
/// is aligned to.
struct Process {
    memory: Memory,
    layout: PythonLayout,
    code_type: u64,
    next: u64,
    align: u64,

    /// `type`, once the tests' types are made, which every type object
    /// after it is an object of.
    metatype: u64,
}

impl Process {
    fn new(layout: PythonLayout) -> Self {
        let mut process = Self {
            memory: Memory::default(),
            layout,
            code_type: 0,
            next: HEAP,
            align: 16,
            metatype: 0,
        };
        process.code_type = process.alloc(16);
        process
    }

    /// Reserves `size` zeroed bytes, aligned as the process aligns them,
    /// and returns their address. The memory past them is mapped, as the
    /// rest of an allocator's pool is, up to [`MAPPED_PAST`] bytes from
    /// their start, or to the end of their alignment.
    fn alloc(&mut self, size: u64) -> u64 {
        let address = self.next.next_multiple_of(self.align);
        let mapped = size.max(MAPPED_PAST).next_multiple_of(self.align);
        let zeros = vec![0; usize::try_from(mapped).expect("an object fits the address space")];
        self.memory.fill(address, &zeros);
        self.next = address + size.next_multiple_of(16);
        address
    }

    /// Places each object from here on in mapped blocks of memory of its
    /// own, which a reader reads by themselves, so that each costs a read.
    const fn spread(&mut self) {
        self.align = objects::BLOCK as u64;
    }

    /// Unmaps the `length` bytes at `address`.
    fn unmap(&mut self, address: u64, length: u64) {
        self.memory
            .bytes
            .retain(|at, _| !(address..address + length).contains(at));
    }

    fn reader(&self) -> Reader<'_> {
        Reader {
            stop: &self.memory,
            layout: &self.layout,
            charge: None,
            blocks: None,
        }
    }

    /// The offset of `field`, which the layout has.
    fn offset(&self, field: Field) -> u64 {
        self.layout.field(field).expect("the layout has the field")
    }

    /// The value of the constant `constant`.
    fn owner(&self, constant: Constant) -> u64 {
        u64::from(self.layout.constant(constant))
    }

    /// Writes `value`, `width` bytes wide, into `field` of the object at
    /// `object`.
    fn put(&mut self, object: u64, field: Field, value: u64, width: usize) {
        let at = object + self.offset(field);
        self.memory.write(at, value, width);
    }

    /// Writes the word `value` into `field` of the object at `object`.
    fn word(&mut self, object: u64, field: Field, value: u64) {
        self.put(object, field, value, 8);
    }

    /// A `str` of ASCII `text`, laid out as CPython's compact ASCII
    /// strings are.
    fn string(&mut self, text: &str) -> u64 {
        let length = text.len() as u64;
        let object = self.alloc(self.offset(Field::UnicodeCompactSize) + length);
        self.word(object, Field::UnicodeLength, length);
        let kind = 1 << self.layout.constant(Constant::UnicodeKindShift);
        let compact = 1 << self.layout.constant(Constant::UnicodeCompactShift);
        let ascii = 1 << self.layout.constant(Constant::UnicodeAsciiShift);
        self.put(object, Field::UnicodeState, kind | compact | ascii, 4);
        let data = object + self.offset(Field::UnicodeAsciiSize);
        self.memory.fill(data, text.as_bytes());
        object
    }

    /// A `bytes` object holding `data`.
    fn bytes(&mut self, data: &[u8]) -> u64 {
        let object = self.alloc(self.offset(Field::BytesData) + data.len() as u64);
        self.word(object, Field::BytesSize, data.len() as u64);
        let at = object + self.offset(Field::BytesData);
        self.memory.fill(at, data);
        object
    }

    /// An empty `tuple`.
    fn empty_tuple(&mut self) -> u64 {
        let object = self.alloc(self.offset(Field::TupleItems));
        self.word(object, Field::TupleSize, 0);
        object
    }

    /// A code object named `name` in [`FILE`], whose first line is
    /// `first_line`. Each run is a count of code units and the line it is
    /// on, as a delta from the first line; the bytecode itself is never
    /// read, only its address.
    fn code(&mut self, name: &str, first_line: u32, runs: &[(u8, u8)]) -> u64 {
        let table: Vec<u8> = runs
            .iter()
            .flat_map(|&(units, delta)| one_run(units, delta))
            .collect();
        let units: u64 = runs.iter().map(|&(units, _)| u64::from(units)).sum();

        let object = self.alloc(self.offset(Field::CodeSize) + CODE_UNIT * units);
        let code_type = self.code_type;
        self.word(object, Field::ObjectType, code_type);
        self.word(object, Field::VarObjectSize, units);
        let qualname = self.string(name);
        self.word(object, Field::CodeQualname, qualname);
        let file = self.string(FILE);
        self.word(object, Field::CodeFilename, file);
        let linetable = self.bytes(&table);
        self.word(object, Field::CodeLinetable, linetable);
        self.put(object, Field::CodeFirstlineno, u64::from(first_line), 4);
        let names = self.empty_tuple();
        self.word(object, Field::CodeLocalsplusnames, names);
        let kinds = self.bytes(&[]);
        self.word(object, Field::CodeLocalspluskinds, kinds);
        object
    }

    /// The address of the code unit at index `unit` of `code`'s bytecode.
    fn instruction(&self, code: u64, unit: u64) -> u64 {
        code + self.offset(Field::CodeCodeAdaptive) + unit * CODE_UNIT
    }

    /// An interpreter frame that runs `code` at `instruction`, owned by
    /// `owner`, whose caller is `previous`.
    fn frame_owned(&mut self, previous: u64, owner: u64, code: u64, instruction: u64) -> u64 {
        let object = self.alloc(self.offset(Field::FrameSize));
        self.word(object, Field::FramePrevious, previous);
        self.word(object, Field::FrameExecutable, code);
        self.word(object, Field::FrameInstruction, instruction);
        self.put(object, Field::FrameOwner, owner, 1);
        object
    }

    /// A frame the thread owns, which runs `code` at `instruction`.
    fn frame(&mut self, previous: u64, code: u64, instruction: u64) -> u64 {
        let owner = self.owner(Constant::OwnerThread);
        self.frame_owned(previous, owner, code, instruction)
    }

    /// An entry frame, which ends the frames the evaluation loop runs
    /// beneath it.
    fn entry(&mut self, previous: u64) -> u64 {
        let owner = self.owner(Constant::OwnerEntry);
        self.frame_owned(previous, owner, 0, 0)
    }

    /// A free-threaded build's `_PyCodeArray`, holding the copies of some
    /// code object's bytecode.
    fn code_array(&mut self, copies: &[u64]) -> u64 {
        let entries = self.offset(Field::CodeArrayEntries);
        let array = self.alloc(entries + 8 * copies.len() as u64);
        self.word(array, Field::CodeArraySize, copies.len() as u64);
        for (index, copy) in copies.iter().enumerate() {
            let at = array + entries + 8 * index as u64;
            self.memory.write(at, *copy, 8);
        }
        array
    }

    /// A runtime whose list of interpreters begins at `interpreters`.
    fn runtime(&mut self, interpreters: u64) -> u64 {
        let object = self.alloc(SMALL);
        self.word(object, Field::RuntimeInterpretersHead, interpreters);
        object
    }

    /// An interpreter numbered `id`, the next in its list, and whose list
    /// of threads begins at `threads`.
    fn interpreter(&mut self, id: i64, next: u64, threads: u64) -> u64 {
        let object = self.alloc(SMALL);
        self.word(object, Field::InterpreterId, id.cast_unsigned());
        self.word(object, Field::InterpreterNext, next);
        self.word(object, Field::InterpreterThreadsHead, threads);
        object
    }

    /// A thread state, the next in its interpreter's list, whose innermost
    /// frame is `frame` and whose native thread is `native`.
    fn thread_state(&mut self, next: u64, frame: u64, native: u64) -> u64 {
        let object = self.alloc(self.offset(Field::ThreadSize));
        self.word(object, Field::ThreadNext, next);
        self.word(object, Field::ThreadCurrentFrame, frame);
        self.word(object, Field::ThreadNativeId, native);
        object
    }
}

/// A thread whose innermost frame is `frame`, with no base frame.
fn innermost(frame: u64) -> ThreadState {
    ThreadState {
        address: VirtualAddress::new(HEAP),
        interpreter_state: VirtualAddress::new(HEAP),
        interpreter: 0,
        native: 1,
        frame: Some(VirtualAddress::new(frame)),
        base: None,
    }
}

/// Walks the thread in `state`, with `code` as the code table.
fn walk(process: &Process, code: &mut CodeTable, state: &ThreadState) -> Walked {
    let mut walk = Walk {
        reader: process.reader(),
        code,
        code_type: VirtualAddress::new(process.code_type),
    };
    walk.thread(state)
}

/// Walks the thread whose innermost frame is `frame`, with a fresh code
/// table.
fn walk_thread(process: &Process, frame: u64) -> Walked {
    walk(process, &mut CodeTable::default(), &innermost(frame))
}

/// A frame's place as its path and line.
fn place(frame: &InterpretedFrame) -> Option<(&str, u32)> {
    frame
        .place
        .as_ref()
        .map(|place| (place.path.as_ref(), place.line))
}

#[test]
fn a_thread_walks_its_segments_innermost_first_and_each_ends_at_an_entry_frame() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let b = process.code("b", 20, &[(1, 0)]);
    let c = process.code("c", 30, &[(1, 0)]);

    let outer_entry = process.entry(0);
    let c_frame = process.frame(outer_entry, c, process.instruction(c, 0));
    let inner_entry = process.entry(c_frame);
    let b_frame = process.frame(inner_entry, b, process.instruction(b, 0));
    let a_frame = process.frame(b_frame, a, process.instruction(a, 0));

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.function.as_deref())
            .collect::<Vec<_>>(),
        [Some("a"), Some("b"), Some("c")]
    );
    assert_eq!(
        frames.iter().map(place).collect::<Vec<_>>(),
        [Some((FILE, 10)), Some((FILE, 20)), Some((FILE, 30))]
    );
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.outermost)
            .collect::<Vec<_>>(),
        [false, true, true]
    );
    assert!(frames.iter().all(|frame| frame.problem.is_none()));
    assert_eq!(end, None);
}

/// A frame as a walker in the kernel reads it: the code object it runs,
/// that object's qualified-name pointer and first line, its instruction's
/// offset from its bytecode, and its owner.
fn sampled(process: &Process, frame: u64) -> SampledFrame {
    let read = |address: u64, width: usize| {
        let mut bytes = [0; 8];
        assert!(
            process
                .memory
                .read(VirtualAddress::new(address), &mut bytes[..width])
        );
        u64::from_le_bytes(bytes)
    };

    let field =
        |object: u64, field: Field, width: usize| read(object + process.offset(field), width);
    let code = field(frame, Field::FrameExecutable, 8);
    let instruction = field(frame, Field::FrameInstruction, 8);
    let owner = u32::try_from(field(frame, Field::FrameOwner, 1)).expect("a byte");
    let (proof, offset) = if code == 0 {
        ((0, 0), None)
    } else {
        let qualname = field(code, Field::CodeQualname, 8);
        let first_line = i32::try_from(field(code, Field::CodeFirstlineno, 4)).expect("a line");
        let bytecode = code + process.offset(Field::CodeCodeAdaptive);
        let offset = instruction.wrapping_sub(bytecode).cast_signed();
        ((qualname, first_line), Some(offset))
    };

    SampledFrame {
        code: VirtualAddress::new(code),
        proof,
        offset,
        owner,
    }
}

/// A frame's function, place, first line, and whether it ends a segment.
type Found<'a> = (Option<&'a str>, Option<(&'a str, u32)>, Option<u32>, bool);

/// What two walks of a thread found that does not depend on where its
/// frames are.
fn found(frames: &[InterpretedFrame]) -> Vec<Found<'_>> {
    frames
        .iter()
        .map(|frame| {
            (
                frame.function.as_deref(),
                place(frame),
                frame.first_line,
                frame.outermost,
            )
        })
        .collect()
}

/// The frames a walker in the kernel read name the functions and lines
/// the stopped walk of the same chain does. A code object freed since, and
/// another put at its address, is no longer the one the walker read, and
/// its frame is unreadable; so is one whose instruction it could not read.
#[test]
fn sampled_frames_read_as_the_stopped_walk_reads_them() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(2, 1)]);
    let b = process.code("b", 20, &[(1, 0), (1, 2)]);
    let outer = process.entry(0);
    let b_frame = process.frame(outer, b, process.instruction(b, 1));
    let inner = process.entry(b_frame);
    let a_frame = process.frame(inner, a, process.instruction(a, 0));
    let (stopped, end) = walk_thread(&process, a_frame);
    assert_eq!(end, None);

    let mut chain: Vec<SampledFrame> = [a_frame, inner, b_frame, outer]
        .into_iter()
        .map(|frame| sampled(&process, frame))
        .collect();
    let mut code = CodeTable::default();
    let mut walk = |process: &Process, chain: &[SampledFrame]| {
        Walk {
            reader: process.reader(),
            code: &mut code,
            code_type: VirtualAddress::new(process.code_type),
        }
        .sampled(chain)
    };

    let (frames, end) = walk(&process, &chain);
    assert_eq!(found(&frames), found(&stopped));
    assert_eq!(
        found(&frames),
        [
            (Some("a"), Some((FILE, 11)), Some(10), true),
            (Some("b"), Some((FILE, 22)), Some(20), true),
        ]
    );
    assert_eq!(end, None);

    let other = process.string("c");
    process.word(a, Field::CodeQualname, other);
    chain[2].offset = None;
    let (frames, _) = walk(&process, &chain);
    assert_eq!(frames[0].function, None);
    assert!(
        frames[0]
            .problem
            .as_deref()
            .is_some_and(|problem| problem.ends_with("the frame ran is gone")),
        "{:?}",
        frames[0].problem
    );
    assert_eq!(frames[1].function.as_deref(), Some("b"));
    assert_eq!(place(&frames[1]), None);
    assert!(
        frames[1]
            .problem
            .as_deref()
            .is_some_and(|problem| problem.ends_with("its instruction could not be read")),
        "{:?}",
        frames[1].problem
    );
}

#[test]
fn a_frame_chain_that_loops_ends_the_walk_and_keeps_the_frames_before_the_loop() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let b = process.code("b", 20, &[(1, 0)]);
    let a_frame = process.frame(0, a, process.instruction(a, 0));
    let b_frame = process.frame(a_frame, b, process.instruction(b, 0));
    process.word(a_frame, Field::FramePrevious, b_frame);

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.function.as_deref())
            .collect::<Vec<_>>(),
        [Some("a"), Some("b")]
    );
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.outermost)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(end.as_deref(), Some("the thread's frames loop"));
}

#[test]
fn a_frame_with_an_unknown_owner_ends_the_walk_and_names_the_owner() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let unknown = process.frame_owned(0, 9, 0, 0);
    let a_frame = process.frame(unknown, a, process.instruction(a, 0));

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.function.as_deref())
            .collect::<Vec<_>>(),
        [Some("a")]
    );
    assert!(frames[0].outermost);
    assert_eq!(
        end.as_deref(),
        Some(
            format!("the frame at {unknown:#x} has owner 9, which this CPython gives no frame")
                .as_str()
        )
    );
}

#[test]
fn a_chain_with_no_entry_frame_at_its_end_marks_its_last_frame_outermost() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let b = process.code("b", 20, &[(1, 0)]);
    let b_frame = process.frame(0, b, process.instruction(b, 0));
    let a_frame = process.frame(b_frame, a, process.instruction(a, 0));

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.outermost)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(
        end.as_deref(),
        Some("the outermost frames have no entry frame beneath them")
    );
}

#[test]
fn a_frame_whose_instruction_lies_outside_its_bytecode_has_no_line() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(3, 0)]);
    let entry = process.entry(0);
    let past_end = process.instruction(a, 3);
    let a_frame = process.frame(entry, a, past_end);

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].function.as_deref(), Some("a"));
    assert_eq!(place(&frames[0]), None);
    assert!(
        frames[0]
            .problem
            .as_deref()
            .is_some_and(|problem| problem.contains("outside its code's bytecode")),
        "the problem: {:?}",
        frames[0].problem
    );
    assert_eq!(end, None);
}

#[test]
fn an_instruction_one_unit_before_the_bytecode_has_the_first_line() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(2, 0)]);
    let entry = process.entry(0);
    let before = process.instruction(a, 0) - CODE_UNIT;
    let a_frame = process.frame(entry, a, before);

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(place(&frames[0]), Some((FILE, 10)));
    assert_eq!(frames[0].problem, None);
    assert_eq!(end, None);
}

#[test]
fn an_instruction_two_units_before_the_bytecode_has_no_line() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(2, 0)]);
    let entry = process.entry(0);
    let before = process.instruction(a, 0) - 2 * CODE_UNIT;
    let a_frame = process.frame(entry, a, before);

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(place(&frames[0]), None);
    assert!(
        frames[0]
            .problem
            .as_deref()
            .is_some_and(|problem| problem.contains("outside its code's bytecode")),
        "the problem: {:?}",
        frames[0].problem
    );
    assert_eq!(end, None);
}

#[test]
fn a_frame_whose_code_object_has_another_type_is_malformed() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    process.word(a, Field::ObjectType, UNMAPPED + 8);
    let entry = process.entry(0);
    let a_frame = process.frame(entry, a, process.instruction(a, 0));

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].function, None);
    assert_eq!(place(&frames[0]), None);
    assert_eq!(
        frames[0].problem.as_deref(),
        Some(format!("the code object at {a:#x} is malformed").as_str())
    );
    assert_eq!(end, None);
}

#[test]
fn a_frame_at_unreadable_memory_ends_the_walk_and_names_the_memory() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let a_frame = process.frame(UNMAPPED, a, process.instruction(a, 0));

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].function.as_deref(), Some("a"));
    assert!(frames[0].outermost);
    assert_eq!(
        end.as_deref(),
        Some(format!("the memory at {UNMAPPED:#x} is unreadable").as_str())
    );
}

#[test]
fn a_base_frame_ends_the_walk_before_it_with_no_problem() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let base = process.frame(0, a, process.instruction(a, 0));
    let entry = process.entry(base);
    let a_frame = process.frame(entry, a, process.instruction(a, 0));

    let state = ThreadState {
        base: Some(VirtualAddress::new(base)),
        ..innermost(a_frame)
    };
    let (frames, end) = walk(&process, &mut CodeTable::default(), &state);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].frame, VirtualAddress::new(a_frame));
    assert!(frames[0].outermost);
    assert_eq!(end, None);
}

/// From 3.15 every thread's chain ends at its base frame, so one that ends
/// before it was read as the program changed it, which the walk says.
#[test]
fn a_chain_that_ends_before_its_base_frame_says_so() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let base = process.frame(0, a, process.instruction(a, 0));
    let entry = process.entry(0);
    let a_frame = process.frame(entry, a, process.instruction(a, 0));

    let state = ThreadState {
        base: Some(VirtualAddress::new(base)),
        ..innermost(a_frame)
    };
    let (frames, end) = walk(&process, &mut CodeTable::default(), &state);
    assert_eq!(frames.len(), 1);
    assert_eq!(
        end.as_deref(),
        Some("the thread's frames end before its base frame")
    );

    // A thread that runs no Python code has no frames to end early.
    let idle = ThreadState {
        base: Some(VirtualAddress::new(base)),
        frame: None,
        ..innermost(a_frame)
    };
    assert_eq!(
        walk(&process, &mut CodeTable::default(), &idle),
        (Vec::new(), None)
    );
}

/// Pins what the walk does when a thread's frames reach its base frame
/// with no entry frame beneath the last one: the last frame is marked
/// outermost and the walk reports the missing entry frame. The base frame
/// ends the chain, so this reports a gap that may not be there.
#[test]
fn a_base_frame_beneath_thread_frames_reports_the_missing_entry_frame() {
    let mut process = Process::new(layout());
    let a = process.code("a", 10, &[(1, 0)]);
    let b = process.code("b", 20, &[(1, 0)]);
    let base = process.frame(0, a, process.instruction(a, 0));
    let b_frame = process.frame(base, b, process.instruction(b, 0));
    let a_frame = process.frame(b_frame, a, process.instruction(a, 0));

    let state = ThreadState {
        base: Some(VirtualAddress::new(base)),
        ..innermost(a_frame)
    };
    let (frames, end) = walk(&process, &mut CodeTable::default(), &state);
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.outermost)
            .collect::<Vec<_>>(),
        [false, true]
    );
    assert_eq!(
        end.as_deref(),
        Some("the outermost frames have no entry frame beneath them")
    );
}

#[test]
fn a_free_threaded_frame_reads_its_line_from_the_copy_of_its_bytecode_it_runs() {
    let mut process = Process::new(free_threaded_layout());
    let a = process.code("a", 10, &[(2, 0)]);
    let copy0 = process.alloc(16);
    let copy1 = process.alloc(16);
    let copies = process.code_array(&[copy0, copy1]);
    process.word(a, Field::CodeTlbc, copies);

    let entry = process.entry(0);
    let a_frame = process.frame(entry, a, copy1 + CODE_UNIT);
    process.put(a_frame, Field::FrameTlbcIndex, 1, 4);

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].function.as_deref(), Some("a"));
    assert_eq!(place(&frames[0]), Some((FILE, 10)));
    assert_eq!(frames[0].problem, None);
    assert_eq!(end, None);
}

#[test]
fn a_free_threaded_frame_naming_no_copy_of_its_bytecode_has_no_line() {
    let mut process = Process::new(free_threaded_layout());
    let a = process.code("a", 10, &[(2, 0)]);
    let copy0 = process.alloc(16);
    let copy1 = process.alloc(16);
    let copies = process.code_array(&[copy0, copy1]);
    process.word(a, Field::CodeTlbc, copies);

    let entry = process.entry(0);
    let a_frame = process.frame(entry, a, copy1 + CODE_UNIT);
    process.put(a_frame, Field::FrameTlbcIndex, 2, 4);

    let (frames, end) = walk_thread(&process, a_frame);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].function.as_deref(), Some("a"));
    assert_eq!(place(&frames[0]), None);
    assert_eq!(
        frames[0].problem.as_deref(),
        Some(format!("the frame at {a_frame:#x} runs copy 2 of 2 of its bytecode").as_str())
    );
    assert_eq!(end, None);
}

#[test]
fn a_code_object_whose_header_changes_is_decoded_again() {
    let mut process = Process::new(layout());
    let code = process.code("old", 10, &[(1, 0)]);
    let entry = process.entry(0);
    let a_frame = process.frame(entry, code, process.instruction(code, 0));
    let mut table = CodeTable::default();

    let (frames, _) = walk(&process, &mut table, &innermost(a_frame));
    assert_eq!(frames[0].function.as_deref(), Some("old"));

    let (frames, _) = walk(&process, &mut table, &innermost(a_frame));
    assert_eq!(frames[0].function.as_deref(), Some("old"));

    let renamed = process.string("new");
    process.word(code, Field::CodeQualname, renamed);
    let (frames, _) = walk(&process, &mut table, &innermost(a_frame));
    assert_eq!(frames[0].function.as_deref(), Some("new"));
}

/// A thread state as the tests compare it: its native thread, its
/// interpreter, and its innermost frame's address.
type ThreadSummary = (u64, i64, Option<u64>);

/// The thread states of `runtime`, summarized, and the gaps the walk
/// reports.
fn states_of(process: &Process, runtime: u64) -> (Vec<ThreadSummary>, Vec<Arc<str>>) {
    let (states, gaps) = threads::thread_states(&process.reader(), VirtualAddress::new(runtime))
        .expect("the runtime's interpreters are readable");
    let states = states
        .iter()
        .map(|state| {
            (
                state.native,
                state.interpreter,
                state.frame.map(VirtualAddress::get),
            )
        })
        .collect();
    (states, gaps)
}

#[test]
fn thread_states_lists_every_thread_of_every_interpreter() {
    let mut process = Process::new(layout());
    let second = process.thread_state(0, 0x5000, 200);
    let first_b = process.thread_state(0, 0, 101);
    let first_a = process.thread_state(first_b, 0x4000, 100);
    let interpreter_nine = process.interpreter(9, 0, second);
    let interpreter_seven = process.interpreter(7, interpreter_nine, first_a);
    let runtime = process.runtime(interpreter_seven);

    let (states, gaps) = states_of(&process, runtime);
    assert_eq!(
        states,
        [
            (100, 7, Some(0x4000)),
            (101, 7, None),
            (200, 9, Some(0x5000)),
        ]
    );
    assert!(gaps.is_empty(), "the gaps: {gaps:?}");
}

#[test]
fn an_interpreter_list_that_loops_is_a_gap_and_keeps_the_interpreters_before_it() {
    let mut process = Process::new(layout());
    let thread = process.thread_state(0, 0x4000, 100);
    let interpreter = process.interpreter(7, 0, thread);
    process.word(interpreter, Field::InterpreterNext, interpreter);
    let runtime = process.runtime(interpreter);

    let (states, gaps) = states_of(&process, runtime);
    assert_eq!(states, [(100, 7, Some(0x4000))]);
    let gaps: Vec<&str> = gaps.iter().map(AsRef::as_ref).collect();
    assert_eq!(gaps, ["CPython's list of interpreters loops"]);
}

#[test]
fn a_thread_list_that_loops_is_a_gap_and_keeps_the_threads_before_it() {
    let mut process = Process::new(layout());
    let first = process.thread_state(0, 0x4000, 100);
    let second = process.thread_state(first, 0, 101);
    process.word(first, Field::ThreadNext, second);
    let interpreter = process.interpreter(7, 0, first);
    let runtime = process.runtime(interpreter);

    let (states, gaps) = states_of(&process, runtime);
    assert_eq!(states, [(100, 7, Some(0x4000)), (101, 7, None)]);
    let gaps: Vec<&str> = gaps.iter().map(AsRef::as_ref).collect();
    assert_eq!(gaps, ["interpreter 7's list of threads loops"]);
}

mod asyncio;
mod expressions;
mod instances;
mod numpy;
mod values;

/// A runtime whose frames cannot be read, as one of a version microscope
/// does not know, still finds the activations of its loop, and says why
/// their frames are missing, which a backtrace notes there.
#[test]
fn an_unreadable_runtime_finds_its_loop_and_says_why_its_frames_are_missing() {
    use crate::runtime_model::{Driven, RuntimeModel};

    let reason: Arc<str> =
        "the Python frames cannot be read: CPython 3.16.0 is not supported".into();
    let model = super::PythonRuntime {
        version: crate::image::python::PyVersion(0x0310_00f0),
        reading: Err(Arc::clone(&reason)),
    };
    let process = Process::new(layout());
    let stacks = model
        .interpreted(&process.memory)
        .expect("the runtime interprets frames");
    assert!(stacks.value.threads.is_empty());
    assert_eq!(stacks.gaps, [reason]);

    let mut evaluating = crate::StackFrame::from_parts(
        0,
        crate::FrameKind::Physical,
        None,
        Some(VirtualAddress::new(0x1000)),
        crate::model::FrameMetadata {
            code_instance: None,
            function: None,
            source: None,
            symbol: Some(crate::SymbolLocation {
                symbol: crate::SymbolId::new(0),
                name: "_PyEval_EvalFrameDefault".into(),
                kind: crate::SymbolKind::Function,
                offset: 0x10,
                provenance: crate::SymbolExtentProvenance::Declared,
            }),
            role: crate::CodeRole::Interpreter,
        },
    );
    assert_eq!(model.driven(&evaluating), Some(Driven::Interpreted));
    evaluating.symbol = None;
    assert_eq!(model.driven(&evaluating), None);
}

#[test]
fn reads_in_a_block_read_before_cost_nothing_and_bytes_in_a_block_not_mapped_whole_are_read_alone()
{
    let block = objects::BLOCK as u64;
    let mut memory = Memory::default();
    memory.fill(HEAP, &[7; objects::BLOCK]);
    let alone = HEAP + 4 * block + 8;
    memory.fill(alone, &[9; 8]);
    let layout = layout();
    let mut inspection = InspectionBudget::new(InspectionLimits::default());
    let budget = super::values::Budget::new(&mut inspection);
    let blocks = crate::runtime_model::StopMemory::default();
    {
        let reader = Reader {
            stop: &memory,
            layout: &layout,
            charge: Some(&budget),
            blocks: Some(&blocks),
        };

        // Each read in the mapped block is paid for by its first.
        assert_eq!(
            reader.word(VirtualAddress::new(HEAP)),
            Ok(0x0707_0707_0707_0707)
        );
        assert_eq!(
            reader.word(VirtualAddress::new(HEAP + block - 8)),
            Ok(0x0707_0707_0707_0707)
        );
        assert_eq!(
            reader.bytes(VirtualAddress::new(HEAP + 3), 2),
            Ok(vec![7, 7])
        );

        // Bytes reaching into a block that cannot be read are unreadable, and
        // mapped bytes in such a block are read by themselves.
        assert!(reader.word(VirtualAddress::new(HEAP + block - 4)).is_err());
        assert_eq!(
            reader.word(VirtualAddress::new(alone)),
            Ok(0x0909_0909_0909_0909)
        );
        assert!(reader.word(VirtualAddress::new(alone + 8)).is_err());
    }

    // The mapped block; the next, which cannot be read, and the bytes
    // reaching into it; the block of the bytes alone, and those bytes; and
    // the bytes after them, in the same block.
    assert_eq!(inspection.usage().memory_reads, 6);
}
