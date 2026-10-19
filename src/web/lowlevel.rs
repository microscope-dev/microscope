//! The program below its source: disassembly, memory, registers,
//! watchpoints, signals, and modules.

use std::fmt::Write as _;
use std::str::FromStr as _;

use microscope::{
    AddressDescription, BlockCompletion, DebuggerHandle, DisassembledInstruction, DisassemblyQuery,
    DisassemblyRange, DisassemblyView, Expression, FrameKind, InstructionContent,
    InstructionReferenceKind, InstructionTokenKind, LoadedModuleSnapshot, RegisterRole, StopId,
    VirtualAddress, WatchpointOptions, WatchpointSpec,
};

use super::describe::{Images, hex};
use super::inspect;
use super::protocol::{
    self, BranchTarget, Disassembled, ErrorKind, FrameAt, Instruction, Memory, Module, Register,
    SignalPolicy, SourceLine, Token, WatchAccess,
};

use super::session::Failure;
use crate::cli::format::{code_name, reference_name, register_bytes};

/// Instructions shown around an address no function holds.
const WINDOW_BEFORE: u32 = 32;
const WINDOW_AFTER: u32 = 96;

/// The most bytes one request reads or writes.
const MOST_BYTES: u64 = 16 * 1024;

/// The hexadecimal digits after a `0x` or `0X` prefix.
fn hex_digits(text: &str) -> Option<&str> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(digits)
}

pub fn address(text: &str) -> Result<VirtualAddress, Failure> {
    hex_digits(text)
        .and_then(|digits| u64::from_str_radix(digits, 16).ok())
        .map(VirtualAddress::new)
        .ok_or_else(|| {
            Failure::new(
                ErrorKind::Invalid,
                format!("{text} is not an address such as 0x7ffff7a3e010"),
            )
        })
}

/// The function holding an address, or the instructions around it: the
/// frame's own code when no address is given, marking the frame's
/// instruction.
pub async fn disassemble(
    handle: &DebuggerHandle,
    images: &Images,
    request: &protocol::Disassemble,
) -> Result<Disassembled, Failure> {
    let at = request.at;
    let context = inspect::context(handle, at.stop, at.execution(), at.frame).await?;
    let view = handle.at(context);
    let trace = view.backtrace().await?;
    let index = trace
        .frames
        .iter()
        .position(|frame| frame.id == context.frame)
        .ok_or_else(|| Failure::new(ErrorKind::Invalid, "the frame is gone"))?;

    // The frame's own code is marked where it is; other code, nowhere.
    let (shown, marked) = if let Some(text) = &request.address {
        (address(text)?, None)
    } else {
        let executing = executing(&trace.frames, index).ok_or_else(|| {
            Failure::new(
                ErrorKind::Invalid,
                "the frame is suspended, and runs no code",
            )
        })?;
        (executing, Some(executing.get()))
    };

    let query = |range| DisassemblyQuery {
        range,
        syntax: match request.syntax {
            Some(protocol::Syntax::Att) => microscope::AssemblySyntax::Att,
            Some(protocol::Syntax::Intel) | None => microscope::AssemblySyntax::Intel,
        },
    };

    let disassembly = match view
        .disassemble(query(DisassemblyRange::Function(shown)))
        .await
    {
        Err(microscope::Error::NoFunctionContainsAddress(_)) => {
            view.disassemble(query(DisassemblyRange::Window {
                address: shown,
                before: WINDOW_BEFORE,
                after: WINDOW_AFTER,
            }))
            .await?
        }
        result => result?,
    };

    let modules = handle.loaded_modules().await?;
    let (function, blocks) = match &disassembly.view {
        DisassemblyView::Function { function, blocks } => {
            (Some(function.name.to_string()), blocks.to_vec())
        }
        DisassemblyView::Window { block, .. } => (None, vec![block.clone()]),
    };

    let mut instructions = Vec::new();
    let mut notes = Vec::new();
    let mut line = None;
    for block in &blocks {
        for instruction in block.instructions.iter() {
            let source = source_line(images, instruction).await;
            let starts = source
                .as_ref()
                .map(|source| (source.path.clone(), source.line));
            let source = (starts != line).then_some(source).flatten();
            line = starts;
            instructions.push(present(instruction, source, &modules));
        }

        match block.completion {
            BlockCompletion::Complete => {}
            BlockCompletion::Unreadable { address, reason } => {
                notes.push(format!("memory at {address} cannot be read: {reason}"));
            }
            BlockCompletion::Limited { next } => {
                notes.push(format!("the code continues at {next}, past what is shown"));
            }
        }
    }

    let marked = marked.and_then(|wanted| {
        blocks
            .iter()
            .flat_map(|block| block.instructions.iter())
            .find(|instruction| {
                instruction.address.get() <= wanted && wanted < instruction.end().get()
            })
            .map(|instruction| hex(instruction.address.get()))
    });

    Ok(Disassembled {
        function,
        marked,
        instructions,
        notes,
    })
}

/// An address inside the instruction a frame is executing. The innermost
/// activation and one a signal interrupted are at their instruction; any
/// other returns past its call, so the call holds the byte before. Inline
/// frames share their activation's address, and frames with no registers,
/// which a runtime's frame drives, show before it but call nothing. A
/// suspended task's frame resumes at its instruction, if it has one.
fn executing(frames: &[microscope::StackFrame], index: usize) -> Option<VirtualAddress> {
    let activation = frames[index..]
        .iter()
        .position(|frame| frame.kind != FrameKind::Inline)
        .map_or(index, |offset| index + offset);
    let frame = &frames[activation];
    let exact = frame.kind == FrameKind::Signal
        || frame.kind.is_suspended()
        || frames[..activation]
            .iter()
            .all(|frame| frame.kind == FrameKind::Inline || !frame.kind.has_registers());
    let instruction = frame.instruction?;

    Some(if exact {
        instruction
    } else {
        VirtualAddress::new(instruction.get().saturating_sub(1))
    })
}

async fn source_line(images: &Images, instruction: &DisassembledInstruction) -> Option<SourceLine> {
    let location = instruction.source.as_ref()?;
    let module = instruction.location.module.as_ref()?.module;
    let image = images.get(module).await?;
    Some(SourceLine {
        path: image.source_file(location.file)?.path.display().to_string(),
        line: location.line.get(),
        column: location.column.map(microscope::ColumnNumber::get),
        first: None,
    })
}

fn present(
    instruction: &DisassembledInstruction,
    source: Option<SourceLine>,
    modules: &LoadedModuleSnapshot,
) -> Instruction {
    let module = instruction
        .location
        .module
        .as_ref()
        .map(|module| module.module);
    let named = |description: &AddressDescription| reference_name(description, module, modules);
    let mut bytes = String::new();
    for (index, byte) in instruction.bytes.iter().enumerate() {
        let _ = write!(bytes, "{}{byte:02x}", if index == 0 { "" } else { " " });
    }

    let symbol = instruction
        .location
        .module
        .as_ref()
        .and_then(|module| module.image.symbol.as_ref())
        .map(|symbol| code_name(None, Some(symbol)));
    let (tokens, invalid, target, comment) = match &instruction.content {
        InstructionContent::Decoded(decoded) => {
            let tokens = decoded
                .tokens
                .iter()
                .map(|token| Token {
                    kind: token_kind(token.kind).to_owned(),
                    text: token.text.to_string(),
                })
                .collect();
            let target = decoded
                .references
                .iter()
                .find(|reference| reference.kind == InstructionReferenceKind::BranchTarget)
                .map(|reference| BranchTarget {
                    address: hex(reference.address.get()),
                    name: named(&reference.description),
                });
            let mut comments = decoded
                .references
                .iter()
                .filter(|reference| reference.kind == InstructionReferenceKind::MemoryOperand)
                .filter_map(|reference| named(&reference.description))
                .collect::<Vec<_>>();
            match decoded.indirect_target.as_deref() {
                Some(
                    microscope::IndirectTarget::Register { target }
                    | microscope::IndirectTarget::Memory { target, .. },
                ) => comments.push(format!(
                    "→ {}",
                    named(target).unwrap_or_else(|| target.address.to_string())
                )),
                Some(microscope::IndirectTarget::Unreadable { address, .. }) => {
                    comments.push(format!("→ unreadable slot at {address}"));
                }
                _ => {}
            }

            (
                tokens,
                None,
                target,
                (!comments.is_empty()).then(|| comments.join(", ")),
            )
        }
        InstructionContent::Invalid => (Vec::new(), Some("(bad)".to_owned()), None, None),
        InstructionContent::Truncated => (Vec::new(), Some("(truncated)".to_owned()), None, None),
    };

    Instruction {
        address: hex(instruction.address.get()),
        bytes,
        tokens,
        invalid,
        target,
        comment,
        symbol,
        source,
    }
}

const fn token_kind(kind: InstructionTokenKind) -> &'static str {
    match kind {
        InstructionTokenKind::Mnemonic => "mnemonic",
        InstructionTokenKind::Prefix => "prefix",
        InstructionTokenKind::Keyword => "keyword",
        InstructionTokenKind::Register => "register",
        InstructionTokenKind::Number => "number",
        InstructionTokenKind::Address => "address",
        InstructionTokenKind::Punctuation => "punctuation",
        _ => "text",
    }
}

pub async fn read_memory(
    handle: &DebuggerHandle,
    request: &protocol::ReadMemory,
) -> Result<Memory, Failure> {
    let start = address(&request.address)?;
    if request.count > MOST_BYTES {
        return Err(Failure::new(
            ErrorKind::Invalid,
            format!("read at most {MOST_BYTES} bytes at once"),
        ));
    }

    let read = handle
        .read_memory_at(StopId::new(request.stop), start, request.count)
        .await?;
    let mut bytes = String::with_capacity(read.bytes.len() * 2);
    for byte in read.bytes.iter() {
        let _ = write!(bytes, "{byte:02x}");
    }

    Ok(Memory {
        address: hex(start.get()),
        bytes,
        unreadable: match read.completion {
            microscope::MemoryReadCompletion::Incomplete { next_address, .. } => {
                Some(hex(next_address.get()))
            }
            _ => None,
        },
    })
}

/// Writes bytes, returning how many were written.
pub async fn write_memory(
    handle: &DebuggerHandle,
    request: &protocol::WriteMemory,
) -> Result<u64, Failure> {
    let start = address(&request.address)?;
    let text = request.bytes.trim();
    let invalid = || Failure::new(ErrorKind::Invalid, "bytes are pairs of hexadecimal digits");
    if text.is_empty() || !text.len().is_multiple_of(2) || text.len() as u64 > MOST_BYTES * 2 {
        return Err(invalid());
    }

    let bytes = (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(invalid)?;

    let written = handle
        .write_memory_at(StopId::new(request.stop), start, &bytes)
        .await?;
    if written != bytes.len() as u64 {
        return Err(Failure::new(
            ErrorKind::Failed,
            format!("only {written} of {} bytes could be written", bytes.len()),
        ));
    }

    Ok(written)
}

pub async fn registers(handle: &DebuggerHandle, at: FrameAt) -> Result<Vec<Register>, Failure> {
    let context = inspect::context(handle, at.stop, at.execution(), at.frame).await?;
    let snapshot = handle.at(context).registers().await?;
    Ok(snapshot
        .registers
        .iter()
        .map(|value| Register {
            name: value.register.name.to_string(),
            value: value
                .bytes
                .as_ref()
                .map(|bytes| register_bytes(bytes, snapshot.target.byte_order)),
            bits: value.register.bits,
            role: value.register.role.and_then(|role| match role {
                RegisterRole::ProgramCounter => Some("pc".to_owned()),
                RegisterRole::StackPointer => Some("sp".to_owned()),
                RegisterRole::FramePointer => Some("fp".to_owned()),
                _ => None,
            }),
        })
        .collect())
}

/// Adds a watchpoint on an expression in a frame, or on `0xADDRESS:BYTES`.
pub async fn add_watchpoint(
    handle: &DebuggerHandle,
    request: protocol::AddWatchpoint,
) -> Result<u64, Failure> {
    let target = request.target.trim();
    let spec = if let Some((start, bytes)) = target.split_once(':')
        && hex_digits(start).is_some()
    {
        WatchpointSpec::Location {
            address: address(start)?,
            byte_size: bytes.trim().parse().map_err(|_| {
                Failure::new(ErrorKind::Invalid, "an address range is 0xADDRESS:BYTES")
            })?,
        }
    } else {
        let (Some(stop), Some(thread)) = (request.stop, request.thread) else {
            return Err(Failure::new(
                ErrorKind::Invalid,
                "an expression is watched in a frame; name the stop and thread",
            ));
        };

        let expression = Expression::parse(target).map_err(super::values::expression_failure)?;
        let context = inspect::context(
            handle,
            stop,
            protocol::execution(thread, request.task),
            request.frame.unwrap_or(0),
        )
        .await?;

        WatchpointSpec::Target(Box::new(
            handle.at(context).resolve_watch_target(&expression).await?,
        ))
    };

    let options = WatchpointOptions {
        condition: given(request.condition)
            .map(|text| microscope::Condition::parse(&text))
            .transpose()?,
        hit_condition: given(request.hit_condition)
            .map(|text| microscope::HitCondition::from_str(&text))
            .transpose()?,
        enabled: true,
    };

    let access = match request.access {
        WatchAccess::Change => microscope::WatchAccess::Change,
        WatchAccess::Write => microscope::WatchAccess::Write,
        WatchAccess::ReadWrite => microscope::WatchAccess::ReadWrite,
        WatchAccess::Read => microscope::WatchAccess::Read,
    };

    Ok(handle
        .add_watchpoint_with(spec, access, options)
        .await?
        .id
        .get())
}

pub async fn edit_watchpoint(
    handle: &DebuggerHandle,
    edit: protocol::EditWatchpoint,
) -> Result<(), Failure> {
    let id = microscope::WatchpointId::new(edit.id);
    let condition = given(edit.condition)
        .map(|text| microscope::Condition::parse(&text))
        .transpose()?;
    let hit_condition = given(edit.hit_condition)
        .map(|text| microscope::HitCondition::from_str(&text))
        .transpose()?;
    handle.set_watchpoint_condition(id, condition).await?;
    handle
        .set_watchpoint_hit_condition(id, hit_condition)
        .await?;

    Ok(())
}

/// Text that says something, or none for blank text.
fn given(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

pub async fn signals(handle: &DebuggerHandle) -> Result<Vec<SignalPolicy>, Failure> {
    let mut signals = Vec::new();
    for code in microscope::signal_codes() {
        let policy = handle.signal_policy(code).await?;
        signals.push(SignalPolicy {
            signal: code,
            name: microscope::signal_name(code).unwrap_or_else(|| format!("signal {code}")),
            stop: policy.stop,
            print: policy.print,
            pass: policy.pass,
        });
    }

    Ok(signals)
}

pub async fn set_signal(handle: &DebuggerHandle, policy: &SignalPolicy) -> Result<String, Failure> {
    handle
        .set_signal_policy(
            policy.signal,
            microscope::SignalPolicy {
                stop: policy.stop,
                // A signal that stops is said, as the terminal debugger says it.
                print: policy.print || policy.stop,
                pass: policy.pass,
            },
        )
        .await?;
    Ok(microscope::signal_name(policy.signal)
        .unwrap_or_else(|| format!("signal {}", policy.signal)))
}

pub async fn modules(handle: &DebuggerHandle, images: &Images) -> Result<Vec<Module>, Failure> {
    let loaded = match handle.loaded_modules().await {
        Ok(loaded) => loaded,
        // Nothing runs, so nothing is loaded.
        Err(microscope::Error::NotRunning) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut modules = Vec::with_capacity(loaded.modules.len());
    for record in loaded.modules.iter() {
        let image = images.get(record.module.id).await;
        let range = image.as_ref().map(|image| image.address_range());
        let bias = record.module.load_bias;
        modules.push(Module {
            id: u64::from(record.module.id.get()),
            name: record.path.file_name().map_or_else(
                || record.path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            path: record.path.display().to_string(),
            start: range.map(|range| hex(bias.wrapping_load(range.start).get())),
            end: range.map(|range| hex(bias.wrapping_load(range.end).get())),
            symbols: match &image {
                Some(image) if image.functions().len() != 0 => "debug",
                Some(image) if image.symbols().next().is_some() => "symbols",
                _ => "none",
            }
            .to_owned(),
            debug_file: image
                .as_ref()
                .and_then(|image| image.separate_debug_file())
                .map(|debug_file| match debug_file {
                    microscope::DebugFile::Used(path)
                    | microscope::DebugFile::Unusable { path, .. } => path.display().to_string(),
                }),
            debug_file_problem: match image.as_ref().and_then(|image| image.separate_debug_file()) {
                Some(microscope::DebugFile::Unusable { reason, .. }) => Some(reason.to_string()),
                _ => None,
            },
            debug_information_problem: image
                .as_deref()
                .and_then(crate::present::debug_information::dwarf_problem),
        });
    }

    Ok(modules)
}
