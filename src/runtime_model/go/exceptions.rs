//! What Go's runtime reports as a program panics or fails, read as the
//! runtime prints it: `printpanics`' chain of panics, the messages `throw`
//! and `fatal` are given, and the names `fatalsignal` prints for signals.
//!
//! Each report is read where the runtime is entered to make it, from the
//! arguments the register ABI passes: the first in rax, the second in rbx.

use std::sync::Arc;

use super::layout::{Missing, constant, member, offset, symbol};
use super::types::TypeTables;
use super::{read_unsigned, word};
use crate::runtime_model::{RuntimeException, RuntimeHook, RuntimeImage, RuntimeStop};
use crate::unwind::{DwarfRegister, RAX, RBX, RegisterFile};
use crate::{ExceptionFilter, ImageAddress, LanguageExceptionKind, VirtualAddress};

/// What Go's runtime reports: a panic nothing recovered and a fatal error
/// stop unless chosen otherwise, and every panic as it begins does not.
pub const EXCEPTION_FILTERS: [ExceptionFilter; 3] = [UNHANDLED, FATAL, PANIC];

const UNHANDLED: ExceptionFilter = ExceptionFilter {
    id: "unhandled",
    language: "Go",
    kind: LanguageExceptionKind::Unhandled,
    label: "Unhandled Go panics",
    description: "Stop where a panic nothing recovered ends the program, with the frame that \
                  panicked selected",
    default: true,
};

const FATAL: ExceptionFilter = ExceptionFilter {
    id: "runtime-fatal",
    language: "Go",
    kind: LanguageExceptionKind::Fatal,
    label: "Fatal Go runtime errors",
    description: "Stop where Go's runtime ends the program with a fatal error, such as a \
                  deadlock",
    default: true,
};

const PANIC: ExceptionFilter = ExceptionFilter {
    id: "raised",
    language: "Go",
    kind: LanguageExceptionKind::Raised,
    label: "Every Go panic",
    description: "Stop wherever a panic begins, whether or not the program then recovers from \
                  it",
    default: false,
};

/// The most panics one chain is read through.
const MAX_PANICS: usize = 64;

/// The longest message read; the runtime prints longer ones in full, so
/// one longer is refused rather than cut.
const MAX_MESSAGE: u64 = 1 << 16;

/// The runtime functions that report exceptions as they are entered.
#[derive(Debug, Clone, Default)]
pub struct Hooks {
    /// `gopanic(e any)`, as any panic begins.
    panic: Option<ImageAddress>,

    /// `fatalpanic(msgs *_panic)`, as a panic nothing recovered ends the
    /// program.
    fatal_panic: Option<ImageAddress>,

    /// `throw(s string)` and `fatal(s string)`, the runtime's fatal errors.
    throws: Vec<ImageAddress>,

    /// `fatalsignal(sig uint32, ...)`, a signal the runtime cannot turn
    /// into a panic.
    fatal_signal: Option<ImageAddress>,

    /// Every one of them, with what it reports.
    all: Vec<RuntimeHook>,
}

impl Hooks {
    /// `gopanic` begins with the stack check by which the runtime grows and
    /// preempts a goroutine's stack, which calls `morestack` to run it
    /// again from its first instruction, so a panic is reported past the
    /// check, in its body, where its argument is still in its registers.
    /// Each of the rest is entered once: it checks no stack, or runs on the
    /// runtime's own.
    pub fn bind(image: &dyn RuntimeImage) -> Self {
        let entry = |name: &str| symbol(image, name).ok();
        let panic = image.function_body("runtime.gopanic");
        let fatal_panic = entry("runtime.fatalpanic");
        let throws = ["runtime.throw", "runtime.fatal"]
            .into_iter()
            .filter_map(entry)
            .collect::<Vec<_>>();
        let fatal_signal = entry("runtime.fatalsignal");
        let hook = |filter| move |address| RuntimeHook { filter, address };
        let all = panic
            .map(hook(&PANIC))
            .into_iter()
            .chain(fatal_panic.map(hook(&UNHANDLED)))
            .chain(throws.iter().copied().chain(fatal_signal).map(hook(&FATAL)))
            .collect();

        Self {
            panic,
            fatal_panic,
            throws,
            fatal_signal,
            all,
        }
    }

    pub fn all(&self) -> &[RuntimeHook] {
        &self.all
    }
}

/// How the runtime lays out what its reports read.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `_panic.arg`, an `any`, and the rest of a panic's state.
    arg: u64,
    link: u64,
    recovered: u64,
    repanicked: u64,
    goexit: u64,

    /// `abi.Type`'s kind, flags, and the offset of its name.
    kind: u64,
    flags: u64,
    name: u64,
    extra_star: u64,
    kinds: Kinds,

    /// The tables of each module's types, which a type's name is in.
    tables: TypeTables,

    /// `runtime.sigtable`, and where each entry's name is.
    signals: ImageAddress,
    signal_stride: u64,
    signal_name: u64,

    /// `errorAddressString.msg`, the text of the runtime's error for a
    /// fault at an address, which a program that never faults may lack.
    address_error_text: Option<u64>,
}

/// The kinds `printpanicval` prints by value.
#[derive(Debug, Clone, Copy)]
struct Kinds {
    boolean: u64,
    int: u64,
    int8: u64,
    int16: u64,
    int32: u64,
    int64: u64,
    uint: u64,
    uint8: u64,
    uint16: u64,
    uint32: u64,
    uint64: u64,
    uintptr: u64,
    float32: u64,
    float64: u64,
    complex64: u64,
    complex128: u64,
    string: u64,
}

impl Layout {
    pub fn bind(image: &dyn RuntimeImage) -> Result<Self, Missing> {
        const PANIC: &str = "runtime._panic";
        const TYPE: &str = "internal/abi.Type";
        let kind = |name: &str| constant(image, &format!("internal/abi.{name}"));
        Ok(Self {
            arg: offset(image, PANIC, &["arg"], 16)?,
            link: offset(image, PANIC, &["link"], 8)?,
            recovered: offset(image, PANIC, &["recovered"], 1)?,
            repanicked: offset(image, PANIC, &["repanicked"], 1)?,
            goexit: offset(image, PANIC, &["goexit"], 1)?,
            kind: offset(image, TYPE, &["Kind_"], 1)?,
            flags: offset(image, TYPE, &["TFlag"], 1)?,
            name: offset(image, TYPE, &["Str"], 4)?,
            extra_star: constant(image, "internal/abi.TFlagExtraStar")?,
            kinds: Kinds {
                boolean: kind("Bool")?,
                int: kind("Int")?,
                int8: kind("Int8")?,
                int16: kind("Int16")?,
                int32: kind("Int32")?,
                int64: kind("Int64")?,
                uint: kind("Uint")?,
                uint8: kind("Uint8")?,
                uint16: kind("Uint16")?,
                uint32: kind("Uint32")?,
                uint64: kind("Uint64")?,
                uintptr: kind("Uintptr")?,
                float32: kind("Float32")?,
                float64: kind("Float64")?,
                complex64: kind("Complex64")?,
                complex128: kind("Complex128")?,
                string: kind("String")?,
            },
            tables: TypeTables::bind(image)?,
            signals: symbol(image, "runtime.sigtable")?,
            signal_stride: member(image, "runtime.sigTabT", &[])?.size,
            signal_name: offset(image, "runtime.sigTabT", &["name"], 16)?,
            address_error_text: offset(image, "runtime.errorAddressString", &["msg"], 16).ok(),
        })
    }
}

/// The exception a thread reports as it enters the hook at `hook`.
pub fn exception(
    hooks: &Hooks,
    layout: &Result<Layout, Missing>,
    stop: &dyn RuntimeStop,
    hook: ImageAddress,
    registers: &RegisterFile,
) -> Result<RuntimeException, Arc<str>> {
    let argument = |register: DwarfRegister| {
        registers
            .get(register)
            .ok_or_else(|| Arc::<str>::from("the runtime's arguments are unavailable"))
    };

    let reader = Reader {
        stop,
        layout: layout.as_ref().map_err(Arc::clone)?,
    };

    if Some(hook) == hooks.fatal_panic {
        let panic = argument(RAX)?;
        let message = reader.panics(panic, 0)?;
        return Ok(RuntimeException {
            message: message.trim_end_matches('\n').into(),
            value: Some(format!("(*(runtime._panic*){panic:#x}).arg").into()),
        });
    }

    if Some(hook) == hooks.panic {
        return Ok(RuntimeException {
            message: reader.raised(argument(RAX)?, argument(RBX)?)?.into(),
            value: None,
        });
    }

    if hooks.throws.contains(&hook) {
        let text = reader.text(argument(RAX)?, argument(RBX)?)?;
        return Ok(RuntimeException {
            message: format!("fatal error: {}", indented(&text)).into(),
            value: None,
        });
    }

    if Some(hook) == hooks.fatal_signal {
        return Ok(RuntimeException {
            message: reader.signal(argument(RAX)? & u64::from(u32::MAX))?,
            value: None,
        });
    }

    Err("no exception is reported there".into())
}

struct Reader<'a> {
    stop: &'a dyn RuntimeStop,
    layout: &'a Layout,
}

impl Reader<'_> {
    fn word(&self, address: u64, what: &str) -> Result<u64, Arc<str>> {
        word(self.stop, VirtualAddress::new(address))
            .ok_or_else(|| format!("{what} is unreadable").into())
    }

    fn byte(&self, address: u64, what: &str) -> Result<u64, Arc<str>> {
        read_unsigned(self.stop, VirtualAddress::new(address), 1)
            .ok_or_else(|| format!("{what} is unreadable").into())
    }

    /// What `printpanics` prints for the panic at `panic` and the panics
    /// before it.
    fn panics(&self, panic: u64, depth: usize) -> Result<String, Arc<str>> {
        if depth == MAX_PANICS {
            return Err(format!("more than {MAX_PANICS} panics are chained").into());
        }

        let layout = self.layout;
        let field = |offset: u64| panic.wrapping_add(offset);
        let flag = |offset: u64, what| Ok::<_, Arc<str>>(self.byte(field(offset), what)? != 0);
        let mut printed = String::new();
        let link = self.word(field(layout.link), "a panic's link")?;
        if link != 0 {
            printed = self.panics(link, depth + 1)?;
            let previous = |offset: u64, what| {
                Ok::<_, Arc<str>>(self.byte(link.wrapping_add(offset), what)? != 0)
            };

            if previous(layout.repanicked, "a panic's state")? {
                return Ok(printed);
            }

            if !previous(layout.goexit, "a panic's state")? {
                printed.push('\t');
            }
        }

        if flag(layout.goexit, "a panic's state")? {
            return Ok(printed);
        }

        let arg = field(layout.arg);
        let value = self.panic_value(
            self.word(arg, "a panic's value")?,
            self.word(arg.wrapping_add(8), "a panic's value")?,
        )?;

        printed.push_str("panic: ");
        printed.push_str(&value);

        match (
            flag(layout.recovered, "a panic's state")?,
            flag(layout.repanicked, "a panic's state")?,
        ) {
            (true, true) => printed.push_str(" [recovered, repanicked]"),
            (true, false) => printed.push_str(" [recovered]"),
            _ => {}
        }

        printed.push('\n');
        Ok(printed)
    }

    /// A panic as it is raised. The runtime turns an error or a stringer
    /// into its text only as a panic ends the program, by calling the
    /// program's own method, so a value of a type that may have methods is
    /// named only by its type; a predeclared type has none, and the
    /// runtime's own errors carry their text as their methods return it.
    fn raised(&self, ty: u64, data: u64) -> Result<String, Arc<str>> {
        if ty == 0 {
            return Ok("panic: nil".to_owned());
        }

        let name = self.type_name(ty)?;
        if PREDECLARED.contains(&name.as_str()) {
            return Ok(format!("panic: {}", self.panic_value(ty, data)?));
        }

        let text = |address: u64| match self.scalar(self.layout.kinds.string, address)? {
            Some(Scalar::Text(text)) => Ok(indented(&text)),
            _ => Err(Arc::<str>::from("the runtime error's text is unreadable")),
        };

        Ok(match (name.as_str(), self.layout.address_error_text) {
            ("runtime.plainError", _) => format!("panic: {}", text(data)?),
            ("runtime.errorString", _) => format!("panic: runtime error: {}", text(data)?),
            ("runtime.errorAddressString", Some(offset)) => {
                format!("panic: runtime error: {}", text(data.wrapping_add(offset))?)
            }
            _ => format!("panic with a {name}"),
        })
    }

    /// What `printpanicval` prints for the value an `any` holds, given its
    /// type and data words.
    fn panic_value(&self, ty: u64, data: u64) -> Result<String, Arc<str>> {
        if ty == 0 {
            return Ok("nil".to_owned());
        }

        let kind = self.byte(ty.wrapping_add(self.layout.kind), "a type's kind")?;
        let name = self.type_name(ty)?;
        let Some(value) = self.scalar(kind, data)? else {
            // `printanycustomtype` prints any other value by its address.
            return Ok(format!("({name}) {data:#x}"));
        };

        if PREDECLARED.contains(&name.as_str()) {
            return Ok(match value {
                Scalar::Text(text) => indented(&text),
                Scalar::Number(number) => number,
            });
        }

        Ok(match value {
            Scalar::Text(text) => format!("{name}(\"{}\")", indented(&text)),
            // A complex number prints with its own parentheses.
            Scalar::Number(number) if number.starts_with('(') => format!("{name}{number}"),
            Scalar::Number(number) => format!("{name}({number})"),
        })
    }

    /// A value of a kind the runtime prints by value, read where `data`
    /// points, or `None` for any other kind.
    fn scalar(&self, kind: u64, data: u64) -> Result<Option<Scalar>, Arc<str>> {
        let kinds = self.layout.kinds;
        let read = |size: usize| {
            read_unsigned(self.stop, VirtualAddress::new(data), size)
                .ok_or_else(|| Arc::<str>::from("the panic's value is unreadable"))
        };

        let signed = |size: usize| -> Result<Option<Scalar>, Arc<str>> {
            let shift = 64 - size * 8;
            #[expect(
                clippy::cast_possible_wrap,
                reason = "the bits are reinterpreted as the two's complement they are"
            )]
            let value = ((read(size)? << shift) as i64) >> shift;
            Ok(Some(Scalar::Number(value.to_string())))
        };

        let unsigned =
            |size: usize| Ok::<_, Arc<str>>(Some(Scalar::Number(read(size)?.to_string())));
        let single = |bits: u64| {
            u32::try_from(bits)
                .map(|bits| f64::from(f32::from_bits(bits)))
                .map_err(|_| Arc::<str>::from("the panic's value is unreadable"))
        };

        Ok(match kind {
            kind if kind == kinds.boolean => Some(Scalar::Number((read(1)? != 0).to_string())),
            kind if kind == kinds.int || kind == kinds.int64 => signed(8)?,
            kind if kind == kinds.int8 => signed(1)?,
            kind if kind == kinds.int16 => signed(2)?,
            kind if kind == kinds.int32 => signed(4)?,
            kind if kind == kinds.uint || kind == kinds.uint64 || kind == kinds.uintptr => {
                unsigned(8)?
            }
            kind if kind == kinds.uint8 => unsigned(1)?,
            kind if kind == kinds.uint16 => unsigned(2)?,
            kind if kind == kinds.uint32 => unsigned(4)?,
            kind if kind == kinds.float32 => {
                Some(Scalar::Number(go_float(single(read(4)?)?, true)))
            }
            kind if kind == kinds.float64 => {
                Some(Scalar::Number(go_float(f64::from_bits(read(8)?), false)))
            }
            kind if kind == kinds.complex64 => {
                let bits = read(8)?;
                Some(Scalar::Number(go_complex(
                    single(bits & u64::from(u32::MAX))?,
                    single(bits >> 32)?,
                    true,
                )))
            }
            kind if kind == kinds.complex128 => {
                let imaginary = read_unsigned(self.stop, VirtualAddress::new(data + 8), 8)
                    .ok_or("the panic's value is unreadable")?;
                Some(Scalar::Number(go_complex(
                    f64::from_bits(read(8)?),
                    f64::from_bits(imaginary),
                    false,
                )))
            }
            kind if kind == kinds.string => Some(Scalar::Text(self.text(
                self.word(data, "the panic's string")?,
                self.word(data.wrapping_add(8), "the panic's string")?,
            )?)),
            _ => None,
        })
    }

    /// The text of a Go string with this data pointer and length.
    fn text(&self, data: u64, length: u64) -> Result<String, Arc<str>> {
        if length > MAX_MESSAGE {
            return Err(
                format!("a {length}-byte message is longer than the debugger reads").into(),
            );
        }

        let mut bytes = vec![0; usize::try_from(length).map_err(|_| "a message is too long")?];
        if !self.stop.read(VirtualAddress::new(data), &mut bytes) {
            return Err("the message is unreadable".into());
        }

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The name the runtime prints for a type: `toRType(t).string()`, its
    /// name resolved in the module whose types hold it.
    fn type_name(&self, ty: u64) -> Result<String, Arc<str>> {
        let layout = self.layout;
        let offset = read_unsigned(
            self.stop,
            VirtualAddress::new(ty.wrapping_add(layout.name)),
            4,
        )
        .ok_or("a type's name is unreadable")?;
        let name = layout.tables.base(self.stop, ty)?.wrapping_add(offset);

        // A name is a flags byte, its length as a varint, then its bytes.
        let mut length = 0_u64;
        let mut at = name.wrapping_add(1);
        for shift in (0..35).step_by(7) {
            let byte = self.byte(at, "a type's name")?;
            at = at.wrapping_add(1);
            length |= (byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                break;
            }
        }

        let text = self.text(at, length)?;
        let flags = self.byte(ty.wrapping_add(layout.flags), "a type's flags")?;
        if flags & layout.extra_star != 0 {
            return Ok(text.get(1..).unwrap_or_default().to_owned());
        }

        Ok(text)
    }

    /// The name `fatalsignal` prints for a signal.
    fn signal(&self, signal: u64) -> Result<Arc<str>, Arc<str>> {
        let layout = self.layout;
        let entry = self
            .stop
            .load_bias()
            .wrapping_load(layout.signals)
            .get()
            .wrapping_add(signal.wrapping_mul(layout.signal_stride))
            .wrapping_add(layout.signal_name);
        let name = self.text(
            self.word(entry, "the signal's name")?,
            self.word(entry.wrapping_add(8), "the signal's name")?,
        )?;
        Ok(name.into())
    }
}

/// The runtime's predeclared types that `printpanicval` prints bare.
const PREDECLARED: [&str; 17] = [
    "bool",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "string",
];

enum Scalar {
    Text(String),
    Number(String),
}

/// `printindented`: each newline is followed by a tab.
fn indented(text: &str) -> String {
    text.replace('\n', "\n\t")
}

/// A float as the runtime prints it, as `strconv.FormatFloat(v, 'g', -1,
/// bits)` does: the shortest decimal that reads back as the same value,
/// in exponent form when its decimal exponent is below -4 or at least 6.
fn go_float(value: f64, single: bool) -> String {
    // Rust's `{:e}` always has an exponent; the value is shown as it is if not.
    go_digits(value, single).unwrap_or_else(|| format!("{value:e}"))
}

fn go_digits(value: f64, single: bool) -> Option<String> {
    if value.is_nan() {
        return Some("NaN".to_owned());
    }

    if value.is_infinite() {
        return Some(if value > 0.0 { "+Inf" } else { "-Inf" }.to_owned());
    }

    let negative = value.is_sign_negative();

    // Rust's `{:e}` prints the same shortest digits.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a float32's value was widened from one"
    )]
    let scientific = if single {
        format!("{:e}", (value as f32).abs())
    } else {
        format!("{:e}", value.abs())
    };

    let (mantissa, exponent) = scientific.split_once('e')?;
    let digits = mantissa.replace('.', "");
    let exponent: i64 = exponent.parse().ok()?;
    let sign = if negative { "-" } else { "" };
    if digits == "0" {
        return Some(format!("{sign}0"));
    }

    let count = i64::try_from(digits.len()).ok()?;
    if !(-4..6).contains(&exponent) {
        let (first, rest) = digits.split_at(1);
        let point = if rest.is_empty() { "" } else { "." };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        return Some(format!(
            "{sign}{first}{point}{rest}e{exponent_sign}{:02}",
            exponent.unsigned_abs()
        ));
    }

    // The decimal point falls after `exponent + 1` digits.
    let point = exponent + 1;
    let mut text = sign.to_owned();
    if point <= 0 {
        text.push_str("0.");
        text.push_str(&"0".repeat(usize::try_from(-point).ok()?));
        text.push_str(&digits);
    } else if point >= count {
        text.push_str(&digits);
        text.push_str(&"0".repeat(usize::try_from(point - count).ok()?));
    } else {
        let (whole, fraction) = digits.split_at(usize::try_from(point).ok()?);
        text.push_str(whole);
        text.push('.');
        text.push_str(fraction);
    }

    Some(text)
}

/// A complex number as the runtime prints it: `strconv.FormatComplex`, its
/// imaginary part always signed.
fn go_complex(real: f64, imaginary: f64, single: bool) -> String {
    let imaginary = go_float(imaginary, single);
    let sign = if imaginary.starts_with(['+', '-']) {
        ""
    } else {
        "+"
    };

    format!("({}{sign}{imaginary}i)", go_float(real, single))
}

#[cfg(test)]
mod tests {
    use super::{go_complex, go_float};

    /// Each case is what Go 1.27's `print` writes for the value.
    #[test]
    fn floats_print_as_the_runtime_prints_them() {
        for (value, single, printed) in [
            (1.5, false, "1.5"),
            (-40.0, false, "-40"),
            (0.0, false, "0"),
            (-0.0, false, "-0"),
            (100_000.0, false, "100000"),
            (1_000_000.0, false, "1e+06"),
            (123_456_789.0, false, "1.23456789e+08"),
            (0.0001, false, "0.0001"),
            (0.000_012_5, false, "1.25e-05"),
            (1e21, false, "1e+21"),
            (1e-300, false, "1e-300"),
            (0.1, true, "0.1"),
            (f64::INFINITY, false, "+Inf"),
            (f64::NEG_INFINITY, false, "-Inf"),
            (f64::NAN, false, "NaN"),
        ] {
            assert_eq!(go_float(value, single), printed, "{value}");
        }

        assert_eq!(go_complex(1.5, -2.0, false), "(1.5-2i)");
        assert_eq!(go_complex(0.1, 3.0, true), "(0.1+3i)");
    }
}
