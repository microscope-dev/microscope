//! The isolate's offsets that reading a running thread's frames relies on,
//! checked against the machine code of each build in the matrix: the
//! displacements from V8's root register, `r13`, at which its embedded
//! builtins save, clear, and set what the generated layout says.
//!
//! - `JSEntry` pushes the exit frame pointer it clears, and from 13.6 the
//!   fast C call's frame pointer and return address, into the slots
//!   `EntryFrameConstants` names, and sets `js_entry_sp` on the outermost
//!   entry.
//! - `CEntry` builds an exit frame, with its marker, before it stores its
//!   frame pointer as `c_entry_fp`, passes `r13` less the root bias as the
//!   isolate, and clears `c_entry_fp` only after `leave`.
//! - A fast C call stores its return address, then its frame pointer.
//! - The deoptimizer's entry clears `stack_is_iterable` and sets it again.
//! - V8's `Isolate::Enter`, which the exported `v8::Isolate::Enter` runs,
//!   stores its new entry where the exported `v8::Isolate::IsInUse` reads
//!   the isolate's entries, and at no word beside it.
//!
//! Debian's libnode names no builtin. V8 generates its builtins' code
//! itself, so a build of one V8 version and configuration has the same
//! bytes in each: each builtin is found there as the whole code the
//! version's build with symbols has for it, which must occur as often as
//! it does in that build, and every copy must read the same
//! displacements. The fast calls are found by their shape in all of its
//! executable code.

use std::collections::BTreeMap;

use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind};

use crate::image::v8::{Constant, V8Layout};

/// What the layout says, as displacements from the root register, and
/// the entry frame's slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Expected {
    pub c_entry_fp: i64,
    pub js_entry_sp: i64,
    pub fast_fp: i64,
    pub fast_pc: i64,
    pub stack_is_iterable: i64,
    pub bias: i64,
    pub exit_slot: i64,
    pub fast_fp_slot: Option<i64>,
    pub fast_pc_slot: Option<i64>,
    pub exit_marker: i64,
}

impl Expected {
    /// The displacements `layout` gives.
    pub(super) fn of(layout: &V8Layout) -> Self {
        let constant = |constant: Constant| {
            layout
                .constant(constant)
                .unwrap_or_else(|| panic!("the layout has no {}", constant.name()))
        };
        let bias = constant(Constant::IsolateRootBias);
        let top = constant(Constant::IsolateThreadLocalTopOffset);
        Self {
            c_entry_fp: top + constant(Constant::ThreadLocalTopCEntryFp) - bias,
            js_entry_sp: top + constant(Constant::ThreadLocalTopJsEntrySp) - bias,
            fast_fp: constant(Constant::IsolateFastCCallCallerFpOffset) - bias,
            fast_pc: constant(Constant::IsolateFastCCallCallerPcOffset) - bias,
            stack_is_iterable: constant(Constant::IsolateStackIsIterableOffset) - bias,
            bias,
            exit_slot: constant(Constant::EntryFrameNextExitFrameFpOffset),
            fast_fp_slot: layout.constant(Constant::EntryFrameNextFastCallFrameFpOffset),
            fast_pc_slot: layout.constant(Constant::EntryFrameNextFastCallFramePcOffset),
            // A typed frame's marker is its type as a Smi.
            exit_marker: constant(Constant::FrameExit) << 1,
        }
    }
}

/// What a build's builtins read, as found in its code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Found {
    /// Each `push [r13+d]; mov qword [r13+d], 0` of `JSEntry`, in order,
    /// with the slot below its frame pointer the push fills.
    pub entry_saves: Vec<(i64, i64)>,

    /// The word `JSEntry` loads into `rax` and stores `rax` back to.
    pub js_entry_sp: Option<i64>,

    /// Every other word `JSEntry` clears: `js_entry_sp`, as the outermost
    /// entry returns.
    pub js_entry_sp_cleared: Vec<i64>,

    /// `CEntry`'s exit frame: the marker it pushes, the displacement it
    /// stores `rbp` to, the isolate it passes, and what it clears after
    /// `leave`.
    pub exit_marker: Option<i64>,
    pub exit_store: Option<i64>,
    pub isolate: Option<i64>,
    pub cleared_after_leave: Option<i64>,

    /// The displacements of the deoptimizer's byte stores of 0 and 1.
    pub iterable_clears: Vec<i64>,
    pub iterable_sets: Vec<i64>,

    /// Each fast call's (return address, frame pointer) displacements,
    /// with how many calls store them.
    pub fast_calls: BTreeMap<(i64, i64), usize>,
}

/// The builtins read, by name.
const JS_ENTRY: &str = "Builtins_JSEntry";
const C_ENTRY: &str = "Builtins_CEntry_Return1_ArgvOnStack_NoBuiltinExit";
const DEOPTIMIZATION_ENTRY: &str = "Builtins_DeoptimizationEntry_Eager";

/// The fewest fast calls a build's builtins make: hundreds of builtins
/// call C fast.
const FEWEST_FAST_CALLS: usize = 100;

/// A build's file, read for its code.
pub(super) struct Build {
    data: Vec<u8>,
}

impl Build {
    pub(super) fn read(path: &std::path::Path) -> Self {
        let data =
            std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        Self { data }
    }

    fn object(&self) -> object::File<'_> {
        object::File::parse(self.data.as_slice()).expect("an ELF file")
    }

    /// Each named builtin's address and code.
    fn builtin(&self, name: &str) -> Option<(u64, &[u8])> {
        let object = self.object();
        let symbol = object.symbols().find(|symbol| symbol.name() == Ok(name))?;
        let section = object.section_by_index(symbol.section_index()?).ok()?;
        let code = section.data_range(symbol.address(), symbol.size()).ok()??;
        // The bytes borrow the file's data, which outlives the object.
        let start = code.as_ptr() as usize - self.data.as_ptr() as usize;
        Some((symbol.address(), &self.data[start..start + code.len()]))
    }

    /// The code at `address`, at most `length` bytes of it.
    fn code_at(&self, address: u64, length: usize) -> Option<&[u8]> {
        self.code().into_iter().find_map(|(start, bytes)| {
            let at = usize::try_from(address.checked_sub(start)?).ok()?;
            let end = bytes.len().min(at.checked_add(length)?);
            bytes.get(at..end)
        })
    }

    /// The address of the exported function `name`.
    fn exported(&self, name: &str) -> Option<u64> {
        self.object()
            .dynamic_symbols()
            .find(|symbol| symbol.name() == Ok(name) && symbol.is_definition())
            .map(|symbol| symbol.address())
    }

    /// Whether symbols name the build's builtins.
    fn names_builtins(&self) -> bool {
        self.builtin(JS_ENTRY).is_some()
    }

    /// The executable sections, each with its address.
    fn code(&self) -> Vec<(u64, &[u8])> {
        let object = self.object();
        object
            .sections()
            .filter(|section| section.kind() == SectionKind::Text)
            .filter_map(|section| {
                let bytes = section.data().ok()?;
                let start = bytes.as_ptr() as usize - self.data.as_ptr() as usize;
                Some((section.address(), &self.data[start..start + bytes.len()]))
            })
            .collect()
    }

    /// Each place in the build's code `prologue` begins, by address, with
    /// the code from there.
    fn copies(&self, prologue: &[u8], length: usize) -> Vec<(u64, &[u8])> {
        let mut found = Vec::new();
        for (address, bytes) in self.code() {
            let mut at = 0;
            while let Some(offset) = find(&bytes[at..], prologue) {
                let start = at + offset;
                let end = (start + length).min(bytes.len());
                found.push((address + start as u64, &bytes[start..end]));
                at = start + 1;
            }
        }
        found
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The displacement of `instruction`'s memory operand from `r13`, with no
/// index register.
fn r13(instruction: &Instruction) -> Option<i64> {
    (0..instruction.op_count())
        .any(|operand| instruction.op_kind(operand) == OpKind::Memory)
        .then_some(())?;
    (instruction.memory_base() == Register::R13 && instruction.memory_index() == Register::None)
        .then(|| instruction.memory_displacement64().cast_signed())
}

fn decode(address: u64, bytes: &[u8]) -> Vec<Instruction> {
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut instructions = Vec::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            break;
        }
        instructions.push(instruction);
    }
    instructions
}

/// Whether `instruction` stores the immediate `value` to memory.
fn stores_immediate(instruction: &Instruction, value: u64) -> bool {
    instruction.mnemonic() == Mnemonic::Mov
        && instruction.op0_kind() == OpKind::Memory
        && matches!(
            instruction.op1_kind(),
            OpKind::Immediate8 | OpKind::Immediate32 | OpKind::Immediate32to64
        )
        && instruction.immediate(1) == value
}

fn reg(instruction: &Instruction, operand: u32, register: Register) -> bool {
    instruction.op_kind(operand) == OpKind::Register && instruction.op_register(operand) == register
}

/// What `JSEntry`'s code saves and sets, to its first return.
fn read_js_entry(found: &mut Found, instructions: &[Instruction]) {
    let mut depth: Option<i64> = None;
    let mut pending: Option<(i64, i64)> = None;
    let mut loaded: Option<i64> = None;
    // `lea r10, [r13+d]`, which 13.6 clears `[r10]` through.
    let mut r10: Option<i64> = None;
    for instruction in instructions {
        if instruction.mnemonic() == Mnemonic::Ret {
            break;
        }
        let saved = pending.take();
        let through_r10 = || {
            (instruction.op0_kind() == OpKind::Memory
                && instruction.memory_base() == Register::R10
                && instruction.memory_index() == Register::None
                && instruction.memory_displacement64() == 0)
                .then_some(r10)
                .flatten()
        };
        if let Some(cleared) = r13(instruction).or_else(through_r10)
            && stores_immediate(instruction, 0)
        {
            match saved {
                Some(save) if cleared == save.0 => found.entry_saves.push(save),
                _ => found.js_entry_sp_cleared.push(cleared),
            }
        }
        if reg(instruction, 0, Register::R10) {
            r10 = (instruction.mnemonic() == Mnemonic::Lea)
                .then(|| r13(instruction))
                .flatten();
        }
        match instruction.mnemonic() {
            Mnemonic::Mov
                if reg(instruction, 0, Register::RBP) && reg(instruction, 1, Register::RSP) =>
            {
                depth = Some(0);
            }
            Mnemonic::Push => {
                if let Some(below) = depth.as_mut() {
                    if let Some(displacement) = r13(instruction) {
                        pending = Some((displacement, -(*below + 8)));
                    }
                    *below += 8;
                }
            }
            Mnemonic::Sub if reg(instruction, 0, Register::RSP) => {
                if let Some(below) = depth.as_mut() {
                    *below += instruction.immediate(1).cast_signed();
                }
            }
            // `mov rax, [r13+d]; test rax, rax; ...; mov rax, rbp; mov
            // [r13+d], rax`: only a load from the isolate replaces `d`.
            Mnemonic::Mov if reg(instruction, 0, Register::RAX) => {
                loaded = r13(instruction).or(loaded);
            }
            Mnemonic::Mov if reg(instruction, 1, Register::RAX) && found.js_entry_sp.is_none() => {
                if let Some(stored) = r13(instruction).filter(|stored| Some(*stored) == loaded) {
                    found.js_entry_sp = Some(stored);
                }
            }
            _ => {}
        }
    }
}

/// What `CEntry`'s code does with its exit frame.
fn read_c_entry(found: &mut Found, instructions: &[Instruction]) {
    for (at, window) in instructions.windows(5).enumerate() {
        let [push_rbp, mov_rbp, marker, zero, store] = window else {
            continue;
        };
        if push_rbp.mnemonic() == Mnemonic::Push
            && reg(push_rbp, 0, Register::RBP)
            && mov_rbp.mnemonic() == Mnemonic::Mov
            && reg(mov_rbp, 0, Register::RBP)
            && reg(mov_rbp, 1, Register::RSP)
            && marker.mnemonic() == Mnemonic::Push
            && marker.op0_kind() != OpKind::Register
            && zero.mnemonic() == Mnemonic::Push
            && store.mnemonic() == Mnemonic::Mov
            && reg(store, 1, Register::RBP)
            && found.exit_store.is_none()
        {
            found.exit_marker = Some(marker.immediate(0).cast_signed());
            found.exit_store = r13(store);
            // The isolate, passed in rdx, the C function's third argument.
            found.isolate = instructions[at..].iter().find_map(|instruction| {
                (instruction.mnemonic() == Mnemonic::Lea && reg(instruction, 0, Register::RDX))
                    .then(|| r13(instruction))
                    .flatten()
            });
        }
    }
    for window in instructions.windows(3) {
        if window[0].mnemonic() == Mnemonic::Leave && found.cleared_after_leave.is_none() {
            found.cleared_after_leave = window[1..]
                .iter()
                .find(|instruction| stores_immediate(instruction, 0))
                .and_then(r13);
        }
    }
}

/// The deoptimizer's byte stores to the isolate.
fn read_deoptimization_entry(found: &mut Found, instructions: &[Instruction]) {
    for instruction in instructions {
        if instruction.mnemonic() != Mnemonic::Mov || instruction.op0_kind() != OpKind::Memory {
            continue;
        }
        if instruction.memory_size() != iced_x86::MemorySize::UInt8 {
            continue;
        }
        let Some(displacement) = r13(instruction) else {
            continue;
        };
        if stores_immediate(instruction, 0) {
            found.iterable_clears.push(displacement);
        } else if stores_immediate(instruction, 1) {
            found.iterable_sets.push(displacement);
        }
    }
}

/// Each fast C call's stores in the code at `address`: `lea r10,
/// [rip+x]; mov [r13+pc], r10; mov [r13+fp], rbp`. The code is decoded as
/// it is read, never held.
fn read_fast_calls(found: &mut Found, address: u64, bytes: &[u8]) {
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut window = [Instruction::default(); 3];
    while decoder.can_decode() {
        window.rotate_left(1);
        window[2] = decoder.decode();
        let [lea, pc, fp] = &window;
        if lea.mnemonic() == Mnemonic::Lea
            && reg(lea, 0, Register::R10)
            && lea.memory_base() == Register::RIP
            && pc.mnemonic() == Mnemonic::Mov
            && reg(pc, 1, Register::R10)
            && fp.mnemonic() == Mnemonic::Mov
            && reg(fp, 1, Register::RBP)
            && let (Some(pc), Some(fp)) = (r13(pc), r13(fp))
        {
            *found.fast_calls.entry((pc, fp)).or_default() += 1;
        }
    }
}

/// What `build`'s code reads. A build without symbols has its builtins
/// found by `named`'s, a build of its V8 version with them.
pub(super) fn read(build: &Build, named: Option<&Build>) -> Result<Found, String> {
    type Reader = fn(&mut Found, &[Instruction]);
    let mut found = Found::default();
    let readers: [(&str, Reader); 3] = [
        (JS_ENTRY, read_js_entry),
        (C_ENTRY, read_c_entry),
        (DEOPTIMIZATION_ENTRY, read_deoptimization_entry),
    ];
    for (name, reader) in readers {
        if build.names_builtins() {
            let (address, code) = build.builtin(name).ok_or_else(|| format!("no {name}"))?;
            reader(&mut found, &decode(address, code));
            continue;
        }
        let named = named.ok_or("no build names the builtins")?;
        let (_, code) = named
            .builtin(name)
            .ok_or_else(|| format!("the named build has no {name}"))?;
        let expected = named.copies(code, code.len()).len();
        let copies = build.copies(code, code.len());
        if copies.len() != expected {
            return Err(format!(
                "{name}'s code occurs {} times, where the named build has it {expected}",
                copies.len()
            ));
        }
        // Every copy must read alike.
        let mut each = Vec::new();
        for (address, code) in copies {
            let mut one = Found::default();
            reader(&mut one, &decode(address, code));
            each.push(one);
        }
        if each.windows(2).any(|pair| pair[0] != pair[1]) {
            return Err(format!("the copies of {name} read differently: {each:?}"));
        }
        let one = each.pop().ok_or_else(|| format!("no copy of {name}"))?;
        found.entry_saves.extend(one.entry_saves);
        found.js_entry_sp = found.js_entry_sp.or(one.js_entry_sp);
        found.js_entry_sp_cleared.extend(one.js_entry_sp_cleared);
        found.exit_marker = found.exit_marker.or(one.exit_marker);
        found.exit_store = found.exit_store.or(one.exit_store);
        found.isolate = found.isolate.or(one.isolate);
        found.cleared_after_leave = found.cleared_after_leave.or(one.cleared_after_leave);
        found.iterable_clears.extend(one.iterable_clears);
        found.iterable_sets.extend(one.iterable_sets);
    }

    if build.names_builtins() {
        let object = build.object();
        let mut builtins: Vec<_> = object
            .symbols()
            .filter(|symbol| {
                symbol
                    .name()
                    .is_ok_and(|name| name.starts_with("Builtins_"))
            })
            .filter_map(|symbol| Some((symbol.name().ok()?.to_owned(), symbol.size())))
            .collect();
        builtins.sort();
        builtins.dedup();
        for (name, _) in builtins {
            if let Some((address, code)) = build.builtin(&name) {
                read_fast_calls(&mut found, address, code);
            }
        }
    } else {
        for (address, code) in build.code() {
            read_fast_calls(&mut found, address, code);
        }
    }
    Ok(found)
}

/// `v8::Isolate::Enter()`, which every build exports with V8's API, and
/// which runs V8's own `Isolate::Enter`.
const ENTER: &str = "_ZN2v87Isolate5EnterEv";

/// How many bytes of `Isolate::Enter` are read for its store of its new
/// entry.
const ENTER_BYTES: usize = 1024;

/// The displacements from a register that the code of V8's
/// `Isolate::Enter` writes a register to, as `entry_stack_ = item` does:
/// `mov` or `xchg` to `[base+d]`, or to `[r]` after `lea r, [base+d]`. The
/// exported `v8::Isolate::Enter` runs it, or jumps to it.
pub(super) fn enter_stores(build: &Build) -> Result<Vec<i64>, String> {
    let exported = build
        .exported(ENTER)
        .ok_or_else(|| format!("no exported {ENTER}"))?;
    let mut address = exported;
    // Past a frame it builds and drops, `push rbp; mov rbp, rsp; pop rbp`.
    let first = decode(
        address,
        build.code_at(address, 16).ok_or("no code at Enter")?,
    );
    let framed = |instruction: &&Instruction| {
        matches!(instruction.mnemonic(), Mnemonic::Push | Mnemonic::Pop)
            && reg(instruction, 0, Register::RBP)
            || instruction.mnemonic() == Mnemonic::Mov
                && reg(instruction, 0, Register::RBP)
                && reg(instruction, 1, Register::RSP)
    };
    if let Some(jump) = first
        .iter()
        .find(|instruction| !framed(instruction))
        .filter(|instruction| {
            instruction.mnemonic() == Mnemonic::Jmp
                && instruction.op0_kind() == OpKind::NearBranch64
        })
    {
        address = jump.near_branch_target();
    }
    let code = build
        .code_at(address, ENTER_BYTES)
        .ok_or("no code at V8's Isolate::Enter")?;
    let mut stores = Vec::new();
    let mut addressed: BTreeMap<Register, i64> = BTreeMap::new();
    for instruction in decode(address, code) {
        let writes = matches!(instruction.mnemonic(), Mnemonic::Mov | Mnemonic::Xchg)
            && instruction.op0_kind() == OpKind::Memory
            && instruction.op1_kind() == OpKind::Register
            && instruction.op1_register().is_gpr64()
            && instruction.memory_index() == Register::None;
        if writes {
            let base = instruction.memory_base();
            let displacement = instruction.memory_displacement64().cast_signed();
            if displacement == 0
                && let Some(&through) = addressed.get(&base)
            {
                stores.push(through);
            } else if base != Register::RSP && base != Register::RIP {
                stores.push(displacement);
            }
        }
        if instruction.mnemonic() == Mnemonic::Lea
            && instruction.op0_kind() == OpKind::Register
            && instruction.memory_index() == Register::None
            && instruction.memory_base() != Register::RIP
        {
            addressed.insert(
                instruction.op0_register(),
                instruction.memory_displacement64().cast_signed(),
            );
        }
        if instruction.mnemonic() == Mnemonic::Ret {
            break;
        }
    }
    Ok(stores)
}

/// Every way what `found` read differs from what `expected` says.
pub(super) fn disagreements(expected: &Expected, found: &Found) -> Vec<String> {
    fn check(wrong: &mut Vec<String>, what: &str, read: Option<i64>, says: i64) {
        if read != Some(says) {
            wrong.push(format!("{what}: the code has {read:?}, the layout {says}"));
        }
    }
    let mut wrong = Vec::new();

    let mut saves = vec![(expected.c_entry_fp, expected.exit_slot)];
    if let (Some(fp), Some(pc)) = (expected.fast_fp_slot, expected.fast_pc_slot) {
        saves.push((expected.fast_fp, fp));
        saves.push((expected.fast_pc, pc));
    }
    if found.entry_saves != saves {
        wrong.push(format!(
            "JSEntry saves and clears {:?}, where the layout says {saves:?}",
            found.entry_saves
        ));
    }
    check(
        &mut wrong,
        "JSEntry's js_entry_sp",
        found.js_entry_sp,
        expected.js_entry_sp,
    );
    if found.js_entry_sp_cleared != [expected.js_entry_sp] {
        wrong.push(format!(
            "JSEntry clears {:?} as it returns, where js_entry_sp is {}",
            found.js_entry_sp_cleared, expected.js_entry_sp
        ));
    }
    check(
        &mut wrong,
        "CEntry's exit frame marker",
        found.exit_marker,
        expected.exit_marker,
    );
    check(
        &mut wrong,
        "CEntry's c_entry_fp",
        found.exit_store,
        expected.c_entry_fp,
    );
    check(
        &mut wrong,
        "CEntry's isolate",
        found.isolate,
        -expected.bias,
    );
    check(
        &mut wrong,
        "CEntry's clear after leave",
        found.cleared_after_leave,
        expected.c_entry_fp,
    );

    let iterable = [expected.stack_is_iterable];
    let distinct = |all: &[i64]| -> Vec<i64> {
        let mut all = all.to_vec();
        all.sort_unstable();
        all.dedup();
        all
    };
    if distinct(&found.iterable_clears) != iterable || distinct(&found.iterable_sets) != iterable {
        wrong.push(format!(
            "the deoptimizer's entry clears {:?} and sets {:?}, where stack_is_iterable is {}",
            found.iterable_clears, found.iterable_sets, expected.stack_is_iterable
        ));
    }

    let calls: Vec<_> = found.fast_calls.iter().collect();
    match calls.as_slice() {
        &[(&(pc, fp), &count)] if count >= FEWEST_FAST_CALLS => {
            check(
                &mut wrong,
                "a fast call's return address",
                Some(pc),
                expected.fast_pc,
            );
            check(
                &mut wrong,
                "a fast call's frame pointer",
                Some(fp),
                expected.fast_fp,
            );
        }
        _ => wrong.push(format!("the fast calls store {:?}", found.fast_calls)),
    }
    wrong
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `build`'s runtime binds its layout to say.
    fn expected(entry: &str, build: &Build) -> Expected {
        let runtime = super::super::runtime(&build.object())
            .unwrap_or_else(|| panic!("{entry}: no V8 runtime"));
        let layout = runtime
            .layout
            .unwrap_or_else(|problem| panic!("{entry}: {problem}"));
        Expected::of(&layout)
    }

    /// Every build's builtins save, clear, and set the isolate's words at
    /// the displacements the generated layout gives. Debian's libnode,
    /// which names no builtin, is read through official 24's, a build of
    /// the same V8: each builtin's whole code must occur in libnode as
    /// often as in official 24, and every copy must read alike. What
    /// cannot be checked there is that a copy is the builtin and not the
    /// same bytes elsewhere.
    #[test]
    fn each_builds_code_uses_the_generated_isolate_offsets() {
        let matrix = super::super::conformance::matrix();
        for (entry, path, _) in &matrix {
            let build = Build::read(path);
            let named = (*entry == "debian-24").then(|| {
                assert!(!build.names_builtins(), "Debian's libnode names no builtin");
                let (_, official, _) = matrix
                    .iter()
                    .find(|(each, _, _)| *each == "24")
                    .expect("24 is in the matrix");
                Build::read(official)
            });
            let found =
                read(&build, named.as_ref()).unwrap_or_else(|problem| panic!("{entry}: {problem}"));
            let wrong = disagreements(&expected(entry, &build), &found);
            assert!(wrong.is_empty(), "{entry}: {wrong:#?}\n{found:#?}");
        }
    }

    /// Where every build's exported `v8::Isolate::IsInUse` reads an
    /// isolate's entries, V8's `Isolate::Enter` stores its new entry, and
    /// a word either way of it is no such store. Debian's libnode runs
    /// its unnamed `Isolate::Enter` through the exported one's jump.
    #[test]
    fn each_build_enters_isolates_where_it_reads_their_entries() {
        for (entry, path, _) in super::super::conformance::matrix() {
            let build = Build::read(&path);
            let runtime = super::super::runtime(&build.object())
                .unwrap_or_else(|| panic!("{entry}: no V8 runtime"));
            let offset = runtime
                .entry_stack
                .unwrap_or_else(|problem| panic!("{entry}: {problem}"))
                .cast_signed();
            let stores =
                enter_stores(&build).unwrap_or_else(|problem| panic!("{entry}: {problem}"));
            assert!(
                stores.contains(&offset),
                "{entry}: {offset:#x} not in {stores:x?}"
            );
            for planted in [offset - 8, offset + 8] {
                assert!(
                    !stores.contains(&planted),
                    "{entry}: Isolate::Enter also stores at {planted:#x}"
                );
            }
        }
    }

    /// Each displacement moved by a word, either way, is caught.
    #[test]
    fn a_planted_offset_is_caught() {
        type Plant = fn(&mut Expected, i64);
        let (entry, path, _) = super::super::conformance::matrix()
            .into_iter()
            .find(|(entry, _, _)| *entry == "26")
            .expect("26 is in the matrix");
        let build = Build::read(&path);
        let expected = expected(entry, &build);
        let found = read(&build, None).expect("26's code");
        assert!(disagreements(&expected, &found).is_empty(), "{entry}");

        let plants: [(&str, Plant); 10] = [
            ("c_entry_fp", |e, d| e.c_entry_fp += d),
            ("js_entry_sp", |e, d| e.js_entry_sp += d),
            ("fast_fp", |e, d| e.fast_fp += d),
            ("fast_pc", |e, d| e.fast_pc += d),
            ("stack_is_iterable", |e, d| e.stack_is_iterable += d),
            ("bias", |e, d| e.bias += d),
            ("exit_slot", |e, d| e.exit_slot += d),
            ("fast_fp_slot", |e, d| {
                e.fast_fp_slot = e.fast_fp_slot.map(|slot| slot + d);
            }),
            ("fast_pc_slot", |e, d| {
                e.fast_pc_slot = e.fast_pc_slot.map(|slot| slot + d);
            }),
            ("exit_marker", |e, d| e.exit_marker += d),
        ];
        for (name, plant) in plants {
            for delta in [-8, 8] {
                let mut planted = expected;
                plant(&mut planted, delta);
                assert!(
                    !disagreements(&planted, &found).is_empty(),
                    "{name} moved by {delta} is not caught"
                );
            }
        }
    }
}
