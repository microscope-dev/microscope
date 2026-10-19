//! Syntax highlighting for source listings: a small lexer per language that
//! finds keywords, strings, comments, and numbers. A file is lexed from its
//! start, so a comment or string that opened above the lines shown is
//! still known.

use std::ops::Range;
use std::path::Path;

use super::terminal::{Renderer, Role};

/// A language the lexer knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    C,
    Cpp,
    Rust,
    Go,
    Zig,
    Odin,

    /// Fortran's free form.
    Fortran,
    D,
    Nim,
    Ada,
    Python,
}

const C_KEYWORDS: &[&str] = &[
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_Bool",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "auto",
    "bool",
    "break",
    "case",
    "char",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "true",
    "typedef",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
];
const CPP_KEYWORDS: &[&str] = &[
    "alignas",
    "alignof",
    "asm",
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "char8_t",
    "char16_t",
    "char32_t",
    "class",
    "co_await",
    "co_return",
    "co_yield",
    "concept",
    "const",
    "const_cast",
    "consteval",
    "constexpr",
    "constinit",
    "continue",
    "decltype",
    "default",
    "delete",
    "do",
    "double",
    "dynamic_cast",
    "else",
    "enum",
    "explicit",
    "export",
    "extern",
    "false",
    "final",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "nullptr",
    "operator",
    "override",
    "private",
    "protected",
    "public",
    "register",
    "reinterpret_cast",
    "requires",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "static_assert",
    "static_cast",
    "struct",
    "switch",
    "template",
    "this",
    "thread_local",
    "throw",
    "true",
    "try",
    "typedef",
    "typeid",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "wchar_t",
    "while",
];
const RUST_KEYWORDS: &[&str] = &[
    "Self", "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "static", "struct", "super", "trait", "true", "type",
    "union", "unsafe", "use", "where", "while", "yield",
];
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "iota",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];
const ZIG_KEYWORDS: &[&str] = &[
    "addrspace",
    "align",
    "allowzero",
    "and",
    "anyframe",
    "anytype",
    "asm",
    "break",
    "callconv",
    "catch",
    "comptime",
    "const",
    "continue",
    "defer",
    "else",
    "enum",
    "errdefer",
    "error",
    "export",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "inline",
    "linksection",
    "noalias",
    "noinline",
    "nosuspend",
    "null",
    "opaque",
    "or",
    "orelse",
    "packed",
    "pub",
    "resume",
    "return",
    "struct",
    "suspend",
    "switch",
    "test",
    "threadlocal",
    "true",
    "try",
    "undefined",
    "union",
    "unreachable",
    "usingnamespace",
    "var",
    "volatile",
    "while",
];

const ODIN_KEYWORDS: &[&str] = &[
    "asm",
    "auto_cast",
    "bit_set",
    "break",
    "case",
    "cast",
    "context",
    "continue",
    "defer",
    "distinct",
    "do",
    "dynamic",
    "else",
    "enum",
    "fallthrough",
    "false",
    "for",
    "foreign",
    "if",
    "import",
    "in",
    "map",
    "matrix",
    "nil",
    "not_in",
    "or_break",
    "or_continue",
    "or_else",
    "or_return",
    "package",
    "proc",
    "return",
    "struct",
    "switch",
    "transmute",
    "true",
    "typeid",
    "union",
    "using",
    "when",
    "where",
];
const FORTRAN_KEYWORDS: &[&str] = &[
    "allocatable",
    "allocate",
    "associate",
    "block",
    "call",
    "case",
    "character",
    "class",
    "close",
    "complex",
    "contains",
    "contiguous",
    "cycle",
    "data",
    "deallocate",
    "default",
    "dimension",
    "do",
    "elemental",
    "else",
    "elseif",
    "end",
    "enddo",
    "endif",
    "exit",
    "external",
    "function",
    "goto",
    "if",
    "implicit",
    "in",
    "inout",
    "integer",
    "intent",
    "interface",
    "intrinsic",
    "logical",
    "module",
    "none",
    "nullify",
    "only",
    "open",
    "optional",
    "out",
    "parameter",
    "pointer",
    "print",
    "private",
    "procedure",
    "program",
    "public",
    "pure",
    "read",
    "real",
    "recursive",
    "result",
    "return",
    "save",
    "select",
    "stop",
    "subroutine",
    "target",
    "then",
    "type",
    "use",
    "value",
    "where",
    "while",
    "write",
];
const D_KEYWORDS: &[&str] = &[
    "abstract",
    "alias",
    "align",
    "asm",
    "assert",
    "auto",
    "bool",
    "break",
    "byte",
    "case",
    "cast",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "dchar",
    "debug",
    "default",
    "delegate",
    "delete",
    "deprecated",
    "do",
    "double",
    "else",
    "enum",
    "export",
    "extern",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "foreach",
    "foreach_reverse",
    "function",
    "goto",
    "if",
    "immutable",
    "import",
    "in",
    "inout",
    "int",
    "interface",
    "invariant",
    "is",
    "lazy",
    "long",
    "mixin",
    "module",
    "new",
    "nothrow",
    "null",
    "out",
    "override",
    "package",
    "pragma",
    "private",
    "protected",
    "public",
    "pure",
    "real",
    "ref",
    "return",
    "scope",
    "shared",
    "short",
    "static",
    "struct",
    "super",
    "switch",
    "synchronized",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typeid",
    "typeof",
    "ubyte",
    "uint",
    "ulong",
    "union",
    "unittest",
    "ushort",
    "version",
    "void",
    "wchar",
    "while",
    "with",
];
const NIM_KEYWORDS: &[&str] = &[
    "addr",
    "and",
    "as",
    "asm",
    "bind",
    "block",
    "break",
    "case",
    "cast",
    "concept",
    "const",
    "continue",
    "converter",
    "defer",
    "discard",
    "distinct",
    "div",
    "do",
    "elif",
    "else",
    "end",
    "enum",
    "except",
    "export",
    "false",
    "finally",
    "for",
    "from",
    "func",
    "if",
    "import",
    "in",
    "include",
    "interface",
    "is",
    "isnot",
    "iterator",
    "let",
    "macro",
    "method",
    "mixin",
    "mod",
    "nil",
    "not",
    "notin",
    "object",
    "of",
    "or",
    "out",
    "proc",
    "ptr",
    "raise",
    "ref",
    "return",
    "shl",
    "shr",
    "static",
    "template",
    "true",
    "try",
    "tuple",
    "type",
    "using",
    "var",
    "when",
    "while",
    "xor",
    "yield",
];
const ADA_KEYWORDS: &[&str] = &[
    "abort",
    "abs",
    "abstract",
    "accept",
    "access",
    "aliased",
    "all",
    "and",
    "array",
    "at",
    "begin",
    "body",
    "case",
    "constant",
    "declare",
    "delay",
    "delta",
    "digits",
    "do",
    "else",
    "elsif",
    "end",
    "entry",
    "exception",
    "exit",
    "for",
    "function",
    "generic",
    "goto",
    "if",
    "in",
    "interface",
    "is",
    "limited",
    "loop",
    "mod",
    "new",
    "not",
    "null",
    "of",
    "or",
    "others",
    "out",
    "overriding",
    "package",
    "parallel",
    "pragma",
    "private",
    "procedure",
    "protected",
    "raise",
    "range",
    "record",
    "rem",
    "renames",
    "requeue",
    "return",
    "reverse",
    "select",
    "separate",
    "some",
    "subtype",
    "synchronized",
    "tagged",
    "task",
    "terminate",
    "then",
    "type",
    "until",
    "use",
    "when",
    "while",
    "with",
    "xor",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "case", "class",
    "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if",
    "import", "in", "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return",
    "try", "type", "while", "with", "yield",
];

/// Whether a Python keyword at the end of `text[..end]` is one there.
/// `match`, `case`, and `type` are soft: keywords only where they begin
/// the statement they name, a `match` or `case` ending in a colon or a
/// `type` alias. Elsewhere they are names, as `type(x)` calls one.
fn python_soft_keyword_holds(text: &[u8], word: &str, end: usize, line_start: bool) -> bool {
    if !matches!(word, "match" | "case" | "type") {
        return true;
    }

    if !line_start {
        return false;
    }

    let line_end = text[end..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(text.len(), |offset| end + offset);
    let rest = &text[end..line_end];
    let after = rest.trim_ascii_start();

    if word == "type" {
        // `type Name = ...` or `type Name[T] = ...`.
        let name = after
            .iter()
            .take_while(|&&byte| is_identifier(byte))
            .count();
        let tail = after[name..].trim_ascii_start();
        return after.len() < rest.len()
            && name > 0
            && (tail.starts_with(b"[") || tail.starts_with(b"=") && !tail.starts_with(b"=="));
    }

    // A name assigned, annotated, or used as an object is no statement.
    if after.first().is_some_and(|byte| b"=:.,)".contains(byte)) {
        return false;
    }

    // The statement's colon ends its line, before any comment.
    let mut quote = None;
    let mut code = rest.len();
    for (offset, &byte) in rest.iter().enumerate() {
        match (quote, byte) {
            (None, b'#') => {
                code = offset;
                break;
            }
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), _) if byte == open => quote = None,
            _ => {}
        }
    }

    rest[..code].trim_ascii_end().ends_with(b":")
}

/// The prefixes a Python string may begin with, in any case.
const PYTHON_STRING_PREFIXES: &[&[u8]] = &[
    b"", b"r", b"u", b"b", b"f", b"t", b"rb", b"br", b"fr", b"rf", b"tr", b"rt",
];

impl Language {
    /// The language a file's extension names. A `.h` header is C's.
    pub fn of(path: &Path) -> Option<Self> {
        Some(match path.extension()?.to_str()? {
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "c++" | "hh" | "hpp" | "hxx" | "h++" | "ipp" | "tcc" => {
                Self::Cpp
            }
            "rs" => Self::Rust,
            "go" => Self::Go,
            "zig" => Self::Zig,
            "odin" => Self::Odin,
            "f90" | "F90" | "f95" | "F95" | "f03" | "F03" | "f08" | "F08" | "f18" | "F18" => {
                Self::Fortran
            }
            "d" | "di" => Self::D,
            "nim" | "nims" => Self::Nim,
            "adb" | "ads" => Self::Ada,
            "py" | "pyi" => Self::Python,
            _ => return None,
        })
    }

    const fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::C => C_KEYWORDS,
            Self::Cpp => CPP_KEYWORDS,
            Self::Rust => RUST_KEYWORDS,
            Self::Go => GO_KEYWORDS,
            Self::Zig => ZIG_KEYWORDS,
            Self::Odin => ODIN_KEYWORDS,
            Self::Fortran => FORTRAN_KEYWORDS,
            Self::D => D_KEYWORDS,
            Self::Nim => NIM_KEYWORDS,
            Self::Ada => ADA_KEYWORDS,
            Self::Python => PYTHON_KEYWORDS,
        }
    }

    /// Whether a keyword is one in any case.
    const fn ignores_case(self) -> bool {
        matches!(self, Self::Fortran | Self::Ada)
    }

    /// What begins a comment that runs to the line's end.
    const fn line_comment(self) -> &'static [u8] {
        match self {
            Self::Fortran => b"!",
            Self::Nim | Self::Python => b"#",
            Self::Ada => b"--",
            _ => b"//",
        }
    }

    /// The block comments: what opens and closes each, and whether they
    /// nest.
    const fn block_comments(self) -> &'static [(&'static [u8], &'static [u8], bool)] {
        match self {
            Self::C | Self::Cpp | Self::Go => &[(b"/*", b"*/", false)],
            Self::Rust | Self::Odin => &[(b"/*", b"*/", true)],
            Self::D => &[(b"/*", b"*/", false), (b"/+", b"+/", true)],
            Self::Nim => &[(b"#[", b"]#", true)],
            Self::Zig | Self::Fortran | Self::Ada | Self::Python => &[],
        }
    }

    /// Whether a quote within a string is written twice, rather than
    /// escaped with a backslash.
    const fn doubles_quotes(self) -> bool {
        matches!(self, Self::Fortran | Self::Ada)
    }
}

/// What a span of source is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    String,
    Comment,
    Number,
}

/// A highlighted byte range of one line.
pub type Span = (Range<usize>, Kind);

/// `text` with its `spans` painted and its tabs expanded to stops every
/// `tab_width` columns, or kept with a width of 0.
pub fn render(text: &str, spans: &[Span], tab_width: usize, renderer: Renderer) -> String {
    let mut output = String::with_capacity(text.len());
    let mut column = 0;

    let expand = |segment: &str, column: &mut usize| {
        let mut expanded = String::with_capacity(segment.len());
        for character in segment.chars() {
            if character == '\t' && tab_width != 0 {
                let spaces = tab_width - *column % tab_width;
                expanded.extend(std::iter::repeat_n(' ', spaces));
                *column += spaces;
            } else {
                expanded.push(character);
                *column += 1;
            }
        }

        expanded
    };

    let mut position = 0;

    for (range, kind) in spans {
        let range = range.start.min(text.len())..range.end.min(text.len());
        if range.start < position
            || !text.is_char_boundary(range.start)
            || !text.is_char_boundary(range.end)
        {
            continue;
        }

        output.push_str(&expand(&text[position..range.start], &mut column));

        let role = match kind {
            Kind::Keyword => Role::Keyword,
            Kind::String => Role::String,
            Kind::Comment => Role::Comment,
            Kind::Number => Role::Number,
        };

        let segment = expand(&text[range.clone()], &mut column);
        output.push_str(&renderer.paint(role, segment).to_string());
        position = range.end;
    }

    output.push_str(&expand(&text[position..], &mut column));
    output
}

const fn is_identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Lexes `text` from its start and returns each line's spans, by line.
pub fn lex(text: &str, language: Language) -> Vec<Vec<Span>> {
    let tokens = tokens(text.as_bytes(), language);

    // Each token is split at the lines it spans.
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut line_start = 0;
    let mut tokens = tokens.into_iter().peekable();

    for (index, byte) in text.bytes().chain(std::iter::once(b'\n')).enumerate() {
        if byte != b'\n' {
            continue;
        }

        let line = line_start..index;

        while let Some((range, kind)) = tokens.peek().cloned() {
            if range.start >= line.end {
                break;
            }

            let start = range.start.max(line.start);
            let end = range.end.min(line.end);
            if start < end {
                spans.push((start - line.start..end - line.start, kind));
            }

            if range.end <= line.end {
                tokens.next();
            } else {
                break;
            }
        }

        lines.push(std::mem::take(&mut spans));
        line_start = index + 1;
    }

    lines
}

/// The highlighted tokens of `text`, in order, by byte range.
fn tokens(text: &[u8], language: Language) -> Vec<Span> {
    let at = |index: usize| text.get(index).copied().unwrap_or(0);
    let starts = |index: usize, prefix: &[u8]| text[index.min(text.len())..].starts_with(prefix);
    let mut tokens = Vec::new();
    let mut index = 0;

    // Whether only whitespace precedes `index` on its line.
    let mut line_start = true;

    while index < text.len() {
        let byte = text[index];
        let previous_is_identifier = index > 0 && is_identifier(text[index - 1]);
        let start = index;
        let block = language
            .block_comments()
            .iter()
            .find(|(open, _, _)| starts(index, open));

        let kind = if let Some((open, close, nests)) = block {
            index = block_comment_end(text, index, open, close, *nests);
            Some(Kind::Comment)
        } else if starts(index, language.line_comment()) {
            index = memchr(text, index, b'\n');
            Some(Kind::Comment)
        } else if language == Language::Zig && starts(index, b"\\\\") {
            index = memchr(text, index, b'\n');
            Some(Kind::String)
        } else if matches!(language, Language::Go | Language::Odin | Language::D) && byte == b'`' {
            index = memchr(text, index + 1, b'`')
                .saturating_add(1)
                .min(text.len());
            Some(Kind::String)
        } else if let Some(end) = (language == Language::Python && !previous_is_identifier)
            .then(|| python_string_end(text, index))
            .flatten()
        {
            index = end;
            Some(Kind::String)
        } else if let Some(end) = (!previous_is_identifier)
            .then(|| raw_string_end(text, index, language))
            .flatten()
        {
            index = end;
            Some(Kind::String)
        } else if byte == b'"' || byte == b'\'' && language == Language::Fortran {
            index = if language.doubles_quotes() {
                doubled_end(text, index, byte)
            } else {
                quoted_end(
                    text,
                    index,
                    b'"',
                    matches!(language, Language::Rust | Language::D),
                )
            };

            Some(Kind::String)
        } else if byte == b'\'' {
            if let Some(end) = character_end(text, index, language) {
                index = end;
                Some(Kind::String)
            } else {
                // A Rust lifetime or label.
                index += 1;
                None
            }
        } else if byte.is_ascii_digit() && !previous_is_identifier {
            index = number_end(text, index);
            Some(Kind::Number)
        } else if is_identifier(byte) {
            while is_identifier(at(index)) {
                index += 1;
            }

            let word = std::str::from_utf8(&text[start..index]).unwrap_or_default();
            let keyword = if language.ignores_case() {
                language
                    .keywords()
                    .iter()
                    .any(|keyword| keyword.eq_ignore_ascii_case(word))
            } else {
                language.keywords().contains(&word)
            } && (language != Language::Python
                || python_soft_keyword_holds(text, word, index, line_start));

            keyword.then_some(Kind::Keyword)
        } else if byte == b'#' && line_start && matches!(language, Language::C | Language::Cpp) {
            // A preprocessor directive's name.
            index += 1;
            while matches!(at(index), b' ' | b'\t') {
                index += 1;
            }

            while is_identifier(at(index)) {
                index += 1;
            }

            Some(Kind::Keyword)
        } else {
            index += 1;
            None
        };

        if let Some(kind) = kind {
            tokens.push((start..index, kind));
        }

        line_start = match text[index - 1] {
            b'\n' => true,
            b' ' | b'\t' => line_start,
            _ => false,
        };
    }

    tokens
}

/// The index of the first `byte` at or after `from`, or the end.
fn memchr(text: &[u8], from: usize, byte: u8) -> usize {
    text[from.min(text.len())..]
        .iter()
        .position(|candidate| *candidate == byte)
        .map_or(text.len(), |offset| from + offset)
}

/// Where a block comment opened at `start` by `open` ends, at `close`,
/// counting those it holds when they nest.
fn block_comment_end(text: &[u8], start: usize, open: &[u8], close: &[u8], nests: bool) -> usize {
    let mut depth = 0;
    let mut index = start;
    while index < text.len() {
        if text[index..].starts_with(open) && (nests || depth == 0) {
            depth += 1;
            index += open.len();
        } else if text[index..].starts_with(close) {
            depth -= 1;
            index += close.len();
            if depth == 0 {
                return index;
            }
        } else {
            index += 1;
        }
    }

    text.len()
}

/// Where a string or character quoted by `quote` from `start` ends, after
/// its closing quote. Only Rust's strings continue past a line's end
/// without an escaped newline.
const fn quoted_end(text: &[u8], start: usize, quote: u8, spans_lines: bool) -> usize {
    let mut index = start + 1;
    while index < text.len() {
        match text[index] {
            b'\\' => index += 2,
            b'\n' if !spans_lines => return index,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }

    text.len()
}

/// Where a Python string from `start` ends, after its closing quotes, if a
/// prefix and a quote begin there. A string quoted once ends with its line
/// unless a backslash escapes the newline, which Python continues.
fn python_string_end(text: &[u8], start: usize) -> Option<usize> {
    let quote_index = PYTHON_STRING_PREFIXES.iter().find_map(|prefix| {
        let end = start + prefix.len();
        let quote = *text.get(end)?;
        (matches!(quote, b'"' | b'\'') && text.get(start..end)?.eq_ignore_ascii_case(prefix))
            .then_some(end)
    })?;

    let quote = text[quote_index];
    let triple = text[quote_index..].starts_with(&[quote; 3]);
    let prefix = &text[start..quote_index];
    if prefix
        .iter()
        .any(|byte| matches!(byte, b'f' | b'F' | b't' | b'T'))
    {
        let raw = prefix.iter().any(|byte| matches!(byte, b'r' | b'R'));
        return Some(formatted_end(text, quote_index, quote, triple, raw));
    }

    if triple {
        Some(triple_quoted_end(text, quote_index, quote))
    } else {
        Some(quoted_end(text, quote_index, quote, false))
    }
}

/// Where an f-string or t-string from `start`, which opens with `quote`,
/// three of them where `triple`, ends after its closing quotes. From 3.12
/// a field, between braces, holds an expression, which may hold strings in
/// the string's own quotes and span lines, then after a `:` its format
/// spec, which is text but for fields of its own; `{{` is a brace.
fn formatted_end(text: &[u8], start: usize, quote: u8, triple: bool, raw: bool) -> usize {
    /// What each open brace or bracket holds.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Open {
        Field,
        Spec,
        Bracket,
    }

    let closing: &[u8] = if triple { &[quote; 3] } else { &[quote] };
    let mut index = start + closing.len();
    let mut open: Vec<Open> = Vec::new();

    while index < text.len() {
        let byte = text[index];

        match open.last() {
            None | Some(Open::Spec) => {
                if byte == b'\\' && !raw {
                    index += 2;
                    continue;
                }
                if open.is_empty() && text[index..].starts_with(closing) {
                    return index + closing.len();
                }
                if byte == b'\n' && !triple {
                    return index;
                }
                match byte {
                    b'{' if open.is_empty() && text.get(index + 1) == Some(&b'{') => index += 1,
                    b'{' => open.push(Open::Field),
                    b'}' if !open.is_empty() => {
                        open.pop();
                    }
                    _ => {}
                }
                index += 1;
            }
            Some(&top) => {
                // An expression, whose strings end none of this one.
                if let Some(end) = (!is_identifier(text[index - 1]))
                    .then(|| python_string_end(text, index))
                    .flatten()
                {
                    index = end;
                    continue;
                }
                match byte {
                    b'(' | b'[' | b'{' => open.push(Open::Bracket),
                    b')' | b']' | b'}' => {
                        open.pop();
                    }
                    b':' if top == Open::Field => {
                        open.pop();
                        open.push(Open::Spec);
                    }
                    _ => {}
                }
                index += 1;
            }
        }
    }

    text.len()
}

/// Where a triple-quoted string from `start`, which opens with three
/// `quote`s, ends after its closing three. Escapes count, and the string
/// may span lines.
fn triple_quoted_end(text: &[u8], start: usize, quote: u8) -> usize {
    let closing = [quote; 3];
    let mut index = start + 3;
    while index < text.len() {
        if text[index] == b'\\' {
            index += 2;
        } else if text[index..].starts_with(&closing) {
            return index + 3;
        } else {
            index += 1;
        }
    }

    text.len()
}

/// Where a string quoted by `quote` from `start` ends, after its closing
/// quote, in a language that writes a quote within it twice. It ends with
/// its line.
fn doubled_end(text: &[u8], start: usize, quote: u8) -> usize {
    let mut index = start + 1;
    while index < text.len() {
        match text[index] {
            b'\n' => return index,
            byte if byte == quote && text.get(index + 1) == Some(&quote) => index += 2,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }

    text.len()
}

/// Where a character literal from `start` ends, or `None` for a Rust
/// lifetime or an Ada attribute, which also begin with a quote.
fn character_end(text: &[u8], start: usize, language: Language) -> Option<usize> {
    if language == Language::Ada {
        // One character between quotes, and not after a name, whose quote
        // begins an attribute.
        let after_name = start > 0 && (is_identifier(text[start - 1]) || text[start - 1] == b')');
        return (!after_name && text.get(start + 2) == Some(&b'\'')).then_some(start + 3);
    }

    if language != Language::Rust {
        return Some(quoted_end(text, start, b'\'', false));
    }

    if text.get(start + 1) == Some(&b'\\') {
        return Some(quoted_end(text, start, b'\'', false));
    }

    // One character, which may take several bytes, then the closing quote.
    let lead = *text.get(start + 1)?;
    let width = match lead {
        b'\'' | b'\n' => return None,
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };

    let after = start + 1 + width;

    (text.get(after) == Some(&b'\'')).then_some(after + 1)
}

/// Where a raw string from `start` ends, when one begins there: Rust's
/// `r#"…"#` and `br"…"`, and C++'s `R"delimiter(…)delimiter"`.
fn raw_string_end(text: &[u8], start: usize, language: Language) -> Option<usize> {
    match language {
        Language::Rust => {
            let mut index = start;
            if text.get(index) == Some(&b'b') {
                index += 1;
            }

            if text.get(index) != Some(&b'r') {
                return None;
            }

            index += 1;
            let hashes = text[index..]
                .iter()
                .take_while(|byte| **byte == b'#')
                .count();
            index += hashes;
            if text.get(index) != Some(&b'"') {
                return None;
            }

            let mut closing = vec![b'"'];
            closing.extend(std::iter::repeat_n(b'#', hashes));
            find(text, index + 1, &closing).map(|end| end + closing.len())
        }
        Language::Cpp => {
            let prefix = ["u8R\"", "uR\"", "UR\"", "LR\"", "R\""]
                .into_iter()
                .find(|prefix| text[start..].starts_with(prefix.as_bytes()))?;
            let open = start + prefix.len();
            let delimiter_end = open
                + text[open..]
                    .iter()
                    .take(17)
                    .position(|byte| *byte == b'(')?;
            let mut closing = vec![b')'];
            closing.extend_from_slice(&text[open..delimiter_end]);
            closing.push(b'"');

            Some(
                find(text, delimiter_end + 1, &closing)
                    .map_or(text.len(), |end| end + closing.len()),
            )
        }
        Language::D | Language::Nim if text[start..].starts_with(b"r\"") => {
            Some(find(text, start + 2, b"\"").map_or(text.len(), |end| end + 1))
        }
        Language::Nim if text[start..].starts_with(b"\"\"\"") => {
            // It ends at the last of the quotes that close it.
            let close = find(text, start + 3, b"\"\"\"").map_or(text.len(), |end| end + 3);
            let extra = text[close.min(text.len())..]
                .iter()
                .take_while(|byte| **byte == b'"')
                .count();

            Some(close + extra)
        }
        _ => None,
    }
}

/// The index at or after `from` where `needle` begins, if it does.
fn find(text: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    text[from.min(text.len())..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Where a number from `start` ends: digits, letters of a radix, suffix, or
/// exponent, and a point followed by a digit, so that a range `0..5` is
/// two numbers.
fn number_end(text: &[u8], start: usize) -> usize {
    let hexadecimal = text[start..].starts_with(b"0x") || text[start..].starts_with(b"0X");
    let mut index = start;
    while index < text.len() {
        let byte = text[index];
        let exponent_sign =
            matches!(byte, b'+' | b'-') && !hexadecimal && matches!(text[index - 1], b'e' | b'E');
        let point = byte == b'.' && text.get(index + 1).is_some_and(u8::is_ascii_digit);
        if is_identifier(byte)
            || byte == b'\'' && text.get(index + 1).is_some_and(u8::is_ascii_digit)
            || exponent_sign
            || point
        {
            index += 1;
        } else {
            break;
        }
    }

    index
}

#[cfg(test)]
mod tests {
    use super::{Kind, Language, lex};

    /// Each line's highlighted text and its kind.
    fn shown(text: &str, language: Language) -> Vec<Vec<(String, Kind)>> {
        lex(text, language)
            .into_iter()
            .zip(text.split('\n'))
            .map(|(spans, line)| {
                spans
                    .into_iter()
                    .map(|(range, kind)| (line[range].to_owned(), kind))
                    .collect()
            })
            .collect()
    }

    fn of(spans: &[(&str, Kind)]) -> Vec<(String, Kind)> {
        spans
            .iter()
            .map(|(text, kind)| ((*text).to_owned(), *kind))
            .collect()
    }

    #[test]
    fn comments_and_strings_that_span_lines_are_known_on_every_line() {
        use Kind::{Comment, Keyword, Number, String};
        let c =
            "#include <stdio.h>\n/* one\n   two */ int x = 0x1f; // done\nchar *s = \"a\\\"b\";";
        assert_eq!(
            shown(c, Language::C),
            [
                of(&[("#include", Keyword)]),
                of(&[("/* one", Comment)]),
                of(&[
                    ("   two */", Comment),
                    ("int", Keyword),
                    ("0x1f", Number),
                    ("// done", Comment),
                ]),
                of(&[("char", Keyword), ("\"a\\\"b\"", String)]),
            ]
        );

        // Rust's comments nest, its strings span lines, and a quote also
        // begins a lifetime.
        let rust = "/* a /* b */ c */ fn f<'a>(x: &'a u8) -> char {\n    let s = \"one\ntwo\";\n    let r = r#\"x\"y\"#; for i in 0..5 {} 'q'\n}";
        assert_eq!(
            shown(rust, Language::Rust),
            [
                of(&[("/* a /* b */ c */", Comment), ("fn", Keyword)]),
                of(&[("let", Keyword), ("\"one", String)]),
                of(&[("two\"", String)]),
                of(&[
                    ("let", Keyword),
                    ("r#\"x\"y\"#", String),
                    ("for", Keyword),
                    ("in", Keyword),
                    ("0", Number),
                    ("5", Number),
                    ("'q'", String),
                ]),
                of(&[]),
            ]
        );

        // Go's raw strings and C++'s span lines; Zig's line strings and C's
        // ordinary strings end with theirs.
        let go = "s := `one\ntwo` + \"x\"";
        assert_eq!(
            shown(go, Language::Go),
            [
                of(&[("`one", String)]),
                of(&[("two`", String), ("\"x\"", String)])
            ]
        );
        let cpp = "auto s = R\"x(one\n)\" still)x\";";
        assert_eq!(
            shown(cpp, Language::Cpp),
            [
                of(&[("auto", Keyword), ("R\"x(one", String)]),
                of(&[(")\" still)x\"", String)]),
            ]
        );
        let zig = "const s =\n    \\\\one \"two\n;";
        assert_eq!(
            shown(zig, Language::Zig),
            [
                of(&[("const", Keyword)]),
                of(&[("\\\\one \"two", String)]),
                of(&[]),
            ]
        );
        let unterminated = "char *s = \"open\nint x;";
        assert_eq!(
            shown(unterminated, Language::C),
            [
                of(&[("char", Keyword), ("\"open", String)]),
                of(&[("int", Keyword)]),
            ]
        );
    }

    /// `match`, `case`, and `type` are keywords only where a statement
    /// they begin is: elsewhere they are names, as `type(x)` calls one.
    #[test]
    fn python_soft_keywords_are_keywords_only_where_they_begin_statements() {
        use Kind::{Comment, Keyword, Number};

        let python = "match command:
    case [x, *rest]:
        type Point = tuple[int, int]
match = re.match(pattern, text)
kind = type(value)
self.type = 1
case(1)
type
match (x):
match x:  # a comment: here
match = 1  # here:
type Pair[T] = tuple[T, T]
type == 1";
        assert_eq!(
            shown(python, Language::Python),
            [
                of(&[("match", Keyword)]),
                of(&[("case", Keyword)]),
                of(&[("type", Keyword)]),
                of(&[]),
                of(&[]),
                of(&[("1", Number)]),
                of(&[("1", Number)]),
                of(&[]),
                of(&[("match", Keyword)]),
                of(&[("match", Keyword), ("# a comment: here", Comment)]),
                of(&[("1", Number), ("# here:", Comment)]),
                of(&[("type", Keyword)]),
                of(&[("1", Number)]),
            ]
        );
    }

    /// From 3.12 an f-string's fields may hold strings in its own quotes,
    /// which end no string; `{{` is a brace, and opens no field.
    #[test]
    #[expect(
        clippy::literal_string_with_formatting_args,
        reason = "the texts are Python's f-strings"
    )]
    fn python_f_strings_hold_strings_in_their_fields() {
        use Kind::{Number, String};

        let python = r#"a = f"{d["key"]}" + 1
b = f'{x!r:>{width}} {{y}}' + 2
c = rf"{d['a'] + "}"}\d" + 3
d = f"""{d["""
x"""]}""" + 4
e = "{d["k"]}" + 5
g = f"{x:'>10}" + f'{x:{"w"}}' + 6
h = f"{
  x
}" + 7"#;
        assert_eq!(
            shown(python, Language::Python),
            [
                of(&[(r#"f"{d["key"]}""#, String), ("1", Number)]),
                of(&[("f'{x!r:>{width}} {{y}}'", String), ("2", Number)]),
                of(&[(r#"rf"{d['a'] + "}"}\d""#, String), ("3", Number)]),
                of(&[(r#"f"""{d[""""#, String)]),
                of(&[(r#"x"""]}""""#, String), ("4", Number)]),
                of(&[(r#""{d[""#, String), (r#""]}""#, String), ("5", Number)]),
                of(&[
                    (r#"f"{x:'>10}""#, String),
                    (r#"f'{x:{"w"}}'"#, String),
                    ("6", Number)
                ]),
                of(&[(r#"f"{"#, String)]),
                of(&[("  x", String)]),
                of(&[(r#"}""#, String), ("7", Number)]),
            ]
        );
    }

    #[test]
    fn python_strings_span_lines_and_take_their_prefixes() {
        use Kind::{Comment, Keyword, Number, String};

        // A single-quoted string ends with its line, a triple-quoted one
        // spans lines, and a `#` or a lone `'` inside a string is its text.
        let python = r#"def f(): return 'it' # note
t = rb'\d' + f"{x}" + "a#b"
n = 0x1f + 1.5e-3j
s = """one
it's "two"
three""" + '''a
b''' if x else 0
@cache"#;
        assert_eq!(
            shown(python, Language::Python),
            [
                of(&[
                    ("def", Keyword),
                    ("return", Keyword),
                    ("'it'", String),
                    ("# note", Comment),
                ]),
                of(&[
                    ("rb'\\d'", String),
                    ("f\"{x}\"", String),
                    ("\"a#b\"", String)
                ]),
                of(&[("0x1f", Number), ("1.5e-3j", Number)]),
                of(&[("\"\"\"one", String)]),
                of(&[("it's \"two\"", String)]),
                of(&[("three\"\"\"", String), ("'''a", String)]),
                of(&[
                    ("b'''", String),
                    ("if", Keyword),
                    ("else", Keyword),
                    ("0", Number),
                ]),
                of(&[]),
            ]
        );

        let unterminated = "x = 'open\ny = 1";
        assert_eq!(
            shown(unterminated, Language::Python),
            [of(&[("'open", String)]), of(&[("1", Number)])]
        );
    }

    #[test]
    fn each_language_has_its_own_comments_strings_and_case() {
        use Kind::{Comment, Keyword, Number, String};

        // Odin's block comments nest, and its raw strings are Go's.
        let odin = "/* a /* b */ c */ x := `r\ns` // done";
        assert_eq!(
            shown(odin, Language::Odin),
            [
                of(&[("/* a /* b */ c */", Comment), ("`r", String)]),
                of(&[("s`", String), ("// done", Comment)]),
            ]
        );

        // Fortran's keywords are any case, its comments begin with `!`, and
        // a quote is doubled within its string.
        let fortran = "INTEGER :: n = 3 ! three\nprint *, 'it''s'";
        assert_eq!(
            shown(fortran, Language::Fortran),
            [
                of(&[("INTEGER", Keyword), ("3", Number), ("! three", Comment)]),
                of(&[("print", Keyword), ("'it''s'", String)]),
            ]
        );

        // D's `/+` comments nest; its strings span lines.
        let d = "/+ a /+ b +/ c +/ auto s = \"one\ntwo\";";
        assert_eq!(
            shown(d, Language::D),
            [
                of(&[
                    ("/+ a /+ b +/ c +/", Comment),
                    ("auto", Keyword),
                    ("\"one", String)
                ]),
                of(&[("two\"", String)]),
            ]
        );

        // Nim's comments begin with `#`, its block comments nest, and its
        // triple-quoted strings span lines.
        let nim = "#[ a #[ b ]# ]# let s = \"\"\"one\n\"two\"\"\"\" # done";
        assert_eq!(
            shown(nim, Language::Nim),
            [
                of(&[
                    ("#[ a #[ b ]# ]#", Comment),
                    ("let", Keyword),
                    ("\"\"\"one", String)
                ]),
                of(&[("\"two\"\"\"\"", String), ("# done", Comment)]),
            ]
        );

        // Ada's keywords are any case and its comments begin with `--`; a
        // quote after a name begins an attribute, not a character.
        let ada =
            "Last : Integer := Items'Last; -- end\nC : Character := 'q'; S : String := \"a\"\"b\";";
        assert_eq!(
            shown(ada, Language::Ada),
            [
                of(&[("-- end", Comment)]),
                of(&[("'q'", String), ("\"a\"\"b\"", String)]),
            ]
        );
    }
}
