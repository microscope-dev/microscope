use std::collections::BTreeMap;

use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind};

use crate::image::v8::{Constant, V8Layout};

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
            exit_marker: constant(Constant::FrameExit) << 1,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Found {
    pub entry_saves: Vec<(i64, i64)>,

    pub js_entry_sp: Option<i64>,

    pub js_entry_sp_cleared: Vec<i64>,

    pub exit_marker: Option<i64>,
    pub exit_store: Option<i64>,
    pub isolate: Option<i64>,
    pub cleared_after_leave: Option<i64>,

    pub iterable_clears: Vec<i64>,
    pub iterable_sets: Vec<i64>,

    pub fast_calls: BTreeMap<(i64, i64), usize>,
}

const JS_ENTRY: &str = "Builtins_JSEntry";
const C_ENTRY: &str = "Builtins_CEntry_Return1_ArgvOnStack_NoBuiltinExit";
const DEOPTIMIZATION_ENTRY: &str = "Builtins_DeoptimizationEntry_Eager";

const FEWEST_FAST_CALLS: usize = 100;

pub(super) struct Build {
    data: Vec<u8>,
    builtins: BTreeMap<String, Option<(u64, std::ops::Range<usize>)>>,
}

impl Build {
    pub(super) fn read(path: &std::path::Path) -> Self {
        let data =
            std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let object = object::File::parse(data.as_slice()).expect("an ELF file");
        let mut builtins = BTreeMap::new();
        for symbol in object.symbols() {
            let Ok(name) = symbol.name() else {
                continue;
            };
            if !name.starts_with("Builtins_") {
                continue;
            }
            builtins.entry(name.to_owned()).or_insert_with(|| {
                let section = object.section_by_index(symbol.section_index()?).ok()?;
                let code = section.data_range(symbol.address(), symbol.size()).ok()??;
                let start = code.as_ptr() as usize - data.as_ptr() as usize;
                Some((symbol.address(), start..start + code.len()))
            });
        }
        Self { data, builtins }
    }

    fn object(&self) -> object::File<'_> {
        object::File::parse(self.data.as_slice()).expect("an ELF file")
    }

    fn builtin(&self, name: &str) -> Option<(u64, &[u8])> {
        let (address, range) = self.builtins.get(name)?.as_ref()?;
        Some((*address, &self.data[range.clone()]))
    }

    fn code_at(&self, address: u64, length: usize) -> Option<&[u8]> {
        self.code().into_iter().find_map(|(start, bytes)| {
            let at = usize::try_from(address.checked_sub(start)?).ok()?;
            let end = bytes.len().min(at.checked_add(length)?);
            bytes.get(at..end)
        })
    }

    fn exported(&self, name: &str) -> Option<u64> {
        self.object()
            .dynamic_symbols()
            .find(|symbol| symbol.name() == Ok(name) && symbol.is_definition())
            .map(|symbol| symbol.address())
    }

    fn names_builtins(&self) -> bool {
        self.builtin(JS_ENTRY).is_some()
    }

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
    memchr::memmem::find(haystack, needle)
}

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

fn read_js_entry(found: &mut Found, instructions: &[Instruction]) {
    let mut depth: Option<i64> = None;
    let mut pending: Option<(i64, i64)> = None;
    let mut loaded: Option<i64> = None;
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

const ENTER: &str = "_ZN2v87Isolate5EnterEv";

const ENTER_BYTES: usize = 1024;

pub(super) fn enter_stores(build: &Build) -> Result<Vec<i64>, String> {
    let exported = build
        .exported(ENTER)
        .ok_or_else(|| format!("no exported {ENTER}"))?;
    let mut address = exported;
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

    fn expected(entry: &str, build: &Build) -> Expected {
        let runtime = super::super::describe(&super::super::exports(&build.object()))
            .unwrap_or_else(|| panic!("{entry}: no V8 runtime"));
        let layout = runtime
            .layout
            .unwrap_or_else(|problem| panic!("{entry}: {problem}"));
        Expected::of(&layout)
    }

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

    #[test]
    fn each_build_enters_isolates_where_it_reads_their_entries() {
        for (entry, path, _) in super::super::conformance::matrix() {
            let build = Build::read(&path);
            let offset = super::super::accessors::entry_stack(&build.object())
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
