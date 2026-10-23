//! Where a V8 module keeps what its process's data says only at run time,
//! read from the code of the short accessors V8 compiles for it: the JS
//! dispatch table's base, from `ExternalReference::js_dispatch_table_address`
//! or, where no symbol names it, from the exported `v8::IsolateGroup`'s
//! `GetDefault` and destructor; and the embedded blob's bounds, from
//! `v8::Isolate::GetEmbeddedCodeRange`.
//! Each decoder accepts only the instruction shapes it names and says which
//! instruction it did not expect otherwise, never guessing at a variant.

use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, Instruction, MemorySize, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind, SymbolKind};

use crate::ImageAddress;
use crate::image::v8::{DispatchTable, EmbeddedBlob, V8Version};

/// What the name of `ExternalReference::js_dispatch_table_address` begins
/// with: 13.6's takes no argument and 14.6's an isolate.
const DISPATCH_TABLE_ADDRESS: &str =
    "_ZN2v88internal17ExternalReference25js_dispatch_table_address";

/// `v8::Isolate::GetEmbeddedCodeRange(void const**, unsigned long*)`, which
/// every build exports with V8's API.
pub(super) const GET_EMBEDDED_CODE_RANGE: &str = "_ZN2v87Isolate20GetEmbeddedCodeRangeEPPKvPm";

/// The exported `v8::IsolateGroup::GetDefault()`, which loads the default
/// group's pointer from its variable.
const GET_DEFAULT_GROUP: &str = "_ZN2v812IsolateGroup10GetDefaultEv";

/// The exported `v8::IsolateGroup::~IsolateGroup()`, by either of the
/// names of its one body, which releases the group it holds.
const GROUP_HANDLE_DESTRUCTORS: [&str; 2] =
    ["_ZN2v812IsolateGroupD1Ev", "_ZN2v812IsolateGroupD2Ev"];

/// The most bytes of an accessor's code decoded.
const ACCESSOR_BYTES: usize = 96;

/// The most bytes of the internal `IsolateGroup`'s destructor decoded:
/// the table's teardown is its second, within its first hundred bytes.
const DESTRUCTOR_BYTES: usize = 192;

/// `ExternalEntityTable::kSegmentSize`, 64 KiB, the size of the table's
/// first segment its teardown frees.
const SEGMENT_SIZE: u64 = 0x1_0000;

/// The address of the function symbol named `name`, or of the first whose
/// name begins with it when `prefix`.
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

/// The instructions at `address`, in a text section of `object`, from
/// which a function's prologue's `endbr64`, `push rbp`, and `mov rbp, rsp`
/// are left out, to the first `ret` or `jmp` within `ACCESSOR_BYTES`.
pub(super) fn instructions(
    object: &object::File<'_>,
    address: u64,
) -> Result<Vec<Instruction>, Arc<str>> {
    instructions_within(object, address, ACCESSOR_BYTES)
}

/// As `instructions`, within `limit` bytes.
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

/// Whether `instruction` is `mov rax, [base + displacement]`, a 64-bit
/// load with no index, and its displacement.
fn load_into_rax(instruction: &Instruction, base: Register) -> Option<u64> {
    load(instruction, Register::RAX, base)
}

/// Whether `instruction` is `mov to, [base + displacement]` with no index,
/// and its displacement.
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

/// Where the JS dispatch table of `version`'s module `object` is: none
/// before 13.6, whose functions hold their code. From 13.6 its
/// `ExternalReference::js_dispatch_table_address` says, a local function
/// that a library stripped of its local symbols, as Debian's libnode,
/// does not name; in 13.6, `exported_dispatch_table` then reads it from
/// exported code.
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

/// Where 13.6's JS dispatch table is, from code V8's API exports, for a
/// module that does not name `js_dispatch_table_address`. In 13.6 the
/// table is the `js_dispatch_table_` member of the one internal
/// `IsolateGroup` that `IsolateGroup::default_isolate_group_` points to,
/// and its base is the table's first word.
///
/// The variable is what `v8::IsolateGroup::GetDefault()` loads, as
/// `deps/v8/src/api/api.cc` and `src/init/isolate-group.h` of Node 24.22.1
/// say, by `decode_group_variable`:
///
/// ```c++
/// IsolateGroup IsolateGroup::GetDefault() {
///   return IsolateGroup(i::IsolateGroup::AcquireDefault());
/// }
/// static IsolateGroup* AcquireDefault() { return GetDefault()->Acquire(); }
/// V8_INLINE static IsolateGroup* GetDefault() { return default_isolate_group_; }
/// ```
///
/// The base's offset in the group is the first word the group's destructor
/// clears, reached from `v8::IsolateGroup::~IsolateGroup()` through
/// `i::IsolateGroup::Release()`'s `delete this` (`decode_handle_release`,
/// `decode_release`, `decode_deletion`). The destructor tears the table down
/// right after its page pool (`src/init/isolate-group.cc`):
///
/// ```c++
/// IsolateGroup::~IsolateGroup() {
///   ...
///   page_pool_->TearDown();
///
/// #ifdef V8_ENABLE_LEAPTIERING
///   js_dispatch_table_.TearDown();
/// #endif  // V8_ENABLE_LEAPTIERING
/// ```
///
/// which frees the table's first segment through `vas_`'s vtable
/// (`src/sandbox/external-entity-table-inl.h`), then clears `base_` before
/// deleting and clearing `vas_` (`src/common/segmented-table-inl.h`):
///
/// ```c++
///     this->vas_->FreePages(this->vas_->base(), kSegmentSize);
///   ...
///   Base::TearDown();
///
/// void SegmentedTable<Entry, size>::TearDown() {
///   base_ = nullptr;
///   delete vas_;
///   vas_ = nullptr;
/// }
/// ```
///
/// and `base_` is the word just before `vas_` (`src/common/segmented-table.h`):
///
/// ```c++
///   Entry* base_ = nullptr;
///   VirtualAddressSpace* vas_ = nullptr;
/// ```
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

/// The next of `code`, as `read` reads it, or the error that it is not of
/// the shape `read` accepts.
fn next_as<'a, T>(
    code: &mut impl Iterator<Item = &'a Instruction>,
    what: &str,
    read: impl FnOnce(&Instruction) -> Option<T>,
) -> Result<T, Arc<str>> {
    let next = code.next();
    next.and_then(read).ok_or_else(|| unexpected(what, next))
}

/// Whether `instruction` writes no register but rsp, or writes a 64-bit
/// register other than `held`: a push, `sub rsp, n`, a store to the
/// frame, a move between registers, or a load of the stack protector's
/// value from `fs:0x28`.
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

/// The variable `GetDefault`'s `instructions` load the group's pointer
/// from: `mov r, [rip + variable]`, or `lea r, [rip + variable]` and `mov
/// r, [r]`, then `Acquire`'s `lock add dword [r], 1`. Only what `keeps`
/// accepts may come before or between them.
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

/// Whether `instruction` is `lock add dword [group], 1`, an increment of
/// the reference count that begins `IsolateGroup`.
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

/// The target of `instruction` if it is a near branch of `mnemonic`.
fn branch(instruction: &Instruction, mnemonic: Mnemonic) -> Option<u64> {
    (instruction.mnemonic() == mnemonic && instruction.op0_kind() == OpKind::NearBranch64)
        .then(|| instruction.near_branch_target())
}

/// Whether `instruction` is `test register, register`.
fn tests(instruction: &Instruction, register: Register) -> bool {
    instruction.mnemonic() == Mnemonic::Test
        && instruction.op0_kind() == OpKind::Register
        && instruction.op1_kind() == OpKind::Register
        && instruction.op0_register() == register
        && instruction.op1_register() == register
}

/// The value of `instruction` if it is `mov register, immediate`.
fn moves_immediate(instruction: &Instruction, register: Register) -> Option<u64> {
    (instruction.mnemonic() == Mnemonic::Mov
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == register
        && instruction.op1_kind() == OpKind::Immediate32)
        .then(|| instruction.immediate(1))
}

/// The displacement of `instruction` if it is `mov qword [base +
/// displacement], 0`.
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

/// Whether `instruction` calls a virtual function through the vtable in
/// rax: `call [rax + slot]`.
fn calls_virtual(instruction: &Instruction) -> bool {
    instruction.mnemonic() == Mnemonic::Call
        && instruction.op0_kind() == OpKind::Memory
        && instruction.memory_base() == Register::RAX
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None
}

/// Where `v8::IsolateGroup::~IsolateGroup`'s `instructions` tail-call the
/// group's `Release`: `mov rdi, [rdi]` (`isolate_group_`), `test rdi,
/// rdi`, `je`, and `jmp Release`.
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

/// Where `Release`'s `instructions` go to delete the group once its count
/// reaches zero: `lock sub dword [rdi], 1` and `je`.
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

/// The destructor `delete this`'s `instructions` call and the size they
/// free: `call destructor`, then `mov esi, size` and `jmp` to `operator
/// delete`. Only what `keeps` accepts may come before the call, and after
/// it only moves of registers, loads from the frame, and `leave`.
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

/// The offset of the dispatch table's base in the group, from the
/// destructor's `instructions`: after its prologue, which may keep `this`
/// in a register a call preserves, `page_pool_->TearDown()` as `mov rdi,
/// [this + page_pool_]` and `call`, then the table's teardown, exactly:
///
/// ```text
/// mov rdi, [this + vas_]
/// mov edx, 0x10000          ; kSegmentSize
/// mov rax, [rdi]
/// mov rsi, [rdi + base]     ; vas_->base()
/// call [rax + FreePages]
/// mov rdi, [this + vas_]
/// mov qword [this + base_], 0
/// test rdi, rdi
/// je the last instruction
/// mov rax, [rdi]
/// call [rax + deleting destructor]
/// mov qword [this + vas_], 0
/// ```
///
/// with `base_ + 8 == vas_`.
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

/// The dispatch table an accessor's `instructions` return the base of:
/// `mov rax, [rdi + offset]` from the isolate it is passed; or the
/// isolate group's pointer variable loaded, by `mov rax, [rip + group]` or
/// by `lea rax, [rip + group]` and `mov rax, [rax]`, then `mov rax,
/// [rax + offset]`. Nothing after may write rax before it returns.
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

/// Checks that the rest of an accessor returns without writing rax, as
/// one that checks its result and calls a fatal error otherwise does.
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

/// What a register of `GetEmbeddedCodeRange` holds, as far as its decoder
/// follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    Unknown,

    /// Where the blob's code is stored: the first argument, rsi.
    CodeOut,

    /// Where its code's size is stored: the second, rdx.
    SizeOut,

    /// What the call at this index of the calls returned.
    Returned(usize),
}

/// The registers a call may change, by the System V ABI.
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

/// What `GetEmbeddedCodeRange` stores and calls: the targets of its calls,
/// and which call's result it stores as the code and which as its size.
struct Flow {
    calls: Vec<u64>,
    code: Option<Value>,
    size: Option<Value>,
}

/// Follows `GetEmbeddedCodeRange`'s `instructions` from its arguments to
/// its stores, through moves, calls, and pushes and pops of callee-saved
/// registers; any other instruction is refused.
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
            (Mnemonic::Ret, _) => return Ok(flow),
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

/// What each general-purpose register holds, by its 64-bit register.
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
        if register.is_gpr64()
            && let Some(slot) = self.0.get_mut(register.number())
        {
            *slot = value;
        }
    }
}

/// Records a store `mov [base], source` of `GetEmbeddedCodeRange`
/// through one of its arguments.
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

/// The variable an embedded blob accessor at `address` returns, and
/// whether it is 64 bits wide, a pointer, rather than 32, a size: `mov
/// rax|eax, [rip + variable]`, then `pop rbp` and zeroing of other
/// registers at most, and `ret`.
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

/// Where `object`'s embedded blob's bounds are kept, from the code of its
/// `v8::Isolate::GetEmbeddedCodeRange`: by data flow, the call whose result
/// it stores through its first argument returns the code, and the one
/// through its second the code's size. Of its two other calls, the one
/// returning a pointer returns the data and the other the data's size.
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

/// The blob's variables, from the calls `flow` stores the results of and
/// the variables and widths of the `accessors` they call.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Instruction> {
        let mut decoder = Decoder::with_ip(64, bytes, 0x1000, DecoderOptions::NONE);
        std::iter::from_fn(|| decoder.can_decode().then(|| decoder.decode())).collect()
    }

    const RET: u8 = 0xc3;

    #[test]
    fn each_shape_of_dispatch_table_accessor_decodes() {
        // mov rax, [rdi + 0x268]; lea rcx, [rax - 1]; cmp rcx, 0x43; jbe; ret
        let isolate = decode(&[
            0x48, 0x8b, 0x87, 0x68, 0x02, 0, 0, 0x48, 0x8d, 0x48, 0xff, 0x48, 0x83, 0xf9, 0x43,
            0x76, 0x01, RET,
        ]);
        assert_eq!(
            decode_dispatch_table(&isolate, "isolate"),
            Ok(DispatchTable::Isolate { offset: 0x268 })
        );

        // mov rax, [rip + 0x100]; mov rax, [rax + 0x3270]; ret
        let loaded = decode(&[
            0x48, 0x8b, 0x05, 0, 1, 0, 0, 0x48, 0x8b, 0x80, 0x70, 0x32, 0, 0, RET,
        ]);
        assert_eq!(
            decode_dispatch_table(&loaded, "group"),
            Ok(DispatchTable::Group {
                group: ImageAddress::new(0x1107),
                offset: 0x3270
            })
        );

        // lea rax, [rip + 0x100]; mov rax, [rax]; mov rax, [rax + 0x3268];
        // xor edx, edx; ret
        let addressed = decode(&[
            0x48, 0x8d, 0x05, 0, 1, 0, 0, 0x48, 0x8b, 0x00, 0x48, 0x8b, 0x80, 0x68, 0x32, 0, 0,
            0x31, 0xd2, RET,
        ]);
        assert_eq!(
            decode_dispatch_table(&addressed, "group"),
            Ok(DispatchTable::Group {
                group: ImageAddress::new(0x1107),
                offset: 0x3268
            })
        );
    }

    #[test]
    fn a_dispatch_table_accessor_of_another_shape_is_refused_by_its_instruction() {
        // A further load of the table's base: mov rax, [rax].
        let loads_again = decode(&[0x48, 0x8b, 0x87, 0x68, 0x02, 0, 0, 0x48, 0x8b, 0x00, RET]);
        let refused = decode_dispatch_table(&loads_again, "accessor").unwrap_err();
        assert!(refused.contains("is `mov rax,[rax]`"), "{refused}");

        // Another register: mov rcx, [rdi + 8].
        let elsewhere = decode(&[0x48, 0x8b, 0x4f, 0x08, RET]);
        assert!(decode_dispatch_table(&elsewhere, "accessor").is_err());

        // A group loaded with no offset after it.
        let short = decode(&[0x48, 0x8b, 0x05, 0, 1, 0, 0, RET]);
        assert!(decode_dispatch_table(&short, "accessor").is_err());
        assert!(decode_dispatch_table(&[], "accessor").is_err());
    }

    #[test]
    fn each_shape_of_get_default_loads_the_groups_variable() {
        // Debian: push rbx; mov rbx, rdi; sub rsp, 0x18; lea rax, [rip +
        // 0x100]; mov rax, [rax]; lock add dword [rax], 1
        let debian = decode(&[
            0x53, 0x48, 0x89, 0xfb, 0x48, 0x83, 0xec, 0x18, 0x48, 0x8d, 0x05, 0, 1, 0, 0, 0x48,
            0x8b, 0x00, 0xf0, 0x83, 0x00, 0x01,
        ]);
        assert_eq!(decode_group_variable(&debian, "debian"), Ok(0x110f));

        // nixpkgs: push rbx; sub rsp, 0x18; lea rax, [rip + 0x100]; mov
        // rbx, fs:0x28; mov [rbp - 0x18], rbx; mov rbx, rdi; mov rax, [rax];
        // lock add dword [rax], 1
        let nixpkgs = decode(&[
            0x53, 0x48, 0x83, 0xec, 0x18, 0x48, 0x8d, 0x05, 0, 1, 0, 0, 0x64, 0x48, 0x8b, 0x1c,
            0x25, 0x28, 0, 0, 0, 0x48, 0x89, 0x5d, 0xe8, 0x48, 0x89, 0xfb, 0x48, 0x8b, 0x00, 0xf0,
            0x83, 0x00, 0x01,
        ]);
        assert_eq!(decode_group_variable(&nixpkgs, "nixpkgs"), Ok(0x110c));

        // Official: mov rdx, [rip + 0x100]; mov rax, rdi; lock add dword
        // [rdx], 1; mov [rdi], rdx; ret
        let official = decode(&[
            0x48, 0x8b, 0x15, 0, 1, 0, 0, 0x48, 0x89, 0xf8, 0xf0, 0x83, 0x02, 0x01, 0x48, 0x89,
            0x17, RET,
        ]);
        assert_eq!(decode_group_variable(&official, "official"), Ok(0x1107));
    }

    #[test]
    fn a_get_default_of_another_shape_is_refused_by_its_instruction() {
        // The group's register overwritten before its load: mov rax, rdi.
        let overwritten = decode(&[
            0x48, 0x8d, 0x05, 0, 1, 0, 0, 0x48, 0x89, 0xf8, 0x48, 0x8b, 0x00, 0xf0, 0x83, 0x00,
            0x01,
        ]);
        let refused = decode_group_variable(&overwritten, "get").unwrap_err();
        assert!(refused.contains("is `mov rax,rdi`"), "{refused}");

        // Another register incremented: lock add dword [rcx], 1.
        let elsewhere = decode(&[0x48, 0x8b, 0x15, 0, 1, 0, 0, 0xf0, 0x83, 0x01, 0x01, RET]);
        assert!(decode_group_variable(&elsewhere, "get").is_err());

        // A group loaded and never acquired.
        let unacquired = decode(&[0x48, 0x8b, 0x15, 0, 1, 0, 0, RET]);
        assert!(decode_group_variable(&unacquired, "get").is_err());
    }

    #[test]
    fn the_group_handles_destructor_leads_to_the_groups_deletion() {
        // mov rdi, [rdi]; test rdi, rdi; je; jmp Release
        let handle = decode(&[
            0x48, 0x8b, 0x3f, 0x48, 0x85, 0xff, 0x74, 0x08, 0xe9, 0, 0, 0, 0,
        ]);
        assert_eq!(decode_handle_release(&handle, "handle"), Ok(0x100d));

        // lock sub dword [rdi], 1; je deletion; ret
        let release = decode(&[0xf0, 0x83, 0x2f, 0x01, 0x74, 0x0a, RET]);
        assert_eq!(decode_release(&release, "release"), Ok(0x1010));

        // Debian: sub rsp, 0x10; mov [rbp - 8], rdi; call destructor; mov
        // rdi, [rbp - 8]; mov esi, 0x3280; leave; jmp operator delete
        let debian = decode(&[
            0x48, 0x83, 0xec, 0x10, 0x48, 0x89, 0x7d, 0xf8, 0xe8, 0, 0, 0, 0, 0x48, 0x8b, 0x7d,
            0xf8, 0xbe, 0x80, 0x32, 0, 0, 0xc9, 0xe9, 0, 0, 0, 0,
        ]);
        assert_eq!(decode_deletion(&debian, "delete"), Ok((0x100d, 0x3280)));

        // Official: push rbx; mov rbx, rdi; sub rsp, 8; call destructor;
        // mov rdi, rbx; mov rbx, [rbp - 8]; mov esi, 0x3280; leave; jmp
        let official = decode(&[
            0x53, 0x48, 0x89, 0xfb, 0x48, 0x83, 0xec, 0x08, 0xe8, 0, 0, 0, 0, 0x48, 0x89, 0xdf,
            0x48, 0x8b, 0x5d, 0xf8, 0xbe, 0x80, 0x32, 0, 0, 0xc9, 0xe9, 0, 0, 0, 0,
        ]);
        assert_eq!(decode_deletion(&official, "delete"), Ok((0x100d, 0x3280)));

        // No size freed before the jump.
        let no_size = decode(&[0xe8, 0, 0, 0, 0, 0xc9, 0xe9, 0, 0, 0, 0]);
        assert!(decode_deletion(&no_size, "delete").is_err());
        // A release that is not a decrement: lock add dword [rdi], 1.
        let added = decode(&[0xf0, 0x83, 0x07, 0x01, 0x74, 0x0a, RET]);
        assert!(decode_release(&added, "release").is_err());
    }

    /// The internal `IsolateGroup`'s destructor, as Debian's begins, with
    /// the table's `base_` and `vas_` at `base` and `vas`, and `base_`
    /// cleared before `vas_` is deleted or, if not `in_order`, after.
    fn teardown(base: u16, vas: u16, in_order: bool) -> Vec<Instruction> {
        let [base_low, base_high] = base.to_le_bytes();
        let [vas_low, vas_high] = vas.to_le_bytes();
        // push rbx; mov rbx, rdi; sub rsp, 0x18; mov rdi, [rdi + 0x10];
        // call PagePool::TearDown
        let mut bytes = vec![
            0x53, 0x48, 0x89, 0xfb, 0x48, 0x83, 0xec, 0x18, 0x48, 0x8b, 0x7f, 0x10, 0xe8, 0, 0, 0,
            0,
        ];
        let load_vas = [0x48, 0x8b, 0xbb, vas_low, vas_high, 0, 0]; // mov rdi, [rbx + vas]
        bytes.extend(load_vas);
        bytes.extend([0xba, 0, 0, 1, 0]); // mov edx, 0x10000
        bytes.extend([0x48, 0x8b, 0x07, 0x48, 0x8b, 0x77, 0x18]); // mov rax, [rdi]; mov rsi, [rdi + 0x18]
        bytes.extend([0xff, 0x50, 0x28]); // call [rax + 0x28]
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
        bytes.extend([0x48, 0x8b, 0x7b, 0x20]); // mov rdi, [rbx + 0x20]: what follows
        decode(&bytes)
    }

    #[test]
    fn the_tables_base_is_the_word_its_teardown_clears() {
        assert_eq!(
            decode_table_teardown(&teardown(0x3270, 0x3278, true), "dtor"),
            Ok(0x3270)
        );
        assert_eq!(
            decode_table_teardown(&teardown(0x3268, 0x3270, true), "dtor"),
            Ok(0x3268)
        );
    }

    #[test]
    fn a_teardown_of_another_shape_is_refused() {
        // base_ cleared only after vas_ is deleted.
        let late = decode_table_teardown(&teardown(0x3270, 0x3278, false), "dtor").unwrap_err();
        assert!(late.contains("is `test rdi,rdi`"), "{late}");

        // A word cleared that is not the one just before vas_.
        let apart = decode_table_teardown(&teardown(0x3268, 0x3278, true), "dtor").unwrap_err();
        assert!(
            apart.contains("clears 0x3268 and deletes 0x3278"),
            "{apart}"
        );

        // Cut short before vas_ is cleared.
        let mut short = teardown(0x3270, 0x3278, true);
        short.truncate(short.len() - 2);
        assert!(decode_table_teardown(&short, "dtor").is_err());
    }

    /// Node 26's `GetEmbeddedCodeRange`, whose calls' results reach its
    /// stores through other registers, and one that stores in the other
    /// order, as Node 24's does.
    fn range(code_first: bool) -> Vec<Instruction> {
        const CALL: [u8; 5] = [0xe8, 0, 0, 0, 0];
        let mut bytes = vec![0x41, 0x57, 0x41, 0x56, 0x53];
        bytes.extend([0x48, 0x89, 0xd3, 0x49, 0x89, 0xf6]); // mov rbx, rdx; mov r14, rsi
        bytes.extend(CALL);
        if code_first {
            bytes.extend([0x49, 0x89, 0xc7]); // mov r15, rax
            bytes.extend(CALL);
            bytes.extend([0x41, 0x89, 0xc4]); // mov r12d, eax
            bytes.extend(CALL);
            bytes.extend(CALL);
            bytes.extend([0x4d, 0x89, 0x3e]); // mov [r14], r15
        } else {
            bytes.extend(CALL);
            bytes.extend(CALL);
            bytes.extend([0x41, 0x89, 0xc4]); // mov r12d, eax
            bytes.extend(CALL);
            bytes.extend([0x49, 0x89, 0x06]); // mov [r14], rax
        }
        bytes.extend([0x44, 0x89, 0xe0, 0x48, 0x89, 0x03]); // mov eax, r12d; mov [rbx], rax
        bytes.extend([0x5b, 0x41, 0x5e, 0x41, 0x5f, RET]);
        decode(&bytes)
    }

    #[test]
    fn the_embedded_blob_is_found_by_where_each_result_is_stored() {
        let pointer = |at: u64| (at, true);
        let size = |at: u64| (at, false);

        let flow = follow(&range(true), "range").expect("a flow");
        assert_eq!(
            (flow.code, flow.size),
            (Some(Value::Returned(0)), Some(Value::Returned(1)))
        );
        let accessors = [pointer(0x10), size(0x18), pointer(0x20), size(0x28)];
        let blob = assign_blob(&flow, &accessors, "range").expect("a blob");
        assert_eq!(
            [blob.code, blob.code_size, blob.data, blob.data_size].map(ImageAddress::get),
            [0x10, 0x18, 0x20, 0x28]
        );

        let flow = follow(&range(false), "range").expect("a flow");
        assert_eq!(
            (flow.code, flow.size),
            (Some(Value::Returned(3)), Some(Value::Returned(2)))
        );
        let accessors = [size(0x28), pointer(0x20), size(0x18), pointer(0x10)];
        let blob = assign_blob(&flow, &accessors, "range").expect("a blob");
        assert_eq!(
            [blob.code, blob.code_size, blob.data, blob.data_size].map(ImageAddress::get),
            [0x10, 0x18, 0x20, 0x28]
        );
    }

    #[test]
    fn an_embedded_code_range_of_another_shape_is_refused() {
        // The code's size stored from a pointer's accessor.
        let flow = follow(&range(true), "range").expect("a flow");
        let wrong = [(0x10, true), (0x18, true), (0x20, true), (0x28, false)];
        assert!(assign_blob(&flow, &wrong, "range").is_err());
        assert!(assign_blob(&flow, &wrong[..3], "range").is_err());

        // A store through a register no argument reached: mov [rcx], rax.
        let mut stray = range(true);
        stray.truncate(stray.len() - 6);
        stray.extend(decode(&[0x48, 0x89, 0x01, RET]));
        let refused = follow(&stray, "range").err().expect("refused");
        assert!(refused.contains("mov [rcx],rax"), "{refused}");

        // An instruction the decoder does not follow: add rax, 1.
        let mut added = decode(&[0x48, 0x83, 0xc0, 0x01]);
        added.extend(range(true));
        assert!(follow(&added, "range").is_err());

        // Returning before storing either.
        let flow = follow(&decode(&[RET]), "range").expect("a flow");
        assert!(assign_blob(&flow, &[], "range").is_err());
    }
}
