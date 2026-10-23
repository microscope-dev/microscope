//! Reading view files (`docs/views.md`), which needs no program.
//!
//! A file is statements in views. Each expression inside is an ordinary
//! expression in the view dialect, so the parser here finds where each one
//! ends and hands its text to the expression parser. An expression ends at
//! the end of its statement, at a `,` or `)` that closes what holds it, at
//! a `{` or `}`, or before one of the words `or`, `for`, `if`, `else`,
//! `let`, and the arrow `=>`. A statement ends at a line that begins another statement
//! or closes the view, so a long expression may continue on the next line.
//!
//! An error in one view skips that view; the views after it are still read.

use std::sync::Arc;

use crate::IntegerValue;
use crate::eval::syntax::Expression;

/// The view language's version, which a file names in its first line.
pub const VERSION: u32 = 1;

/// The largest view file read, in bytes.
pub const MAX_FILE_BYTES: usize = 256 * 1024;

/// The most views one file may hold.
pub const MAX_VIEWS: usize = 1024;

/// The most statements one view may hold.
const MAX_STATEMENTS: usize = 256;

/// How deeply shapes may nest in `if` branches.
const MAX_SHAPE_DEPTH: usize = 16;

/// How many members a `record` may have.
pub const MAX_RECORD_MEMBERS: usize = 64;

/// How many arms a `match` may have.
const MAX_ARMS: usize = 64;

/// How deeply type patterns may nest in arguments.
const MAX_PATTERN_DEPTH: usize = 16;

/// The most generators one sequence or map may nest.
pub const MAX_CLAUSES: usize = 4;

/// What a kernel's name may be, which is also the name of its file.
pub const KERNEL_NAME: &str = "a kernel's name is 1 to 64 letters, digits, `_`, and `-`";

/// What a renderer's name may be, which is also the name of its file.
pub const RENDERER_NAME: &str = "a renderer's name is 1 to 64 letters, digits, `_`, and `-`";

/// How many inputs one `visualize` may name.
pub const MAX_INPUTS: usize = 64;

/// Whether `name` may name a kernel: see [`KERNEL_NAME`].
#[must_use]
pub fn is_kernel_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-".contains(character))
}

/// Whether `name` may name a renderer: see [`RENDERER_NAME`].
#[must_use]
pub fn is_renderer_name(name: &str) -> bool {
    is_kernel_name(name)
}

/// Words that end an expression, so a member with one of these names must
/// be written in backticks.
const STOP_WORDS: [&str; 5] = ["or", "for", "if", "else", "let"];

/// Words that begin a statement.
const STATEMENTS: [&str; 13] = [
    "let",
    "type",
    "check",
    "summary",
    "field",
    "show",
    "if",
    "match",
    "hide",
    "format",
    "visualize",
    "view",
    "extend",
];

/// The languages a view applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    C,
    Cpp,
    Rust,
    Go,
    Zig,
    Odin,
    D,
    Nim,
    Ada,

    /// Python, whose objects' types only their values know.
    Python,

    /// JavaScript, whose objects' classes only their values know, as
    /// TypeScript's are too.
    JavaScript,

    /// Every compiled language.
    Any,
}

impl Language {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "c" => Self::C,
            "c++" => Self::Cpp,
            "rust" => Self::Rust,
            "go" => Self::Go,
            "zig" => Self::Zig,
            "odin" => Self::Odin,
            "d" => Self::D,
            "nim" => Self::Nim,
            "ada" => Self::Ada,
            "python" => Self::Python,
            "javascript" => Self::JavaScript,
            "any" => Self::Any,
            _ => return None,
        })
    }
}

/// A problem in a view file, at its line and column, counted from one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub source: Arc<str>,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{}:{}: {}",
            self.source, self.line, self.column, self.message
        )
    }
}

/// A parsed view file: the views it holds, and the problems that kept
/// others out.
#[derive(Debug, Clone)]
pub struct File {
    pub views: Vec<Arc<View>>,
    pub errors: Vec<Error>,
}

/// One view: which types it matches and how it presents them.
#[derive(Debug, Clone)]
pub struct View {
    /// The file it came from.
    pub source: Arc<str>,

    /// The line of its `view` keyword.
    pub line: u32,
    pub language: Language,
    pub pattern: Pattern,

    /// The language and pattern as written, to name the view by.
    pub header: Arc<str>,

    /// Whether it is an `extend`, which adds to the view that presents
    /// the type rather than presenting it.
    pub extend: bool,
    pub statements: Vec<Statement>,
}

impl View {
    /// The names of the kernels the view calls.
    #[must_use]
    pub fn kernel_names(&self) -> Vec<&str> {
        fn walk<'v>(shape: &'v Shape, names: &mut Vec<&'v str>) {
            match shape {
                Shape::Sequence { clauses, .. } | Shape::Map { clauses, .. } => {
                    names.extend(clauses.iter().filter_map(|clause| match &clause.generator {
                        Generator::Kernel { name, .. } => Some(name.as_str()),
                        _ => None,
                    }));
                }

                Shape::If {
                    then, otherwise, ..
                } => {
                    walk(then, names);
                    walk(otherwise, names);
                }
                _ => {}
            }
        }

        let mut names = Vec::new();
        for statement in &self.statements {
            if let Statement::Show(shape) = statement {
                walk(shape, &mut names);
            }
        }

        names
    }

    /// The names of the renderers the view's `visualize` statements call.
    #[must_use]
    pub fn renderer_names(&self) -> Vec<&str> {
        self.statements
            .iter()
            .filter_map(|statement| match statement {
                Statement::Visualize(visualize) => Some(visualize.name.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// An expression and the line it was written on.
#[derive(Debug, Clone)]
pub struct Expr {
    pub expression: Expression,
    pub line: u32,
}

impl Expr {
    /// The expression as written.
    #[must_use]
    pub fn text(&self) -> &str {
        self.expression.text().trim()
    }
}

/// A type a `type` statement names.
#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// A type by name: a captured argument, a `type` statement's name, or
    /// a program type, with pointers.
    Named { name: String, pointers: u8 },

    /// The type of an expression's value.
    TypeOf(Expr),

    /// One argument of a type, by position.
    Arg { of: Box<Self>, index: u32 },

    /// A type declared inside another, as Zig's `Self.Header`.
    Nested { of: Box<Self>, name: String },
}

/// One `{expression}` or literal piece of a `summary`.
#[derive(Debug, Clone)]
pub enum Piece {
    Literal(String),

    /// `{EXPR}`, or `{EXPR as FORMAT}`.
    Hole {
        value: Expr,
        format: Option<Format>,
    },
}

/// A statement of a view's body.
#[derive(Debug, Clone)]
pub enum Statement {
    /// `let NAME = EXPR or EXPR…`: the first alternative that binds.
    Let {
        name: String,
        alternatives: Vec<Expr>,
        line: u32,
    },

    /// `type NAME = TYPE or TYPE…`: the first alternative that resolves.
    Type {
        name: String,
        alternatives: Vec<TypeExpr>,
        line: u32,
    },

    /// `check EXPR`: an invariant checked before the value is presented.
    Check(Expr),

    /// `summary "TEXT {EXPR} TEXT"`.
    Summary(Vec<Piece>),

    /// `field NAME = EXPR`: a named child.
    Field { name: String, value: Expr },

    /// `show SHAPE`.
    Show(Shape),

    /// `hide NAME, …`: members and fields left out of the children.
    Hide { names: Vec<String>, line: u32 },

    /// `format NAME, … as FORMAT`: members and fields written another way.
    Format {
        names: Vec<String>,
        format: Format,
        line: u32,
    },

    /// `visualize "NAME" { INPUT = EXPR, … }`: a drawing the web page
    /// makes of the value with the renderer `NAME`.
    Visualize(Visualize),
}

/// A `visualize` statement: the renderer it calls and the inputs it hands
/// it.
#[derive(Debug, Clone)]
pub struct Visualize {
    pub name: String,
    pub line: u32,
    pub inputs: Vec<Input>,
}

/// One input of a `visualize`: the name the renderer reads it by, and
/// what it is.
#[derive(Debug, Clone)]
pub struct Input {
    pub name: String,
    pub line: u32,
    pub value: InputValue,
}

/// What an input hands the renderer.
#[derive(Debug, Clone)]
pub enum InputValue {
    /// A value, by the first alternative that binds.
    Value(Vec<Expr>),

    /// `bytes(PTR, LEN)`: `LEN` bytes of memory at `PTR`, read at once.
    Bytes { pointer: Expr, length: Expr },

    /// A string in double quotes, which the renderer gets as it is.
    Text(String),
}

/// How `format` writes a value.
#[derive(Debug, Clone)]
pub enum Format {
    /// An integer in hexadecimal, in its type's width.
    Hex,

    /// An integer as the character it codes.
    Char,

    /// A value's bytes in memory, in hexadecimal.
    Bytes,

    /// An array or slice of bytes as text, when they are valid UTF-8
    /// without control characters.
    Utf8,

    /// An array or slice of 16-bit units as UTF-16 text.
    Utf16,

    /// An integer as the enumerators of `TYPE` whose bits it sets.
    Flags(TypeExpr),

    /// An integer as the enumerator of `TYPE` it equals.
    Enum(TypeExpr),

    /// An integer count of a unit of time, as a duration.
    Duration(TimeUnit),

    /// An integer count of a unit of time since the Unix epoch, as the UTC
    /// date and time it is.
    Time(TimeUnit),
}

/// The unit a `duration` or `time` format counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Nanoseconds,
    Microseconds,
    Milliseconds,
    Seconds,
}

impl TimeUnit {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "ns" => Self::Nanoseconds,
            "us" => Self::Microseconds,
            "ms" => Self::Milliseconds,
            "s" => Self::Seconds,
            _ => return None,
        })
    }

    /// How many nanoseconds one unit is.
    #[must_use]
    pub const fn nanoseconds(self) -> u128 {
        match self {
            Self::Nanoseconds => 1,
            Self::Microseconds => 1_000,
            Self::Milliseconds => 1_000_000,
            Self::Seconds => 1_000_000_000,
        }
    }
}

/// How many elements a sequence or map declares.
#[derive(Debug, Clone)]
pub enum Count {
    Known(Expr),

    /// `_`: the generators decide.
    Unknown,
}

/// `P => EXPR`: how a generator reaches the node after `P`.
#[derive(Debug, Clone)]
pub struct Link {
    pub parameter: String,
    pub expression: Expr,
}

/// The values a clause's variable takes, in order.
#[derive(Debug, Clone)]
pub enum Generator {
    /// `range(N)`: 0, 1, …, N - 1.
    Range(Expr),

    /// `list(HEAD, P => NEXT)`: HEAD, then each node's next, until a null
    /// pointer or HEAD again.
    List { head: Expr, next: Link },

    /// `inorder(ROOT, P => LEFT, P => RIGHT)`: a binary tree's nodes, each
    /// after its left subtree and before its right; a null pointer is an
    /// empty tree.
    Inorder { root: Expr, left: Link, right: Link },

    /// `kernel("NAME", ARG, …)`: the items a kernel yields, each a word
    /// for each of the clause's variables.
    Kernel {
        name: String,
        line: u32,
        arguments: Vec<Expr>,
    },
}

/// What follows a clause's generator, in order: a condition each value
/// must meet, or a name computed once for each value.
#[derive(Debug, Clone)]
pub enum Item {
    Filter(Expr),
    Let { name: String, value: Expr },
}

/// `for VAR[, VAR…] in GENERATOR [if FILTER | let NAME = EXPR]…`. Only a
/// kernel's items have several variables.
#[derive(Debug, Clone)]
pub struct Clause {
    pub variables: Vec<String>,
    pub generator: Generator,
    pub items: Vec<Item>,
}

/// What a view presents a value as.
#[derive(Debug, Clone)]
pub enum Shape {
    /// `text(PTR [, LEN])`.
    Text { pointer: Expr, length: Option<Expr> },

    /// `value(EXPR)`: present the value as another.
    Value(Expr),

    /// `empty("TEXT")`.
    /// `empty("TEXT {EXPR} TEXT")`: a value that holds nothing, summarized
    /// as its text, with each hole's value's summary in its place.
    Empty(Vec<Piece>),

    /// `sequence(COUNT) CLAUSES => ELEMENT`.
    Sequence {
        count: Count,
        clauses: Vec<Clause>,
        element: Expr,
    },

    /// `map(COUNT) CLAUSES => KEY : VALUE`.
    Map {
        count: Count,
        clauses: Vec<Clause>,
        key: Expr,
        value: Expr,
    },

    /// `if COND { SHAPE } else { SHAPE }`.
    If {
        condition: Expr,
        then: Box<Self>,
        otherwise: Box<Self>,
    },

    /// `record { NAME = EXPR, … }`: a record of the members it names, each
    /// a name or a position; or `tuple { … }`, whose members are a tuple's
    /// elements, in order, whatever their names.
    Record {
        members: Vec<(String, Expr)>,
        tuple: bool,
    },

    /// `dynamic(PTR, TYPE)`: what a pointer points to, as a type that may
    /// be chosen as the program runs.
    Dynamic { pointer: Expr, ty: DynamicType },

    /// What a `match` with no `_` arm shows when no arm names its value:
    /// a problem that says the value.
    Unmatched(Expr),
}

/// The type a `dynamic` shape presents its pointer's target as.
#[derive(Debug, Clone)]
pub enum DynamicType {
    /// One type, as a `type` statement names it.
    Fixed(TypeExpr),

    /// `arg(TYPE, EXPR)`: the argument of a type at a position the
    /// program's data holds, as a `std::variant`'s index does.
    Argument { of: TypeExpr, index: Expr },

    /// `TYPE of CODE`: a type that names the type arguments of the
    /// function whose code `CODE` addresses by its parameters' names.
    Function { ty: TypeExpr, code: Expr },
}

/// A path segment of a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Name(String),

    /// `**`: any run of segments, including none.
    AnyRun,
}

/// A pattern of type identities: `path::base<arguments>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub path: Vec<Segment>,
    pub base: String,

    /// The arguments' patterns, when the pattern has an argument list. A
    /// type may have more arguments than its pattern lists.
    pub arguments: Option<Vec<ArgumentPattern>>,
}

/// A pattern of one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentPattern {
    /// `_`: anything.
    Wildcard,

    /// A capital name, which captures the argument for the body.
    Capture(String),

    /// An integer argument's value.
    Value(IntegerValue),

    /// A type's pattern.
    Type(Pattern),
}

/// Parses one view file, named `source`.
#[must_use]
pub fn parse(source: &str, text: &str) -> File {
    let source: Arc<str> = source.into();
    let mut parser = Parser::new(&source, text);
    if text.len() > MAX_FILE_BYTES {
        return File {
            views: Vec::new(),
            errors: vec![parser.error_at(
                0,
                format!("a view file may be at most {MAX_FILE_BYTES} bytes long"),
            )],
        };
    }

    let mut views = Vec::new();
    let mut errors = Vec::new();
    if let Err(error) = parser.header() {
        return File {
            views,
            errors: vec![error],
        };
    }

    loop {
        parser.skip_blank();
        if parser.at_end() {
            break;
        }

        let start = parser.position;

        match parser.view() {
            Ok(view) => {
                debug_assert!(
                    parser.position > start,
                    "the view at offset {start} was parsed without consuming it"
                );
                if views.len() == MAX_VIEWS {
                    errors.push(parser.error_at(
                        start,
                        format!("a view file may hold at most {MAX_VIEWS} views"),
                    ));
                    break;
                }

                views.push(Arc::new(view));
            }
            Err(error) => {
                errors.push(error);

                // Resume at the next view.
                parser.position = start;
                let resumed = parser.skip_to_next_view();
                debug_assert!(
                    parser.position > start,
                    "resuming after the view at offset {start} did not advance"
                );
                if !resumed {
                    break;
                }
            }
        }
    }

    File { views, errors }
}

struct Parser<'a> {
    source: &'a Arc<str>,
    text: &'a str,
    position: usize,

    /// The offset of each line's first byte.
    lines: Vec<usize>,

    /// Whether the expressions read are members of braces, which end at a
    /// line that begins the next member.
    members: bool,
}

type Parsed<T> = Result<T, Error>;

const fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

const fn is_word_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

impl<'a> Parser<'a> {
    fn new(source: &'a Arc<str>, text: &'a str) -> Self {
        let lines = std::iter::once(0)
            .chain(
                text.bytes()
                    .enumerate()
                    .filter(|(_, byte)| *byte == b'\n')
                    .map(|(index, _)| index + 1),
            )
            .collect();

        Self {
            source,
            text,
            position: 0,
            lines,
            members: false,
        }
    }

    /// The line and column of a byte offset, from one.
    fn location(&self, offset: usize) -> (u32, u32) {
        let line = self.lines.partition_point(|&start| start <= offset);
        let start = self.lines[line.saturating_sub(1)];
        let column = self
            .text
            .get(start..offset)
            .map_or(1, |prefix| prefix.chars().count() + 1);
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(column).unwrap_or(u32::MAX),
        )
    }

    fn error_at(&self, offset: usize, message: impl Into<String>) -> Error {
        let (line, column) = self.location(offset);
        Error {
            source: Arc::clone(self.source),
            line,
            column,
            message: message.into(),
        }
    }

    fn error(&self, message: impl Into<String>) -> Error {
        self.error_at(self.position, message)
    }

    fn rest(&self) -> &'a str {
        &self.text[self.position..]
    }

    const fn at_end(&self) -> bool {
        self.position >= self.text.len()
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.position).copied()
    }

    /// Skips spaces and comments on this line.
    fn skip_inline(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\r' => self.position += 1,
                b'#' => self.skip_comment(),
                _ => break,
            }
        }
    }

    /// Skips spaces, comments, and line ends.
    fn skip_blank(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' | b'\r' | b'\n' => self.position += 1,
                b'#' => self.skip_comment(),
                _ => break,
            }
        }
    }

    fn skip_comment(&mut self) {
        self.position = self
            .rest()
            .find('\n')
            .map_or(self.text.len(), |end| self.position + end);
    }

    /// The word at the cursor, without taking it.
    fn peek_word(&self) -> Option<&'a str> {
        let rest = self.rest();
        let bytes = rest.as_bytes();
        if !bytes.first().copied().is_some_and(is_word_start) {
            return None;
        }

        let length = bytes
            .iter()
            .take_while(|byte| is_word_continue(**byte))
            .count();
        Some(&rest[..length])
    }

    fn word(&mut self) -> Option<&'a str> {
        let word = self.peek_word()?;
        self.position += word.len();
        Some(word)
    }

    fn eat_word(&mut self, expected: &str) -> bool {
        if self.peek_word() == Some(expected) {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, expected: &str, context: &str) -> Parsed<()> {
        if self.eat_word(expected) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("`{expected}` {context}")))
        }
    }

    fn eat(&mut self, expected: &str) -> bool {
        if self.rest().starts_with(expected) {
            self.position += expected.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: &str, context: &str) -> Parsed<()> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(self.unexpected(&format!("`{expected}` {context}")))
        }
    }

    fn unexpected(&self, expected: &str) -> Error {
        let found = match self.rest().chars().next() {
            None => "the end of the file".to_owned(),
            Some('\n') => "the end of the line".to_owned(),
            Some(_) => format!(
                "`{}`",
                self.peek_word()
                    .map_or_else(|| self.rest().chars().take(1).collect(), str::to_owned)
            ),
        };

        self.error(format!("expected {expected}, found {found}"))
    }

    /// A name a statement declares.
    fn name(&mut self, what: &str) -> Parsed<String> {
        self.skip_inline();
        let word = self.word();
        word.map(str::to_owned).ok_or_else(|| self.unexpected(what))
    }

    /// `microscope-views 1`.
    fn header(&mut self) -> Parsed<()> {
        self.skip_blank();
        let start = self.position;
        if !self.eat("microscope-views") {
            return Err(self.error_at(
                start,
                format!("a view file begins with `microscope-views {VERSION}`"),
            ));
        }

        self.skip_inline();
        let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
        let version = self.rest()[..digits].parse::<u32>().ok();
        self.position += digits;
        if version != Some(VERSION) {
            return Err(self.error_at(
                start,
                format!("this microscope reads views of version {VERSION}"),
            ));
        }

        self.end_of_statement()
    }

    /// Moves past the view the cursor is in, to the next line that begins
    /// with `view` or `extend`, and returns whether there is one.
    fn skip_to_next_view(&mut self) -> bool {
        // The view the cursor is at, or in, is skipped first.
        self.position += self.rest().chars().next().map_or(1, char::len_utf8);
        while !self.at_end() {
            let line_start = self.position == 0 || self.text.as_bytes()[self.position - 1] == b'\n';
            if line_start && matches!(self.peek_word(), Some("view" | "extend")) {
                return true;
            }

            self.position += self.rest().chars().next().map_or(1, char::len_utf8);
        }

        false
    }

    fn end_of_statement(&mut self) -> Parsed<()> {
        self.skip_inline();
        match self.peek() {
            None | Some(b'\n' | b'}') => Ok(()),
            Some(b';') => {
                self.position += 1;
                Ok(())
            }
            Some(_) => Err(self.unexpected("the end of the statement")),
        }
    }

    fn view(&mut self) -> Parsed<View> {
        let start = self.position;
        let line = self.location(start).0;
        let extend = match self.peek_word() {
            Some("view") => false,
            Some("extend") => true,
            _ => return Err(self.unexpected("`view` or `extend`")),
        };

        self.position += if extend { "extend".len() } else { "view".len() };
        self.skip_inline();
        let language_start = self.position;
        let language_word = self
            .rest()
            .split(|character: char| character.is_whitespace())
            .next()
            .unwrap_or_default();
        let language = Language::parse(language_word).ok_or_else(|| {
            self.error(
                "expected a language: `c`, `c++`, `rust`, `go`, `zig`, `odin`, `d`, `nim`, `ada`, \
                 `python`, `javascript`, or `any`",
            )
        })?;
        self.position += language_word.len();
        self.skip_inline();
        let pattern_start = self.position;

        // A pattern's segments in backticks may hold braces.
        let pattern_end = outside_backticks(self.rest())
            .find(|(_, character)| *character == '{')
            .map(|(end, _)| self.position + end);
        let Some(pattern_end) =
            pattern_end.filter(|end| !self.text[pattern_start..*end].contains('\n'))
        else {
            return Err(self.unexpected("a type pattern, then `{`"));
        };

        let pattern_text = self.text[pattern_start..pattern_end].trim();
        let pattern = parse_pattern(pattern_text, 0).map_err(|message| {
            self.error_at(pattern_start, format!("in the pattern: {message}"))
        })?;
        let header = format!(
            "{} {pattern_text}",
            &self.text[language_start..language_start + language_word.len()]
        );

        self.position = pattern_end + 1;
        let mut statements = Vec::new();
        let mut shown = false;

        loop {
            self.skip_blank();
            if self.eat("}") {
                break;
            }

            if self.at_end() {
                return Err(self.error_at(start, "the view is not closed with `}`"));
            }

            if statements.len() == MAX_STATEMENTS {
                return Err(self.error(format!(
                    "a view may hold at most {MAX_STATEMENTS} statements"
                )));
            }

            let statement_start = self.position;
            let statement = self.statement()?;
            match statement {
                Statement::Show(_) if extend => {
                    return Err(self.error_at(
                        statement_start,
                        "an `extend` adds to the view that shows the value; it does not `show`",
                    ));
                }
                Statement::Summary(_) | Statement::Check(_) if extend => {
                    return Err(self.error_at(
                        statement_start,
                        "an `extend` adds fields, `hide`s, and `format`s, not checks or summaries",
                    ));
                }
                Statement::Show(_) if shown => {
                    return Err(self.error_at(statement_start, "a view shows its value once"));
                }
                Statement::Show(_) => shown = true,
                _ => {}
            }

            statements.push(statement);
            self.end_of_statement()?;
        }

        self.end_of_statement()?;
        Ok(View {
            source: Arc::clone(self.source),
            line,
            language,
            pattern,
            header: header.into(),
            extend,
            statements,
        })
    }

    fn statement(&mut self) -> Parsed<Statement> {
        let start = self.position;
        let line = self.location(start).0;
        let Some(keyword) = self.peek_word() else {
            return Err(self.unexpected("a statement"));
        };

        match keyword {
            "let" => {
                self.position += keyword.len();
                let name = self.name("a name")?;
                self.skip_inline();
                self.expect("=", "after the name")?;
                let mut alternatives = vec![self.expression()?];
                while self.eat_word_after_blank("or") {
                    alternatives.push(self.expression()?);
                }

                Ok(Statement::Let {
                    name,
                    alternatives,
                    line,
                })
            }
            "type" => {
                self.position += keyword.len();
                let name = self.name("a name")?;
                self.skip_inline();
                self.expect("=", "after the name")?;
                let mut alternatives = vec![self.type_expr(0)?];
                while self.eat_word_after_blank("or") {
                    alternatives.push(self.type_expr(0)?);
                }

                Ok(Statement::Type {
                    name,
                    alternatives,
                    line,
                })
            }
            "check" => {
                self.position += keyword.len();
                Ok(Statement::Check(self.expression()?))
            }
            "summary" => {
                self.position += keyword.len();
                self.skip_inline();
                Ok(Statement::Summary(self.template()?))
            }
            "field" => {
                self.position += keyword.len();
                let name = self.name("a field's name")?;
                self.skip_inline();
                self.expect("=", "after the field's name")?;
                let value = self.expression()?;
                Ok(Statement::Field { name, value })
            }
            "show" => {
                self.position += keyword.len();
                Ok(Statement::Show(self.shape(0)?))
            }

            // A statement-level `if` or `match` chooses between shapes.
            "if" | "match" => Ok(Statement::Show(self.shape(0)?)),
            "hide" => {
                self.position += keyword.len();
                Ok(Statement::Hide {
                    names: self.names("a member's or field's name")?,
                    line,
                })
            }
            "visualize" => {
                self.position += keyword.len();
                Ok(Statement::Visualize(self.visualize(line)?))
            }
            "format" => {
                self.position += keyword.len();
                let names = self.names("a member's or field's name")?;
                self.skip_inline();
                self.expect_word("as", "after the names `format` writes")?;
                self.skip_inline();
                Ok(Statement::Format {
                    names,
                    format: self.format()?,
                    line,
                })
            }
            _ => Err(self.unexpected(
                "a statement: `let`, `type`, `check`, `summary`, `field`, `show`, `hide`, `format`, or `visualize`",
            )),
        }
    }

    /// What follows `visualize`: the renderer's name, then its inputs in
    /// braces, separated by line ends or commas. Without braces, the one
    /// input is `values = self`.
    fn visualize(&mut self, line: u32) -> Parsed<Visualize> {
        self.skip_inline();
        let start = self.position;
        let name = self.string()?;
        if !is_renderer_name(&name) {
            return Err(self.error_at(start, RENDERER_NAME.to_owned()));
        }

        self.skip_inline();
        if !self.eat("{") {
            return Ok(Visualize {
                name,
                line,
                inputs: vec![Input {
                    name: "values".to_owned(),
                    line,
                    value: InputValue::Value(vec![self.synthetic_self(start)?]),
                }],
            });
        }

        let mut inputs: Vec<Input> = Vec::new();

        loop {
            self.skip_blank();
            if self.eat("}") {
                return Ok(Visualize { name, line, inputs });
            }

            if inputs.len() == MAX_INPUTS {
                return Err(self.error(format!(
                    "a `visualize` may name at most {MAX_INPUTS} inputs"
                )));
            }

            let start = self.position;
            let input = self.name("an input's name")?;
            if inputs.iter().any(|existing| existing.name == input) {
                return Err(self.error_at(start, format!("the drawing has two inputs `{input}`")));
            }

            self.skip_inline();
            self.expect("=", "after the input's name")?;
            self.skip_inline();
            let input_line = self.location(start).0;
            let value = if self.peek() == Some(b'"') {
                InputValue::Text(self.string()?)
            } else if self.eat_call("bytes")? {
                self.skip_blank();
                let pointer = self.member_expression()?;
                self.skip_blank();
                self.expect(",", "between the pointer and the length")?;
                self.skip_blank();
                let length = self.member_expression()?;
                self.close_call("bytes")?;
                InputValue::Bytes { pointer, length }
            } else {
                let mut alternatives = vec![self.member_expression()?];
                while self.eat_word_after_blank("or") {
                    alternatives.push(self.member_expression()?);
                }

                InputValue::Value(alternatives)
            };

            inputs.push(Input {
                name: input,
                line: input_line,
                value,
            });
            self.skip_inline();
            self.eat(",");
        }
    }

    /// `self`, as the expression of the input a `visualize` without
    /// braces hands its renderer.
    fn synthetic_self(&self, at: usize) -> Parsed<Expr> {
        Expression::parse_view("self")
            .map(|expression| Expr {
                expression,
                line: self.location(at).0,
            })
            .map_err(|error| self.error_at(at, error.to_string()))
    }

    /// An expression that is one member of braces, which also ends at a line
    /// that begins the next member, `NAME =`.
    fn member_expression(&mut self) -> Parsed<Expr> {
        let members = std::mem::replace(&mut self.members, true);
        let expression = self.expression();
        self.members = members;
        expression
    }

    /// `NAME, …`: one or more names.
    fn names(&mut self, what: &str) -> Parsed<Vec<String>> {
        let mut names = vec![self.name(what)?];
        loop {
            self.skip_inline();
            if !self.eat(",") {
                return Ok(names);
            }

            names.push(self.name(what)?);
        }
    }

    /// Where the expression of the hole between `start` and `end` ends,
    /// and the format it is written in when it ends `as FORMAT`; a cast to
    /// a type, as `{x as u64}`, is the expression's own.
    fn hole_format(&mut self, start: usize, end: usize) -> (usize, Option<Format>) {
        let hole = &self.text[start..end];
        let Some(at) = hole.rfind(" as ") else {
            return (end, None);
        };

        let saved = self.position;
        self.position = start + at + " as ".len();
        let format = self.format().ok().filter(|_| {
            self.skip_inline();
            self.position == end
        });
        self.position = saved;
        format.map_or((end, None), |format| (start + at, Some(format)))
    }

    /// What follows `format NAME as`: `hex`, `char`, `bytes`, `utf8`,
    /// `utf16`, `flags(TYPE)`, `enum(TYPE)`, `duration(UNIT)`, or
    /// `time(UNIT)`.
    fn format(&mut self) -> Parsed<Format> {
        let Some(word) = self.word().map(str::to_owned) else {
            return Err(self.unexpected("a format"));
        };

        Ok(match word.as_str() {
            "hex" => Format::Hex,
            "char" => Format::Char,
            "bytes" => Format::Bytes,
            "utf8" => Format::Utf8,
            "utf16" => Format::Utf16,
            "flags" | "enum" => {
                self.open_call(&word)?;
                let ty = self.type_expr(0)?;
                self.close_call(&word)?;
                if word == "flags" {
                    Format::Flags(ty)
                } else {
                    Format::Enum(ty)
                }
            }
            "duration" | "time" => {
                self.open_call(&word)?;
                self.skip_blank();
                let unit = self
                    .word()
                    .and_then(TimeUnit::parse)
                    .ok_or_else(|| self.unexpected("a unit of time: `ns`, `us`, `ms`, or `s`"))?;
                self.close_call(&word)?;
                if word == "duration" {
                    Format::Duration(unit)
                } else {
                    Format::Time(unit)
                }
            }
            _ => {
                return Err(self.error(format!(
                    "`{word}` is no format: write `hex`, `char`, `bytes`, `utf8`, `utf16`, `flags(TYPE)`, `enum(TYPE)`, `duration(UNIT)`, or `time(UNIT)`"
                )));
            }
        })
    }

    /// Takes `word` when it is the next word, on this line or a later one.
    fn eat_word_after_blank(&mut self, word: &str) -> bool {
        let start = self.position;
        self.skip_blank();
        if self.eat_word(word) {
            return true;
        }

        self.position = start;
        false
    }

    #[expect(clippy::too_many_lines, reason = "one arm per shape")]
    fn shape(&mut self, depth: usize) -> Parsed<Shape> {
        if depth > MAX_SHAPE_DEPTH {
            return Err(self.error(format!("shapes may nest at most {MAX_SHAPE_DEPTH} deep")));
        }

        self.skip_inline();
        let Some(word) = self.peek_word() else {
            return Err(self.unexpected("a shape"));
        };

        match word {
            "text" => {
                self.position += word.len();
                self.open_call("text")?;
                let pointer = self.expression()?;
                self.skip_blank();
                let length = if self.eat(",") {
                    Some(self.expression()?)
                } else {
                    None
                };

                self.close_call("text")?;
                Ok(Shape::Text { pointer, length })
            }
            "value" => {
                self.position += word.len();
                self.open_call("value")?;
                let value = self.expression()?;
                self.close_call("value")?;
                Ok(Shape::Value(value))
            }
            "empty" => {
                self.position += word.len();
                self.open_call("empty")?;
                self.skip_blank();
                let text = self.template()?;
                self.close_call("empty")?;
                Ok(Shape::Empty(text))
            }
            "sequence" | "map" => {
                self.position += word.len();
                self.open_call(word)?;
                self.skip_blank();
                let count = if self.eat_word("_") {
                    Count::Unknown
                } else {
                    Count::Known(self.expression()?)
                };

                self.close_call(word)?;
                let clauses = self.clauses()?;
                self.skip_blank();
                if word == "sequence" {
                    self.expect("=>", "before the element")?;
                    let element = self.expression()?;
                    return Ok(Shape::Sequence {
                        count,
                        clauses,
                        element,
                    });
                }
                self.expect("=>", "before the entry's key")?;
                let key = self.key()?;
                self.skip_blank();
                self.expect(":", "between the entry's key and value")?;
                let value = self.expression()?;
                Ok(Shape::Map {
                    count,
                    clauses,
                    key,
                    value,
                })
            }
            "if" => {
                self.position += word.len();
                let condition = self.expression()?;
                let then = self.branch(depth)?;
                self.skip_blank();
                self.expect_word("else", "after the `if` branch")?;
                self.skip_blank();
                let otherwise = if self.peek_word() == Some("if") {
                    self.shape(depth + 1)?
                } else {
                    self.branch(depth)?
                };

                Ok(Shape::If {
                    condition,
                    then: Box::new(then),
                    otherwise: Box::new(otherwise),
                })
            }
            "record" | "tuple" => {
                self.position += word.len();
                self.record(word == "tuple")
            }
            "dynamic" => {
                self.position += word.len();
                self.open_call("dynamic")?;
                let pointer = self.expression()?;
                self.skip_blank();
                self.expect(",", "between `dynamic`'s pointer and type")?;
                self.skip_blank();
                let ty = if self.eat_call("arg")? {
                    let of = self.type_expr(0)?;
                    self.skip_blank();
                    self.expect(",", "between `arg`'s type and position")?;
                    let index = self.expression()?;
                    self.close_call("arg")?;
                    DynamicType::Argument { of, index }
                } else {
                    let ty = self.type_expr(0)?;
                    if self.eat_word_after_blank("of") {
                        DynamicType::Function {
                            ty,
                            code: self.expression()?,
                        }
                    } else {
                        DynamicType::Fixed(ty)
                    }
                };

                self.close_call("dynamic")?;
                Ok(Shape::Dynamic { pointer, ty })
            }
            "match" => {
                self.position += word.len();
                self.match_shape(depth)
            }
            _ => Err(self.unexpected(
                "a shape: `text`, `value`, `empty`, `sequence`, `map`, `record`, `tuple`, `dynamic`, \
                 or `if`",
            )),
        }
    }

    /// `EXPR { VALUE => SHAPE, … _ => SHAPE }`, after `match`, as the
    /// chain of `if`s it means: each arm whose value equals the matched
    /// expression's, in order, then `_`, or a problem naming the value.
    fn match_shape(&mut self, depth: usize) -> Parsed<Shape> {
        let scrutinee = self.expression()?;
        self.skip_blank();
        self.expect("{", "to open the match's arms")?;
        let mut arms = Vec::new();
        let mut default = None;

        loop {
            self.skip_blank();
            if self.eat("}") {
                break;
            }

            if arms.len() == MAX_ARMS {
                return Err(self.error(format!("a match may have at most {MAX_ARMS} arms")));
            }

            let arm_start = self.position;
            if default.is_some() {
                return Err(self.error("the `_` arm is a match's last"));
            }

            let value = if self.eat_word("_") {
                None
            } else {
                Some(self.expression()?)
            };

            self.skip_blank();
            self.expect("=>", "after the arm's value")?;
            self.skip_blank();
            self.eat_word("show");
            let shape = self.shape(depth + 1)?;
            match value {
                Some(value) => {
                    let line = value.line;
                    let text = format!("({}) == ({})", scrutinee.text(), value.text());
                    let expression = Expression::parse_view(&text)
                        .map_err(|error| self.error_at(arm_start, error.to_string()))?;
                    arms.push((Expr { expression, line }, shape));
                }
                None => default = Some(shape),
            }

            self.skip_blank();
            self.eat(",");
        }

        if arms.is_empty() {
            return Err(self.error("a match needs an arm with a value"));
        }

        let mut shape = default.unwrap_or(Shape::Unmatched(scrutinee));

        for (condition, then) in arms.into_iter().rev() {
            shape = Shape::If {
                condition,
                then: Box::new(then),
                otherwise: Box::new(shape),
            };
        }

        Ok(shape)
    }

    /// `{ NAME = EXPR, … }`, after `record`: names, or positions from 0,
    /// each once.
    fn record(&mut self, tuple: bool) -> Parsed<Shape> {
        self.skip_blank();
        self.expect("{", "to open the record")?;
        let mut members: Vec<(String, Expr)> = Vec::new();

        loop {
            self.skip_blank();
            if self.eat("}") {
                return Ok(Shape::Record { members, tuple });
            }

            if members.len() == MAX_RECORD_MEMBERS {
                return Err(self.error(format!(
                    "a record may have at most {MAX_RECORD_MEMBERS} members"
                )));
            }

            let start = self.position;
            let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
            let name = if digits > 0 {
                self.position += digits;
                self.text[start..self.position].to_owned()
            } else {
                self.name("a member's name")?
            };

            if members.iter().any(|(existing, _)| *existing == name) {
                return Err(self.error_at(start, format!("the record has two members `{name}`")));
            }

            self.skip_inline();
            self.expect("=", "after the member's name")?;
            members.push((name, self.member_expression()?));
            self.skip_blank();
            self.eat(",");
        }
    }

    /// `for VAR in GENERATOR [if FILTER | let NAME = EXPR]…`, one or more,
    /// nested.
    fn clauses(&mut self) -> Parsed<Vec<Clause>> {
        let mut clauses = Vec::new();

        loop {
            self.skip_blank();
            if self.peek_word() != Some("for") {
                break;
            }

            if clauses.len() == MAX_CLAUSES {
                return Err(self.error(format!("generators may nest at most {MAX_CLAUSES} deep")));
            }

            self.position += "for".len();
            let mut variables = vec![self.name("the generator's variable")?];

            loop {
                self.skip_inline();
                if !self.eat(",") {
                    break;
                }

                if variables.len() == crate::view::kernel::MAX_WORDS {
                    return Err(self.error(format!(
                        "a kernel's items have at most {} words",
                        crate::view::kernel::MAX_WORDS
                    )));
                }

                variables.push(self.name("the generator's next variable")?);
            }

            self.expect_word("in", "after the generator's variable")?;
            self.skip_inline();
            let start = self.position;
            let generator = self.generator()?;
            if variables.len() > 1 && !matches!(generator, Generator::Kernel { .. }) {
                return Err(self.error_at(start, "only a kernel's items have several variables"));
            }

            let mut items = Vec::new();

            loop {
                self.skip_blank();
                if self.eat_word("if") {
                    items.push(Item::Filter(self.expression()?));
                } else if self.eat_word("let") {
                    let name = self.name("a name")?;
                    self.skip_inline();
                    self.expect("=", "after the name")?;
                    items.push(Item::Let {
                        name,
                        value: self.expression()?,
                    });
                } else {
                    break;
                }
            }

            clauses.push(Clause {
                variables,
                generator,
                items,
            });
        }

        if clauses.is_empty() {
            return Err(self.unexpected("`for` and a generator after the count"));
        }

        Ok(clauses)
    }

    /// `range(N)`, `list(HEAD, P => NEXT)`, `inorder(ROOT, P => LEFT,
    /// P => RIGHT)`, or `kernel("NAME", ARG, …)`.
    fn generator(&mut self) -> Parsed<Generator> {
        let Some(word @ ("range" | "list" | "inorder" | "kernel")) = self.peek_word() else {
            return Err(self.unexpected("a generator: `range`, `list`, `inorder`, or `kernel`"));
        };

        self.position += word.len();
        self.open_call(word)?;
        if word == "kernel" {
            return self.kernel();
        }

        let first = self.expression()?;
        let generator = match word {
            "range" => Generator::Range(first),
            "list" => Generator::List {
                head: first,
                next: self.link()?,
            },
            _ => Generator::Inorder {
                root: first,
                left: self.link()?,
                right: self.link()?,
            },
        };

        self.close_call(word)?;
        Ok(generator)
    }

    /// A kernel's name and arguments, after `kernel(`.
    fn kernel(&mut self) -> Parsed<Generator> {
        self.skip_blank();
        let start = self.position;
        let name = self.string()?;
        if !is_kernel_name(&name) {
            return Err(self.error_at(start, KERNEL_NAME.to_owned()));
        }

        let mut arguments = Vec::new();

        loop {
            self.skip_blank();
            if !self.eat(",") {
                break;
            }

            if arguments.len() == crate::view::kernel::MAX_ARGUMENTS {
                return Err(self.error(format!(
                    "a kernel takes at most {} arguments",
                    crate::view::kernel::MAX_ARGUMENTS
                )));
            }

            self.skip_blank();
            arguments.push(self.expression()?);
        }

        self.close_call("kernel")?;
        Ok(Generator::Kernel {
            name,
            line: self.location(start).0,
            arguments,
        })
    }

    /// `, P => EXPR`.
    fn link(&mut self) -> Parsed<Link> {
        self.skip_blank();
        self.expect(",", "before the next argument")?;
        self.skip_blank();
        let parameter = self.name("the node's name")?;
        self.skip_blank();
        self.expect("=>", "after the node's name")?;
        let expression = self.expression()?;
        Ok(Link {
            parameter,
            expression,
        })
    }

    /// An entry's key, which ends at a `:` of its own.
    fn key(&mut self) -> Parsed<Expr> {
        self.skip_inline();
        let start = self.position;
        let end = self.scan_expression_until(true);
        let expression = self.expression_text(start, end)?;
        self.position = end;
        Ok(expression)
    }

    /// `{ [show] SHAPE }`.
    fn branch(&mut self, depth: usize) -> Parsed<Shape> {
        self.skip_blank();
        self.expect("{", "to open the branch")?;
        self.skip_blank();
        self.eat_word("show");
        let shape = self.shape(depth + 1)?;
        self.skip_blank();
        self.expect("}", "to close the branch")?;
        Ok(shape)
    }

    /// Takes `name(` when it is next, as a call rather than a name.
    fn eat_call(&mut self, name: &str) -> Parsed<bool> {
        let call = self.peek_word() == Some(name)
            && self.rest()[name.len()..].trim_start().starts_with('(');
        if call {
            self.position += name.len();
            self.open_call(name)?;
        }

        Ok(call)
    }

    fn open_call(&mut self, name: &str) -> Parsed<()> {
        self.skip_inline();
        self.expect("(", &format!("after `{name}`"))
    }

    fn close_call(&mut self, name: &str) -> Parsed<()> {
        self.skip_blank();
        self.expect(")", &format!("to close `{name}(`"))
    }

    /// A string literal: `"text"`, with the expression language's escapes.
    fn string(&mut self) -> Parsed<String> {
        let start = self.position;
        if !self.eat("\"") {
            return Err(self.unexpected("a string in double quotes"));
        }

        let mut text = String::new();

        loop {
            let Some(character) = self.rest().chars().next() else {
                return Err(self.error_at(start, "the string is not closed"));
            };

            self.position += character.len_utf8();
            match character {
                '"' => return Ok(text),
                '\n' => return Err(self.error_at(start, "the string is not closed")),
                '\\' => text.push(self.escape(start)?),
                character => text.push(character),
            }
        }
    }

    fn escape(&mut self, start: usize) -> Parsed<char> {
        let Some(character) = self.rest().chars().next() else {
            return Err(self.error_at(start, "the string is not closed"));
        };

        self.position += character.len_utf8();
        Ok(match character {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '0' => '\0',
            '\\' | '"' | '\'' | '{' | '}' => character,
            _ => {
                return Err(self.error_at(
                    self.position - character.len_utf8() - 1,
                    format!("`\\{character}` is not an escape"),
                ));
            }
        })
    }

    /// A summary's template: literal text and `{expression}` holes.
    fn template(&mut self) -> Parsed<Vec<Piece>> {
        let start = self.position;
        if !self.eat("\"") {
            return Err(self.unexpected("a summary in double quotes"));
        }

        let mut pieces = Vec::new();
        let mut literal = String::new();

        loop {
            let Some(character) = self.rest().chars().next() else {
                return Err(self.error_at(start, "the summary is not closed"));
            };

            match character {
                '"' => {
                    self.position += 1;
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(literal));
                    }

                    return Ok(pieces);
                }
                '\n' => return Err(self.error_at(start, "the summary is not closed")),
                '\\' => {
                    self.position += 1;
                    literal.push(self.escape(start)?);
                }
                '{' => {
                    self.position += 1;
                    if !literal.is_empty() {
                        pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                    }

                    let end = self.rest().find('}').ok_or_else(|| {
                        self.error("the `{` in the summary is not closed with `}`")
                    })?;
                    let hole_start = self.position;
                    let hole = &self.text[hole_start..hole_start + end];
                    if hole.contains(['"', '\n']) {
                        return Err(self.error("a summary's `{…}` holds one expression"));
                    }

                    let (value_end, format) = self.hole_format(hole_start, hole_start + end);
                    pieces.push(Piece::Hole {
                        value: self.expression_text(hole_start, value_end)?,
                        format,
                    });
                    self.position = hole_start + end + 1;
                }
                '}' => return Err(self.error("write `\\}` for a `}` in a summary")),
                character => {
                    self.position += character.len_utf8();
                    literal.push(character);
                }
            }
        }
    }

    /// A type for a `type` statement.
    fn type_expr(&mut self, depth: usize) -> Parsed<TypeExpr> {
        if depth > MAX_PATTERN_DEPTH {
            return Err(self.error("the type nests too deeply"));
        }

        self.skip_inline();
        let mut ty = self.type_operand(depth)?;

        // `.NAME` or `::NAME` after `typeof(…)` or `arg(…)` names a type
        // declared inside it.
        while matches!(
            ty,
            TypeExpr::TypeOf(_) | TypeExpr::Arg { .. } | TypeExpr::Nested { .. }
        ) {
            let separator = if self.rest().starts_with("::") {
                2
            } else if self.rest().starts_with('.') {
                1
            } else {
                break;
            };

            self.position += separator;
            let Some(name) = self.word() else {
                return Err(self.unexpected("the name of a type declared inside it"));
            };

            ty = TypeExpr::Nested {
                of: Box::new(ty),
                name: name.to_owned(),
            };
        }

        Ok(ty)
    }

    /// A type for a `type` statement, without the types declared inside it.
    fn type_operand(&mut self, depth: usize) -> Parsed<TypeExpr> {
        if self.eat_call("typeof")? {
            let expression = self.expression()?;
            self.close_call("typeof")?;
            return Ok(TypeExpr::TypeOf(expression));
        }

        if self.eat_call("arg")? {
            let of = self.type_expr(depth + 1)?;
            self.skip_blank();
            self.expect(",", "between `arg`'s type and position")?;
            self.skip_blank();
            let digits = self.rest().bytes().take_while(u8::is_ascii_digit).count();
            let index = self.rest()[..digits]
                .parse::<u32>()
                .map_err(|_| self.unexpected("an argument's position"))?;
            self.position += digits;
            self.close_call("arg")?;
            return Ok(TypeExpr::Arg {
                of: Box::new(of),
                index,
            });
        }

        let start = self.position;
        let end = self.scan_type_name();
        let text = self.text[start..end].trim();
        if text.is_empty() {
            return Err(self.unexpected("a type"));
        }

        self.position = end;
        let name =
            text.trim_end_matches(|character: char| character == '*' || character.is_whitespace());
        let pointers = text[name.len()..].matches('*').count();
        Ok(TypeExpr::Named {
            name: name.to_owned(),
            pointers: u8::try_from(pointers).unwrap_or(u8::MAX),
        })
    }

    /// The end of a type's name: before `or`, `,`, or `)` outside its
    /// brackets, or the end of the line.
    fn scan_type_name(&self) -> usize {
        let bytes = self.text.as_bytes();
        let mut depth = 0_usize;
        let mut index = self.position;

        while let Some(&byte) = bytes.get(index) {
            match byte {
                b'`' => {
                    index += 1 + self.text[index + 1..].find('`').unwrap_or(self.text.len());
                }
                b'<' | b'(' | b'[' => depth += 1,
                b'>' | b']' => depth = depth.saturating_sub(1),
                b')' | b',' | b'\n' | b'#' if depth == 0 => return index,
                b')' => depth -= 1,
                byte if depth == 0
                    && is_word_start(byte)
                    && (index == 0 || !is_word_continue(bytes[index - 1]))
                    && (self.text[index..].starts_with("or")
                        || self.text[index..].starts_with("of"))
                    && !bytes.get(index + 2).copied().is_some_and(is_word_continue) =>
                {
                    return index;
                }
                _ => {}
            }

            index += 1;
        }

        index.min(self.text.len())
    }

    /// The expression from the cursor to where it ends.
    fn expression(&mut self) -> Parsed<Expr> {
        self.skip_inline();
        let start = self.position;
        let end = self.scan_expression_until(false);
        let expression = self.expression_text(start, end)?;
        self.position = end;
        Ok(expression)
    }

    /// Parses the text between two offsets, with comments blanked out, as
    /// a view's expression.
    fn expression_text(&self, start: usize, end: usize) -> Parsed<Expr> {
        let raw = &self.text[start..end];
        if raw.trim().is_empty() {
            return Err(self.error_at(start, "expected an expression"));
        }

        let text = blank_comments(raw)
            .ok_or_else(|| self.error_at(start, "the text has no character here"))?;
        let line = self.location(start).0;

        Expression::parse_view(&text)
            .map(|expression| Expr { expression, line })
            .map_err(|error| {
                let at = start + (error.span.start as usize).min(raw.len());
                self.error_at(at, error.to_string())
            })
    }

    /// Where the expression at the cursor ends; with `colon`, also at a `:`
    /// that no `?` before it in the expression takes and that does not
    /// begin `::`.
    fn scan_expression_until(&self, colon: bool) -> usize {
        let bytes = self.text.as_bytes();
        let mut depth = 0_usize;
        let mut conditionals = 0_usize;
        let mut index = self.position;
        let mut end = index;

        while let Some(&byte) = bytes.get(index) {
            match byte {
                b'?' if depth == 0 => conditionals += 1,
                b':' if depth == 0 && bytes.get(index + 1) == Some(&b':') => {
                    index += 2;
                    end = index;
                    continue;
                }
                b':' if depth == 0 && conditionals > 0 => conditionals -= 1,
                b':' if depth == 0 && colon => return end,
                b'"' | b'\'' => {
                    index = skip_literal(self.text, index, byte);
                    end = index;
                    continue;
                }
                b'`' => {
                    index = self.text[index + 1..]
                        .find('`')
                        .map_or(self.text.len(), |close| index + close + 2);
                    end = index;
                    continue;
                }
                b'#' => {
                    index = self.text[index..]
                        .find('\n')
                        .map_or(self.text.len(), |line_end| index + line_end);
                    continue;
                }
                b'(' | b'[' => depth += 1,
                b')' | b']' | b'{' | b'}' | b',' | b';' if depth == 0 => return end,
                b')' | b']' => depth -= 1,
                b'=' if depth == 0 && bytes.get(index + 1) == Some(&b'>') => return end,
                b'\n' if depth == 0 && self.statement_ends_before(index + 1) => return end,
                byte if is_word_start(byte)
                    && (index == 0 || !is_word_continue(bytes[index - 1])) =>
                {
                    let length = bytes[index..]
                        .iter()
                        .take_while(|byte| is_word_continue(**byte))
                        .count();
                    let word = &self.text[index..index + length];
                    if depth == 0 && STOP_WORDS.contains(&word) {
                        return end;
                    }

                    index += length;
                    end = index;
                    continue;
                }
                _ => {}
            }

            index += 1;
            if !matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                end = index;
            }
        }

        end
    }

    /// Whether the line at `offset`, or the next line that is not blank,
    /// begins a statement or closes a view, which ends an expression.
    fn statement_ends_before(&self, offset: usize) -> bool {
        let mut rest = &self.text[offset.min(self.text.len())..];

        loop {
            let trimmed = rest.trim_start_matches([' ', '\t', '\r']);
            if let Some(after) = trimmed.strip_prefix('\n') {
                rest = after;
                continue;
            }

            if trimmed.starts_with('#') {
                match trimmed.find('\n') {
                    Some(line_end) => {
                        rest = &trimmed[line_end + 1..];
                        continue;
                    }
                    None => return true,
                }
            }

            if trimmed.is_empty() || trimmed.starts_with('}') {
                return true;
            }

            let length = trimmed
                .bytes()
                .take_while(|byte| is_word_continue(*byte))
                .count();

            // In braces of members, a line that begins `NAME =` begins the
            // next member.
            if self.members && length > 0 {
                let after = trimmed[length..].trim_start_matches([' ', '\t']);
                if after.starts_with('=') && !after.starts_with("==") && !after.starts_with("=>") {
                    return true;
                }
            }

            return STATEMENTS.contains(&&trimmed[..length]);
        }
    }
}

/// The offset after a string or character literal that begins at `start`.
fn skip_literal(text: &str, start: usize, quote: u8) -> usize {
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\\' => index += 2,
            b'\n' => return index,
            byte if byte == quote => return index + 1,
            _ => index += 1,
        }
    }

    text.len()
}

/// The text with every comment replaced by spaces, so its offsets stay.
fn blank_comments(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'"' | b'\'') => {
                let end = skip_literal(text, index, quote).min(text.len());
                out.push_str(&text[index..end]);
                index = end;
            }
            b'`' => {
                let end = text[index + 1..]
                    .find('`')
                    .map_or(text.len(), |close| index + close + 2);
                out.push_str(&text[index..end]);
                index = end;
            }
            b'#' => {
                let end = text[index..]
                    .find('\n')
                    .map_or(text.len(), |line_end| index + line_end);
                out.extend(std::iter::repeat_n(' ', end - index));
                index = end;
            }
            _ => {
                let character = text[index..].chars().next()?;
                out.push(character);
                index += character.len_utf8();
            }
        }
    }

    Some(out)
}

/// Parses a type with arguments, written as a pattern is.
pub fn type_pattern(text: &str) -> Result<Pattern, String> {
    parse_pattern(text, 0)
}

/// Parses a type pattern.
fn parse_pattern(text: &str, depth: usize) -> Result<Pattern, String> {
    if depth > MAX_PATTERN_DEPTH {
        return Err("the pattern nests too deeply".to_owned());
    }

    let text = text.trim();
    if text.is_empty() {
        return Err("expected a type pattern".to_owned());
    }

    let (qualified, arguments) = split_arguments(text)?;
    let mut segments = split_path(qualified.trim())?;
    let base = match segments.pop() {
        Some(Segment::Name(base)) => base,
        Some(Segment::AnyRun) => return Err("`**` cannot end a pattern".to_owned()),
        None => return Err("expected a type's name".to_owned()),
    };

    let arguments = arguments
        .map(|arguments| {
            arguments
                .into_iter()
                .map(|argument| parse_argument(argument, depth))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;

    Ok(Pattern {
        path: segments,
        base,
        arguments,
    })
}

fn parse_argument(text: &str, depth: usize) -> Result<ArgumentPattern, String> {
    let text = text.trim();
    if text == "_" {
        return Ok(ArgumentPattern::Wildcard);
    }

    if let Some(value) = crate::type_identity::parse_integer(text) {
        return Ok(ArgumentPattern::Value(value));
    }

    let capture = text.starts_with(|character: char| character.is_ascii_uppercase())
        && text.bytes().all(is_word_continue);
    if capture {
        return Ok(ArgumentPattern::Capture(text.to_owned()));
    }

    parse_pattern(text, depth + 1).map(ArgumentPattern::Type)
}

/// Splits `a::b<x, y>` into `a::b` and its arguments, in `<>`, `()`, or
/// `[]`.
fn split_arguments(text: &str) -> Result<(&str, Option<Vec<&str>>), String> {
    let Some(open) = outside_backticks(text)
        .find(|(_, character)| matches!(character, '<' | '(' | '['))
        .map(|(index, _)| index)
    else {
        return Ok((text, None));
    };

    let close = match text.as_bytes()[open] {
        b'<' => '>',
        b'(' => ')',
        _ => ']',
    };

    let inner = text[open + 1..]
        .strip_suffix(close)
        .ok_or_else(|| format!("the arguments must end the pattern with `{close}`"))?;
    let mut arguments = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0;

    for (index, character) in inner.char_indices() {
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| "the pattern's brackets do not balance".to_owned())?;
            }
            ',' if depth == 0 => {
                arguments.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }

    if depth != 0 {
        return Err("the pattern's brackets do not balance".to_owned());
    }

    if !inner.trim().is_empty() {
        arguments.push(&inner[start..]);
    }

    if arguments.iter().any(|argument| argument.trim().is_empty()) {
        return Err("an argument is empty".to_owned());
    }

    Ok((&text[..open], Some(arguments)))
}

/// The characters of `text` outside backticks, with their offsets.
fn outside_backticks(text: &str) -> impl Iterator<Item = (usize, char)> + '_ {
    let mut quoted = false;
    text.char_indices().filter(move |(_, character)| {
        if *character == '`' {
            quoted = !quoted;
            return false;
        }
        !quoted
    })
}

/// Splits a path at `::` or `.` outside backticks.
fn split_path(text: &str) -> Result<Vec<Segment>, String> {
    let text = text.strip_prefix("::").unwrap_or(text);
    let mut parts = Vec::new();
    let mut start = 0;
    let mut skip = 0;

    for (index, character) in outside_backticks(text) {
        if index < skip {
            continue;
        }

        if text[index..].starts_with("::") {
            parts.push(&text[start..index]);
            start = index + 2;
            skip = start;
        } else if character == '.' && !text[index..].starts_with("**") {
            parts.push(&text[start..index]);
            start = index + 1;
        }
    }

    parts.push(&text[start..]);

    let mut segments = Vec::new();

    for part in parts {
        let part = part.trim();
        if part == "**" {
            if segments.last() == Some(&Segment::AnyRun) {
                return Err("`**` follows `**`".to_owned());
            }

            segments.push(Segment::AnyRun);
            continue;
        }

        let name = part
            .strip_prefix('`')
            .and_then(|quoted| quoted.strip_suffix('`'))
            .unwrap_or(part);
        let plain = part.starts_with('`')
            || name.bytes().next().is_some_and(is_word_start) && name.bytes().all(is_word_continue);
        if name.is_empty() || !plain {
            return Err(format!("`{part}` is not a name"));
        }

        segments.push(Segment::Name(name.to_owned()));
    }

    Ok(segments)
}
