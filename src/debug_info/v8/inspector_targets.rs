use std::sync::Arc;

use iced_x86::{FlowControl, InstructionInfoFactory, Mnemonic, OpAccess, OpKind, Register};

use crate::ImageAddress;
use crate::image::v8::InspectorTargets;

const GET: &str = "_ZN4node9inspector19InspectorIoDelegate12GetTargetIdsB5cxx11Ev";
const START: &str = "_ZN4node9inspector21InspectorSocketServer5StartEv";
const END: &str = "_ZN4node9inspector21InspectorSocketServer17SessionTerminatedEi";
const LIST: &str = "_ZN4node9inspector21InspectorSocketServer16SendListResponseEPNS0_15InspectorSocketERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEPNS0_13SocketSessionE";
const EXISTS: &str = "_ZN4node9inspector21InspectorSocketServer12TargetExistsERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEE";

fn body(object: &object::File<'_>, name: &str) -> Option<Vec<iced_x86::Instruction>> {
    let (address, size) = super::named_function(object, name)?;
    let size = usize::try_from(size)
        .ok()
        .filter(|size| (1..=65536).contains(size))?;
    super::instructions(super::image_bytes(object, address, size, true)?, address)
}

fn caller(object: &object::File<'_>, name: &str) -> Option<ImageAddress> {
    let body = body(object, name)?;
    let mut calls = body.iter().filter(|instruction| {
        // SocketServerDelegate declares GetTargetIds as its fifth virtual.
        instruction.code() == iced_x86::Code::Call_rm64
            && instruction.op0_kind() == OpKind::Memory
            && instruction.memory_base().is_gpr64()
            && instruction.memory_index() == Register::None
            && instruction.segment_prefix() == Register::None
            && instruction.memory_displacement64() == 32
    });
    let call = calls.next()?;
    calls
        .next()
        .is_none()
        .then(|| ImageAddress::new(call.next_ip()))
}

fn prologue(instruction: &iced_x86::Instruction) -> bool {
    let register =
        |at| instruction.op_kind(at) == OpKind::Register && instruction.op_register(at).is_gpr64();
    let stack = matches!(instruction.memory_base(), Register::RBP | Register::RSP)
        && instruction.memory_index() == Register::None
        && instruction.segment_prefix() == Register::None;
    match instruction.mnemonic() {
        Mnemonic::Endbr64 => true,
        Mnemonic::Push => register(0),
        Mnemonic::Sub => {
            instruction.op0_register() == Register::RSP
                && matches!(
                    instruction.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                )
        }
        Mnemonic::Lea => register(0) && stack,
        Mnemonic::Mov => {
            (register(0) && register(1))
                || (instruction.op0_kind() == OpKind::Memory && stack && register(1))
                || (register(0)
                    && instruction.segment_prefix() == Register::FS
                    && instruction.memory_base() == Register::None
                    && instruction.memory_index() == Register::None
                    && instruction.memory_displacement64() == 40)
        }
        _ => false,
    }
}

fn source(object: &object::File<'_>) -> Option<(ImageAddress, u64)> {
    let body = body(object, GET)?;
    let mut loads = Vec::new();
    let mut factory = InstructionInfoFactory::new();
    for instruction in &body {
        if instruction.flow_control() != FlowControl::Next {
            return None;
        }
        // Before the first branch/call, rsi still denotes the entry delegate.
        if factory
            .info(instruction)
            .used_registers()
            .iter()
            .any(|used| {
                used.register().full_register() == Register::RSI
                    && matches!(
                        used.access(),
                        OpAccess::Write
                            | OpAccess::ReadWrite
                            | OpAccess::CondWrite
                            | OpAccess::ReadCondWrite
                    )
            })
        {
            return None;
        }
        if instruction.is_ip_rel_memory_operand() || instruction.memory_base() != Register::RSI {
            if !prologue(instruction) {
                return None;
            }
            continue;
        }
        if instruction.mnemonic() != Mnemonic::Mov
            || instruction.op0_kind() != OpKind::Register
            || !instruction.op0_register().is_gpr64()
            || instruction.op1_kind() != OpKind::Memory
            || instruction.memory_index() != Register::None
            || instruction.segment_prefix() != Register::None
        {
            return None;
        }
        loads.push(instruction.memory_displacement64());
        if loads.len() == 2 {
            loads.sort_unstable();
            return (loads[0] <= 4096 && loads[0].checked_add(8) == Some(loads[1]))
                .then(|| (ImageAddress::new(body[0].ip()), loads[0]));
        }
    }
    None
}

pub(super) fn layout(object: &object::File<'_>) -> Result<InspectorTargets, Arc<str>> {
    let (entry, source) =
        source(object).ok_or("Node's immutable target source cannot be proven")?;
    let hidden = [START, END, LIST]
        .map(|name| caller(object, name).ok_or("Node's target listing caller cannot be proven"));
    let [start, end, list] = hidden;
    Ok(InspectorTargets {
        entry,
        source,
        hidden: [start?, end?, list?],
        upgrade: caller(object, EXISTS)
            .ok_or("Node's target validation caller cannot be proven")?,
    })
}

#[cfg(test)]
mod tests {
    use object::{Object, ObjectSection};

    use super::*;

    #[test]
    fn every_target_caller_and_source_is_proven_and_mutations_refuse() {
        let root = std::env::var_os("MICROSCOPE_NODE").expect("pinned Node builds");
        for entry in [
            "22",
            "24",
            "26",
            "nixpkgs-22",
            "nixpkgs-24",
            "nixpkgs-26",
            "debian-24",
        ] {
            let directory = std::path::Path::new(&root).join(entry);
            let path = directory.join(if entry == "debian-24" {
                "root/usr/lib/x86_64-linux-gnu/libnode.so.137"
            } else {
                "node"
            });
            let mut bytes = std::fs::read(path).unwrap();
            let object = object::File::parse(bytes.as_slice()).unwrap();
            let layout = layout(&object).unwrap_or_else(|why| panic!("{entry}: {why}"));
            let file_offset = |address: u64| {
                object
                    .sections()
                    .find_map(|section| {
                        let offset = address.checked_sub(section.address())?;
                        if offset >= section.size() {
                            return None;
                        }
                        let (file, _) = section.file_range()?;
                        usize::try_from(file + offset).ok()
                    })
                    .unwrap()
            };
            let source = body(&object, GET)
                .unwrap()
                .into_iter()
                .find(|instruction| instruction.memory_base() == Register::RSI)
                .unwrap();
            let mutations = layout
                .hidden
                .into_iter()
                .chain([layout.upgrade])
                .map(|after| file_offset(after.get() - 1));
            let mutations: [usize; 7] = mutations
                .chain([
                    file_offset(source.next_ip() - 1),
                    file_offset(layout.entry.get()),
                    file_offset(source.ip()),
                ])
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            for (index, offset) in mutations.into_iter().enumerate() {
                let original = bytes[offset];
                bytes[offset] = match index {
                    5 => 0x75,          // A divergent prefix cannot prove unconditional loads.
                    6 => original & !8, // A 32-bit load is not the source pointer.
                    _ => original ^ 4,
                };
                let object = object::File::parse(bytes.as_slice()).unwrap();
                assert!(super::layout(&object).is_err(), "{entry}: mutation {index}");
                bytes[offset] = original;
            }
        }
    }
}
