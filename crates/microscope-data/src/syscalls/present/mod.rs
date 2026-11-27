//! Tokenized `strace`-style formatting for syscall entries, returns, and trace events.

pub mod dumps;
mod names;
pub mod sets;
mod structs;
pub mod summary;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use crate::syscalls::blobs;
use crate::syscalls::fds::FdTable;
use crate::syscalls::filter::fd_value;
use crate::syscalls::specs::{self, Arg, Count, Direction, MemKind, Ret, Spec};
use crate::syscalls::{
    Abi, Capture, CaptureState, Errno, EventFlags, Outcome, Pid, ProcessEnd, SyscallEvent,
    TraceEvent,
};
use nix::libc;

use sets::Shape;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Name,
    Punctuation,
    Number,
    String,

    Constant,

    Fd,

    Path,
    Address,

    Error,

    Comment,

    Time,
    Pid,

    Missing,
    Text,
}

impl Kind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Punctuation => "punct",
            Self::Number => "number",
            Self::String => "string",
            Self::Constant => "constant",
            Self::Fd => "fd",
            Self::Path => "path",
            Self::Address => "address",
            Self::Error => "error",
            Self::Comment => "comment",
            Self::Time => "time",
            Self::Pid => "pid",
            Self::Missing => "missing",
            Self::Text => "text",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: Kind,
    pub text: String,
}

const STRING_LIMIT: usize = 32;

#[derive(Debug, Default)]
pub struct Tokens(pub Vec<Token>);

impl Tokens {
    pub fn push(&mut self, kind: Kind, text: impl Into<String> + AsRef<str>) {
        if text.as_ref().is_empty() {
            return;
        }

        if let Some(last) = self.0.last_mut()
            && last.kind == kind
        {
            last.text.push_str(text.as_ref());
            return;
        }

        self.0.push(Token {
            kind,
            text: text.into(),
        });
    }

    fn punct(&mut self, text: &str) {
        self.push(Kind::Punctuation, text);
    }

    fn number(&mut self, value: impl std::fmt::Display) {
        self.push(Kind::Number, value.to_string());
    }

    fn field(&mut self, name: &str) {
        self.push(Kind::Text, name);
        self.punct("=");
    }

    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.text_len());
        self.append_to(&mut text);
        text
    }

    pub fn append_to(&self, text: &mut String) {
        for token in &self.0 {
            text.push_str(&token.text);
        }
    }

    #[must_use]
    pub fn text_len(&self) -> usize {
        self.0.iter().map(|token| token.text.len()).sum()
    }
}

#[must_use]
pub fn errno(Errno(number): Errno) -> (String, &'static str) {
    if let Some(&(_, name, message)) = names::ERRNOS.iter().find(|&&(n, _, _)| n == number) {
        return (name.to_owned(), message);
    }

    match number {
        512 => (
            "ERESTARTSYS".to_owned(),
            "To be restarted if SA_RESTART is set",
        ),
        513 => ("ERESTARTNOINTR".to_owned(), "To be restarted"),
        514 => ("ERESTARTNOHAND".to_owned(), "To be restarted if no handler"),
        515 => ("ENOIOCTLCMD".to_owned(), "Unknown ioctl command"),
        516 => ("ERESTART_RESTARTBLOCK".to_owned(), "Interrupted by signal"),
        517 => ("EPROBE_DEFER".to_owned(), "Driver requests probe retry"),
        518 => ("EOPENSTALE".to_owned(), "Open found a stale dentry"),
        524 => ("ENOTSUPP".to_owned(), "Operation is not supported"),
        _ => (format!("errno {number}"), "Unknown error"),
    }
}

/// Returns whether `errno` is a kernel-internal `ERESTART*` code visible to tracers before syscall restart.
#[must_use]
pub const fn restarts(Errno(number): Errno) -> bool {
    matches!(number, 512..=514 | 516)
}

#[must_use]
pub fn errno_named(name: &str) -> Option<Errno> {
    names::ERRNOS
        .iter()
        .find(|&&(_, candidate, _)| candidate == name)
        .map(|&(number, _, _)| Errno(number))
}

#[must_use]
pub fn signal(number: i32) -> String {
    if let Some(&(_, name)) = names::SIGNALS.iter().find(|&&(n, _)| n == number) {
        return format!("SIG{name}");
    }

    match number {
        32 => "SIGRTMIN".to_owned(),
        33..=64 => format!("SIGRT_{}", number - 32),
        _ => number.to_string(),
    }
}

#[must_use]
pub fn signal_named(name: &str) -> Option<i32> {
    let bare = name.strip_prefix("SIG").unwrap_or(name);
    names::SIGNALS
        .iter()
        .find(|&&(_, candidate)| candidate == bare)
        .map(|&(number, _)| number)
        .or_else(|| match bare {
            "RTMIN" => Some(32),
            "RTMAX" => Some(64),
            _ => bare
                .strip_prefix("RT_")?
                .parse::<i32>()
                .ok()
                .filter(|n| (1..=32).contains(n))
                .map(|n| n + 32),
        })
}

#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub fds: Option<&'a FdTable>,

    pub before: bool,
}

#[must_use]
pub fn call(event: &SyscallEvent, context: Context<'_>) -> Tokens {
    let mut out = entry(event, context);
    let spec = specs::spec(event.call);
    result(&mut out, event, spec, context);
    out
}

#[must_use]
pub fn returned(event: &SyscallEvent, context: Context<'_>) -> Tokens {
    let mut out = Tokens::default();
    let spec = specs::spec(event.call);
    result(&mut out, event, spec, context);
    out
}

#[must_use]
pub fn argument_values(event: &SyscallEvent, context: Context<'_>) -> Vec<(String, Tokens)> {
    let Some(spec) = specs::spec(event.call) else {
        return event
            .args
            .iter()
            .enumerate()
            .map(|(index, &word)| {
                let mut out = Tokens::default();
                hex(&mut out, word);
                (format!("arg{index}"), out)
            })
            .collect();
    };

    let names = specs::arg_names(event.call);
    let shown = shown_arguments(event, spec);

    spec.args
        .iter()
        .enumerate()
        .take(shown)
        .map(|(index, arg)| {
            let mut out = Tokens::default();
            argument(&mut out, event, index, *arg, spec, context);
            let name = names
                .and_then(|names| names.get(index))
                .map_or_else(|| format!("arg{index}"), |&name| name.to_owned());
            (name, out)
        })
        .collect()
}

#[must_use]
pub fn entry(event: &SyscallEvent, context: Context<'_>) -> Tokens {
    let mut out = Tokens::default();
    let name = event.call.display_name();
    out.push(Kind::Name, name.as_ref());
    out.punct("(");
    let spec = specs::spec(event.call);

    match spec {
        Some(spec) => arguments(&mut out, event, spec, context),
        None => {
            for (index, &word) in event.args.iter().enumerate() {
                if index > 0 {
                    out.punct(", ");
                }

                hex(&mut out, word);
            }
        }
    }

    out.punct(")");
    out
}

fn shown_arguments(event: &SyscallEvent, spec: &Spec) -> usize {
    const O_CREAT: u64 = 0o100;
    const O_TMPFILE: u64 = 0o20_000_000;
    let a = event.args;
    let all = spec.args.len();

    match event.call.name().unwrap_or_default() {
        "open" if a[1] & (O_CREAT | O_TMPFILE) == 0 => 2,
        "openat" if a[2] & (O_CREAT | O_TMPFILE) == 0 => 3,
        "fcntl" if matches!(a[1] & 0xffff_ffff, 1 | 3 | 9 | 11 | 1025 | 1032 | 1034) => 2,
        "futex" => match a[1] & 0x7f {
            1 => 3,
            0 | 6 | 13 => 4,
            3 => 5,
            7 | 8 => 2,
            _ => all,
        },
        _ => all,
    }
}

fn clone_arguments(out: &mut Tokens, event: &SyscallEvent) {
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    let [flag_word, stack, parent_tid, child_tid, tls, _] = event.args;
    out.field("child_stack");
    pointer(out, stack);
    out.punct(", ");
    out.field("flags");
    flags(out, event, "clone", flag_word);
    if flag_word & CLONE_PARENT_SETTID != 0 {
        out.punct(", ");
        out.field("parent_tid");
        pointer(out, parent_tid);
    }

    if flag_word & (CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID) != 0 {
        out.punct(", ");
        out.field("child_tidptr");
        pointer(out, child_tid);
    }

    if flag_word & CLONE_SETTLS != 0 {
        out.punct(", ");
        out.field("tls");
        pointer(out, tls);
    }
}

fn arguments(out: &mut Tokens, event: &SyscallEvent, spec: &Spec, context: Context<'_>) {
    if event.call.name() == Some("clone") && event.call.abi == Abi::X86_64 {
        clone_arguments(out, event);
        return;
    }

    let shown = shown_arguments(event, spec);

    for (index, arg) in spec.args.iter().enumerate().take(shown) {
        if index > 0 {
            out.punct(", ");
        }

        argument(out, event, index, *arg, spec, context);
    }
}

fn hex(out: &mut Tokens, word: u64) {
    if word == 0 {
        out.number("0");
    } else {
        out.number(format!("{word:#x}"));
    }
}

fn pointer(out: &mut Tokens, word: u64) {
    if word == 0 {
        out.push(Kind::Constant, "NULL");
    } else {
        out.push(Kind::Address, format!("{word:#x}"));
    }
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the kernel reads the low bits"
)]
const fn int(word: u64) -> i32 {
    word as i32
}

#[expect(clippy::cast_possible_wrap, reason = "the kernel reads a long")]
const fn long(word: u64) -> i64 {
    word as i64
}

fn octal(out: &mut Tokens, value: u64) {
    if value == 0 {
        out.number("000");
    } else {
        out.number(format!("0{value:o}"));
    }
}

pub(super) fn descriptor(out: &mut Tokens, process: Pid, fd: i64, context: Context<'_>) {
    out.push(Kind::Fd, fd.to_string());
    let Some(table) = context.fds.filter(|_| fd >= 0) else {
        return;
    };

    let described = {
        if context.before {
            table.describe_before(process, fd)
        } else {
            table.describe(process, fd)
        }
    };

    if described.is_none() && table.knows(process) {
        return;
    }

    out.punct("<");
    match described {
        Some(description) => out.push(Kind::Path, description.text.as_ref()),
        None => out.push(Kind::Missing, "?"),
    }

    out.punct(">");
}

fn argument(
    out: &mut Tokens,
    event: &SyscallEvent,
    index: usize,
    arg: Arg,
    spec: &Spec,
    context: Context<'_>,
) {
    #[expect(
        clippy::indexing_slicing,
        reason = "spec parsing keeps argument numbers below six, the length of args"
    )]
    let word = event.args[index];

    match arg {
        Arg::Int | Arg::Pid | Arg::Uid | Arg::Gid => out.number(int(word)),
        Arg::Long => out.number(long(word)),
        Arg::Uint => out.number(int(word).cast_unsigned()),
        Arg::Size => out.number(word),
        Arg::Hex => hex(out, word),
        Arg::Octal | Arg::Mode => octal(out, word & 0xffff_ffff),
        Arg::Ptr => pointer(out, word),
        Arg::Fd => descriptor(out, event.process, fd_value(word), context),
        Arg::DirFd => {
            if int(word) == libc::AT_FDCWD {
                out.push(Kind::Constant, "AT_FDCWD");
                if let Some(cwd) = context
                    .fds
                    .and_then(|table| table.describe(event.process, i64::from(libc::AT_FDCWD)))
                {
                    out.punct("<");
                    out.push(Kind::Path, cwd.text.as_ref());
                    out.punct(">");
                }
            } else {
                descriptor(out, event.process, fd_value(word), context);
            }
        }
        Arg::Signal => out.push(Kind::Constant, signal(int(word))),
        Arg::Flags(set) => flags(out, event, set, word),
        Arg::Enum("ipproto") if !matches!(event.args[0], 2 | 10) => out.number(int(word)),
        Arg::Enum(set) => enumeration(out, set, word),
        Arg::Path | Arg::Str => string_argument(out, event, index, word, arg == Arg::Path),
        Arg::StrArray => string_array(out, event, index, word),
        Arg::Mem {
            direction,
            kind,
            count,
        } => memory(out, event, index, direction, kind, count, spec, context),
    }
}

fn named_bits(names: &[(u64, &str)], mut value: u64) -> Vec<String> {
    let mut parts = Vec::new();
    for &(bit, name) in names {
        if bit != 0 && value & bit == bit {
            parts.push(name.to_owned());
            value &= !bit;
        }
    }

    if value != 0 {
        parts.push(format!("{value:#x}"));
    }

    parts
}

fn flags(out: &mut Tokens, event: &SyscallEvent, set: &str, word: u64) {
    let Some(definition) = sets::set(set) else {
        hex(out, word);
        return;
    };

    let value = if definition.narrow {
        word & 0xffff_ffff
    } else {
        word
    };

    let mut parts = Vec::new();
    let mut rest = value;

    match set {
        "open" => {
            let mode = rest & 3;
            parts.push(
                sets::ACCESS_MODES
                    .iter()
                    .find(|&&(m, _)| m == mode)
                    .map_or_else(|| format!("{mode:#x}"), |&(_, name)| name.to_owned()),
            );
            rest &= !3;

            if rest & 0o4_010_000 == 0o4_010_000 {
                parts.push("O_SYNC".to_owned());
                rest &= !0o4_010_000;
            }

            if rest & 0o20_200_000 == 0o20_200_000 {
                parts.push("O_TMPFILE".to_owned());
                rest &= !0o20_200_000;
            }
        }
        "mmap_flags" => {
            let kind = rest & 0xf;
            parts.push(
                sets::MAP_TYPES
                    .iter()
                    .find(|&&(m, _)| m == kind)
                    .map_or_else(|| format!("{kind:#x}"), |&(_, name)| name.to_owned()),
            );
            rest &= !0xf;
        }
        "sock_type" => {
            let kind = rest & 0xf;
            parts.push(
                sets::SOCK_TYPES
                    .iter()
                    .find(|&&(m, _)| m == kind)
                    .map_or_else(|| format!("{kind:#x}"), |&(_, name)| name.to_owned()),
            );
            rest &= !0xf;
        }
        "clone" if event.call.name() == Some("clone") => {
            let exit = rest & 0xff;
            rest &= !0xff;
            let mut named = named_bits(definition.names, rest);
            if exit != 0 {
                named.push(signal(i32::try_from(exit).unwrap_or_default()));
            }

            parts = named;
            rest = 0;
        }
        _ => {}
    }

    if rest != 0 || parts.is_empty() {
        parts.extend(
            named_bits(definition.names, rest)
                .into_iter()
                .filter(|part| part != "0x0"),
        );
    }

    if parts.is_empty() {
        match set {
            "access" => parts.push("F_OK".to_owned()),
            "mmap_prot" => parts.push("PROT_NONE".to_owned()),
            _ => {
                out.number("0");
                return;
            }
        }
    }

    out.push(Kind::Constant, parts.join("|"));
}

fn enumeration(out: &mut Tokens, set: &str, word: u64) {
    let Some(definition) = sets::set(set) else {
        hex(out, word);
        return;
    };

    let value = if definition.narrow {
        word & 0xffff_ffff
    } else {
        word
    };

    if set == "futex" {
        let operation = value & 0x7f;
        let base = definition.names.iter().find(|&&(v, _)| v == operation);
        let Some(&(_, base)) = base else {
            hex(out, value);
            return;
        };

        let mut text = base.to_owned();
        if value & 128 != 0 {
            text.push_str("_PRIVATE");
        }

        if value & 256 != 0 {
            text.push_str("|FUTEX_CLOCK_REALTIME");
        }

        let rest = value & !0x1ff;
        if rest != 0 {
            let _ = write!(text, "|{rest:#x}");
        }

        out.push(Kind::Constant, text);
        return;
    }

    match definition.names.iter().find(|&&(v, _)| v == value) {
        Some(&(_, name)) => out.push(Kind::Constant, name),
        None if definition.shape == Shape::Enum => {
            if set == "ioctl" {
                hex(out, value);
            } else {
                out.number(int(value));
            }
        }
        None => hex(out, value),
    }
}

fn quote(out: &mut Tokens, bytes: &[u8], limit: Option<usize>, truncated: bool, all_hex: bool) {
    let shown = limit.map_or(bytes, |limit| bytes.get(..limit).unwrap_or(bytes));
    let mut text = String::with_capacity(shown.len() + 2);
    text.push('"');

    for (position, &byte) in shown.iter().enumerate() {
        if all_hex {
            let _ = write!(text, "\\x{byte:02x}");
            continue;
        }

        match byte {
            b'"' => text.push_str("\\\""),
            b'\\' => text.push_str("\\\\"),
            b'\n' => text.push_str("\\n"),
            b'\t' => text.push_str("\\t"),
            b'\r' => text.push_str("\\r"),
            0x0c => text.push_str("\\f"),
            0x0b => text.push_str("\\v"),
            0x20..=0x7e => text.push(char::from(byte)),
            _ => {
                let next_is_digit = shown.get(position + 1).is_some_and(u8::is_ascii_digit);
                if next_is_digit {
                    let _ = write!(text, "\\{byte:03o}");
                } else {
                    let _ = write!(text, "\\{byte:o}");
                }
            }
        }
    }

    text.push('"');
    out.push(Kind::String, text);
    if truncated || shown.len() < bytes.len() {
        out.punct("...");
    }
}

fn unread(out: &mut Tokens, word: u64, capture: Option<&Capture>) {
    pointer(out, word);
    if word != 0 && capture.is_some_and(|capture| capture.state == CaptureState::Unreadable) {
        out.push(Kind::Missing, " <unreadable>");
    }
}

fn string_argument(out: &mut Tokens, event: &SyscallEvent, index: usize, word: u64, path: bool) {
    #[expect(clippy::cast_possible_truncation, reason = "six arguments")]
    let capture = event.capture(index as u8);

    match capture {
        Some(capture)
            if matches!(
                capture.state,
                CaptureState::Complete | CaptureState::Truncated { .. }
            ) =>
        {
            let bytes = capture
                .bytes
                .split(|&byte| byte == 0)
                .next()
                .unwrap_or_default();
            let truncated = matches!(capture.state, CaptureState::Truncated { .. });

            quote(
                out,
                bytes,
                (!path).then_some(STRING_LIMIT),
                truncated,
                false,
            );
        }
        _ => unread(out, word, capture),
    }
}

fn string_array(out: &mut Tokens, event: &SyscallEvent, index: usize, word: u64) {
    #[expect(clippy::cast_possible_truncation, reason = "six arguments")]
    let capture = event.capture(index as u8);
    let Some(capture) = capture.filter(|capture| {
        matches!(
            capture.state,
            CaptureState::Complete | CaptureState::Truncated { .. }
        )
    }) else {
        unread(out, word, capture);
        return;
    };

    let (count, elements) = blobs::strings(&capture.bytes);

    let environment = match event.call.name() {
        Some("execve") => Some(2),
        Some("execveat") => Some(3),
        _ => None,
    };
    if environment == Some(index) {
        pointer(out, word);
        let vars = count.map_or_else(|| format!("{}+", elements.len()), |count| count.to_string());
        out.push(Kind::Comment, format!(" /* {vars} vars */"));
        return;
    }

    out.punct("[");
    for (position, element) in elements.iter().enumerate() {
        if position > 0 {
            out.punct(", ");
        }

        quote(out, element, Some(STRING_LIMIT), false, false);
    }

    let complete = count.is_some_and(|count| count as usize == elements.len());
    if !complete {
        if !elements.is_empty() {
            out.punct(", ");
        }

        out.punct("...");
    }

    out.punct("]");
}

#[expect(
    clippy::too_many_arguments,
    reason = "everything an argument depends on"
)]
fn memory(
    out: &mut Tokens,
    event: &SyscallEvent,
    index: usize,
    direction: Direction,
    kind: MemKind,
    count: Count,
    spec: &Spec,
    context: Context<'_>,
) {
    #[expect(
        clippy::indexing_slicing,
        reason = "spec parsing keeps argument numbers below six, the length of args"
    )]
    let word = event.args[index];
    #[expect(clippy::cast_possible_truncation, reason = "six arguments")]
    let capture = event.capture(index as u8);

    let failed = !matches!(event.outcome, Outcome::Returned(_));
    let readable = capture.filter(|capture| {
        matches!(
            capture.state,
            CaptureState::Complete | CaptureState::Truncated { .. }
        )
    });
    let Some(capture) = readable.filter(|_| word != 0 && !(failed && direction == Direction::Out))
    else {
        unread(out, word, capture);
        return;
    };

    match kind {
        MemKind::Bytes => {
            let length = match count {
                Count::Return => event
                    .outcome
                    .raw()
                    .map_or(0, |value| usize::try_from(value).unwrap_or(0)),
                Count::Arg(arg) => event
                    .args
                    .get(usize::from(arg))
                    .map_or(usize::MAX, |&count| {
                        usize::try_from(count).unwrap_or(usize::MAX)
                    }),
                Count::PointedBy(_) | Count::One => capture.bytes.len(),
            };

            let shown = capture.bytes.get(..length).unwrap_or(&capture.bytes);
            let truncated =
                matches!(capture.state, CaptureState::Truncated { .. }) || shown.len() < length;

            let random = event.call.name() == Some("getrandom");
            quote(out, shown, Some(STRING_LIMIT), truncated, random);
        }
        MemKind::Struct(structure) if event.call.abi != Abi::X86_64 => {
            let _ = structure;
            pointer(out, word);
        }
        MemKind::Struct(structure) => {
            let elements = match count {
                Count::One | Count::PointedBy(_) => 1,
                Count::Arg(arg) => event
                    .args
                    .get(usize::from(arg))
                    .map_or(0, |&count| usize::try_from(count).unwrap_or(0)),
                Count::Return => event
                    .outcome
                    .raw()
                    .map_or(0, |value| usize::try_from(value).unwrap_or(0)),
            };

            let decoded = structs::Decode {
                out,
                event,
                index,
                spec,
                context,
            };

            decoded.structure(
                structure,
                &capture.bytes,
                elements,
                matches!(capture.state, CaptureState::Truncated { .. }),
            );
        }
    }
}

fn result(out: &mut Tokens, event: &SyscallEvent, spec: Option<&Spec>, context: Context<'_>) {
    out.punct(" = ");
    match event.outcome {
        Outcome::Returned(value) => {
            let ret = spec.map_or(Ret::Int, |spec| spec.ret);

            match ret {
                Ret::Hex => hex(out, value.cast_unsigned()),
                Ret::Octal => octal(out, value.cast_unsigned()),
                Ret::Fd => descriptor(out, event.process, value, context),
                Ret::Int | Ret::Uint | Ret::Pid | Ret::None => out.number(value),
            }
        }
        Outcome::Failed(error) => {
            let (name, message) = errno(error);
            if restarts(error) {
                out.push(Kind::Missing, "?");
            } else {
                out.number("-1");
            }

            out.punct(" ");
            out.push(Kind::Error, format!("{name} ({message})"));
        }
        Outcome::ProcessExited => out.push(Kind::Missing, "?"),
        Outcome::Unfinished => {
            out.push(Kind::Missing, "?");
            out.push(Kind::Comment, " <unfinished ...>");
        }
        Outcome::Unavailable => {
            out.push(Kind::Missing, "?");
            out.push(Kind::Comment, " <unavailable>");
        }
    }

    if event.flags.contains(EventFlags::ENTRY_UNSEEN) {
        out.push(Kind::Comment, " /* entry unseen */");
    }

    if event.flags.contains(EventFlags::ABI_UNCERTAIN) {
        out.push(Kind::Comment, " /* ABI uncertain */");
    }
}

#[must_use]
pub fn duration(nanoseconds: u64) -> String {
    #[expect(clippy::cast_precision_loss, reason = "shown rounded")]
    let value = nanoseconds as f64;

    if nanoseconds < 1_000 {
        format!("{nanoseconds} ns")
    } else if nanoseconds < 1_000_000 {
        format!("{:.3} µs", value / 1e3).replace(".000", "")
    } else if nanoseconds < 1_000_000_000 {
        format!("{:.3} ms", value / 1e6)
    } else {
        format!("{:.3} s", value / 1e9)
    }
}

#[must_use]
pub fn other(event: &TraceEvent) -> Option<Tokens> {
    let mut out = Tokens::default();

    match *event {
        TraceEvent::Signal {
            signal: number,
            ref info,
            ..
        } if info.as_deref().is_some_and(|info| info.len() >= 48) => {
            out.punct("--- ");
            out.push(Kind::Constant, signal(number));
            out.punct(" ");
            structs::siginfo(&mut out, info.as_deref().unwrap_or_default())?;
            out.punct(" ---");
        }

        TraceEvent::Signal {
            signal: number,
            code,
            sender,
            ..
        } => {
            let name = signal(number);
            out.punct("--- ");
            out.push(Kind::Constant, name.clone());
            out.punct(" {");
            out.field("si_signo");
            out.push(Kind::Constant, name);
            if let Some(code) = code {
                out.punct(", ");
                out.field("si_code");
                out.push(Kind::Constant, structs::si_code(number, code));
            }

            if let Some(sender) = sender {
                out.punct(", ");
                out.field("si_pid");
                out.push(Kind::Pid, sender.to_string());
            }

            out.punct("} ---");
        }
        TraceEvent::Exit { end, .. } | TraceEvent::ThreadExit { end, .. } => {
            out.punct("+++ ");
            match end {
                ProcessEnd::Exited(code) => out.push(Kind::Text, format!("exited with {code}")),
                ProcessEnd::Killed {
                    signal: number,
                    core,
                } => {
                    out.push(Kind::Text, "killed by ");
                    out.push(Kind::Constant, signal(number));
                    if core {
                        out.push(Kind::Text, " (core dumped)");
                    }
                }
            }

            out.punct(" +++");
        }
        TraceEvent::Lost { count, .. } => {
            out.punct("--- ");
            out.push(Kind::Missing, format!("lost {count} events"));
            out.punct(" ---");
        }

        TraceEvent::Call(_)
        | TraceEvent::Spawn { .. }
        | TraceEvent::Exec { .. }
        | TraceEvent::Descriptor { .. } => return None,
    }

    Some(out)
}
