use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction, Mnemonic, OpKind, Register};
use object::{Object, ObjectSection, ObjectSymbol, SectionKind};

use crate::image::v8::NodeInspector;

#[path = "inspector_targets.rs"]
mod targets;

const AGENT_URL: &str = "_ZNK4node9inspector5Agent8GetWsUrlB5cxx11Ev";
const IO_URL: &str = "_ZNK4node9inspector11InspectorIo8GetWsUrlB5cxx11Ev";
const FORMAT: &str = "_ZN4node9inspector15FormatWsAddressERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEiS8_b";

const DISPATCH: &str = "_ZN4node9inspector12_GLOBAL__N_126SameThreadInspectorSession8DispatchERKN12v8_inspector10StringViewE";
const STOP: &str = "_ZN4node9inspector5Agent4StopEv";
const CONNECT: &str = "_ZN4node9inspector5Agent7ConnectESt10unique_ptrINS0_24InspectorSessionDelegateESt14default_deleteIS3_EEb";

// The supported GCC constructor returns its new InspectorSession through the saved sret.
// Relative operands vary; the allocation, both vptr stores, and return paths do not.
const SESSION: &[u8] = &[
    0xbf, 0x20, 0, 0, 0, 0xe8, 0x38, 0xc7, 0xb2, 0xff, 0xf3, 0x41, 0x0f, 0x6f, 0x47, 0x08, 0x48,
    0x89, 0xc3, 0x0f, 0x12, 0xf0, 0x66, 0x49, 0x0f, 0x7e, 0xf4, 0x4d, 0x85, 0xe4, 0x0f, 0x84, 0x6e,
    0x01, 0, 0, 0x4c, 0x89, 0xe7, 0x0f, 0x29, 0x85, 0x40, 0xff, 0xff, 0xff, 0xe8, 0x4f, 0xb4, 0xd2,
    0xff, 0x48, 0x8d, 0x05, 0x90, 0xb5, 0xd3, 0x03, 0x66, 0x0f, 0x6f, 0x85, 0x40, 0xff, 0xff, 0xff,
    0x48, 0x89, 0x03, 0x8b, 0x85, 0x54, 0xff, 0xff, 0xff, 0x0f, 0x11, 0x43, 0x10, 0x89, 0x43, 0x08,
    0x48, 0x8b, 0x05, 0xc1, 0x11, 0xdc, 0x03, 0x80, 0x38, 0, 0x0f, 0x84, 0xe0, 0x03, 0, 0, 0x41,
    0x8b, 0x44, 0x24, 0x0c, 0x8d, 0x50, 0x01, 0x41, 0x89, 0x54, 0x24, 0x0c, 0x83, 0xf8, 0xff, 0x0f,
    0x84, 0x6e, 0xb5, 0xb6, 0xff, 0x48, 0x8b, 0x85, 0x58, 0xff, 0xff, 0xff, 0x4c, 0x89, 0xe7, 0x48,
    0x89, 0x18, 0xe8, 0x58, 0x9b, 0xc3, 0xff, 0x4c, 0x8b, 0x65, 0xe0, 0x48, 0x8b, 0x85, 0x58, 0xff,
    0xff, 0xff, 0x48, 0x8b, 0x5d, 0xd8, 0x4c, 0x8b, 0x6d, 0xe8, 0x4c, 0x8b, 0x75, 0xf0, 0x4c, 0x8b,
    0x7d, 0xf8, 0xc9, 0xc3,
];
const SESSION_EMPTY: &[u8] = &[
    0x48, 0x8d, 0x05, 0x31, 0xb4, 0xd3, 0x03, 0x48, 0xc7, 0x43, 0x18, 0, 0, 0, 0, 0x48, 0x89, 0x03,
    0x8b, 0x85, 0x54, 0xff, 0xff, 0xff, 0x66, 0x0f, 0xd6, 0x43, 0x10, 0x89, 0x43, 0x08, 0x48, 0x8b,
    0x85, 0x58, 0xff, 0xff, 0xff, 0x48, 0x89, 0x18, 0x4c, 0x8b, 0x65, 0xe0, 0xe9, 0xc9, 0xfe, 0xff,
    0xff,
];

fn image_bytes<'a>(
    object: &'a object::File<'_>,
    address: u64,
    length: usize,
    text: bool,
) -> Option<&'a [u8]> {
    object.sections().find_map(|section| {
        let object::SectionFlags::Elf { sh_flags } = section.flags() else {
            return None;
        };
        let required = u64::from(object::elf::SHF_ALLOC)
            | if text {
                u64::from(object::elf::SHF_EXECINSTR)
            } else {
                0
            };
        if sh_flags & required != required
            || text && sh_flags & u64::from(object::elf::SHF_WRITE) != 0
        {
            return None;
        }
        let offset = usize::try_from(address.checked_sub(section.address())?).ok()?;
        section
            .data()
            .ok()?
            .get(offset..offset.checked_add(length)?)
    })
}

fn instructions(bytes: &[u8], address: u64) -> Option<Vec<Instruction>> {
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut result = Vec::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        if instruction.is_invalid() {
            return None;
        }
        result.push(instruction);
    }
    Some(result)
}

fn constructor_shape(
    object: &object::File<'_>,
    address: u64,
    template: &[u8],
) -> Option<Vec<Instruction>> {
    let actual = instructions(image_bytes(object, address, template.len(), true)?, address)?;
    let expected = instructions(template, 0)?;
    if actual.len() != expected.len() {
        return None;
    }
    for (actual, expected) in actual.iter().zip(expected) {
        let mut normalized = *actual;
        normalized.set_ip(expected.ip());
        if actual.is_ip_rel_memory_operand() {
            normalized.set_memory_displacement64(expected.memory_displacement64());
        }
        if actual.op0_kind() == OpKind::NearBranch64 {
            normalized.set_near_branch64(expected.near_branch_target());
        }
        if normalized != expected {
            return None;
        }
    }
    Some(actual)
}

fn relative_pointer(object: &object::File<'_>, address: u64) -> Option<u64> {
    if !address.is_multiple_of(8) {
        return None;
    }
    let bytes: [u8; 8] = image_bytes(object, address, 8, false)?.try_into().ok()?;
    let mut found = None;
    for (at, relocation) in object.dynamic_relocations().into_iter().flatten() {
        if at != address {
            continue;
        }
        if found.is_some()
            || !matches!(relocation.flags(), object::RelocationFlags::Elf { r_type } if r_type == object::elf::R_X86_64_RELATIVE)
            || !matches!(relocation.target(), object::RelocationTarget::Absolute)
        {
            return None;
        }
        let value = u64::try_from(relocation.addend()).ok()?;
        if value != u64::from_le_bytes(bytes) {
            return None;
        }
        found = Some(value);
    }
    found
}

fn named_function(object: &object::File<'_>, name: &str) -> Option<(u64, u64)> {
    let mut found = None;
    for symbol in object.symbols().chain(object.dynamic_symbols()) {
        if !symbol.is_definition() || symbol.name().ok() != Some(name) {
            continue;
        }
        let value = (symbol.address(), symbol.size());
        if symbol.kind() != object::SymbolKind::Text
            || found.is_some_and(|previous| previous != value)
        {
            return None;
        }
        found = Some(value);
    }
    found
}

#[expect(
    clippy::too_many_lines,
    reason = "the constructor branches and vtable form one proof"
)]
fn dispatch_from_connect(object: &object::File<'_>) -> Option<crate::ImageAddress> {
    if object.architecture() != object::Architecture::X86_64
        || !object.is_little_endian()
        || super::accessors::node_release(object).ok()?.major != 24
    {
        return None;
    }
    let (start, size) = named_function(object, CONNECT)?;
    let size = usize::try_from(size)
        .ok()
        .filter(|size| (1..=16384).contains(size))?;
    let bytes = image_bytes(object, start, size, true)?;
    let end = start.checked_add(u64::try_from(size).ok()?)?;
    let body = instructions(bytes, start)?;
    let by_ip: BTreeMap<_, _> = body
        .iter()
        .map(|instruction| (instruction.ip(), instruction))
        .collect();
    let mut queue = vec![start];
    let mut reached = BTreeSet::new();
    while let Some(ip) = queue.pop() {
        if !reached.insert(ip) {
            continue;
        }
        let Some(instruction) = by_ip.get(&ip) else {
            if (start..end).contains(&ip) {
                return None;
            }
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
            FlowControl::Return | FlowControl::Exception => {}
            _ => return None,
        }
    }
    // GCC saves the incoming sret in this private frame slot, never its address.
    constructor_shape(
        object,
        start,
        &[
            0x55, 0x48, 0x8d, 0x05, 0x74, 0x65, 0x20, 0x03, 0x48, 0x89, 0xe5, 0x48, 0x81, 0xec,
            0xf0, 0, 0, 0, 0x4c, 0x89, 0x6d, 0xe8, 0x49, 0x89, 0xd5, 0x4c, 0x89, 0x75, 0xf0, 0x41,
            0x89, 0xce, 0x4c, 0x89, 0x7d, 0xf8, 0x49, 0x89, 0xf7, 0x48, 0x89, 0x5d, 0xd8, 0x48,
            0x8b, 0x1e, 0x48, 0x89, 0xbd, 0x58, 0xff, 0xff, 0xff,
        ],
    )?;
    let mut factory = iced_x86::InstructionInfoFactory::new();
    for instruction in &body {
        if instruction.ip() != start.checked_add(8)?
            && instruction.mnemonic() != Mnemonic::Leave
            && factory
                .info(instruction)
                .used_registers()
                .iter()
                .any(|register| {
                    register.register().full_register() == Register::RBP
                        && matches!(
                            register.access(),
                            iced_x86::OpAccess::Write
                                | iced_x86::OpAccess::ReadWrite
                                | iced_x86::OpAccess::CondWrite
                                | iced_x86::OpAccess::ReadCondWrite
                        )
                })
        {
            return None;
        }
        if instruction.memory_base() == Register::RBP {
            let low = instruction.memory_displacement64().cast_signed();
            let high = low.checked_add(i64::try_from(instruction.memory_size().size()).ok()?)?;
            if instruction.op0_kind() == OpKind::Memory
                && low < -160
                && high > -168
                && instruction.ip() != start.checked_add(0x2e)?
            {
                return None;
            }
            if instruction.mnemonic() == Mnemonic::Lea && (-168..-160).contains(&low) {
                return None;
            }
        }
    }
    let mut result = None;
    for entry in &body {
        if !reached.contains(&entry.ip())
            || entry.mnemonic() != Mnemonic::Mov
            || entry.op0_register() != Register::EDI
            || entry.op1_kind() != OpKind::Immediate32
            || entry.immediate32() != 32
        {
            continue;
        }
        let at = entry.ip();
        let Some(session) = constructor_shape(object, at, SESSION) else {
            continue;
        };
        if session.last()?.next_ip() > end
            || !plt_symbol(object, session[1].near_branch_target(), "_Znwm")
        {
            return None;
        }
        let empty_at = session[7].near_branch_target();
        let empty = constructor_shape(object, empty_at, SESSION_EMPTY)?;
        let vtable = session[11].ip_rel_memory_address();
        if empty_at < start
            || empty.last()?.next_ip() > end
            || empty[0].ip_rel_memory_address() != vtable
            || empty.last()?.near_branch_target() != at.checked_add(0x8e)?
            || !cold_trap(object, session[24].near_branch_target())
        {
            return None;
        }
        // The atomic weak-count path rejoins the same overflow check and publication.
        let atomic = constructor_shape(
            object,
            session[19].near_branch_target(),
            &[
                0xb8, 1, 0, 0, 0, 0xf0, 0x41, 0x0f, 0xc1, 0x44, 0x24, 0x0c, 0xe9, 0x1c, 0xfc, 0xff,
                0xff,
            ],
        )?;
        if atomic[0].ip() < start
            || atomic.last()?.next_ip() > end
            || atomic.last()?.near_branch_target() != at.checked_add(0x6f)?
        {
            return None;
        }
        // No branch may bypass the allocation or enter a constructor instruction's middle.
        for branch in &body {
            if branch.flow_control() == FlowControl::Return && branch.ip() != session.last()?.ip() {
                return None;
            }
            if branch.mnemonic() == Mnemonic::Leave
                && by_ip.get(&branch.next_ip())?.flow_control() != FlowControl::Return
            {
                return None;
            }
            if !matches!(
                branch.flow_control(),
                FlowControl::ConditionalBranch | FlowControl::UnconditionalBranch
            ) {
                continue;
            }
            let target = branch.near_branch_target();
            if target == atomic[0].ip() && branch.ip() != session[19].ip()
                || target > atomic[0].ip() && target < atomic.last()?.next_ip()
            {
                return None;
            }
            if target == at.checked_add(0x8e)? && branch.ip() != empty.last()?.ip() {
                // The only other return is the permission/uninitialized null session.
                constructor_shape(
                    object,
                    branch.ip().checked_sub(18)?,
                    &[
                        0x48, 0x8b, 0x85, 0x58, 0xff, 0xff, 0xff, 0x48, 0xc7, 0, 0, 0, 0, 0, 0x4c,
                        0x8b, 0x65, 0xe0, 0xe9, 0, 0, 0, 0,
                    ],
                )?;
            }
            if target > start && target < start.checked_add(0x35)? {
                return None;
            }
            if target == empty_at && branch.ip() != session[7].ip() {
                return None;
            }
            if target == at.checked_add(0x6f)? && branch.ip() != atomic.last()?.ip() {
                return None;
            }
            if target > at
                && target < at.checked_add(u64::try_from(SESSION.len()).ok()?)?
                && target != at.checked_add(0x8e)?
                && target != at.checked_add(0x6f)?
            {
                return None;
            }
            if target > empty_at && target < empty.last()?.next_ip() {
                return None;
            }
        }
        let table_start = vtable.checked_sub(16)?;
        let table_end = vtable.checked_add(24)?;
        if !object.sections().any(|section| {
            section.name().ok() == Some(".data.rel.ro")
                && table_start >= section.address()
                && section
                    .address()
                    .checked_add(section.size())
                    .is_some_and(|end| table_end <= end)
        }) || image_bytes(object, table_start, 16, false)? != [0; 16]
        {
            return None;
        }
        // Itanium's two destructor entries precede InspectorSession::Dispatch.
        let dispatch = relative_pointer(object, vtable.checked_add(16)?)?;
        let destructor = relative_pointer(object, vtable)?;
        image_bytes(object, destructor, 1, true)?;
        let deleting = relative_pointer(object, vtable.checked_add(8)?)?;
        let delete = constructor_shape(
            object,
            deleting,
            &[
                0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x10, 0x48, 0x89, 0x7d, 0xf8, 0xe8, 0x0f,
                0xfd, 0xff, 0xff, 0x48, 0x8b, 0x7d, 0xf8, 0xbe, 0x20, 0, 0, 0, 0xc9, 0xe9, 0xf0,
                0xd7, 0xb1, 0xff,
            ],
        )?;
        if delete[4].near_branch_target() != destructor
            || !plt_symbol(object, delete.last()?.near_branch_target(), "_ZdlPvm")
        {
            return None;
        }
        if image_bytes(object, dispatch, 19, true)?
            != [
                0x55, 0x48, 0x89, 0xe5, 0x48, 0x81, 0xec, 0, 1, 0, 0, 0x48, 0x89, 0x5d, 0xe8, 0x48,
                0x8b, 0x5f, 0x18,
            ]
        {
            return None;
        }
        if result.replace(crate::ImageAddress::new(dispatch)).is_some() {
            return None;
        }
    }
    result
}

fn close_functions(
    object: &object::File<'_>,
) -> Option<(crate::ImageAddress, crate::ImageAddress)> {
    let dispatch = named_function(object, DISPATCH)
        .map(|(address, _)| crate::ImageAddress::new(address))
        .or_else(|| dispatch_from_connect(object))?;
    let (stop, _) = named_function(object, STOP)?;
    image_bytes(object, dispatch.get(), 1, true)?;
    image_bytes(object, stop, 1, true)?;
    Some((dispatch, crate::ImageAddress::new(stop)))
}

pub(super) fn layout(object: &object::File<'_>) -> Result<NodeInspector, Arc<str>> {
    let symbol = |name| {
        object
            .symbols()
            .chain(object.dynamic_symbols())
            .find(|symbol| symbol.is_definition() && symbol.name().ok() == Some(name))
            .map(|symbol| symbol.address())
    };
    let (registry, vector, signal_handler) = super::inspector_activation::activation(object)?;
    let agent_url = symbol(AGENT_URL).ok_or("Node's agent URL accessor is unavailable")?;
    let io_url = symbol(IO_URL).ok_or("Node's inspector URL accessor is unavailable")?;
    let format = symbol(FORMAT).ok_or("Node's inspector URL formatter is unavailable")?;
    let terminal =
        |target| plt_symbol(object, target, "__stack_chk_fail") || cold_trap(object, target);
    let agent_args = arguments(
        &code(object, agent_url)?,
        agent_url,
        &|at| at == io_url || plt_symbol(object, at, IO_URL),
        &terminal,
    )?;
    let Expr::Pointer(io, 0) = agent_args[0] else {
        return Err("Node's agent URL accessor has an unsupported layout".into());
    };
    let args = arguments(
        &code(object, io_url)?,
        io_url,
        &|at| at == format || plt_symbol(object, at, FORMAT),
        &terminal,
    )?;
    let [
        Expr::Pointer(host_port, host),
        Expr::Port(port_base, port),
        Expr::Object(id),
    ] = args
    else {
        return Err("Node's inspector URL accessor has an unsupported layout".into());
    };
    if host_port != port_base {
        return Err("Node's inspector URL accessor uses different host and port objects".into());
    }
    Ok(NodeInspector {
        registry,
        signal_handler,
        vector,
        io,
        host_port,
        host,
        port,
        id,
        close: close_functions(object),
        startup: startup(object, vector).ok(),
        targets: targets::layout(object)?,
    })
}

fn startup(
    object: &object::File<'_>,
    vector: bool,
) -> Result<crate::image::v8::InspectorStartup, Arc<str>> {
    let (address, bytes) = super::inspector_activation::watchdog(object)?;
    let body = instructions(&bytes, address).ok_or("Node's watchdog instructions are invalid")?;
    let waits = body
        .iter()
        .filter(|instruction| {
            instruction.flow_control() == FlowControl::Call
                && plt_symbol(object, instruction.near_branch_target(), "uv_sem_wait")
        })
        .collect::<Vec<_>>();
    let [wait] = waits.as_slice() else {
        return Err("Node's watchdog has no unique semaphore-wait checkpoint".into());
    };
    let named =
        |name| named_function(object, name).map(|(address, _)| crate::ImageAddress::new(address));
    let async_offset = if vector {
        let request = named("_ZN4node9inspector5Agent20RequestIoThreadStartEv")
            .ok_or("Node's startup posting function is unavailable")?
            .get();
        let args = flow_arguments(
            &code(object, request)?,
            request,
            &|at| plt_symbol(object, at, "uv_async_send"),
            &|at| plt_symbol(object, at, "__stack_chk_fail") || cold_trap(object, at),
            [Register::RDI; 4],
            true,
        )?;
        let Expr::Pointer(offset, 0) = args[0] else {
            return Err("Node's startup async handle cannot be proven".into());
        };
        offset
    } else {
        0
    };
    Ok(crate::image::v8::InspectorStartup {
        before_wait: crate::ImageAddress::new(wait.ip()),
        after_wait: crate::ImageAddress::new(wait.next_ip()),
        check: named("_ZN4node11Environment14CheckImmediateEP10uv_check_s")
            .ok_or("Node's check-phase callback is unavailable")?,
        async_offset,
    })
}

fn code(object: &object::File<'_>, address: u64) -> Result<Vec<u8>, Arc<str>> {
    let size = object
        .symbols()
        .chain(object.dynamic_symbols())
        .find(|symbol| symbol.is_definition() && symbol.address() == address && symbol.size() != 0)
        .and_then(|symbol| usize::try_from(symbol.size()).ok())
        .filter(|size| *size <= 512)
        .ok_or("Node's inspector accessor has no bounded function symbol")?;
    object
        .sections()
        .find_map(|section| {
            if section.kind() != SectionKind::Text {
                return None;
            }
            let offset = address.checked_sub(section.address())?;
            let at = usize::try_from(offset).ok()?;
            let bytes = section.data().ok()?;
            Some(bytes.get(at..at.checked_add(size)?)?.to_vec())
        })
        .ok_or_else(|| "Node's inspector accessor has no immutable image code".into())
}

fn plt_symbol(object: &object::File<'_>, address: u64, name: &str) -> bool {
    super::inspector_activation::resolves(object, address, name)
}

fn cold_trap(object: &object::File<'_>, address: u64) -> bool {
    object.sections().any(|section| {
        if section.kind() != SectionKind::Text {
            return false;
        }
        let Some(at) = address
            .checked_sub(section.address())
            .and_then(|at| usize::try_from(at).ok())
        else {
            return false;
        };
        section
            .data()
            .ok()
            .and_then(|bytes| bytes.get(at..at.saturating_add(2)))
            == Some(&[0x0f, 0x0b])
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Expr {
    Unknown,
    Object(u64),
    Pointer(u64, u64),
    Port(u64, u64),
}

fn shifted(value: Expr, offset: u64) -> Expr {
    match value {
        Expr::Object(at) => at.checked_add(offset).map_or(Expr::Unknown, Expr::Object),
        Expr::Pointer(field, at) => at
            .checked_add(offset)
            .map_or(Expr::Unknown, |at| Expr::Pointer(field, at)),
        _ => Expr::Unknown,
    }
}

fn memory(instruction: &Instruction, registers: &BTreeMap<Register, Expr>) -> Expr {
    if instruction.memory_index() != Register::None
        || instruction.segment_prefix() != Register::None
    {
        return Expr::Unknown;
    }
    shifted(
        registers
            .get(&instruction.memory_base())
            .copied()
            .unwrap_or(Expr::Unknown),
        instruction.memory_displacement64(),
    )
}

fn arguments(
    bytes: &[u8],
    address: u64,
    callee: &dyn Fn(u64) -> bool,
    terminal: &dyn Fn(u64) -> bool,
) -> Result<[Expr; 3], Arc<str>> {
    flow_arguments(
        bytes,
        address,
        callee,
        terminal,
        [Register::RSI, Register::RSI, Register::RDX, Register::RCX],
        false,
    )
}

fn flow_arguments(
    bytes: &[u8],
    address: u64,
    callee: &dyn Fn(u64) -> bool,
    terminal: &dyn Fn(u64) -> bool,
    arguments: [Register; 4],
    tail: bool,
) -> Result<[Expr; 3], Arc<str>> {
    let mut decoder = Decoder::with_ip(64, bytes, address, DecoderOptions::NONE);
    let mut instructions = BTreeMap::new();
    while decoder.can_decode() {
        let instruction = decoder.decode();
        instructions.insert(instruction.ip(), instruction);
    }
    let mut queue = vec![(address, BTreeMap::from([(arguments[0], Expr::Object(0))]))];
    let mut seen = BTreeSet::new();
    let mut answer = None;
    let mut factory = iced_x86::InstructionInfoFactory::new();
    for _ in 0..4096 {
        let Some((ip, mut registers)) = queue.pop() else {
            return answer
                .ok_or_else(|| "Node's inspector accessor never calls its URL formatter".into());
        };
        if !seen.insert((ip, registers.clone())) {
            continue;
        }
        if !instructions.contains_key(&ip) && terminal(ip) {
            continue;
        }
        let instruction = instructions
            .get(&ip)
            .ok_or("Node's inspector accessor branches outside its bounded instructions")?;
        if instruction.is_invalid() {
            return Err("Node's inspector accessor has an invalid instruction".into());
        }
        match instruction.flow_control() {
            FlowControl::Call | FlowControl::UnconditionalBranch
                if (instruction.flow_control() == FlowControl::UnconditionalBranch) == tail
                    && callee(instruction.near_branch_target()) =>
            {
                let args = [arguments[1], arguments[2], arguments[3]]
                    .map(|register| registers.get(&register).copied().unwrap_or(Expr::Unknown));
                if answer.is_some_and(|previous| previous != args) {
                    return Err(
                        "Node's inspector accessor paths disagree about its URL arguments".into(),
                    );
                }
                answer = Some(args);
                continue;
            }
            FlowControl::Call if terminal(instruction.near_branch_target()) => continue,
            FlowControl::Return => continue,
            FlowControl::ConditionalBranch => {
                queue.push((instruction.near_branch_target(), registers.clone()));
            }
            FlowControl::UnconditionalBranch => {
                queue.push((instruction.near_branch_target(), registers));
                continue;
            }
            FlowControl::Call | FlowControl::IndirectCall => {
                for register in [
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::RSI,
                    Register::RDI,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                ] {
                    registers.remove(&register);
                }
                queue.push((instruction.next_ip(), registers));
                continue;
            }
            FlowControl::Next => {}
            _ => return Err("Node's inspector accessor has unsupported control flow".into()),
        }
        transfer(instruction, &mut registers, &mut factory);
        queue.push((instruction.next_ip(), registers));
    }
    Err("Node's inspector accessor exceeds the control-flow proof budget".into())
}

fn transfer(
    instruction: &Instruction,
    registers: &mut BTreeMap<Register, Expr>,
    factory: &mut iced_x86::InstructionInfoFactory,
) {
    let destination = instruction.op0_register();
    let full = destination.full_register();
    let value = match instruction.mnemonic() {
        Mnemonic::Mov | Mnemonic::Movzx | Mnemonic::Lea
            if instruction.op0_kind() == OpKind::Register =>
        {
            let value = match instruction.op1_kind() {
                OpKind::Register
                    if destination.size() == 8 && instruction.op1_register().size() == 8 =>
                {
                    registers.get(&instruction.op1_register()).copied()
                }
                OpKind::Memory => {
                    let location = memory(instruction, registers);
                    match (
                        instruction.mnemonic(),
                        destination.size(),
                        instruction.memory_size().size(),
                        location,
                    ) {
                        (Mnemonic::Lea, 8, _, value) => Some(value),
                        (Mnemonic::Mov, 8, 8, Expr::Object(field)) => Some(Expr::Pointer(field, 0)),
                        (Mnemonic::Movzx, 4 | 8, 2, Expr::Pointer(field, offset)) => {
                            Some(Expr::Port(field, offset))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            Some(value.unwrap_or(Expr::Unknown))
        }
        Mnemonic::Add
            if instruction.op0_kind() == OpKind::Register
                && destination.size() == 8
                && matches!(
                    instruction.op1_kind(),
                    OpKind::Immediate8to64 | OpKind::Immediate32to64
                ) =>
        {
            Some(shifted(
                registers
                    .get(&destination)
                    .copied()
                    .unwrap_or(Expr::Unknown),
                instruction.immediate(1),
            ))
        }
        _ => None,
    };
    for register in factory.info(instruction).used_registers() {
        if matches!(
            register.access(),
            iced_x86::OpAccess::Write
                | iced_x86::OpAccess::CondWrite
                | iced_x86::OpAccess::ReadWrite
                | iced_x86::OpAccess::ReadCondWrite
        ) {
            registers.remove(&register.register().full_register());
        }
    }
    if let Some(value) = value {
        registers.insert(full, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stripped_debian_inspector_dispatch_has_an_exact_native_entry() {
        let root = std::env::var_os("MICROSCOPE_NODE").expect("the pinned Node matrix");
        let bytes = std::fs::read(
            std::path::Path::new(&root)
                .join("debian-24/root/usr/lib/x86_64-linux-gnu/libnode.so.137"),
        )
        .unwrap();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        assert_eq!(
            close_functions(&object),
            Some((
                crate::ImageAddress::new(0x0085_bc80),
                crate::ImageAddress::new(0x0085_63d0),
            ))
        );
        assert_eq!(layout(&object).unwrap().close, close_functions(&object));
    }

    #[test]
    fn stripped_dispatch_rejects_changed_construction_and_relocations() {
        let root = std::env::var_os("MICROSCOPE_NODE").expect("the pinned Node matrix");
        let mut bytes = std::fs::read(
            std::path::Path::new(&root)
                .join("debian-24/root/usr/lib/x86_64-linux-gnu/libnode.so.137"),
        )
        .unwrap();
        let object = object::File::parse(bytes.as_slice()).unwrap();
        assert!(dispatch_from_connect(&object).is_some());
        assert!(image_bytes(&object, u64::MAX, 8, true).is_none());
        let offset = |address: u64| {
            object
                .sections()
                .find_map(|section| {
                    let relative = address.checked_sub(section.address())?;
                    let (file, length) = section.file_range()?;
                    (relative < length).then_some(usize::try_from(file + relative).unwrap())
                })
                .unwrap()
        };
        let mutations = [
            (0x0085_98c0, 0xb5), // The frame no longer saves the incoming sret.
            (0x0085_9fcf, 0x30), // Allocation has the wrong object size.
            (0x0085_9fe0, 0xc1), // The returned object no longer comes from new.
            (0x0085_9fee, 0x6f), // The empty-client branch enters an instruction.
            (0x0085_a004, 0x98), // The two constructors disagree about their vptr.
            (0x0085_a012, 0x0b), // The vptr is stored from a different register.
            (0x0085_a171, 0x0b),
            (0x0085_a052, 0x19), // The session is not published through sret.
            (0x0085_a02c, 0xe1), // The atomic branch enters an instruction.
            (0x0085_a41d, 0x1d), // The atomic path skips the overflow comparison.
            (0x0085_a18f, 0xca), // The empty-client return skips an instruction.
            (0x0085_bc80, 0x90), // The selected entry is no longer the proven prologue.
        ]
        .map(|(address, byte)| (offset(address), byte));
        let rela = object.section_by_name(".rela.dyn").unwrap();
        let rela_start = usize::try_from(rela.file_range().unwrap().0).unwrap();
        let relocation = rela
            .data()
            .unwrap()
            .as_chunks::<24>()
            .0
            .iter()
            .position(|entry| u64::from_le_bytes(entry[..8].try_into().unwrap()) == 0x0459_55a8)
            .unwrap();
        let relocation = rela_start + relocation * 24;
        for (offset, byte) in mutations.into_iter().chain([
            (
                relocation + 8,
                u8::try_from(object::elf::R_X86_64_64).unwrap(),
            ),
            (relocation + 16, 0x81), // Relocation and the stored addend disagree.
            (relocation - 24, 0xa8), // Duplicate relocation of the Dispatch slot.
        ]) {
            let original = bytes[offset];
            assert_ne!(original, byte);
            bytes[offset] = byte;
            let object = object::File::parse(bytes.as_slice()).unwrap();
            assert!(
                dispatch_from_connect(&object).is_none(),
                "accepted mutation at file {offset:x}"
            );
            bytes[offset] = original;
        }
    }

    fn body(host_port: u8) -> Vec<u8> {
        vec![
            0x48, 0x89, 0xf3, 0x48, 0x8b, 0x73, host_port, 0x0f, 0xb7, 0x56, 0x48, 0x48, 0x83,
            0xc6, 0x28, 0x48, 0x8d, 0x8b, 0xb8, 0, 0, 0,
        ]
    }

    fn call(bytes: &mut Vec<u8>) {
        let next = i32::try_from(bytes.len() + 5).unwrap();
        bytes.push(0xe8);
        bytes.extend_from_slice(&(0x1000 - next).to_le_bytes());
    }

    #[test]
    fn accessor_proof_rejects_divergent_offsets_partial_registers_and_unknown_branches() {
        let expected = [
            Expr::Pointer(0x20, 0x28),
            Expr::Port(0x20, 0x48),
            Expr::Object(0xb8),
        ];
        let mut good = body(0x20);
        call(&mut good);
        assert_eq!(
            arguments(&good, 0x1000, &|at| at == 0x2000, &|_| false).unwrap(),
            expected
        );

        let mut partial = good.clone();
        partial[..3].copy_from_slice(&[0x89, 0xf3, 0x90]);
        assert_ne!(
            arguments(&partial, 0x1000, &|at| at == 0x2000, &|_| false).unwrap(),
            expected
        );

        assert!(arguments(&[0xff, 0xe0], 0x1000, &|at| at == 0x2000, &|_| false).is_err());
        for alternate in [0x20, 0x30] {
            let mut branch = vec![0x75, u8::try_from(good.len()).unwrap()];
            branch.extend_from_slice(&body(0x20));
            call(&mut branch);
            branch.extend_from_slice(&body(alternate));
            call(&mut branch);
            let result = arguments(&branch, 0x1000, &|at| at == 0x2000, &|_| false);
            if alternate == 0x20 {
                assert_eq!(result.unwrap(), expected);
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn startup_proof_rejects_a_changed_wait_callee_and_truncated_async_pointer() {
        let root = std::env::var_os("MICROSCOPE_NODE").expect("the pinned Node matrix");
        for entry in ["24", "nixpkgs-24"] {
            let mut bytes =
                std::fs::read(std::path::Path::new(&root).join(entry).join("node")).unwrap();
            let object = object::File::parse(bytes.as_slice()).unwrap();
            let layout = layout(&object).unwrap();
            let startup = startup(&object, layout.vector).unwrap();
            let offset = |address: u64| {
                object
                    .sections()
                    .find_map(|section| {
                        let relative = address.checked_sub(section.address())?;
                        let (file, length) = section.file_range()?;
                        (relative < length).then_some(usize::try_from(file + relative).unwrap())
                    })
                    .unwrap()
            };
            let mut mutations = vec![offset(startup.before_wait.get()) + 1];
            if layout.vector {
                let (request, _) =
                    named_function(&object, "_ZN4node9inspector5Agent20RequestIoThreadStartEv")
                        .unwrap();
                let body = instructions(&code(&object, request).unwrap(), request).unwrap();
                let loads = body
                    .iter()
                    .filter(|instruction| {
                        instruction.mnemonic() == Mnemonic::Mov
                            && instruction.op0_register() == Register::RDI
                            && instruction.op1_kind() == OpKind::Memory
                            && instruction.memory_displacement64() == startup.async_offset
                    })
                    .collect::<Vec<_>>();
                let [load] = loads.as_slice() else {
                    panic!("one startup async pointer load")
                };
                mutations.push(offset(load.ip()));
            }
            for offset in mutations {
                bytes[offset] ^= 8;
                let changed = object::File::parse(bytes.as_slice()).unwrap();
                assert!(
                    super::startup(&changed, layout.vector).is_err(),
                    "{entry} accepted mutation at {offset:x}"
                );
                bytes[offset] ^= 8;
            }
        }
    }

    #[test]
    fn native_inspector_accessors_are_proven_on_every_unstripped_node_build() {
        let root = std::env::var_os("MICROSCOPE_NODE").expect("the pinned Node matrix");
        for entry in [
            "26",
            "24",
            "22",
            "nixpkgs-26",
            "nixpkgs-24",
            "nixpkgs-22",
            "debian-24",
        ] {
            let directory = std::path::Path::new(&root).join(entry);
            let file = if entry == "debian-24" {
                directory.join("root/usr/lib/x86_64-linux-gnu/libnode.so.137")
            } else {
                directory.join("node")
            };
            let bytes = std::fs::read(file).unwrap();
            let object = object::File::parse(bytes.as_slice()).unwrap();
            let layout = layout(&object).unwrap_or_else(|reason| panic!("{entry}: {reason}"));
            assert_ne!(layout.registry.get(), 0, "{entry}");
            assert!(
                layout.close.is_some(),
                "{entry}: native close is unavailable"
            );
            assert_eq!(
                layout.startup,
                Some(
                    startup(&object, layout.vector)
                        .unwrap_or_else(|reason| panic!("{entry}: {reason}"))
                )
            );
            if entry != "debian-24" {
                let (dispatch, _) = named_function(&object, DISPATCH).unwrap();
                let (stop, _) = named_function(&object, STOP).unwrap();
                assert_eq!(
                    layout.close,
                    Some((
                        crate::ImageAddress::new(dispatch),
                        crate::ImageAddress::new(stop)
                    )),
                    "{entry}"
                );
            }
        }
    }
}
