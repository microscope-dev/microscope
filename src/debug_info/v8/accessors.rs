//! Where a V8 module keeps what its process's data says only at run time,
//! read from the code of the short accessors V8 compiles for it: the JS
//! dispatch table's base, from `ExternalReference::js_dispatch_table_address`,
//! and the embedded blob's bounds, from `v8::Isolate::GetEmbeddedCodeRange`.
//! Each decoder accepts only the instruction shapes it names and says which
//! instruction it did not expect otherwise, never guessing at a variant.

use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, Instruction, Mnemonic, OpKind, Register};
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

/// The most bytes of an accessor's code decoded.
const ACCESSOR_BYTES: usize = 96;

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
/// are left out.
fn instructions(object: &object::File<'_>, address: u64) -> Result<Vec<Instruction>, Arc<str>> {
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
            data.get(at..data.len().min(at + ACCESSOR_BYTES))
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
        if instruction.mnemonic() == Mnemonic::Ret {
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
    (instruction.mnemonic() == Mnemonic::Mov
        && instruction.op_count() == 2
        && instruction.op0_kind() == OpKind::Register
        && instruction.op0_register() == Register::RAX
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
/// does not name.
pub(super) fn dispatch_table(
    object: &object::File<'_>,
    version: V8Version,
) -> Result<Option<DispatchTable>, Arc<str>> {
    if (version.major, version.minor) < super::LEAPTIERING {
        return Ok(None);
    }

    let address = function(object, DISPATCH_TABLE_ADDRESS, true).ok_or_else(|| -> Arc<str> {
        format!(
            "V8 {version} names no ExternalReference::js_dispatch_table_address, whose code \
             says where its JS dispatch table is"
        )
        .into()
    })?;
    let what = format!("V8 {version}'s ExternalReference::js_dispatch_table_address");
    decode_dispatch_table(&instructions(object, address)?, &what).map(Some)
}

/// The dispatch table an accessor's `instructions` return the base of:
/// `mov rax, [rdi + offset]` from the isolate it is passed; or the
/// isolate group's pointer variable loaded, by `mov rax, [rip + group]` or
/// by `lea rax, [rip + group]` and `mov rax, [rax]`, then `mov rax,
/// [rax + offset]`. Nothing after may write rax before it returns.
fn decode_dispatch_table(
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
