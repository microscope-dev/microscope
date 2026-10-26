//! System calls as strace prints them, for the CLI, the DAP console, and
//! the web UI. Decoding is pure: a call's spec, its raw words, and the
//! bytes its engine captured become tokens, which the CLI colors and the
//! web turns into spans.

pub mod dumps;
mod names;
pub mod sets;
mod structs;
pub mod summary;
#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use microscope::syscalls::blobs;
use microscope::syscalls::fds::{FdTable, Kind as FdKind};
use microscope::syscalls::filter::fd_value;
use microscope::syscalls::specs::{self, Arg, Count, Direction, MemKind, Ret, Spec};
use microscope::syscalls::{
    Abi, Capture, CaptureState, Errno, EventFlags, Outcome, Pid, ProcessEnd, SyscallEvent, Tid,
    TraceEvent,
};
use nix::libc;

use sets::Shape;

/// What a token is, which decides its color and, in the web UI, its span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The call's name.
    Name,
    Punctuation,
    Number,
    String,

    /// A named constant: a flag, an enumeration value, `NULL`.
    Constant,

    /// A descriptor's number.
    Fd,

    /// What a descriptor refers to, between `<` and `>`.
    Path,
    Address,

    /// An error's name and message.
    Error,

    /// A note about the decoding, as `/* 23 vars */`.
    Comment,

    /// A timestamp or duration.
    Time,
    Pid,

    /// Something the engine could not see: `<unreadable>`, `?`.
    Missing,
    Text,
}

impl Kind {
    /// The kind's name on the wire.
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

/// How strings are escaped (`-x`, `-xx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hex {
    #[default]
    Never,
    NonAscii,
    All,
}

/// How constants print (`-X`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Constants {
    /// Names, as `O_RDONLY|O_CLOEXEC`.
    #[default]
    Abbrev,

    /// Numbers only.
    Raw,

    /// Numbers with their names in comments.
    Verbose,
}

/// What `-y` prints beside descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FdStyle {
    #[default]
    Number,
    Paths,
    All,
}

/// Everything that changes how a call prints.
#[derive(Debug, Clone)]
pub struct Style {
    /// `-s`: bytes of a string or buffer shown.
    pub string_limit: usize,
    pub hex: Hex,
    pub constants: Constants,

    /// `-v`: whole structures and environments.
    pub verbose: bool,
    pub fds: FdStyle,

    /// `-N`: arguments named as the kernel names them.
    pub arg_names: bool,

    /// `-e raw=`: this call prints raw words.
    pub raw: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            string_limit: 32,
            hex: Hex::Never,
            constants: Constants::Abbrev,
            verbose: false,
            fds: FdStyle::Number,
            arg_names: false,
            raw: false,
        }
    }
}

/// Builds a line of tokens.
#[derive(Debug, Default)]
pub struct Tokens(pub Vec<Token>);

impl Tokens {
    pub fn push(&mut self, kind: Kind, text: impl Into<String> + AsRef<str>) {
        if text.as_ref().is_empty() {
            return;
        }

        // Neighbors of one kind merge, which keeps lines short on the wire.
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

    /// The plain text of the line.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.text_len());
        self.append_to(&mut text);
        text
    }

    /// Appends the plain text of the line to `text`.
    pub fn append_to(&self, text: &mut String) {
        for token in &self.0 {
            text.push_str(&token.text);
        }
    }

    /// The bytes of the line's plain text.
    #[must_use]
    pub fn text_len(&self) -> usize {
        self.0.iter().map(|token| token.text.len()).sum()
    }
}

/// The name and message of an error number, as strace prints them,
/// including the kernel's internal restart codes.
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

/// Whether an error is one of the kernel's restart codes, which a call
/// returns only to be restarted.
#[must_use]
pub const fn restarts(Errno(number): Errno) -> bool {
    matches!(number, 512..=514 | 516)
}

/// The error number an errno name names.
#[must_use]
pub fn errno_named(name: &str) -> Option<Errno> {
    names::ERRNOS
        .iter()
        .find(|&&(_, candidate, _)| candidate == name)
        .map(|&(number, _, _)| Errno(number))
}

/// A signal's name: `SIGINT`, `SIGRTMIN`, or `SIGRT_3`.
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

/// The signal a name names, with or without its `SIG` prefix.
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

/// What the formatter knows besides the event.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub fds: Option<&'a FdTable>,

    /// Descriptors as they were before the call: true for its arguments,
    /// false for what it returned.
    pub before: bool,
}

/// The call a catchpoint stopped at, as strace prints it: its arguments,
/// with the path it names, and at its exit what it returned.
#[must_use]
pub fn caught(stop: &microscope::SyscallStop) -> String {
    let event = SyscallEvent {
        thread: Tid::new(0),
        process: Pid::new(0),
        call: stop.call,
        args: stop.args,
        captured: stop.captured.to_vec(),
        outcome: stop
            .returned
            .map_or(Outcome::Unfinished, Outcome::of_return),
        entered: None,
        exited: None,
        stack: None,
        flags: microscope::syscalls::EventFlags::default(),
    };

    let style = Style::default();
    let context = Context {
        fds: None,
        before: true,
    };

    if stop.returned.is_some() {
        call(&event, &style, context).text()
    } else {
        entry(&event, &style, context).text()
    }
}

/// Formats `name(arguments) = result`, without a prefix or duration.
#[must_use]
pub fn call(event: &SyscallEvent, style: &Style, context: Context<'_>) -> Tokens {
    let mut out = entry(event, style, context);
    let spec = (!style.raw).then(|| specs::spec(event.call)).flatten();
    result(&mut out, event, spec, style, context);
    out
}

/// Formats ` = result`: what follows [`entry`], read against the
/// descriptors as the call left them.
#[must_use]
pub fn returned(event: &SyscallEvent, style: &Style, context: Context<'_>) -> Tokens {
    let mut out = Tokens::default();
    let spec = (!style.raw).then(|| specs::spec(event.call)).flatten();
    result(&mut out, event, spec, style, context);
    out
}

/// Each argument a call shows, with the kernel's name for it, apart: what
/// scripts read.
#[must_use]
pub fn argument_values(
    event: &SyscallEvent,
    style: &Style,
    context: Context<'_>,
) -> Vec<(String, Tokens)> {
    let Some(spec) = (!style.raw).then(|| specs::spec(event.call)).flatten() else {
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
            argument(&mut out, event, index, *arg, spec, style, context);
            let name = names
                .and_then(|names| names.get(index))
                .map_or_else(|| format!("arg{index}"), |&name| name.to_owned());
            (name, out)
        })
        .collect()
}

/// Formats `name(arguments)`: a call as its entry shows it.
#[must_use]
pub fn entry(event: &SyscallEvent, style: &Style, context: Context<'_>) -> Tokens {
    let mut out = Tokens::default();
    let name = event.call.display_name();
    out.push(Kind::Name, name.as_ref());
    out.punct("(");
    let spec = (!style.raw).then(|| specs::spec(event.call)).flatten();

    match spec {
        Some(spec) => arguments(&mut out, event, spec, style, context),
        None => {
            // Without a spec every word prints, in hex.
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

/// How many of a call's arguments strace prints: some calls' last
/// arguments mean nothing for some of their operations.
fn shown_arguments(event: &SyscallEvent, spec: &Spec) -> usize {
    const O_CREAT: u64 = 0o100;
    const O_TMPFILE: u64 = 0o20_000_000;
    let a = event.args;
    let all = spec.args.len();

    match event.call.name().unwrap_or_default() {
        "open" if a[1] & (O_CREAT | O_TMPFILE) == 0 => 2,
        "openat" if a[2] & (O_CREAT | O_TMPFILE) == 0 => 3,
        // F_GETFD, F_GETFL, F_GETOWN, F_GETSIG, F_GETLEASE, F_GETPIPE_SZ,
        // and F_GET_SEALS take nothing.
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

/// strace's own form of `clone`: named arguments, of which only those
/// the flags use.
fn clone_arguments(out: &mut Tokens, event: &SyscallEvent, style: &Style) {
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    let [flag_word, stack, parent_tid, child_tid, tls, _] = event.args;
    out.field("child_stack");
    pointer(out, stack);
    out.punct(", ");
    out.field("flags");
    flags(out, event, "clone", flag_word, style);
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

fn arguments(
    out: &mut Tokens,
    event: &SyscallEvent,
    spec: &Spec,
    style: &Style,
    context: Context<'_>,
) {
    if event.call.name() == Some("clone")
        && event.call.abi == Abi::X86_64
        && style.constants != Constants::Raw
    {
        clone_arguments(out, event, style);
        return;
    }

    let names = style
        .arg_names
        .then(|| specs::arg_names(event.call))
        .flatten();
    let shown = shown_arguments(event, spec);

    for (index, arg) in spec.args.iter().enumerate().take(shown) {
        if index > 0 {
            out.punct(", ");
        }

        if let Some(name) = names.and_then(|names| names.get(index)) {
            out.field(name);
        }

        argument(out, event, index, *arg, spec, style, context);
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

/// A descriptor, with what it refers to under `-y`.
pub(super) fn descriptor(
    out: &mut Tokens,
    process: Pid,
    fd: i64,
    style: &Style,
    context: Context<'_>,
) {
    out.push(Kind::Fd, fd.to_string());
    if style.fds == FdStyle::Number || fd < 0 {
        return;
    }

    let described = context.fds.and_then(|table| {
        if context.before {
            table.describe_before(process, fd)
        } else {
            table.describe(process, fd)
        }
    });
    if described.is_none() && closed(process, context) {
        return;
    }

    out.punct("<");
    match described {
        Some(description) => {
            out.push(Kind::Path, description.text.as_ref());
            if let (FdStyle::All, FdKind::Device(device)) = (style.fds, &description.kind) {
                out.punct("<");
                out.push(
                    Kind::Path,
                    format!(
                        "{} {}:{}",
                        if device.block { "block" } else { "char" },
                        device.major,
                        device.minor
                    ),
                );
                out.punct(">");
            }
        }
        None => out.push(Kind::Missing, "?"),
    }

    out.punct(">");
}

/// Whether a descriptor the table lacks is known not to be open, which
/// strace prints as a bare number.
fn closed(process: Pid, context: Context<'_>) -> bool {
    context.fds.is_some_and(|table| table.knows(process))
}

fn argument(
    out: &mut Tokens,
    event: &SyscallEvent,
    index: usize,
    arg: Arg,
    spec: &Spec,
    style: &Style,
    context: Context<'_>,
) {
    let word = event.args[index];

    // -X raw changes only names: numbers, sizes, and NULL print as ever.
    match arg {
        Arg::Int | Arg::Pid | Arg::Uid | Arg::Gid => out.number(int(word)),
        Arg::Long => out.number(long(word)),
        Arg::Uint => out.number(int(word).cast_unsigned()),
        Arg::Size => out.number(word),
        Arg::Hex => hex(out, word),
        Arg::Octal | Arg::Mode => octal(out, word & 0xffff_ffff),
        Arg::Ptr => pointer(out, word),
        Arg::Fd => descriptor(out, event.process, fd_value(word), style, context),
        Arg::DirFd => {
            if int(word) == libc::AT_FDCWD {
                match style.constants {
                    Constants::Abbrev => out.push(Kind::Constant, "AT_FDCWD"),
                    Constants::Raw => out.number(libc::AT_FDCWD),
                    Constants::Verbose => {
                        out.number(libc::AT_FDCWD);
                        out.push(Kind::Comment, " /* AT_FDCWD */");
                    }
                }

                // strace names the working directory under -y.
                if style.fds != FdStyle::Number
                    && let Some(cwd) = context
                        .fds
                        .and_then(|table| table.describe(event.process, i64::from(libc::AT_FDCWD)))
                {
                    out.punct("<");
                    out.push(Kind::Path, cwd.text.as_ref());
                    out.punct(">");
                }
            } else {
                descriptor(out, event.process, fd_value(word), style, context);
            }
        }
        Arg::Signal => constant(
            out,
            &signal(int(word)),
            u64::from(int(word).cast_unsigned()),
            style,
        ),
        Arg::Flags(set) => flags(out, event, set, word, style),
        // Protocols have names only within the internet families.
        Arg::Enum("ipproto") if !matches!(event.args[0], 2 | 10) => out.number(int(word)),
        Arg::Enum(set) => enumeration(out, set, word, style),
        Arg::Path | Arg::Str => string_argument(out, event, index, word, arg == Arg::Path, style),
        Arg::StrArray => string_array(out, event, index, word, style),
        Arg::Mem {
            direction,
            kind,
            count,
        } => memory(
            out, event, index, direction, kind, count, spec, style, context,
        ),
    }
}

fn constant(out: &mut Tokens, name: &str, value: u64, style: &Style) {
    match style.constants {
        Constants::Abbrev => out.push(Kind::Constant, name),
        Constants::Raw => hex(out, value),
        Constants::Verbose => {
            hex(out, value);
            out.push(Kind::Comment, format!(" /* {name} */"));
        }
    }
}

/// Bits by name, `|`-joined, with any left over in hex.
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

fn flags(out: &mut Tokens, event: &SyscallEvent, set: &str, word: u64, style: &Style) {
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

            // O_SYNC is O_DSYNC with a bit of its own.
            if rest & 0o4_010_000 == 0o4_010_000 {
                parts.push("O_SYNC".to_owned());
                rest &= !0o4_010_000;
            }

            // O_TMPFILE includes O_DIRECTORY.
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
            // The low byte is the signal the child sends at its exit.
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

    constant(out, &parts.join("|"), value, style);
}

fn enumeration(out: &mut Tokens, set: &str, word: u64, style: &Style) {
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
        // The operation, then its modifiers.
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

        constant(out, &text, value, style);
        return;
    }

    match definition.names.iter().find(|&&(v, _)| v == value) {
        Some(&(_, name)) => constant(out, name, value, style),
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

/// Escapes bytes as strace quotes them.
fn quote(out: &mut Tokens, bytes: &[u8], limit: Option<usize>, truncated: bool, style: &Style) {
    let shown = limit.map_or(bytes, |limit| &bytes[..bytes.len().min(limit)]);
    let mut text = String::with_capacity(shown.len() + 2);
    text.push('"');
    let all_hex = style.hex == Hex::All
        || (style.hex == Hex::NonAscii
            && shown
                .iter()
                .any(|&byte| !(0x20..0x7f).contains(&byte) && !b"\t\n\r".contains(&byte)));

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
                // An octal escape stays short unless a digit follows it.
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

/// An address the engine could not read through, or chose not to.
fn unread(out: &mut Tokens, word: u64, capture: Option<&Capture>) {
    pointer(out, word);
    if word != 0 && capture.is_some_and(|capture| capture.state == CaptureState::Unreadable) {
        out.push(Kind::Missing, " <unreadable>");
    }
}

fn string_argument(
    out: &mut Tokens,
    event: &SyscallEvent,
    index: usize,
    word: u64,
    path: bool,
    style: &Style,
) {
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

            // Paths print whole, as strace prints them.
            quote(
                out,
                bytes,
                (!path).then_some(style.string_limit),
                truncated,
                style,
            );
        }
        _ => unread(out, word, capture),
    }
}

fn string_array(out: &mut Tokens, event: &SyscallEvent, index: usize, word: u64, style: &Style) {
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

    // The environment, the second array, abbreviates to its size.
    if index == 2
        && !style.verbose
        && event
            .call
            .name()
            .is_some_and(|name| name.starts_with("execve"))
    {
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

        quote(out, element, Some(style.string_limit), false, style);
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
    style: &Style,
    context: Context<'_>,
) {
    let word = event.args[index];
    #[expect(clippy::cast_possible_truncation, reason = "six arguments")]
    let capture = event.capture(index as u8);

    // What a failed call would have written is not there.
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
                Count::Arg(arg) => {
                    usize::try_from(event.args[usize::from(arg)]).unwrap_or(usize::MAX)
                }
                Count::PointedBy(_) | Count::One => capture.bytes.len(),
            };

            let shown = &capture.bytes[..capture.bytes.len().min(length)];
            let truncated =
                matches!(capture.state, CaptureState::Truncated { .. }) || shown.len() < length;

            // Random bytes are not text; strace prints them in hex.
            let random = Style {
                hex: Hex::All,
                ..style.clone()
            };

            let style = if event.call.name() == Some("getrandom") {
                &random
            } else {
                style
            };

            quote(out, shown, Some(style.string_limit), truncated, style);
        }
        MemKind::Struct(structure) if event.call.abi != Abi::X86_64 => {
            // Compat layouts differ; the address is all that prints.
            let _ = structure;
            pointer(out, word);
        }
        MemKind::Struct(structure) => {
            let elements = match count {
                Count::One | Count::PointedBy(_) => 1,
                Count::Arg(arg) => usize::try_from(event.args[usize::from(arg)]).unwrap_or(0),
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
                style,
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

/// The ` = result` part.
fn result(
    out: &mut Tokens,
    event: &SyscallEvent,
    spec: Option<&Spec>,
    style: &Style,
    context: Context<'_>,
) {
    out.punct(" = ");
    match event.outcome {
        Outcome::Returned(value) => {
            let ret = spec.map_or(Ret::Int, |spec| spec.ret);

            match ret {
                Ret::Hex => hex(out, value.cast_unsigned()),
                Ret::Octal => octal(out, value.cast_unsigned()),
                Ret::Fd => descriptor(out, event.process, value, style, context),
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

/// How finely `-T` and `-r` print a time: strace's PRECISION.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Precision {
    Seconds,
    Milliseconds,
    #[default]
    Microseconds,
    Nanoseconds,
}

impl Precision {
    /// strace's names, in any case: `s`, `ms`, `us`, `ns`.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "s" => Some(Self::Seconds),
            "ms" => Some(Self::Milliseconds),
            "us" => Some(Self::Microseconds),
            "ns" => Some(Self::Nanoseconds),
            _ => None,
        }
    }

    /// The digits after the point, and the nanoseconds in the last.
    const fn digits(self) -> (usize, u64) {
        match self {
            Self::Seconds => (0, 1_000_000_000),
            Self::Milliseconds => (3, 1_000_000),
            Self::Microseconds => (6, 1_000),
            Self::Nanoseconds => (9, 1),
        }
    }
}

/// Seconds to `precision`, cut rather than rounded, as `-T` and `-r` print
/// them: `0.000003` in microseconds, `0` in seconds.
#[must_use]
pub fn seconds(nanoseconds: u64, precision: Precision) -> String {
    let whole = nanoseconds / 1_000_000_000;
    match precision.digits() {
        (0, _) => whole.to_string(),
        (digits, unit) => format!("{whole}.{:0digits$}", nanoseconds % 1_000_000_000 / unit),
    }
}

/// A duration for people: `11 µs`, `2.41 s`.
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

/// strace's line for an event other than a call.
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
