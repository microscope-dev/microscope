use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, ObjectSymbolTable, SectionFlags, SymbolKind};

use crate::ImageAddress;

const START: &str = "_ZN4node9inspector5Agent5StartERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEERKNS_12DebugOptionsESt10shared_ptrINS_15ExclusiveAccessINS_8HostPortENS_9MutexBaseINS_16LibuvMutexTraitsEEEEEEb";
const REGISTER: &str = "_ZN4node21RegisterSignalHandlerEiPFviP9siginfo_tPvEb";
const REQUEST: &str = "_ZN4node9inspector5Agent20RequestIoThreadStartEv";
const REGISTRIES: [(&str, bool); 4] = [
    (
        "_ZN4node9inspector12_GLOBAL__N_1L22start_io_thread_agentsE",
        true,
    ),
    (
        "_ZN4node9inspector12_GLOBAL__N_122start_io_thread_agentsE",
        true,
    ),
    (
        "_ZN4node9inspector12_GLOBAL__N_1L21start_io_thread_asyncE",
        false,
    ),
    (
        "_ZN4node9inspector12_GLOBAL__N_121start_io_thread_asyncE",
        false,
    ),
];
const HANDLERS: [&str; 2] = [
    "_ZN4node9inspector12_GLOBAL__N_1L19StartIoThreadWakeupEiP9siginfo_tPv",
    "_ZN4node9inspector12_GLOBAL__N_119StartIoThreadWakeupEiP9siginfo_tPv",
];
const UNSUPPORTED: &str = "Node's inspector activation instructions are unsupported";

// Only relative call and RIP operands vary in this supported watchdog body.
// Its branches, pointer stride, callee-saved registers, and all writes are checked.
const WATCHDOG: &[u8] = &[
    0x55, 0x48, 0x8d, 0x3d, 0xc7, 0x95, 0x20, 0x03, 0x48, 0x89, 0xe5, 0x41, 0x55, 0x4c, 0x8d, 0x2d,
    0x9c, 0x90, 0xdf, 0x03, 0x41, 0x54, 0x53, 0x48, 0x83, 0xec, 0x08, 0xe8, 0xe0, 0xc5, 0xb2, 0xff,
    0x48, 0x8d, 0x3d, 0xf9, 0x90, 0xdf, 0x03, 0xe8, 0x44, 0xab, 0xb3, 0xff, 0x48, 0x8d, 0x3d, 0xad,
    0x90, 0xdf, 0x03, 0xe8, 0xb8, 0x46, 0xb3, 0xff, 0x49, 0x8b, 0x5d, 0x00, 0x4d, 0x8b, 0x65, 0x08,
    0x4c, 0x39, 0xe3, 0x74, 0x1c, 0x66, 0x66, 0x2e, 0x0f, 0x1f, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x48, 0x8b, 0x3b, 0x48, 0x83, 0xc3, 0x08, 0xe8, 0x44, 0x8b, 0xb3, 0xff, 0x49, 0x39, 0xdc, 0x75,
    0xef, 0x48, 0x8d, 0x3d, 0x78, 0x90, 0xdf, 0x03, 0xe8, 0xb3, 0x29, 0xb2, 0xff, 0xeb, 0xb1,
];
const HANDLER: &[u8] = &[
    0x48, 0x8d, 0x3d, 0x59, 0x9d, 0xdf, 0x03, 0xe9, 0x34, 0x9e, 0xb2, 0xff,
];
const APPEND: &[u8] = &[
    0x48, 0x8b, 0x05, 0xdb, 0x42, 0xdf, 0x03, 0x48, 0x8b, 0x0d, 0xdc, 0x42, 0xdf, 0x03, 0x48, 0x39,
    0xc8, 0x0f, 0x84, 0x7b, 0x05, 0, 0, 0x48, 0x89, 0x18, 0x48, 0x83, 0x05, 0xc0, 0x42, 0xdf, 0x03,
    0x08,
];

/// Static identities only; compare the live signal action before delivering SIGUSR1.
pub(super) fn activation(
    object: &object::File<'_>,
) -> Result<(ImageAddress, bool, ImageAddress), Arc<str>> {
    if object.architecture() != object::Architecture::X86_64 || !object.is_little_endian() {
        return Err(UNSUPPORTED.into());
    }
    if let Some(result) = named(object)? {
        return Ok(result);
    }
    fallback(object)
}

fn symbol(object: &object::File<'_>, name: &str) -> Result<Option<(u64, u64)>, Arc<str>> {
    let mut found = None;
    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if symbol.is_definition() && symbol.name().ok() == Some(name) {
            let value = (symbol.address(), symbol.size());
            if found.is_some_and(|previous| previous != value) {
                return Err("Node's inspector activation symbols disagree".into());
            }
            found = Some(value);
        }
    }
    Ok(found)
}

fn named(
    object: &object::File<'_>,
) -> Result<Option<(ImageAddress, bool, ImageAddress)>, Arc<str>> {
    let mut registry = None;
    for (name, vector) in REGISTRIES {
        if let Some((address, _)) = symbol(object, name)? {
            let candidate = (address, vector);
            if registry.is_some_and(|previous| previous != candidate) {
                return Err("Node's inspector registry symbols disagree".into());
            }
            registry = Some(candidate);
        }
    }
    let mut handler = None;
    for name in HANDLERS {
        if let Some((address, _)) = symbol(object, name)? {
            if handler.is_some_and(|previous| previous != address) {
                return Err("Node's inspector signal handler symbols disagree".into());
            }
            handler = Some(address);
        }
    }
    let (Some((registry, vector)), Some(handler)) = (registry, handler) else {
        return Ok(None);
    };
    writable(object, registry, if vector { 24 } else { 8 })?;
    text(object, handler, 1)?;
    Ok(Some((
        ImageAddress::new(registry),
        vector,
        ImageAddress::new(handler),
    )))
}

fn flags<'a>(section: &impl ObjectSection<'a>, required: u64, forbidden: u64) -> bool {
    matches!(section.flags(), SectionFlags::Elf { sh_flags }
        if sh_flags & required == required && sh_flags & forbidden == 0)
}

fn text<'a>(
    object: &'a object::File<'_>,
    address: u64,
    length: usize,
) -> Result<&'a [u8], Arc<str>> {
    let length64 = u64::try_from(length).map_err(|_| Arc::<str>::from(UNSUPPORTED))?;
    address.checked_add(length64).ok_or(UNSUPPORTED)?;
    object
        .sections()
        .find_map(|section| {
            if !flags(
                &section,
                u64::from(object::elf::SHF_ALLOC | object::elf::SHF_EXECINSTR),
                u64::from(object::elf::SHF_WRITE),
            ) {
                return None;
            }
            let offset = usize::try_from(address.checked_sub(section.address())?).ok()?;
            section
                .data()
                .ok()?
                .get(offset..offset.checked_add(length)?)
        })
        .ok_or_else(|| {
            "Node's inspector activation code is outside immutable executable sections".into()
        })
}

fn writable(object: &object::File<'_>, address: u64, length: u64) -> Result<(), Arc<str>> {
    let end = address.checked_add(length).ok_or(UNSUPPORTED)?;
    if address.is_multiple_of(8)
        && object.sections().any(|section| {
            flags(
                &section,
                u64::from(object::elf::SHF_ALLOC | object::elf::SHF_WRITE),
                u64::from(object::elf::SHF_EXECINSTR),
            ) && address >= section.address()
                && section
                    .address()
                    .checked_add(section.size())
                    .is_some_and(|limit| end <= limit)
        })
    {
        Ok(())
    } else {
        Err("Node's inspector activation state is outside aligned writable sections".into())
    }
}

fn decode(bytes: &[u8], address: u64) -> Result<Vec<Instruction>, Arc<str>> {
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut instructions = Vec::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            return Err(
                "Node's inspector activation has invalid or unsupported instructions".into(),
            );
        }
        instructions.push(instruction);
    }
    Ok(instructions)
}

fn calls(object: &object::File<'_>, instruction: &Instruction, name: &str) -> bool {
    if instruction.op0_kind() != OpKind::NearBranch64 {
        return false;
    }
    resolves(object, instruction.near_branch_target(), name)
}

pub(super) fn resolves(object: &object::File<'_>, address: u64, name: &str) -> bool {
    if object
        .symbols()
        .chain(object.dynamic_symbols())
        .any(|symbol| {
            symbol.is_definition()
                && symbol.kind() == SymbolKind::Text
                && symbol.address() == address
                && symbol.name().ok() == Some(name)
        })
    {
        return true;
    }
    let Ok(bytes) = text(object, address, 16) else {
        return false;
    };
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut jump = decoder.decode();
    if jump.mnemonic() == Mnemonic::Endbr64 {
        jump = decoder.decode();
    }
    if jump.mnemonic() != Mnemonic::Jmp
        || !jump.is_ip_rel_memory_operand()
        || jump.memory_index() != Register::None
        || jump.segment_prefix() != Register::None
        || jump.memory_size().size() != 8
    {
        return false;
    }
    let slot = jump.ip_rel_memory_address();
    if writable(object, slot, 8).is_err() {
        return false;
    }
    let mut found = None;
    for (at, relocation) in object.dynamic_relocations().into_iter().flatten() {
        if at != slot {
            continue;
        }
        if found.is_some() || relocation.addend() != 0 {
            return false;
        }
        if !matches!(relocation.flags(), object::RelocationFlags::Elf { r_type }
            if r_type == object::elf::R_X86_64_JUMP_SLOT || r_type == object::elf::R_X86_64_GLOB_DAT)
        {
            return false;
        }
        let object::RelocationTarget::Symbol(index) = relocation.target() else {
            return false;
        };
        found = Some(
            object
                .dynamic_symbol_table()
                .and_then(|table| table.symbol_by_index(index).ok())
                .is_some_and(|symbol| symbol.name().ok() == Some(name)),
        );
    }
    found == Some(true)
}

fn reachable(instructions: &[Instruction]) -> BTreeSet<u64> {
    let by_ip: BTreeMap<_, _> = instructions
        .iter()
        .map(|instruction| (instruction.ip(), instruction))
        .collect();
    let mut queue: Vec<_> = instructions
        .first()
        .map(Instruction::ip)
        .into_iter()
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(ip) = queue.pop() {
        if !seen.insert(ip) {
            continue;
        }
        let Some(instruction) = by_ip.get(&ip) else {
            continue;
        };
        match instruction.flow_control() {
            FlowControl::Next | FlowControl::Call | FlowControl::IndirectCall => {
                queue.push(instruction.next_ip());
            }
            FlowControl::ConditionalBranch => {
                queue.push(instruction.near_branch_target());
                queue.push(instruction.next_ip());
            }
            FlowControl::UnconditionalBranch => queue.push(instruction.near_branch_target()),
            _ => {}
        }
    }
    seen
}

fn argument(
    instructions: &[Instruction],
    call: usize,
    register: Register,
) -> Result<u64, Arc<str>> {
    let mut factory = iced_x86::InstructionInfoFactory::new();
    for instruction in instructions[..call].iter().rev().take(64) {
        if !matches!(instruction.flow_control(), FlowControl::Next) {
            return Err("Node's inspector activation argument crosses control flow".into());
        }
        let writes = factory
            .info(instruction)
            .used_registers()
            .iter()
            .any(|used| {
                used.register().full_register() == register
                    && matches!(
                        used.access(),
                        iced_x86::OpAccess::Write
                            | iced_x86::OpAccess::CondWrite
                            | iced_x86::OpAccess::ReadWrite
                            | iced_x86::OpAccess::ReadCondWrite
                    )
            });
        if !writes {
            continue;
        }
        if instructions.iter().any(|branch| {
            matches!(
                branch.flow_control(),
                FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
            ) && branch.near_branch_target() > instruction.ip()
                && branch.near_branch_target() <= instructions[call].ip()
        }) {
            return Err("Node's inspector activation argument can be bypassed".into());
        }
        let destination = instruction.op0_register();
        if instruction.op0_kind() != OpKind::Register || destination.full_register() != register {
            return Err(UNSUPPORTED.into());
        }
        return match instruction.mnemonic() {
            Mnemonic::Lea
                if destination.size() == 8
                    && instruction.is_ip_rel_memory_operand()
                    && instruction.memory_index() == Register::None
                    && instruction.segment_prefix() == Register::None =>
            {
                Ok(instruction.ip_rel_memory_address())
            }
            Mnemonic::Xor
                if destination.size() >= 4
                    && instruction.op1_kind() == OpKind::Register
                    && instruction.op1_register() == destination =>
            {
                Ok(0)
            }
            Mnemonic::Mov
                if destination.size() >= 4
                    && matches!(
                        instruction.op1_kind(),
                        OpKind::Immediate32 | OpKind::Immediate64
                    ) =>
            {
                Ok(instruction.immediate(1))
            }
            _ => Err(UNSUPPORTED.into()),
        };
    }
    Err("Node's inspector activation argument is unproved".into())
}

fn shape(
    bytes: &[u8],
    address: u64,
    template: &[u8],
    tail: bool,
) -> Result<Vec<Instruction>, Arc<str>> {
    let instructions = decode(bytes, address)?;
    let expected = decode(template, 0)?;
    if instructions.len() != expected.len() {
        return Err(UNSUPPORTED.into());
    }
    for (index, (actual, expected)) in instructions.iter().zip(&expected).enumerate() {
        let mut normalized = *actual;
        normalized.set_ip(expected.ip());
        if actual.is_ip_rel_memory_operand() {
            normalized.set_memory_displacement64(expected.memory_displacement64());
        }
        if actual.op0_kind() == OpKind::NearBranch64 {
            let target = if actual.flow_control() == FlowControl::Call
                || tail && index + 1 == instructions.len()
            {
                expected.near_branch_target()
            } else {
                actual
                    .near_branch_target()
                    .checked_sub(address)
                    .ok_or(UNSUPPORTED)?
            };
            normalized.set_near_branch64(target);
        }
        if normalized != *expected {
            return Err(UNSUPPORTED.into());
        }
    }
    Ok(instructions)
}

fn prove_writer(
    object: &object::File<'_>,
    instructions: &[Instruction],
    registry: u64,
) -> Result<(), Arc<str>> {
    let reached = reachable(instructions);
    let mut appended = false;
    for window in instructions.windows(6) {
        let address = window[0].ip();
        if window[5].next_ip().checked_sub(address) != u64::try_from(APPEND.len()).ok() {
            continue;
        }
        let Ok(candidate) = text(object, address, APPEND.len()) else {
            continue;
        };
        let Ok(append) = shape(candidate, address, APPEND, false) else {
            continue;
        };
        if !reached.contains(&address)
            || append[0].ip_rel_memory_address() != registry.checked_add(8).ok_or(UNSUPPORTED)?
            || append[1].ip_rel_memory_address() != registry.checked_add(16).ok_or(UNSUPPORTED)?
            || append[5].ip_rel_memory_address() != registry.checked_add(8).ok_or(UNSUPPORTED)?
            || appended
        {
            return Err("Node's inspector registry reader and writer disagree".into());
        }
        appended = true;
    }
    if !appended {
        return Err("Node's inspector registry writer is unsupported".into());
    }
    Ok(())
}

fn fallback(object: &object::File<'_>) -> Result<(ImageAddress, bool, ImageAddress), Arc<str>> {
    if object.architecture() != object::Architecture::X86_64 || !object.is_little_endian() {
        return Err(UNSUPPORTED.into());
    }
    let (start, size) =
        symbol(object, START)?.ok_or("Node's inspector activation entry is unavailable")?;
    let size = usize::try_from(size)
        .ok()
        .filter(|size| (1..=16384).contains(size))
        .ok_or(UNSUPPORTED)?;
    let instructions = decode(text(object, start, size)?, start)?;
    let reached = reachable(&instructions);
    let mut handler = None;
    let mut watchdog = None;
    for (index, instruction) in instructions.iter().enumerate() {
        if instruction.flow_control() != FlowControl::Call || !reached.contains(&instruction.ip()) {
            continue;
        }
        if calls(object, instruction, REGISTER) {
            if handler.is_some()
                || argument(&instructions, index, Register::RDI)? != 10
                || argument(&instructions, index, Register::RDX)? != 0
            {
                return Err(UNSUPPORTED.into());
            }
            handler = Some(argument(&instructions, index, Register::RSI)?);
        }
        if calls(object, instruction, "pthread_create") {
            if watchdog.is_some() || argument(&instructions, index, Register::RCX)? != 0 {
                return Err(UNSUPPORTED.into());
            }
            watchdog = Some(argument(&instructions, index, Register::RDX)?);
        }
    }
    let handler = handler.ok_or("Node's inspector signal registration is unsupported")?;
    let watchdog = watchdog.ok_or("Node's inspector watchdog registration is unsupported")?;
    let wake = shape(
        text(object, handler, HANDLER.len())?,
        handler,
        HANDLER,
        true,
    )?;
    if !calls(object, &wake[1], "uv_sem_post") {
        return Err(UNSUPPORTED.into());
    }
    let semaphore = wake[0].ip_rel_memory_address();
    writable(object, semaphore, 8)?;
    let body = shape(
        text(object, watchdog, WATCHDOG.len())?,
        watchdog,
        WATCHDOG,
        false,
    )?;
    let references: Vec<_> = body
        .iter()
        .filter(|instruction| instruction.is_ip_rel_memory_operand())
        .map(Instruction::ip_rel_memory_address)
        .collect();
    let [_, registry, waited, mutex, unlocked] = references.as_slice() else {
        return Err(UNSUPPORTED.into());
    };
    if *waited != semaphore || mutex != unlocked {
        return Err(UNSUPPORTED.into());
    }
    writable(object, *registry, 24)?;
    writable(object, *mutex, 8)?;
    prove_writer(object, &instructions, *registry)?;
    let expected_calls = [
        "uv_thread_setname",
        "uv_sem_wait",
        "uv_mutex_lock",
        REQUEST,
        "uv_mutex_unlock",
    ];
    let actual_calls: Vec<_> = body
        .iter()
        .filter(|instruction| instruction.flow_control() == FlowControl::Call)
        .collect();
    if actual_calls.len() != expected_calls.len()
        || !actual_calls
            .iter()
            .zip(expected_calls)
            .all(|(instruction, name)| calls(object, instruction, name))
    {
        return Err(UNSUPPORTED.into());
    }
    Ok((
        ImageAddress::new(*registry),
        true,
        ImageAddress::new(handler),
    ))
}

pub(super) fn watchdog(object: &object::File<'_>) -> Result<(u64, Vec<u8>), Arc<str>> {
    for name in [
        "_ZN4node9inspector12_GLOBAL__N_117StartIoThreadMainEPv",
        "_ZN4node9inspector12_GLOBAL__N_1L17StartIoThreadMainEPv",
    ] {
        if let Some((address, size)) = symbol(object, name)? {
            let size = usize::try_from(size)
                .ok()
                .filter(|size| (1..=512).contains(size))
                .ok_or(UNSUPPORTED)?;
            return Ok((address, text(object, address, size)?.to_vec()));
        }
    }
    // The stripped fallback has already proved this registration and body.
    fallback(object)?;
    let (start, size) = symbol(object, START)?.ok_or(UNSUPPORTED)?;
    let size = usize::try_from(size).map_err(|_| Arc::<str>::from(UNSUPPORTED))?;
    let body = decode(text(object, start, size)?, start)?;
    let reached = reachable(&body);
    let mut found = None;
    for (index, instruction) in body.iter().enumerate() {
        if reached.contains(&instruction.ip()) && calls(object, instruction, "pthread_create") {
            if found.is_some() {
                return Err(UNSUPPORTED.into());
            }
            found = Some(argument(&body, index, Register::RDX)?);
        }
    }
    let address = found.ok_or(UNSUPPORTED)?;
    Ok((address, text(object, address, WATCHDOG.len())?.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use object::{Object, ObjectSection};

    fn root() -> std::path::PathBuf {
        std::env::var_os("MICROSCOPE_NODE")
            .expect("the pinned Node matrix")
            .into()
    }

    fn debian() -> Vec<u8> {
        std::fs::read(root().join("debian-24/root/usr/lib/x86_64-linux-gnu/libnode.so.137"))
            .unwrap()
    }

    #[test]
    fn named_activation_covers_six_builds_and_agrees_with_supported_fallbacks() {
        for entry in ["26", "24", "22", "nixpkgs-26", "nixpkgs-24", "nixpkgs-22"] {
            let bytes = std::fs::read(root().join(entry).join("node")).unwrap();
            let object = object::File::parse(bytes.as_slice()).unwrap();
            let expected = named(&object).unwrap().unwrap();
            assert_eq!(activation(&object).unwrap(), expected, "{entry}");
            // Calling fallback directly excludes all private registry/handler names.
            match fallback(&object) {
                Ok(found) => assert_eq!(found, expected, "{entry}"),
                Err(reason) => {
                    assert!(reason.contains("unsupported"), "{entry}: {reason}");
                    eprintln!("{entry}: named activation supported; stripped fallback: {reason}");
                }
            }
        }
    }

    #[test]
    fn arguments_reject_partial_writes_clobbers_bypasses_and_missing_definitions() {
        let good = decode(&[0x31, 0xd2, 0xe8, 0, 0, 0, 0], 0x1000).unwrap();
        assert_eq!(argument(&good, 1, Register::RDX).unwrap(), 0);
        for bytes in [
            vec![0x31, 0xd2, 0xb2, 1, 0xe8, 0, 0, 0, 0],
            vec![0x31, 0xd2, 0x66, 0x31, 0xd2, 0xe8, 0, 0, 0, 0],
            vec![0x31, 0xd2, 0xe8, 0, 0, 0, 0, 0xe8, 0, 0, 0, 0],
            vec![0x75, 2, 0x31, 0xd2, 0xe8, 0, 0, 0, 0],
            vec![0xe8, 0, 0, 0, 0],
        ] {
            let instructions = decode(&bytes, 0x1000).unwrap();
            assert!(argument(&instructions, instructions.len() - 1, Register::RDX).is_err());
        }
        assert!(decode(&[0x48, 0x8d], 0x1000).is_err());
        assert!(shape(&WATCHDOG[..WATCHDOG.len() - 1], 0x1000, WATCHDOG, false).is_err());
        let mut extra = HANDLER.to_vec();
        extra.push(0xc3);
        assert!(shape(&extra, 0x1000, HANDLER, true).is_err());
    }

    #[test]
    fn activation_ranges_and_plt_relocation_identity_are_checked() {
        let mut bytes = debian();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        assert!(text(&object, u64::MAX, 16).is_err());
        assert!(text(&object, 0x0464_f830, 16).is_err());
        assert!(writable(&object, 0x0085_5b40, 8).is_err());
        assert!(writable(&object, 0x0464_f831, 24).is_err());
        assert!(writable(&object, u64::MAX - 7, 24).is_err());
        let call = decode(&[0xe8, 0x9d, 0x1b, 0xb2, 0xff], 0x0085_b96e).unwrap()[0];
        assert!(calls(&object, &call, REGISTER));
        assert!(!calls(&object, &call, "pthread_create"));
        let section = object.section_by_name(".rela.plt").unwrap();
        let index = section
            .data()
            .unwrap()
            .as_chunks::<24>()
            .0
            .iter()
            .position(|relocation| {
                u64::from_le_bytes(relocation[..8].try_into().unwrap()) == 0x0460_cf58
            })
            .unwrap();
        let first = usize::try_from(section.file_range().unwrap().0).unwrap();
        let at = first + index * 24 + 8;
        let original = bytes[at];
        bytes[at] = u8::try_from(object::elf::R_X86_64_64).unwrap();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        assert!(!calls(&object, &call, REGISTER));
        assert!(fallback(&object).is_err());
        bytes[at] = original;
        assert_ne!(index, 0);
        bytes[first..first + 8].copy_from_slice(&0x0460_cf58_u64.to_le_bytes());
        let object = object::File::parse(bytes.as_slice()).unwrap();
        assert!(
            !calls(&object, &call, REGISTER),
            "ambiguous GOT relocations"
        );
    }

    #[test]
    fn stripped_debian_activation_is_proven_without_private_symbols() {
        let bytes = debian();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        assert_eq!(object.symbols().count(), 0);
        assert_eq!(
            fallback(&object).unwrap(),
            (
                ImageAddress::new(0x0464_f830),
                true,
                ImageAddress::new(0x0085_5b40)
            )
        );
        assert_eq!(activation(&object).unwrap(), fallback(&object).unwrap());
    }

    #[test]
    fn mutated_debian_activation_is_refused() {
        let mut bytes = debian();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        fallback(&object).unwrap();
        let mutations: [(u64, u8); 18] = [
            (0x0085_b960, 0x90), // The reset flag is no longer proved zero.
            (0x0085_b96a, 0x0b), // SIGSEGV, not SIGUSR1.
            (0x0085_b965, 0x00), // The handler no longer points at its proved body.
            (0x0085_b84a, 0x00), // pthread_create no longer takes the watchdog entry.
            (0x0085_67bd, 0x89), // A write instead of the vector-end load.
            (0x0085_67bf, 0x10), // Capacity instead of vector end.
            (0x0085_67d6, 0x10), // Sixteen-byte rather than pointer stride.
            (0x0085_67de, 0x90), // Flags no longer come from the loop comparison.
            (0x0085_67e0, 0xee), // Loop branch enters an instruction's middle.
            (0x0085_67a3, 0x00), // Wait and post use different semaphores.
            (0x0085_67e4, 0x00), // Lock and unlock use different mutexes.
            (0x0085_5b40, 0x89), // The handler no longer loads the semaphore address.
            (0x0085_5b47, 0xe8), // A call without a return is not a tail jump.
            (0x0037_f980, 0x90), // The semaphore post PLT stub is not proved.
            (0x0037_d510, 0x90), // The signal registration PLT stub is not proved.
            (0x0038_f320, 0x90), // The loop's request PLT stub is not proved.
            (0x0085_6790, 0xa4), // Reader and writer name different aligned storage.
            (0x0085_b573, 0x00), // The vector end is updated in different storage.
        ];
        let offsets: Vec<_> = mutations
            .iter()
            .map(|&(address, byte)| {
                let offset = object
                    .sections()
                    .find_map(|section| {
                        let relative = address.checked_sub(section.address())?;
                        let (file, length) = section.file_range()?;
                        (relative < length).then_some(file + relative)
                    })
                    .unwrap();
                (usize::try_from(offset).unwrap(), byte)
            })
            .collect();
        for ((address, _), (offset, byte)) in mutations.into_iter().zip(offsets) {
            let original = bytes[offset];
            assert_ne!(
                original, byte,
                "mutation at {address:x} must change the image"
            );
            bytes[offset] = byte;
            let object = object::File::parse(bytes.as_slice()).unwrap();
            assert!(
                fallback(&object).is_err(),
                "accepted mutation at {address:x}"
            );
            bytes[offset] = original;
        }
    }
}
