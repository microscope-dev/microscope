use std::collections::{BTreeMap, BTreeSet};

use iced_x86::{Decoder, DecoderOptions, Mnemonic};

use super::client::{Evaluated, Purpose};
use super::corpus::Variant;
use super::facts::{self, Facts, Line};
use super::kernel::shadow::{Position, Shadow};
use super::kernel::{CallPhase, Kernel, State, Thread, Tid};
use super::loader::Image;
use super::markers::{Marker, Verdict};
use super::marks::Mark;
use crate::{
    Backtrace, ExecutionContext, FrameKind, PresentedFrame, ScalarValue, StepKind, StopReason,
    UnwindTermination, VariableSnapshot, VariableState, VariableValue, VariableValueSource,
    VirtualAddress,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwound {
    Whole,

    Truncated,

    Corrupt,
}

fn tid_of(thread: crate::ThreadId) -> Tid {
    Tid::try_from(thread.get()).expect("a simulated tid fits")
}

fn stopped(kernel: &Kernel, thread: crate::ThreadId) -> Option<&Thread> {
    kernel
        .threads
        .get(&tid_of(thread))
        .filter(|thread| matches!(thread.state, State::Stopped { .. }) && !thread.shadow.lost)
}

pub fn backtrace(kernel: &Kernel, backtrace: &Backtrace) -> Result<Option<Unwound>, String> {
    let Some(thread) = backtrace
        .context
        .as_thread()
        .and_then(|thread| stopped(kernel, thread))
    else {
        return Ok(None);
    };

    let process = kernel
        .process_of(thread.tid)
        .expect("a stopped thread's process exists");
    let calls = &thread.shadow.calls;
    let depth = calls.len() + 1;
    let complete = backtrace.termination == UnwindTermination::Complete;
    let physical = backtrace
        .frames
        .iter()
        .filter(|frame| !matches!(frame.kind, FrameKind::Inline | FrameKind::TailCall))
        .collect::<Vec<_>>();

    for (level, frame) in physical.iter().enumerate() {
        let Some(address) = frame.instruction.map(VirtualAddress::get) else {
            return Err(format!(
                "thread {}'s frame {level} has no instruction",
                thread.tid
            ));
        };

        if level == 0 {
            if address != thread.registers.rip {
                return Err(format!(
                    "thread {}'s innermost frame is at {address:#x}, but the thread is at {:#x}",
                    thread.tid, thread.registers.rip
                ));
            }

            continue;
        }

        let Some(call) = calls.len().checked_sub(level).map(|index| calls[index]) else {
            return Err(format!(
                "thread {}'s frame {level} is at {address:#x}, but the thread is only {depth} \
                 frames deep: {calls:x?}",
                thread.tid
            ));
        };

        let held = process
            .space
            .peek_bytes(call.slot, 8)
            .map(|bytes| u64::from_le_bytes(bytes[..].try_into().expect("eight bytes")));
        if held != Some(call.return_address) {
            let held = held.unwrap_or_default();
            if address != held {
                return Err(format!(
                    "thread {}'s frame {level} is at {address:#x}; its return address \
                     {:#x} at {:#x} was overwritten with {held:#x}",
                    thread.tid, call.return_address, call.slot
                ));
            }

            if level + 1 != physical.len() || complete {
                return Err(format!(
                    "thread {}'s backtrace goes on past frame {level}, whose return address \
                     the program overwrote, ending {}",
                    thread.tid, backtrace.termination
                ));
            }

            return Ok(Some(Unwound::Corrupt));
        }

        if address != call.return_address {
            return Err(format!(
                "thread {}'s frame {level} is at {address:#x}, but its call returns to {:#x}",
                thread.tid, call.return_address
            ));
        }
    }

    match (physical.len() < depth, complete) {
        (true, true) => Err(format!(
            "thread {}'s backtrace says it is complete after {} frames, but the thread is \
             {depth} frames deep",
            thread.tid,
            physical.len()
        )),
        (true, false) => Ok(Some(Unwound::Truncated)),
        (false, false) => Err(format!(
            "thread {}'s backtrace found all {depth} frames, but ends {}",
            thread.tid, backtrace.termination
        )),
        (false, true) => Ok(Some(Unwound::Whole)),
    }
}

pub fn tail_frames(
    kernel: &Kernel,
    facts: &Facts,
    bias: u64,
    backtrace: &Backtrace,
) -> Result<usize, String> {
    let Some(thread) = backtrace
        .context
        .as_thread()
        .and_then(|thread| stopped(kernel, thread))
    else {
        return Ok(0);
    };

    let function = |address: u64| {
        address
            .checked_sub(bias)
            .and_then(|address| facts.function(address))
            .map(|function| function.start + bias)
    };

    let calls = &thread.shadow.calls;
    let mut shown = 0;
    let mut level = 0;
    let mut frames = backtrace
        .frames
        .iter()
        .filter(|frame| frame.kind != FrameKind::Inline)
        .peekable();

    while let Some(frame) = frames.next() {
        let Some(instruction) = frame.instruction else {
            return Err(format!(
                "thread {}'s frame {} has no instruction",
                thread.tid, frame.level
            ));
        };

        let mut tails = Vec::new();

        while let Some(tail) = frames.next_if(|frame| frame.kind == FrameKind::TailCall) {
            tails.push(tail);
        }

        if tails.is_empty() {
            level += 1;
            continue;
        }

        let tid = thread.tid;
        let Some(index) = calls.len().checked_sub(level + 1) else {
            return Err(format!(
                "thread {tid}'s frame at {instruction} has functions that left by tail calls below it,                  but no call began its activation"
            ));
        };

        let call = calls[index];
        let jumps = thread.shadow.jumps(index);

        let code = instruction.get() - u64::from(level != 0);
        let mut inner = function(code);
        shown += tails.len();

        for tail in tails {
            let Some(at) = tail.instruction.map(VirtualAddress::get) else {
                return Err(format!(
                    "thread {tid}'s frame {} has no instruction",
                    tail.level
                ));
            };

            let Some(jump) = jumps.iter().find(|jump| at == jump.from || at == jump.next) else {
                return Err(format!(
                    "thread {tid}'s frame {} says its function left by a tail call at                      {at:#x}, but its activation took no such jump: {jumps:x?}",
                    tail.level
                ));
            };

            if Some(jump.to) != inner {
                return Err(format!(
                    "thread {tid}'s frame {} left by the jump at {:#x} to {:#x}, but the                      frame inside it runs the function at {inner:x?}",
                    tail.level, jump.from, jump.to
                ));
            }

            inner = function(jump.from);
        }

        if inner != Some(call.target) {
            return Err(format!(
                "thread {tid}'s tail calls below the frame at {instruction} began in the function at                  {inner:x?}, but the call entered {:#x}",
                call.target
            ));
        }

        level += 1;
    }

    Ok(shown)
}

#[derive(Debug, Clone)]
pub struct Begun {
    pub tid: Tid,
    pub kind: StepKind,
    pub rip: u64,
    pub shadow: Shadow,

    pub retired: u64,

    pub restarts: u64,

    pub inside_call: bool,

    pub frame: Option<PresentedFrame>,
    pub hidden_inline_frames: u32,

    pub targets: BTreeSet<u64>,

    pub into_return: Option<u64>,

    pub outer: usize,
}

impl Begun {
    #[must_use]
    pub fn new(
        thread: &Thread,
        kind: StepKind,
        presentation: Option<&crate::FramePresentation>,
    ) -> Self {
        Self {
            tid: thread.tid,
            kind,
            rip: thread.registers.rip,
            shadow: thread.shadow.clone(),
            retired: thread.retired,
            restarts: thread.restarts,
            inside_call: thread.call != CallPhase::Outside,
            frame: presentation.map(|presentation| presentation.frame.clone()),
            hidden_inline_frames: presentation
                .map_or(0, |presentation| presentation.hidden_inline_frames),
            targets: BTreeSet::new(),
            into_return: None,
            outer: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judged {
    Instructions,

    Loosely,

    Exactly,
}

pub fn step(
    kernel: &Kernel,
    begun: &Begun,
    positions: &[Position],
    variant: &Variant,
) -> Result<Option<Judged>, String> {
    let Some(thread) = kernel
        .threads
        .get(&begun.tid)
        .filter(|thread| matches!(thread.state, State::Stopped { .. }))
    else {
        return Err(format!(
            "the step of thread {} completed, but the thread is not stopped",
            begun.tid
        ));
    };

    if thread.shadow.lost || begun.shadow.lost {
        return Ok(None);
    }

    let end = thread.registers.rip;
    let retired = thread.retired - begun.retired;
    let start = begun.rip;
    let after = &thread.shadow.calls;
    let before = &begun.shadow.calls;
    let again = thread.restarts - begun.restarts;

    let one_instruction = || {
        if retired == 1 + again || begun.inside_call && retired == again && end == start {
            Ok(Some(Judged::Instructions))
        } else {
            Err(format!(
                "a {:?} step from {start:#x} executed {retired} instructions, ending at {end:#x}",
                begun.kind
            ))
        }
    };

    match begun.kind {
        StepKind::Instruction => one_instruction(),
        StepKind::OverInstruction => {
            let instruction = original_instruction(&variant.image, start)?;
            if instruction.mnemonic() != Mnemonic::Call {
                return one_instruction();
            }

            if end != instruction.next_ip() || after != before {
                return Err(format!(
                    "stepping over the call at {start:#x} ended at {end:#x}, {} calls deep; \
                     it returns to {:#x}, {} calls deep",
                    after.len(),
                    instruction.next_ip(),
                    before.len()
                ));
            }

            Ok(Some(Judged::Instructions))
        }
        StepKind::Out => {
            let lines = Lines::of(variant);
            let described = |address| lines.line(address).is_some();

            let stepped = before.len().checked_sub(begun.outer + 1);
            let returned = stepped.is_some_and(|index| {
                let (call, callers) = (&before[index], &before[..index]);
                (end == call.return_address && after.as_slice() == callers)
                    || callers.starts_with(after)
                        && !described(call.return_address)
                        && described(end)
                        && positions
                            .iter()
                            .filter(|position| {
                                position.depth <= index
                                    && activation_at(&begun.shadow, position.depth)
                                        == Some(position.activation)
                            })
                            .all(|position| !described(position.rip))
            });

            let inline = begun.outer == 0 && matches!(begun.frame, Some(PresentedFrame::Inline(_)));
            if returned || inline && after == before {
                return Ok(Some(Judged::Instructions));
            }

            Err(format!(
                "stepping out of {} frame {} up at {start:#x}, {} calls deep, ended at {end:#x}, \
                 {} calls deep; it returns to {:#x}",
                if inline { "an inline" } else { "the" },
                begun.outer,
                before.len(),
                after.len(),
                stepped.map_or(0, |index| before[index].return_address)
            ))
        }
        StepKind::IntoSource | StepKind::OverSource | StepKind::IntoNewTask => {
            source_step(begun, thread, retired, positions, variant).map(Some)
        }
        StepKind::Advance => Err("an advance is judged by `advance`".to_owned()),
    }
}

pub fn advance(
    kernel: &Kernel,
    begun: &Begun,
    positions: &[Position],
    reason: Option<&StopReason>,
    stopped: bool,
    variant: &Variant,
) -> Result<Option<Judged>, String> {
    let mut stood = false;
    for position in positions {
        if !begun.targets.contains(&position.rip) {
            continue;
        }

        if position.rip == begun.rip && !stood {
            stood = true;
            continue;
        }

        return Err(format!(
            "thread {} advancing to {:#x?} executed the instruction at {:#x}, where it had to \
             stop",
            begun.tid, begun.targets, position.rip
        ));
    }

    if !stopped {
        return Ok(None);
    }

    match reason {
        Some(StopReason::Step {
            kind: StepKind::Advance,
        }) => {
            let Some(thread) = kernel
                .threads
                .get(&begun.tid)
                .filter(|thread| matches!(thread.state, State::Stopped { .. }))
            else {
                return Err(format!(
                    "the advance of thread {} completed, but the thread is not stopped",
                    begun.tid
                ));
            };

            if !begun.targets.contains(&thread.registers.rip) {
                return Err(format!(
                    "thread {} advancing to {:#x?} stopped at {:#x}",
                    begun.tid, begun.targets, thread.registers.rip
                ));
            }

            Ok(Some(Judged::Instructions))
        }

        Some(StopReason::Step {
            kind: StepKind::Out,
        }) => step(
            kernel,
            &Begun {
                kind: StepKind::Out,
                ..begun.clone()
            },
            positions,
            variant,
        ),
        _ => Ok(None),
    }
}

pub(super) fn original_instruction(
    image: &Image,
    address: u64,
) -> Result<iced_x86::Instruction, String> {
    let bytes = (0..15)
        .map_while(|offset| image.original_byte(address + offset))
        .collect::<Vec<_>>();
    let mut decoder = Decoder::with_ip(64, &bytes, address, DecoderOptions::NONE);
    let instruction = decoder.decode();
    if decoder.last_error() == iced_x86::DecoderError::None {
        Ok(instruction)
    } else {
        Err(format!(
            "a step began at {address:#x}, which holds no instruction"
        ))
    }
}

fn activation_at(shadow: &Shadow, depth: usize) -> Option<u64> {
    match depth {
        0 => Some(shadow.base),
        depth => shadow.calls.get(depth - 1).map(|call| call.activation),
    }
}

struct Lines<'a> {
    facts: &'a Facts,
    bias: u64,
}

impl<'a> Lines<'a> {
    fn of(variant: &'a Variant) -> Self {
        Self {
            facts: &variant.facts,
            bias: variant.image.bias(),
        }
    }

    fn row(&self, address: u64) -> Option<&'a facts::Range> {
        address
            .checked_sub(self.bias)
            .and_then(|image| self.facts.range(image))
    }

    fn line(&self, address: u64) -> Option<Line> {
        self.row(address).and_then(|range| range.line)
    }

    fn statement_at(&self, address: u64) -> Option<&'a facts::Range> {
        self.row(address)
            .filter(|range| range.start + self.bias == address && range.statement)
    }

    fn epilogue(&self, address: u64) -> bool {
        address
            .checked_sub(self.bias)
            .is_some_and(|image| self.facts.epilogues.contains(&image))
    }

    fn describe(&self, line: Option<Line>) -> String {
        line.map_or_else(
            || "no line".to_owned(),
            |line| format!("{}:{}", self.facts.file(line), line.line),
        )
    }
}

fn source_step(
    begun: &Begun,
    thread: &Thread,
    retired: u64,
    positions: &[Position],
    variant: &Variant,
) -> Result<Judged, String> {
    let lines = Lines::of(variant);
    let (start, end) = (begun.rip, thread.registers.rip);
    let before = &begun.shadow.calls;
    let after = &thread.shadow.calls;
    let from = lines.line(start);
    let describe = |line| lines.describe(line);
    let entered = after.len() > before.len() && after[..before.len()] == before[..];

    let kind = match begun.kind {
        _ if lines.row(start).is_none() => StepKind::IntoSource,
        StepKind::IntoNewTask => StepKind::OverSource,
        kind => kind,
    };

    if let Some(into_return) = begun.into_return
        && entered
        && after[before.len()].return_address != into_return
    {
        return Err(format!(
            "a step from {start:#x} into the call returning to {into_return:#x} entered the \
             call returning to {:#x}, and stopped at {end:#x}",
            after[before.len()].return_address
        ));
    }

    if kind == StepKind::IntoSource && retired == 0 && end == start {
        if begun.hidden_inline_frames > 0 {
            return Ok(Judged::Loosely);
        }

        return Err(format!(
            "a step in at {start:#x} moved nothing, and no inline frame was hidden there"
        ));
    }

    if kind == StepKind::OverSource
        && !(after.len() <= before.len() && before[..after.len()] == after[..])
    {
        return Err(format!(
            "stepping over {} from {start:#x} stopped at {end:#x} in a callee, {} calls deep \
             from {}",
            describe(from),
            after.len(),
            before.len()
        ));
    }

    if lines.epilogue(end) {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) stopped at {end:#x}, where the compiler \
             marks an epilogue",
            describe(from)
        ));
    }

    let Some(stop) = lines.row(end).filter(|range| range.line.is_some()) else {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) stopped at {end:#x}, which no source line \
             describes",
            describe(from)
        ));
    };

    if !(stop.statement || kind == StepKind::IntoSource && entered && lines.facts.optimized) {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) stopped at {end:#x} ({}), which is not a \
             statement",
            describe(from),
            describe(stop.line)
        ));
    }

    if lines.facts.optimized {
        return Ok(Judged::Loosely);
    }

    if thread.shadow.activation() == begun.shadow.activation() && stop.line == from {
        return Err(format!(
            "a {kind:?} step from {start:#x} stopped at {end:#x} on the line it began on, {}",
            describe(from)
        ));
    }

    if let Some((position, line)) = missed_stop(begun, positions, &lines) {
        return Err(format!(
            "a {kind:?} step from {start:#x} ({}) ran past {:#x} ({}), {} calls deep, where it \
             had to stop, to {end:#x} ({})",
            describe(from),
            position.rip,
            describe(line),
            position.depth,
            describe(stop.line)
        ));
    }

    Ok(Judged::Exactly)
}

fn missed_stop(
    begun: &Begun,
    positions: &[Position],
    lines: &Lines<'_>,
) -> Option<(Position, Option<Line>)> {
    let from = lines.line(begun.rip);
    let before = &begun.shadow.calls;
    let mut crossed = BTreeSet::new();

    positions.iter().find_map(|&position| {
        if lines.epilogue(position.rip) {
            crossed.insert(position.activation);
        }

        if crossed.contains(&position.activation) {
            return None;
        }

        let line = lines.statement_at(position.rip)?.line;
        if line.is_none() || line == from {
            return None;
        }

        let must_stop = position.activation == begun.shadow.activation()
            || position.depth < before.len()
                && activation_at(&begun.shadow, position.depth) == Some(position.activation)
                && lines.line(before[position.depth].return_address) != line;
        must_stop.then_some((position, line))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inspected {
    Held,

    Unavailable,
}

fn named<'a>(variables: &'a [crate::Variable], name: &str) -> Option<&'a crate::Variable> {
    variables
        .iter()
        .rev()
        .find(|variable| &*variable.name == name && variable.kind != crate::VariableKind::Returned)
}

pub fn variables(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
    source: &str,
    markers: &[Marker],
) -> Result<Option<Inspected>, String> {
    let Some(marker) = marker_at(kernel, snapshot, backtrace, variant, source, markers) else {
        return Ok(None);
    };

    let mut values = BTreeMap::new();

    for name in marker.condition.variables() {
        let state = named(&snapshot.variables, name).map(|variable| &variable.state);
        match state.and_then(integer) {
            Some(value) => {
                values.insert(name.to_owned(), value);
            }
            None if !variant.facts.optimized => {
                return Err(format!(
                    "at line {} of {source}, unoptimized code, variable {name} is {state:?}",
                    marker.line
                ));
            }
            None => {}
        }
    }

    match marker.condition.evaluate(&values) {
        Verdict::Holds => Ok(Some(Inspected::Held)),
        Verdict::Unknown(_) => Ok(Some(Inspected::Unavailable)),
        Verdict::Fails(comparison) => Err(format!(
            "at line {} of {source}, {}: {comparison}",
            marker.line, marker.text
        )),
    }
}

pub fn evaluations(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
    source: &str,
    markers: &[Marker],
    evaluations: &[Evaluated],
) -> Result<Vec<Mark>, String> {
    let marker = marker_at(kernel, snapshot, backtrace, variant, source, markers);

    let innermost = backtrace
        .frames
        .iter()
        .find(|frame| frame.id == snapshot.stack_frame)
        .is_some_and(|frame| frame.level == 0);
    let shown = |name: &str| named(&snapshot.variables, name).map(|variable| &variable.state);
    let mut marks = Vec::new();

    for evaluated in evaluations {
        let state = match &evaluated.result {
            Ok(crate::Evaluation::Value { value, .. }) => Ok(&value.state),
            Ok(other) => Err(format!("{other:?}")),
            Err(error) => Err(error.clone()),
        };

        let wrong = |expected: &str| {
            format!(
                "`{}` evaluated to {state:?}, not {expected}",
                evaluated.text
            )
        };

        match &evaluated.purpose {
            Purpose::Marker { .. } | Purpose::Expected => {
                let Some(marker) = marker else { continue };
                let expected = evaluated.purpose != Purpose::Marker { negated: true };
                match &state {
                    Ok(VariableState::Available {
                        value: VariableValue::Scalar(ScalarValue::Boolean(truth)),
                        ..
                    }) if *truth == expected => match evaluated.purpose {
                        Purpose::Expected => marks.push(Mark::ExpectationHeld),
                        Purpose::Marker { negated: false } => marks.push(Mark::MarkerEvaluated),
                        _ => {}
                    },
                    Ok(VariableState::Unavailable(_)) | Err(_) if variant.facts.optimized => {}
                    _ => {
                        let what = if evaluated.purpose == Purpose::Expected {
                            "what it expected, "
                        } else {
                            ""
                        };

                        return Err(format!(
                            "at line {} of {source}, {what}{}",
                            marker.line,
                            wrong(&expected.to_string())
                        ));
                    }
                }
            }
            Purpose::Name(name) => match (shown(name), &state) {
                (
                    Some(VariableState::Available { value: view, .. }),
                    Ok(state @ VariableState::Available { value, .. }),
                ) if value == view => {
                    marks.push(Mark::NameEvaluated);
                    marks.extend(stored(kernel, snapshot, innermost, &evaluated.text, state)?);
                }

                (
                    Some(VariableState::Unavailable(view)),
                    Ok(VariableState::Unavailable(reason)),
                ) if reason == view => {}
                (view, _) => return Err(wrong(&format!("{view:?}, as the variables view shows"))),
            },
            Purpose::Address(name) => match (shown(name), &state) {
                (
                    Some(VariableState::Available {
                        source: VariableValueSource::Memory(view),
                        ..
                    }),
                    Ok(VariableState::Available {
                        value: VariableValue::Address(address),
                        ..
                    }),
                ) if address.address == *view => marks.push(Mark::AddressEvaluated),
                (view, _) => return Err(wrong(&format!("the address of {view:?}"))),
            },
            Purpose::Cast { .. } | Purpose::IllTyped | Purpose::Arithmetic { .. } => {
                marks.extend(computed(evaluated, &state, shown)?);
            }
        }
    }

    Ok(marks)
}

fn computed<'v>(
    evaluated: &Evaluated,
    state: &Result<&VariableState, String>,
    shown: impl Fn(&str) -> Option<&'v VariableState>,
) -> Result<Option<Mark>, String> {
    let wrong = |expected: &str| {
        format!(
            "`{}` evaluated to {state:?}, not {expected}",
            evaluated.text
        )
    };

    match &evaluated.purpose {
        Purpose::Cast { name, bits, signed } => {
            let Some(value) = shown(name).and_then(integer) else {
                return Err(format!("`{}` casts a variable not shown", evaluated.text));
            };

            let low = value.cast_unsigned() & ((1 << bits) - 1);
            let truncated = if *signed && low >> (bits - 1) == 1 {
                low.cast_signed() - (1 << bits)
            } else {
                low.cast_signed()
            };

            match state {
                Ok(state) if integer(state) == Some(truncated) => Ok(Some(Mark::CastEvaluated)),
                _ => Err(wrong(&format!(
                    "{truncated}, {value} truncated to {bits} bits"
                ))),
            }
        }
        Purpose::IllTyped => match state {
            Err(_) => Ok(Some(Mark::IllTypedRefused)),
            Ok(_) => Err(format!(
                "`{}` is ill-typed, but evaluated to {state:?}",
                evaluated.text
            )),
        },
        Purpose::Arithmetic {
            left,
            operator,
            right,
        } => {
            let (Some(a), Some(b)) = (
                shown(left).and_then(integer),
                shown(right).and_then(integer),
            ) else {
                return Err(format!("`{}` combines variables not shown", evaluated.text));
            };

            let exact = match operator {
                '+' => a.checked_add(b),
                '-' => a.checked_sub(b),
                _ => a.checked_mul(b),
            };

            let Some(exact) = exact else { return Ok(None) };
            match state {
                Ok(state) if integer(state) == Some(exact) => Ok(Some(Mark::ArithmeticEvaluated)),
                _ => Err(wrong(&format!("{exact}, the exact result"))),
            }
        }
        _ => unreachable!("only computed expressions are judged here"),
    }
}

fn stored(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    innermost: bool,
    text: &str,
    state: &VariableState,
) -> Result<Option<Mark>, String> {
    let (
        VariableState::Available {
            source,
            raw: Some(raw),
            ..
        },
        Some(thread),
    ) = (
        state,
        snapshot
            .context
            .as_thread()
            .and_then(|thread| stopped(kernel, thread)),
    )
    else {
        return Ok(None);
    };

    let (place, held, mark) = match source {
        VariableValueSource::Memory(address) => (
            address.to_string(),
            kernel.processes[&thread.tgid]
                .space
                .read_user(address.get(), raw.len() as u64)
                .ok(),
            Mark::StorageTrue,
        ),
        VariableValueSource::Register(register) if innermost && raw.len() <= 8 => {
            let Some(index) = GENERAL_REGISTERS
                .iter()
                .position(|name| *name == &*register.name)
            else {
                return Ok(None);
            };

            let bytes = thread.registers.general[index].to_le_bytes();
            (
                register.name.to_string(),
                Some(bytes[..raw.len()].to_vec()),
                Mark::RegisterTrue,
            )
        }
        _ => return Ok(None),
    };

    if held.as_deref() != Some(&**raw) {
        return Err(format!(
            "`{text}` showed bytes {raw:?} from {place}, which holds {held:?}"
        ));
    }

    Ok(Some(mark))
}

pub fn entry_values(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
) -> Result<Vec<Mark>, String> {
    let ExecutionContext::Thread(thread) = snapshot.context else {
        return Ok(Vec::new());
    };

    let Some(frame) = backtrace
        .frames
        .iter()
        .find(|frame| frame.id == snapshot.stack_frame)
        .filter(|frame| frame.kind == FrameKind::Physical)
    else {
        return Ok(Vec::new());
    };

    let Some((thread, instruction)) = stopped(kernel, thread).zip(frame.instruction) else {
        return Ok(Vec::new());
    };

    let below = backtrace
        .frames
        .iter()
        .filter(|other| other.level < frame.level && other.kind == FrameKind::Physical)
        .count();
    let calls = &thread.shadow.calls;
    let Some(index) = calls.len().checked_sub(below + 1) else {
        return Ok(Vec::new());
    };

    let at = if frame.level == 0 {
        instruction.get()
    } else {
        instruction.get() - 1
    };
    let bias = variant.image.bias();
    let Some(address) = at.checked_sub(bias) else {
        return Ok(Vec::new());
    };

    let call = &calls[index];
    let start = variant
        .facts
        .function(address)
        .map(|function| function.start + bias);
    let returns_here = calls
        .get(index + 1)
        .is_none_or(|callee| callee.return_address == instruction.get());
    let reentered = thread
        .shadow
        .jumps(index)
        .iter()
        .any(|jump| jump.to == call.target);
    if start != Some(call.target) || !returns_here || reentered {
        return Ok(Vec::new());
    }

    let shown = snapshot
        .variables
        .iter()
        .map(|variable| match &variable.state {
            VariableState::Available { raw: Some(raw), .. } => (&*variable.name, Some(&raw[..])),
            _ => (&*variable.name, None),
        })
        .collect::<Vec<_>>();
    judge_entry_values(&variant.facts, address, &call.entry, &shown)
}

fn judge_entry_values(
    facts: &Facts,
    address: u64,
    entered: &[u64; 16],
    shown: &[(&str, Option<&[u8]>)],
) -> Result<Vec<Mark>, String> {
    let mut marks = Vec::new();

    for &(name, raw) in shown {
        let Some(raw) = raw.filter(|raw| raw.len() <= 8) else {
            continue;
        };

        let Some(register) = facts.entry_register(name, address) else {
            continue;
        };

        let Some(index) = GENERAL_REGISTERS.iter().position(|name| *name == register) else {
            continue;
        };

        if shown.iter().filter(|(other, _)| *other == name).count() > 1 {
            continue;
        }

        let value = entered[index].to_le_bytes();
        if value.get(..raw.len()) != Some(raw) {
            return Err(format!(
                "`{name}` showed bytes {raw:?}, but its function was entered with {register} = {:#x}",
                entered[index]
            ));
        }

        marks.push(Mark::EntryValueTrue);
    }

    Ok(marks)
}

pub fn frame_lines(
    facts: &Facts,
    bias: u64,
    source: &str,
    backtrace: &Backtrace,
) -> Result<(), String> {
    if facts.optimized {
        return Ok(());
    }

    for frame in backtrace.frames.iter() {
        let Some(address) = frame.instruction.map(VirtualAddress::get) else {
            continue;
        };

        let at = if frame.level == 0 {
            address
        } else {
            address - 1
        };
        let Some(image) = at.checked_sub(bias) else {
            continue;
        };

        let shown = frame.source.as_ref().map(|source| source.line.get());
        let line = facts
            .range(image)
            .and_then(|range| range.line)
            .filter(|line| facts.file(*line) == source);
        if let Some(line) = line
            && shown != Some(line.line)
        {
            return Err(format!(
                "frame {} at {address:#x} shows line {shown:?}, but {at:#x} is line {}",
                frame.level, line.line
            ));
        }

        let named = frame.function.as_ref().map(|function| &*function.name);
        if let Some(function) = facts.function(image)
            && named.map_or_else(|| line.is_some(), |named| named != function.name)
        {
            return Err(format!(
                "frame {} at {address:#x} shows function {named:?}, but {at:#x} is in {}",
                frame.level, function.name
            ));
        }
    }

    Ok(())
}

pub fn registers(
    kernel: &Kernel,
    snapshot: &crate::RegisterSnapshot,
    frame: crate::StackFrameId,
    backtrace: &Backtrace,
) -> Result<Vec<Mark>, String> {
    let ExecutionContext::Thread(thread) = snapshot.context else {
        return Ok(Vec::new());
    };

    let Some(thread) = stopped(kernel, thread).filter(|thread| !thread.shadow.lost) else {
        return Ok(Vec::new());
    };

    let Some(frame) = backtrace.frames.iter().find(|shown| shown.id == frame) else {
        return Ok(Vec::new());
    };

    let shown = |name: &str| {
        let value = snapshot
            .registers
            .iter()
            .find(|value| &*value.register.name == name)?;
        Some(u64::from_le_bytes(value.bytes.as_deref()?.try_into().ok()?))
    };
    let judge = |name: &str, held: u64| match shown(name) {
        Some(value) if value != held => Err(format!(
            "frame {} shows {name} = {value:#x}, but it holds {held:#x}",
            frame.level
        )),
        _ => Ok(()),
    };

    if frame.level == 0 {
        for (index, name) in GENERAL_REGISTERS.iter().enumerate() {
            judge(name, thread.registers.general[index])?;
        }

        judge("rip", thread.registers.rip)?;
        return Ok(vec![Mark::StoppedRegistersTrue]);
    }

    let physical = backtrace
        .frames
        .iter()
        .take_while(|below| below.level <= frame.level)
        .all(|below| below.kind == FrameKind::Physical);
    let calls = &thread.shadow.calls;
    let made = usize::try_from(frame.level)
        .ok()
        .and_then(|level| calls.len().checked_sub(level))
        .and_then(|index| calls.get(index))
        .filter(|call| frame.instruction.map(VirtualAddress::get) == Some(call.return_address));
    let Some(call) = made.filter(|_| physical) else {
        return Ok(Vec::new());
    };

    judge("rip", call.return_address)?;
    judge("rsp", call.slot + 8)?;
    for name in ["rbx", "rbp", "r12", "r13", "r14", "r15"] {
        let index = GENERAL_REGISTERS
            .iter()
            .position(|general| *general == name)
            .expect("a callee-saved register is general");
        judge(name, call.entry[index])?;
    }

    Ok(vec![Mark::CallerRegistersTrue])
}

const GENERAL_REGISTERS: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13",
    "r14", "r15",
];

fn marker_at<'m>(
    kernel: &Kernel,
    snapshot: &VariableSnapshot,
    backtrace: &Backtrace,
    variant: &Variant,
    source: &str,
    markers: &'m [Marker],
) -> Option<&'m Marker> {
    let thread = stopped(kernel, snapshot.context.as_thread()?)?;
    let facts = &variant.facts;
    let line = thread
        .registers
        .rip
        .checked_sub(variant.image.bias())
        .and_then(|image| facts.range(image))
        .filter(|range| range.start + variant.image.bias() == thread.registers.rip)
        .and_then(|range| range.line)
        .filter(|line| facts.file(*line) == source)?;
    let marker = markers.iter().find(|marker| marker.line == line.line)?;

    backtrace
        .frames
        .iter()
        .find(|frame| frame.id == snapshot.stack_frame)
        .is_some_and(|frame| {
            frame.level == 0
                && frame.source.as_ref().map(|source| source.line.get()) == Some(line.line)
        })
        .then_some(marker)
}

fn integer(state: &VariableState) -> Option<i128> {
    let VariableState::Available { value, .. } = state else {
        return None;
    };

    match value {
        VariableValue::Scalar(ScalarValue::Signed(value)) => Some(*value),
        VariableValue::Scalar(ScalarValue::Unsigned(value)) => i128::try_from(*value).ok(),
        VariableValue::Enumeration { value, .. } => match value {
            crate::IntegerValue::Signed(value) => Some(*value),
            crate::IntegerValue::Unsigned(value) => i128::try_from(*value).ok(),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::sim::corpus::Corpus;
    use crate::sim::kernel::shadow::Call;
    use crate::{StackFrame, ThreadId, VirtualAddress};

    #[test]
    fn advances_stop_where_they_first_reach_their_location() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let (kernel, tid) = kernel_with_calls(variant, &[]);
        let rip = kernel.threads[&tid].registers.rip;
        let begun = Begun {
            targets: BTreeSet::from([rip, 0x9999]),
            ..Begun::new(&kernel.threads[&tid], StepKind::Advance, None)
        };

        let at = |rip| Position {
            rip,
            depth: 0,
            activation: 0,
        };

        let advanced = StopReason::Step {
            kind: StepKind::Advance,
        };

        let judge = |begun: &Begun, positions: &[Position], reason: Option<&StopReason>| {
            advance(&kernel, begun, positions, reason, true, variant)
        };

        assert!(judge(&begun, &[at(rip), at(rip + 1)], Some(&advanced)).is_ok());
        assert!(judge(&begun, &[at(rip), at(rip + 1), at(rip)], Some(&advanced)).is_err());
        assert!(judge(&begun, &[at(rip + 1), at(0x9999)], None).is_err());
        let elsewhere = Begun {
            targets: BTreeSet::from([0x9999]),
            ..begun
        };

        assert!(judge(&elsewhere, &[], Some(&advanced)).is_err());
    }

    #[test]
    fn a_restarted_call_executes_its_syscall_again() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let (mut kernel, tid) = kernel_with_calls(variant, &[]);
        let begun = Begun::new(&kernel.threads[&tid], StepKind::Instruction, None);
        let mut ran = |retired: u64, restarts: u64| {
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.retired = begun.retired + retired;
            thread.restarts = begun.restarts + restarts;
            step(&kernel, &begun, &[], variant)
        };

        assert_eq!(ran(1, 0), Ok(Some(Judged::Instructions)));
        assert_eq!(ran(2, 1), Ok(Some(Judged::Instructions)));
        assert!(ran(2, 0).is_err());
        assert!(ran(1, 1).is_err());

        kernel.threads.get_mut(&tid).expect("the thread").call = CallPhase::Restarting;
        let begun = Begun::new(&kernel.threads[&tid], StepKind::Instruction, None);
        let thread = kernel.threads.get_mut(&tid).expect("the thread");
        thread.retired = begun.retired + 1;
        thread.restarts = begun.restarts + 1;

        assert_eq!(
            step(&kernel, &begun, &[], variant),
            Ok(Some(Judged::Instructions))
        );
    }

    fn kernel_with_calls(variant: &Variant, calls: &[u64]) -> (Kernel, Tid) {
        let mut kernel = Kernel::new(100);
        let tid = kernel.spawn(Arc::clone(&variant.image), &variant.path, &[], [0; 16]);
        let rsp = kernel.threads[&tid].registers.general[crate::sim::cpu::RSP];

        for (index, &return_address) in calls.iter().enumerate() {
            let slot = rsp - 64 * (index as u64 + 1);
            kernel
                .poke(tid, slot, return_address)
                .expect("write the stack");
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.shadow.calls.push(Call {
                return_address,
                slot,
                activation: 10 + index as u64,
                target: 0,
                entry: [0; 16],
            });
        }

        (kernel, tid)
    }

    fn backtrace_of(tid: Tid, frames: &[u64], termination: UnwindTermination) -> Backtrace {
        Backtrace {
            context: ThreadId::new(u64::try_from(tid).expect("a positive tid")).into(),
            frames: frames
                .iter()
                .enumerate()
                .map(|(level, &address)| {
                    StackFrame::new(
                        u32::try_from(level).expect("few frames"),
                        FrameKind::Physical,
                        None,
                        VirtualAddress::new(address),
                    )
                })
                .collect(),
            termination,
            unfollowed: Arc::from([]),
        }
    }

    #[test]
    fn frames_show_binutils_lines() {
        let row = |start, end, line| facts::Range {
            start,
            end,
            line: Some(facts::Line { file: 0, line }),
            statement: true,
        };
        let mut facts = Facts {
            files: vec!["a.c".into()],
            ranges: vec![row(0x10, 0x18, 3), row(0x18, 0x30, 4)],
            ..Facts::default()
        };

        let backtrace = |lines: [Option<u64>; 2]| {
            let mut backtrace = backtrace_of(1, &[0x1018, 0x1019], UnwindTermination::Complete);
            let frames = Arc::get_mut(&mut backtrace.frames).expect("one owner");

            for (frame, line) in frames.iter_mut().zip(lines) {
                frame.source = line.map(|line| crate::SourceLocation {
                    file: crate::SourceFileId::new(0),
                    line: crate::LineNumber::new(line).expect("a line"),
                    column: None,
                });
            }

            backtrace
        };

        assert_eq!(
            frame_lines(&facts, 0x1000, "a.c", &backtrace([Some(4), Some(4)])),
            Ok(())
        );
        assert!(frame_lines(&facts, 0x1000, "a.c", &backtrace([Some(3), Some(4)])).is_err());
        assert!(frame_lines(&facts, 0x1000, "a.c", &backtrace([Some(4), None])).is_err());

        facts.functions = vec![facts::Function {
            name: "f".into(),
            start: 0x10,
            end: 0x30,
        }];
        assert!(frame_lines(&facts, 0x1000, "a.c", &backtrace([Some(4), Some(4)])).is_err());
    }

    #[test]
    fn tail_frames_are_jumps_the_activation_took() {
        use crate::sim::kernel::shadow::Jump;

        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let (mut kernel, tid) = kernel_with_calls(variant, &[0x9000]);

        let function = |name: &str, start| facts::Function {
            name: name.into(),
            start,
            end: start + 0x100,
        };

        let facts = Facts {
            functions: vec![
                function("a", 0x1000),
                function("b", 0x1100),
                function("c", 0x1200),
            ],
            ..Facts::default()
        };

        let shadow = &mut kernel.threads.get_mut(&tid).expect("the thread").shadow;
        shadow.calls[0].target = 0x1000;
        shadow.jumps = vec![vec![
            Jump {
                from: 0x1050,
                next: 0x1055,
                to: 0x1100,
            },
            Jump {
                from: 0x1150,
                next: 0x1152,
                to: 0x1200,
            },
        ]];

        let judge = |tails: &[u64]| {
            let frames = std::iter::once((FrameKind::Physical, 0x1210))
                .chain(tails.iter().map(|&address| (FrameKind::TailCall, address)))
                .chain([(FrameKind::Physical, 0x9000)])
                .enumerate()
                .map(|(level, (kind, address))| {
                    StackFrame::new(
                        u32::try_from(level).expect("few frames"),
                        kind,
                        None,
                        VirtualAddress::new(address),
                    )
                })
                .collect();

            tail_frames(
                &kernel,
                &facts,
                0,
                &Backtrace {
                    context: ThreadId::new(u64::try_from(tid).expect("a positive tid")).into(),
                    frames,
                    termination: UnwindTermination::Complete,
                    unfollowed: Arc::from([]),
                },
            )
        };

        assert_eq!(judge(&[0x1150, 0x1050]), Ok(2));
        assert_eq!(judge(&[0x1152, 0x1055]), Ok(2));
        assert_eq!(judge(&[]), Ok(0));

        assert!(judge(&[0x1150, 0x1060]).is_err());

        assert!(judge(&[0x1050, 0x1150]).is_err());

        assert!(judge(&[0x1150]).is_err());

        assert!(judge(&[0x1050]).is_err());
    }

    #[test]
    fn names_reach_variables_but_not_returned_values() {
        let variable = |kind| crate::Variable {
            kind,
            global: None,
            name: Arc::from("count"),
            declaration: None,
            type_info: None,
            unresolved_shape: None,
            unnamed: None,
            state: VariableState::Unavailable(crate::UnsupportedVariableFeature::Tls.into()),
        };

        let local = variable(crate::VariableKind::Local);
        let returned = variable(crate::VariableKind::Returned);
        assert_eq!(
            named(&[local.clone(), returned.clone()], "count"),
            Some(&local)
        );
        assert_eq!(named(&[returned], "count"), None);
        assert_eq!(named(&[local], "other"), None);
    }

    #[test]
    fn backtraces_show_every_call_or_say_why_not() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let variant = &corpus.programs[0].variants[0];
        let (mut kernel, tid) = kernel_with_calls(variant, &[0x40_1111, 0x40_2222]);
        let rip = kernel.threads[&tid].registers.rip;
        let stopped = UnwindTermination::NoUnwindInfo {
            address: VirtualAddress::new(0x40_2222),
        };

        let judge = |kernel: &Kernel, frames: &[u64], termination: UnwindTermination| {
            backtrace(kernel, &backtrace_of(tid, frames, termination))
        };

        let complete = UnwindTermination::Complete;
        assert_eq!(
            judge(&kernel, &[rip, 0x40_2222, 0x40_1111], complete.clone()),
            Ok(Some(Unwound::Whole))
        );
        assert_eq!(
            judge(&kernel, &[rip, 0x40_2222], stopped.clone()),
            Ok(Some(Unwound::Truncated))
        );
        assert!(judge(&kernel, &[rip, 0x40_2222], complete.clone()).is_err());
        assert!(judge(&kernel, &[rip, 0x40_2223, 0x40_1111], complete.clone()).is_err());
        assert!(
            judge(
                &kernel,
                &[rip, 0x40_2222, 0x40_1111, 0x40_3333],
                complete.clone()
            )
            .is_err()
        );
        assert!(judge(&kernel, &[rip, 0x40_2222, 0x40_1111], stopped.clone()).is_err());

        let slot = kernel.threads[&tid].shadow.calls[0].slot;
        kernel.poke(tid, slot, 1).expect("write the stack");
        assert_eq!(
            judge(&kernel, &[rip, 0x40_2222, 1], stopped.clone()),
            Ok(Some(Unwound::Corrupt))
        );
        assert!(judge(&kernel, &[rip, 0x40_2222, 1], complete).is_err());
        assert!(judge(&kernel, &[rip, 0x40_2222, 0x40_1111], stopped.clone()).is_err());
        assert!(judge(&kernel, &[rip, 0x40_2222, 1, 0x40_4444], stopped).is_err());
    }

    #[test]
    fn entry_values_are_what_the_call_entered_with() {
        let mut corpus = Corpus::load().expect("load the golden corpus");
        let facts = &mut corpus.programs[0].variants[0].facts;
        facts.entry_values = vec![facts::EntryValue {
            name: "n".into(),
            addresses: 0x10..0x20,
            register: "rdi".into(),
        }];
        let facts = &*facts;
        let mut entered = [0; 16];
        entered[7] = 0x1122_3344_5566_7788;

        let judge =
            |shown: &[(&str, Option<&[u8]>)]| judge_entry_values(facts, 0x18, &entered, shown);

        let low = [0x88, 0x77, 0x66, 0x55];
        assert_eq!(
            judge(&[("n", Some(&low)), ("m", Some(&[1]))]),
            Ok(vec![Mark::EntryValueTrue])
        );
        assert!(judge(&[("n", Some(&[0x87, 0x77, 0x66, 0x55]))]).is_err());
        assert_eq!(judge(&[("n", Some(&low)), ("n", Some(&[1]))]), Ok(vec![]));
        let mut wide = [0; 16];
        wide[..8].copy_from_slice(&entered[7].to_le_bytes());
        assert_eq!(judge(&[("n", Some(&wide))]), Ok(vec![]));
        assert_eq!(
            judge_entry_values(facts, 0x20, &entered, &[("n", Some(&[1]))]),
            Ok(vec![])
        );
    }

    #[test]
    fn source_steps_stop_at_the_first_line_they_reach() {
        let corpus = Corpus::load().expect("load the golden corpus");
        let program = corpus
            .programs
            .iter()
            .find(|program| program.name == "straight")
            .expect("the straight program");
        let variant = program
            .variants
            .iter()
            .find(|variant| variant.name == "straight-gcc-O0")
            .expect("its unoptimized variant");
        let facts = &variant.facts;
        let main = facts
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main");

        let mut lines = Vec::new();

        for range in facts
            .ranges
            .iter()
            .filter(|range| range.statement && (main.start..main.end).contains(&range.start))
        {
            if range.line.is_some()
                && lines
                    .last()
                    .is_none_or(|last: &facts::Range| last.line != range.line)
            {
                lines.push(*range);
            }
        }

        let [first, second, third, ..] = lines[..] else {
            panic!("main has three lines");
        };

        let (mut kernel, tid) = kernel_with_calls(variant, &[0x40_1111]);
        let begun = {
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.registers.rip = first.start;
            Begun::new(thread, StepKind::OverSource, None)
        };

        let activation = begun.shadow.activation();
        {
            let thread = kernel.threads.get_mut(&tid).expect("the thread");
            thread.registers.rip = third.start;
            thread.retired += 3;
        }

        let passed = |rip| Position {
            rip,
            depth: 1,
            activation,
        };

        let judge = |positions: &[Position]| step(&kernel, &begun, positions, variant);
        assert_eq!(
            judge(&[passed(first.start), passed(second.start - 1)]),
            Ok(Some(Judged::Exactly))
        );
        let past = judge(&[passed(first.start), passed(second.start)]);
        assert!(
            past.as_ref()
                .is_err_and(|message| message.contains("ran past")),
            "{past:?}"
        );
    }
}
