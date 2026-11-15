use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, Instruction, MemorySize, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::ImageAddress;
use crate::image::v8::{DispatchTable, EmbeddedBlob, NodeRelease, V8Version};

const DISPATCH_TABLE_ADDRESS: &str =
    "_ZN2v88internal17ExternalReference25js_dispatch_table_address";

pub(super) const GET_EMBEDDED_CODE_RANGE: &str = "_ZN2v87Isolate20GetEmbeddedCodeRangeEPPKvPm";

const GET_DEFAULT_GROUP: &str = "_ZN2v812IsolateGroup10GetDefaultEv";

const GROUP_HANDLE_DESTRUCTORS: [&str; 2] =
    ["_ZN2v812IsolateGroupD1Ev", "_ZN2v812IsolateGroupD2Ev"];

pub(super) const IS_IN_USE: &str = "_ZN2v87Isolate7IsInUseEv";

const ACCESSOR_BYTES: usize = 96;

const DESTRUCTOR_BYTES: usize = 192;

const SEGMENT_SIZE: u64 = 0x1_0000;

const _: () = assert!(
    DESTRUCTOR_BYTES >= 100,
    "the destructor's teardown lies within its first 100 bytes"
);
const _: () = assert!(
    SEGMENT_SIZE <= u32::MAX as u64,
    "the segment size must fit the 32-bit immediate the teardown moves"
);

fn function(object: &object::File<'_>, name: &str, prefix: bool) -> Option<u64> {
    object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| {
            symbol.kind() == SymbolKind::Text
                && symbol.is_definition()
                && symbol.name().is_ok_and(|own| {
                    if prefix {
                        own.starts_with(name)
                    } else {
                        own == name
                    }
                })
        })
        .map(|symbol| symbol.address())
}

pub(super) fn instructions(
    object: &object::File<'_>,
    address: u64,
) -> Result<Vec<Instruction>, Arc<str>> {
    instructions_within(object, address, ACCESSOR_BYTES)
}

fn instructions_within(
    object: &object::File<'_>,
    address: u64,
    limit: usize,
) -> Result<Vec<Instruction>, Arc<str>> {
    let bytes = object
        .sections()
        .find_map(|section| {
            let start = section.address();
            let end = start.checked_add(section.size())?;
            if section.kind() != SectionKind::Text || !(start..end).contains(&address) {
                return None;
            }
            let data = section.data().ok()?;
            let at = usize::try_from(address - start).ok()?;
            data.get(at..data.len().min(at.saturating_add(limit)))
        })
        .ok_or_else(|| -> Arc<str> { format!("no code at {address:#x}").into() })?;

    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut kept = Vec::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        let prologue = instruction.mnemonic() == Mnemonic::Endbr64
            || super::super::x86_64::is_push_register(&instruction, Register::RBP)
            || super::super::x86_64::is_register_move(&instruction, Register::RBP, Register::RSP);
        if !prologue {
            kept.push(instruction);
        }
        if matches!(instruction.mnemonic(), Mnemonic::Ret | Mnemonic::Jmp) {
            break;
        }
    }
    Ok(kept)
}

fn unexpected(what: &str, instruction: Option<&Instruction>) -> Arc<str> {
    instruction.map_or_else(
        || format!("{what} ends before its shape does").into(),
        |instruction| {
            format!(
                "{what} at {:#x} is `{instruction}`, not an instruction of the shape \
                 microscope reads",
                instruction.ip()
            )
            .into()
        },
    )
}

fn load_into_rax(instruction: &Instruction, base: Register) -> Option<u64> {
    load(instruction, Register::RAX, base)
}

fn load(instruction: &Instruction, to: Register, base: Register) -> Option<u64> {
    (instruction.mnemonic() == Mnemonic::Mov
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == to
        && instruction.op1_kind() == OpKind::Memory
        && instruction.memory_base() == base
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None)
        .then(|| instruction.memory_displacement64())
}

pub(super) fn dispatch_table(
    object: &object::File<'_>,
    version: V8Version,
) -> Result<Option<DispatchTable>, Arc<str>> {
    if (version.major, version.minor) < super::LEAPTIERING {
        return Ok(None);
    }

    let Some(address) = function(object, DISPATCH_TABLE_ADDRESS, true) else {
        let unnamed = format!(
            "V8 {version} names no ExternalReference::js_dispatch_table_address, whose code \
             says where its JS dispatch table is"
        );
        if (version.major, version.minor) != super::LEAPTIERING {
            return Err(unnamed.into());
        }
        return exported_dispatch_table(object)
            .map(Some)
            .map_err(|problem| format!("{unnamed}, and {problem}").into());
    };
    let what = format!("V8 {version}'s ExternalReference::js_dispatch_table_address");
    decode_dispatch_table(&instructions(object, address)?, &what).map(Some)
}

pub(super) fn exported_dispatch_table(
    object: &object::File<'_>,
) -> Result<DispatchTable, Arc<str>> {
    let get_default = function(object, GET_DEFAULT_GROUP, false)
        .ok_or_else(|| -> Arc<str> { "the module names no v8::IsolateGroup::GetDefault".into() })?;
    let group = decode_group_variable(
        &instructions(object, get_default)?,
        "v8::IsolateGroup::GetDefault",
    )?;

    let handle = GROUP_HANDLE_DESTRUCTORS
        .iter()
        .find_map(|name| function(object, name, false))
        .ok_or_else(|| -> Arc<str> {
            "the module names no v8::IsolateGroup::~IsolateGroup".into()
        })?;
    let release = decode_handle_release(
        &instructions(object, handle)?,
        "v8::IsolateGroup::~IsolateGroup",
    )?;
    let what = format!("i::IsolateGroup::Release at {release:#x}");
    let deletion = decode_release(&instructions(object, release)?, &what)?;
    let what = format!("i::IsolateGroup::Release's deletion at {deletion:#x}");
    let (destructor, size) = decode_deletion(&instructions(object, deletion)?, &what)?;
    let what = format!("i::IsolateGroup::~IsolateGroup at {destructor:#x}");
    let code = instructions_within(object, destructor, DESTRUCTOR_BYTES)?;
    let offset = decode_table_teardown(&code, &what)?;
    if offset.checked_add(16).is_none_or(|end| end > size) {
        return Err(format!(
            "{what} clears the table's base at {offset:#x}, outside the {size:#x} bytes \
             Release deletes"
        )
        .into());
    }

    Ok(DispatchTable::Group {
        group: ImageAddress::new(group),
        offset,
    })
}

fn next_as<'a, T>(
    code: &mut impl Iterator<Item = &'a Instruction>,
    what: &str,
    read: impl FnOnce(&Instruction) -> Option<T>,
) -> Result<T, Arc<str>> {
    let next = code.next();
    next.and_then(read).ok_or_else(|| unexpected(what, next))
}

fn keeps(instruction: &Instruction, held: Option<Register>) -> bool {
    let register = |operand: u32| {
        (instruction.op_kind(operand) == OpKind::Register)
            .then(|| instruction.op_register(operand))
            .filter(|register| register.is_gpr64())
    };
    let other = register(0).is_some_and(|to| Some(to) != held);
    match instruction.mnemonic() {
        Mnemonic::Push => register(0).is_some(),
        Mnemonic::Sub => {
            register(0) == Some(Register::RSP)
                && matches!(
                    instruction.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                )
        }
        Mnemonic::Mov => match (instruction.op0_kind(), instruction.op1_kind()) {
            (OpKind::Memory, OpKind::Register) => {
                instruction.memory_base() == Register::RBP
                    && instruction.memory_index() == Register::None
                    && register(1).is_some()
            }
            (OpKind::Register, OpKind::Register) => other && register(1).is_some(),
            (OpKind::Register, OpKind::Memory) => {
                other
                    && instruction.segment_prefix() == Register::FS
                    && instruction.memory_base() == Register::None
                    && instruction.memory_index() == Register::None
                    && instruction.memory_displacement64() == 0x28
            }
            _ => false,
        },
        _ => false,
    }
}

fn decode_group_variable(instructions: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut addressed: Option<(Register, u64)> = None;
    let mut loaded: Option<(Register, u64)> = None;
    for instruction in instructions {
        let held = loaded.or(addressed).map(|(register, _)| register);
        if keeps(instruction, held) {
            continue;
        }

        let to = (instruction.op0_kind() == OpKind::Register)
            .then(|| instruction.op0_register())
            .filter(|register| register.is_gpr64());
        match (addressed, loaded, to) {
            (_, Some((group, variable)), _) => {
                return acquires(instruction, group)
                    .then_some(variable)
                    .ok_or_else(|| unexpected(what, Some(instruction)));
            }
            (Some((register, variable)), None, _)
                if load(instruction, register, register) == Some(0) =>
            {
                loaded = Some((register, variable));
            }
            (None, None, Some(to))
                if matches!(instruction.mnemonic(), Mnemonic::Mov | Mnemonic::Lea)
                    && instruction.is_ip_rel_memory_operand() =>
            {
                let variable = Some((to, instruction.ip_rel_memory_address()));
                if instruction.mnemonic() == Mnemonic::Mov {
                    loaded = variable;
                } else {
                    addressed = variable;
                }
            }
            _ => return Err(unexpected(what, Some(instruction))),
        }
    }

    Err(unexpected(what, None))
}

fn acquires(instruction: &Instruction, group: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Add
        && instruction.has_lock_prefix()
        && instruction.op0_kind() == OpKind::Memory
        && instruction.memory_base() == group
        && instruction.memory_index() == Register::None
        && instruction.memory_displacement64() == 0
        && instruction.memory_size() == MemorySize::UInt32
        && instruction.op1_kind() == OpKind::Immediate8to32
        && instruction.immediate(1) == 1
}

fn branch(instruction: &Instruction, mnemonic: Mnemonic) -> Option<u64> {
    (instruction.mnemonic() == mnemonic && instruction.op0_kind() == OpKind::NearBranch64)
        .then(|| instruction.near_branch_target())
}

fn tests(instruction: &Instruction, register: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Test
        && instruction.op0_kind() == OpKind::Register
        && instruction.op1_kind() == OpKind::Register
        && instruction.op0_register() == register
        && instruction.op1_register() == register
}

fn moves_immediate(instruction: &Instruction, register: Register) -> Option<u64> {
    (instruction.mnemonic() == Mnemonic::Mov
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == register
        && instruction.op1_kind() == OpKind::Immediate32)
        .then(|| instruction.immediate(1))
}

fn clears(instruction: &Instruction, base: Register) -> Option<u64> {
    (instruction.mnemonic() == Mnemonic::Mov
        && instruction.op0_kind() == OpKind::Memory
        && instruction.memory_base() == base
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None
        && instruction.memory_size() == MemorySize::UInt64
        && instruction.op1_kind() == OpKind::Immediate32to64
        && instruction.immediate(1) == 0)
        .then(|| instruction.memory_displacement64())
}

fn calls_virtual(instruction: &Instruction) -> bool {
    instruction.mnemonic() == Mnemonic::Call
        && instruction.op0_kind() == OpKind::Memory
        && instruction.memory_base() == Register::RAX
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None
}

fn decode_handle_release(instructions: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut code = instructions.iter();
    next_as(&mut code, what, |next| {
        (load(next, Register::RDI, Register::RDI) == Some(0)).then_some(())
    })?;
    next_as(&mut code, what, |next| {
        tests(next, Register::RDI).then_some(())
    })?;
    next_as(&mut code, what, |next| branch(next, Mnemonic::Je))?;
    next_as(&mut code, what, |next| branch(next, Mnemonic::Jmp))
}

fn decode_release(instructions: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut code = instructions.iter();
    next_as(&mut code, what, |next| {
        (next.mnemonic() == Mnemonic::Sub
            && next.has_lock_prefix()
            && next.op0_kind() == OpKind::Memory
            && next.memory_base() == Register::RDI
            && next.memory_index() == Register::None
            && next.memory_displacement64() == 0
            && next.memory_size() == MemorySize::UInt32
            && next.op1_kind() == OpKind::Immediate8to32
            && next.immediate(1) == 1)
            .then_some(())
    })?;
    next_as(&mut code, what, |next| branch(next, Mnemonic::Je))
}

fn decode_deletion(instructions: &[Instruction], what: &str) -> Result<(u64, u64), Arc<str>> {
    let mut code = instructions.iter();
    let destructor = loop {
        let next = code.next();
        match next {
            Some(next) if keeps(next, None) => {}
            _ => {
                break next
                    .and_then(|next| branch(next, Mnemonic::Call))
                    .ok_or_else(|| unexpected(what, next))?;
            }
        }
    };

    let mut size = None;
    for next in code {
        let to = (next.op0_kind() == OpKind::Register)
            .then(|| next.op0_register())
            .filter(|register| register.is_gpr64());
        let restores = next.mnemonic() == Mnemonic::Leave
            || to.is_some_and(|to| {
                load(next, to, Register::RBP).is_some()
                    || (next.mnemonic() == Mnemonic::Mov
                        && next.op1_kind() == OpKind::Register
                        && next.op1_register().is_gpr64())
            });
        if restores {
            continue;
        }
        if size.is_none()
            && let Some(freed) = moves_immediate(next, Register::ESI)
        {
            size = Some(freed);
            continue;
        }
        return match (size, branch(next, Mnemonic::Jmp)) {
            (Some(size), Some(_)) => Ok((destructor, size)),
            _ => Err(unexpected(what, Some(next))),
        };
    }

    Err(unexpected(what, None))
}

fn decode_table_teardown(instructions: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut code = instructions.iter();
    let mut this = Register::RDI;
    loop {
        let next = code.next();
        match next {
            Some(next)
                if next.mnemonic() == Mnemonic::Push && next.op0_kind() == OpKind::Register => {}
            Some(next) if next.mnemonic() == Mnemonic::Sub && keeps(next, None) => {}
            Some(next)
                if next.mnemonic() == Mnemonic::Mov
                    && next.op0_kind() == OpKind::Register
                    && next.op0_register().is_gpr64()
                    && next.op1_kind() == OpKind::Register
                    && next.op1_register() == Register::RDI =>
            {
                this = next.op0_register();
            }
            Some(next)
                if load(next, Register::RDI, Register::RDI).is_some()
                    || load(next, Register::RDI, this).is_some() =>
            {
                break;
            }
            _ => return Err(unexpected(what, next)),
        }
    }
    let call = code.next();
    if call.and_then(|call| branch(call, Mnemonic::Call)).is_none()
        || !matches!(
            this,
            Register::RBX | Register::R12 | Register::R13 | Register::R14 | Register::R15
        )
    {
        return Err(unexpected(what, call));
    }

    let vas = next_as(&mut code, what, |next| load(next, Register::RDI, this))?;
    next_as(&mut code, what, |next| {
        (moves_immediate(next, Register::EDX) == Some(SEGMENT_SIZE)).then_some(())
    })?;
    next_as(&mut code, what, |next| {
        (load(next, Register::RAX, Register::RDI) == Some(0)).then_some(())
    })?;
    next_as(&mut code, what, |next| {
        load(next, Register::RSI, Register::RDI)
    })?;
    next_as(&mut code, what, |next| calls_virtual(next).then_some(()))?;
    next_as(&mut code, what, |next| {
        (load(next, Register::RDI, this) == Some(vas)).then_some(())
    })?;
    let base = next_as(&mut code, what, |next| clears(next, this))?;
    next_as(&mut code, what, |next| {
        tests(next, Register::RDI).then_some(())
    })?;
    let unset = next_as(&mut code, what, |next| branch(next, Mnemonic::Je))?;
    next_as(&mut code, what, |next| {
        (load(next, Register::RAX, Register::RDI) == Some(0)).then_some(())
    })?;
    next_as(&mut code, what, |next| calls_virtual(next).then_some(()))?;
    next_as(&mut code, what, |next| {
        (clears(next, this) == Some(vas) && next.ip() == unset).then_some(())
    })?;

    if base.checked_add(8) != Some(vas) {
        return Err(format!(
            "{what} clears {base:#x} and deletes {vas:#x}, not a table's base_ and the vas_ \
             just after it"
        )
        .into());
    }
    Ok(base)
}

pub(super) fn decode_dispatch_table(
    instructions: &[Instruction],
    what: &str,
) -> Result<DispatchTable, Arc<str>> {
    let mut code = instructions.iter();
    let first = code.next().ok_or_else(|| unexpected(what, None))?;
    let table = if let Some(offset) = load_into_rax(first, Register::RDI) {
        DispatchTable::Isolate { offset }
    } else {
        let loads_group = match first.mnemonic() {
            Mnemonic::Mov => load_into_rax(first, Register::RIP).is_some(),
            Mnemonic::Lea => {
                first.op0_register() == Register::RAX
                    && first.is_ip_rel_memory_operand()
                    && code
                        .next()
                        .and_then(|next| load_into_rax(next, Register::RAX))
                        == Some(0)
            }
            _ => false,
        };
        if !loads_group {
            return Err(unexpected(what, Some(first)));
        }

        let next = code.next();
        let offset = next
            .and_then(|next| load_into_rax(next, Register::RAX))
            .ok_or_else(|| unexpected(what, next))?;
        DispatchTable::Group {
            group: ImageAddress::new(first.ip_rel_memory_address()),
            offset,
        }
    };

    returns_rax(code, what)?;
    Ok(table)
}

fn returns_rax<'a>(
    code: impl Iterator<Item = &'a Instruction>,
    what: &str,
) -> Result<(), Arc<str>> {
    for instruction in code {
        match instruction.mnemonic() {
            Mnemonic::Ret => return Ok(()),
            Mnemonic::Cmp | Mnemonic::Test | Mnemonic::Jbe | Mnemonic::Ja | Mnemonic::Nop => {}
            Mnemonic::Lea | Mnemonic::Xor
                if instruction.op0_kind() == OpKind::Register
                    && instruction.op0_register().full_register() != Register::RAX => {}
            _ => return Err(unexpected(what, Some(instruction))),
        }
    }

    Err(unexpected(what, None))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    Unknown,

    CodeOut,

    SizeOut,

    Returned(usize),
}

const CALLER_SAVED: [Register; 8] = [
    Register::RCX,
    Register::RDX,
    Register::RSI,
    Register::RDI,
    Register::R8,
    Register::R9,
    Register::R10,
    Register::R11,
];

struct Flow {
    calls: Vec<u64>,
    code: Option<Value>,
    size: Option<Value>,
}

fn follow(instructions: &[Instruction], what: &str) -> Result<Flow, Arc<str>> {
    let mut registers = Registers([Value::Unknown; 16]);
    registers.set(Register::RSI, Value::CodeOut);
    registers.set(Register::RDX, Value::SizeOut);
    let mut flow = Flow {
        calls: Vec::new(),
        code: None,
        size: None,
    };
    for instruction in instructions {
        let refused = || unexpected(what, Some(instruction));
        let register = |operand: u32| {
            (instruction.op_kind(operand) == OpKind::Register)
                .then(|| instruction.op_register(operand))
                .filter(|register| register.full_register().is_gpr64())
        };
        match (instruction.mnemonic(), instruction.op0_kind()) {
            (Mnemonic::Ret, _) => {
                debug_assert!(
                    !matches!(flow.code, Some(Value::Returned(call)) if call >= flow.calls.len()),
                    "{what} stores the result of call {:?} of {} as the code",
                    flow.code,
                    flow.calls.len()
                );
                debug_assert!(
                    !matches!(flow.size, Some(Value::Returned(call)) if call >= flow.calls.len()),
                    "{what} stores the result of call {:?} of {} as the size",
                    flow.size,
                    flow.calls.len()
                );
                return Ok(flow);
            }
            (Mnemonic::Nop, _) | (Mnemonic::Push, OpKind::Register) => {}
            (Mnemonic::Sub | Mnemonic::Add, OpKind::Register)
                if instruction.op0_register() == Register::RSP => {}
            (Mnemonic::Pop, OpKind::Register) => {
                registers.set(register(0).ok_or_else(refused)?, Value::Unknown);
            }
            (Mnemonic::Xor, OpKind::Register) if register(0) == register(1) => {
                registers.set(register(0).ok_or_else(refused)?, Value::Unknown);
            }
            (Mnemonic::Mov, OpKind::Register) => {
                let (Some(to), Some(from)) = (register(0), register(1)) else {
                    return Err(refused());
                };
                registers.set(to, registers.get(from));
            }
            (Mnemonic::Call, OpKind::NearBranch64) => {
                flow.calls.push(instruction.near_branch_target());
                for saved in CALLER_SAVED {
                    registers.set(saved, Value::Unknown);
                }
                registers.set(Register::RAX, Value::Returned(flow.calls.len() - 1));
            }
            (Mnemonic::Mov, OpKind::Memory) => store(instruction, &registers, &mut flow, what)?,
            _ => return Err(refused()),
        }
    }

    Err(unexpected(what, None))
}

struct Registers([Value; 16]);

impl Registers {
    fn get(&self, register: Register) -> Value {
        let register = register.full_register();
        self.0
            .get(register.number())
            .copied()
            .filter(|_| register.is_gpr64())
            .unwrap_or(Value::Unknown)
    }

    fn set(&mut self, register: Register, value: Value) {
        let register = register.full_register();
        debug_assert!(
            register.is_gpr64(),
            "setting {register:?}, which is not a general-purpose register"
        );
        if register.is_gpr64()
            && let Some(slot) = self.0.get_mut(register.number())
        {
            *slot = value;
        }
    }
}

fn store(
    instruction: &Instruction,
    registers: &Registers,
    flow: &mut Flow,
    what: &str,
) -> Result<(), Arc<str>> {
    if instruction.memory_index() != Register::None
        || instruction.memory_displacement64() != 0
        || instruction.op1_kind() != OpKind::Register
    {
        return Err(unexpected(what, Some(instruction)));
    }

    let source = registers.get(instruction.op1_register());
    let stored = match registers.get(instruction.memory_base()) {
        Value::CodeOut => &mut flow.code,
        Value::SizeOut => &mut flow.size,
        _ => return Err(unexpected(what, Some(instruction))),
    };
    if stored.replace(source).is_some() {
        return Err(format!("{what} stores through one argument twice").into());
    }

    Ok(())
}

fn blob_accessor(object: &object::File<'_>, address: u64) -> Result<(u64, bool), Arc<str>> {
    let what = format!("the embedded blob accessor at {address:#x}");
    let code = instructions(object, address)?;
    let mut code = code.iter();
    let load = code.next();
    let Some(load) = load.filter(|load| {
        load.mnemonic() == Mnemonic::Mov
            && load.op0_kind() == OpKind::Register
            && load.op0_register().full_register() == Register::RAX
            && load.is_ip_rel_memory_operand()
    }) else {
        return Err(unexpected(&what, load));
    };

    let wide = match load.op0_register() {
        Register::RAX => true,
        Register::EAX => false,
        _ => return Err(unexpected(&what, Some(load))),
    };
    for instruction in code.by_ref() {
        match instruction.mnemonic() {
            Mnemonic::Ret => return Ok((load.ip_rel_memory_address(), wide)),
            Mnemonic::Pop if instruction.op0_register() == Register::RBP => {}
            Mnemonic::Xor
                if instruction.op0_kind() == OpKind::Register
                    && instruction.op0_register().full_register() != Register::RAX => {}
            _ => return Err(unexpected(&what, Some(instruction))),
        }
    }

    Err(unexpected(&what, None))
}

pub(super) fn embedded_blob(object: &object::File<'_>) -> Result<EmbeddedBlob, Arc<str>> {
    let what = "v8::Isolate::GetEmbeddedCodeRange";
    let address = function(object, GET_EMBEDDED_CODE_RANGE, false)
        .ok_or_else(|| -> Arc<str> { format!("the module names no {what}").into() })?;
    let flow = follow(&instructions(object, address)?, what)?;
    let accessors = flow
        .calls
        .iter()
        .map(|&call| blob_accessor(object, call))
        .collect::<Result<Vec<_>, _>>()?;
    assign_blob(&flow, &accessors, what)
}

fn assign_blob(
    flow: &Flow,
    accessors: &[(u64, bool)],
    what: &str,
) -> Result<EmbeddedBlob, Arc<str>> {
    let (Some(Value::Returned(code)), Some(Value::Returned(size))) = (flow.code, flow.size) else {
        return Err(format!("{what} stores no call's result as the code and its size").into());
    };

    let wide = |index: usize| accessors.get(index).map(|&(_, wide)| wide);
    if accessors.len() != 4 || code == size || wide(code) != Some(true) || wide(size) != Some(false)
    {
        return Err(format!(
            "{what} calls {} accessors and stores calls {code} and {size}, not a pointer \
             and a size of four",
            accessors.len()
        )
        .into());
    }

    let others: Vec<_> = (0..accessors.len())
        .filter(|&index| index != code && index != size)
        .filter_map(|index| accessors.get(index))
        .collect();
    let (data, data_size) = match others.as_slice() {
        [first, second] if first.1 && !second.1 => (first.0, second.0),
        [first, second] if !first.1 && second.1 => (second.0, first.0),
        _ => return Err(format!("{what}'s other accessors are not a pointer and a size").into()),
    };

    debug_assert!(
        code < accessors.len(),
        "the code's call {code} is past the {} accessors",
        accessors.len()
    );
    debug_assert!(
        size < accessors.len(),
        "the size's call {size} is past the {} accessors",
        accessors.len()
    );
    let variable = |index: usize| {
        accessors
            .get(index)
            .map_or(ImageAddress::new(0), |&(at, _)| ImageAddress::new(at))
    };
    Ok(EmbeddedBlob {
        code: variable(code),
        code_size: variable(size),
        data: ImageAddress::new(data),
        data_size: ImageAddress::new(data_size),
    })
}

const GET_NODE_VERSION: &str = "napi_get_node_version";

const NODE_VERSION: &str = "_ZZ21napi_get_node_versionE7version";

pub(super) fn node_release(object: &object::File<'_>) -> Result<NodeRelease, Arc<str>> {
    let named = object
        .symbols()
        .find(|symbol| symbol.is_definition() && symbol.name() == Ok(NODE_VERSION))
        .map(|symbol| symbol.address());
    let address = if let Some(address) = named {
        address
    } else {
        let function = function(object, GET_NODE_VERSION, false).ok_or_else(|| -> Arc<str> {
            format!("the module names no {GET_NODE_VERSION}").into()
        })?;
        decode_node_version(&instructions(object, function)?, GET_NODE_VERSION)?
    };
    let bytes = object
        .sections()
        .find_map(|section| section.data_range(address, 12).ok().flatten())
        .ok_or_else(|| -> Arc<str> {
            format!("the module keeps no napi_node_version at {address:#x}").into()
        })?;
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|word| word.try_into().ok())
            .map_or(0, u32::from_le_bytes)
    };
    Ok(NodeRelease {
        major: word(0),
        minor: word(4),
        patch: word(8),
    })
}

fn decode_node_version(instructions: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let zeroes_vector = |instruction: &&Instruction| {
        matches!(instruction.mnemonic(), Mnemonic::Pxor | Mnemonic::Xorps)
            && instruction.op0_kind() == OpKind::Register
            && instruction.op1_kind() == OpKind::Register
            && instruction.op0_register().is_xmm()
            && instruction.op0_register() == instruction.op1_register()
    };
    let mut code = instructions
        .iter()
        .filter(|instruction| !zeroes_vector(instruction))
        .peekable();
    if code
        .peek()
        .is_some_and(|instruction| moves_immediate(instruction, Register::EAX).is_some())
    {
        code.next();
    }
    for register in [Register::RDI, Register::RSI] {
        next_as(&mut code, what, |instruction| {
            tests(instruction, register).then_some(())
        })?;
        next_as(&mut code, what, |instruction| {
            branch(instruction, Mnemonic::Je)
        })?;
    }

    let stores = |instruction: &Instruction| {
        instruction.mnemonic() == Mnemonic::Mov
            && instruction.op0_kind() == OpKind::Memory
            && instruction.memory_base() == Register::RSI
            && instruction.memory_index() == Register::None
            && instruction.memory_displacement64() == 0
            && instruction.segment_prefix() == Register::None
            && instruction.memory_size() == MemorySize::UInt64
    };
    let first = code.next();
    if let Some(instruction) = first
        && stores(instruction)
        && instruction.op1_kind() == OpKind::Immediate32to64
    {
        return Ok(instruction.immediate(1));
    }
    let (register, address) = first
        .filter(|instruction| {
            instruction.mnemonic() == Mnemonic::Lea
                && instruction.op0_kind() == OpKind::Register
                && instruction.op0_register().is_gpr64()
                && instruction.is_ip_rel_memory_operand()
        })
        .map(|instruction| {
            (
                instruction.op0_register(),
                instruction.ip_rel_memory_address(),
            )
        })
        .ok_or_else(|| unexpected(what, first))?;
    next_as(&mut code, what, |instruction| {
        (stores(instruction)
            && instruction.op1_kind() == OpKind::Register
            && instruction.op1_register() == register)
            .then_some(address)
    })
}

pub(super) fn entry_stack(object: &object::File<'_>) -> Result<u64, Arc<str>> {
    let function = function(object, IS_IN_USE, false)
        .ok_or_else(|| -> Arc<str> { format!("the module names no {IS_IN_USE}").into() })?;
    decode_entry_stack(&instructions(object, function)?, IS_IN_USE)
}

fn decode_entry_stack(instructions: &[Instruction], what: &str) -> Result<u64, Arc<str>> {
    let mut code = instructions.iter();
    let offset = next_as(&mut code, what, |instruction| {
        load_into_rax(instruction, Register::RDI)
    })?;
    next_as(&mut code, what, |instruction| {
        tests(instruction, Register::RAX).then_some(())
    })?;
    next_as(&mut code, what, |instruction| {
        (instruction.mnemonic() == Mnemonic::Setne
            && instruction.op0_kind() == OpKind::Register
            && instruction.op0_register() == Register::AL)
            .then_some(())
    })?;
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Instruction> {
        let mut decoder = Decoder::with_ip(64, bytes, 0x1000, DecoderOptions::NONE);
        std::iter::from_fn(|| decoder.can_decode().then(|| decoder.decode())).collect()
    }

    const RET: u8 = 0xc3;

    #[test]
    fn napi_get_node_version_of_another_shape_is_refused() {
        const CHECKS: [u8; 10] = [0x48, 0x85, 0xff, 0x74, 0x10, 0x48, 0x85, 0xf6, 0x74, 0x10];

        let other = decode(
            &[
                &CHECKS[..],
                &[0x48, 0x8d, 0x05, 0, 1, 0, 0, 0x48, 0x89, 0x0e, RET],
            ]
            .concat(),
        );
        let refused = decode_node_version(&other, "napi_get_node_version").unwrap_err();
        assert!(refused.contains("is `mov [rsi],rcx`"), "{refused}");

        let elsewhere = decode(&[&CHECKS[..], &[0x48, 0xc7, 0x07, 0, 0x10, 0, 0, RET]].concat());
        assert!(decode_node_version(&elsewhere, "napi_get_node_version").is_err());

        let unchecked = decode(&[
            0x48, 0x85, 0xff, 0x74, 0x10, 0x48, 0xc7, 0x06, 0, 0x10, 0, 0, RET,
        ]);
        assert!(decode_node_version(&unchecked, "napi_get_node_version").is_err());
        assert!(decode_node_version(&[], "napi_get_node_version").is_err());
    }

    #[test]
    fn a_dispatch_table_accessor_of_another_shape_is_refused_by_its_instruction() {
        let loads_again = decode(&[0x48, 0x8b, 0x87, 0x68, 0x02, 0, 0, 0x48, 0x8b, 0x00, RET]);
        let refused = decode_dispatch_table(&loads_again, "accessor").unwrap_err();
        assert!(refused.contains("is `mov rax,[rax]`"), "{refused}");

        let elsewhere = decode(&[0x48, 0x8b, 0x4f, 0x08, RET]);
        assert!(decode_dispatch_table(&elsewhere, "accessor").is_err());

        let short = decode(&[0x48, 0x8b, 0x05, 0, 1, 0, 0, RET]);
        assert!(decode_dispatch_table(&short, "accessor").is_err());
        assert!(decode_dispatch_table(&[], "accessor").is_err());
    }

    #[test]
    fn a_get_default_of_another_shape_is_refused_by_its_instruction() {
        let overwritten = decode(&[
            0x48, 0x8d, 0x05, 0, 1, 0, 0, 0x48, 0x89, 0xf8, 0x48, 0x8b, 0x00, 0xf0, 0x83, 0x00,
            0x01,
        ]);
        let refused = decode_group_variable(&overwritten, "get").unwrap_err();
        assert!(refused.contains("is `mov rax,rdi`"), "{refused}");

        let elsewhere = decode(&[0x48, 0x8b, 0x15, 0, 1, 0, 0, 0xf0, 0x83, 0x01, 0x01, RET]);
        assert!(decode_group_variable(&elsewhere, "get").is_err());

        let unacquired = decode(&[0x48, 0x8b, 0x15, 0, 1, 0, 0, RET]);
        assert!(decode_group_variable(&unacquired, "get").is_err());
    }

    #[test]
    fn the_group_handles_destructor_leads_to_the_groups_deletion() {
        let handle = decode(&[
            0x48, 0x8b, 0x3f, 0x48, 0x85, 0xff, 0x74, 0x08, 0xe9, 0, 0, 0, 0,
        ]);
        assert_eq!(decode_handle_release(&handle, "handle"), Ok(0x100d));

        let release = decode(&[0xf0, 0x83, 0x2f, 0x01, 0x74, 0x0a, RET]);
        assert_eq!(decode_release(&release, "release"), Ok(0x1010));

        let debian = decode(&[
            0x48, 0x83, 0xec, 0x10, 0x48, 0x89, 0x7d, 0xf8, 0xe8, 0, 0, 0, 0, 0x48, 0x8b, 0x7d,
            0xf8, 0xbe, 0x80, 0x32, 0, 0, 0xc9, 0xe9, 0, 0, 0, 0,
        ]);
        assert_eq!(decode_deletion(&debian, "delete"), Ok((0x100d, 0x3280)));

        let official = decode(&[
            0x53, 0x48, 0x89, 0xfb, 0x48, 0x83, 0xec, 0x08, 0xe8, 0, 0, 0, 0, 0x48, 0x89, 0xdf,
            0x48, 0x8b, 0x5d, 0xf8, 0xbe, 0x80, 0x32, 0, 0, 0xc9, 0xe9, 0, 0, 0, 0,
        ]);
        assert_eq!(decode_deletion(&official, "delete"), Ok((0x100d, 0x3280)));

        let no_size = decode(&[0xe8, 0, 0, 0, 0, 0xc9, 0xe9, 0, 0, 0, 0]);
        assert!(decode_deletion(&no_size, "delete").is_err());
        let added = decode(&[0xf0, 0x83, 0x07, 0x01, 0x74, 0x0a, RET]);
        assert!(decode_release(&added, "release").is_err());
    }

    fn teardown(base: u16, vas: u16, in_order: bool) -> Vec<Instruction> {
        let [base_low, base_high] = base.to_le_bytes();
        let [vas_low, vas_high] = vas.to_le_bytes();
        let mut bytes = vec![
            0x53, 0x48, 0x89, 0xfb, 0x48, 0x83, 0xec, 0x18, 0x48, 0x8b, 0x7f, 0x10, 0xe8, 0, 0, 0,
            0,
        ];
        let load_vas = [0x48, 0x8b, 0xbb, vas_low, vas_high, 0, 0];
        bytes.extend(load_vas);
        bytes.extend([0xba, 0, 0, 1, 0]);
        bytes.extend([0x48, 0x8b, 0x07, 0x48, 0x8b, 0x77, 0x18]);
        bytes.extend([0xff, 0x50, 0x28]);
        bytes.extend(load_vas);
        let clear_base = [0x48, 0xc7, 0x83, base_low, base_high, 0, 0, 0, 0, 0, 0];
        let delete = [
            0x48, 0x85, 0xff, 0x74, 0x06, 0x48, 0x8b, 0x07, 0xff, 0x50, 0x08,
        ];
        if in_order {
            bytes.extend(clear_base);
            bytes.extend(delete);
        } else {
            bytes.extend(delete);
            bytes.extend(clear_base);
        }
        bytes.extend([0x48, 0xc7, 0x83, vas_low, vas_high, 0, 0, 0, 0, 0, 0]);
        bytes.extend([0x48, 0x8b, 0x7b, 0x20]);
        decode(&bytes)
    }

    #[test]
    fn a_teardown_of_another_shape_is_refused() {
        let late = decode_table_teardown(&teardown(0x3270, 0x3278, false), "dtor").unwrap_err();
        assert!(late.contains("is `test rdi,rdi`"), "{late}");

        let apart = decode_table_teardown(&teardown(0x3268, 0x3278, true), "dtor").unwrap_err();
        assert!(
            apart.contains("clears 0x3268 and deletes 0x3278"),
            "{apart}"
        );

        let mut short = teardown(0x3270, 0x3278, true);
        short.truncate(short.len() - 2);
        assert!(decode_table_teardown(&short, "dtor").is_err());
    }

    fn range(code_first: bool) -> Vec<Instruction> {
        const CALL: [u8; 5] = [0xe8, 0, 0, 0, 0];
        let mut bytes = vec![0x41, 0x57, 0x41, 0x56, 0x53];
        bytes.extend([0x48, 0x89, 0xd3, 0x49, 0x89, 0xf6]);
        bytes.extend(CALL);
        if code_first {
            bytes.extend([0x49, 0x89, 0xc7]);
            bytes.extend(CALL);
            bytes.extend([0x41, 0x89, 0xc4]);
            bytes.extend(CALL);
            bytes.extend(CALL);
            bytes.extend([0x4d, 0x89, 0x3e]);
        } else {
            bytes.extend(CALL);
            bytes.extend(CALL);
            bytes.extend([0x41, 0x89, 0xc4]);
            bytes.extend(CALL);
            bytes.extend([0x49, 0x89, 0x06]);
        }
        bytes.extend([0x44, 0x89, 0xe0, 0x48, 0x89, 0x03]);
        bytes.extend([0x5b, 0x41, 0x5e, 0x41, 0x5f, RET]);
        decode(&bytes)
    }

    #[test]
    fn an_embedded_code_range_of_another_shape_is_refused() {
        let flow = follow(&range(true), "range").expect("a flow");
        let wrong = [(0x10, true), (0x18, true), (0x20, true), (0x28, false)];
        assert!(assign_blob(&flow, &wrong, "range").is_err());
        assert!(assign_blob(&flow, &wrong[..3], "range").is_err());

        let mut stray = range(true);
        stray.truncate(stray.len() - 6);
        stray.extend(decode(&[0x48, 0x89, 0x01, RET]));
        let refused = follow(&stray, "range").err().expect("refused");
        assert!(refused.contains("mov [rcx],rax"), "{refused}");

        let mut added = decode(&[0x48, 0x83, 0xc0, 0x01]);
        added.extend(range(true));
        assert!(follow(&added, "range").is_err());

        let flow = follow(&decode(&[RET]), "range").expect("a flow");
        assert!(assign_blob(&flow, &[], "range").is_err());
    }

    #[test]
    fn is_in_use_gives_the_offset_of_the_entries() {
        let official = decode(&[
            0x48, 0x8b, 0x87, 0xe0, 0xff, 0, 0, 0x48, 0x85, 0xc0, 0x0f, 0x95, 0xc0, RET,
        ]);
        assert_eq!(decode_entry_stack(&official, IS_IN_USE), Ok(0xffe0));

        let other = decode(&[
            0x48, 0x8b, 0x86, 0xe0, 0xff, 0, 0, 0x48, 0x85, 0xc0, 0x0f, 0x95, 0xc0, RET,
        ]);
        let refused = decode_entry_stack(&other, IS_IN_USE).unwrap_err();
        assert!(refused.contains("is `mov rax,[rsi+0FFE0h]`"), "{refused}");
        let sete = decode(&[
            0x48, 0x8b, 0x87, 0xe0, 0xff, 0, 0, 0x48, 0x85, 0xc0, 0x0f, 0x94, 0xc0, RET,
        ]);
        assert!(decode_entry_stack(&sete, IS_IN_USE).is_err());
        assert!(decode_entry_stack(&[], IS_IN_USE).is_err());
    }
}
