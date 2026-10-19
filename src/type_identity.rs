//! Type identities: how the names producers give types split into paths,
//! bases, and arguments, and an image's index from identities to types.
//!
//! Debug information's structure comes first. A name is parsed only for
//! what the structure does not say, and an argument that does not resolve to
//! exactly one type stays unknown rather than being guessed.

pub mod functions;

use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;
use std::sync::Arc;

use foldhash::{HashMap, HashMapExt};
use rayon::prelude::*;

use crate::eval::types::c_type_key_of_name;
use crate::{
    ArrayOrdering, IntegerValue, ModuleImageId, SourceLanguage, TypeArgument, TypeId, TypeInfo,
    TypeKind, TypeModifier, TypeNode, TypeReference,
};

/// How a path spells a namespace without a name. Each unit's is its own.
pub const ANONYMOUS_NAMESPACE: &str = "(anonymous namespace)";

/// How deeply argument matching may nest before it gives up.
const MAX_ARGUMENT_DEPTH: usize = 16;

/// How a language spells qualified and generic type names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSyntax {
    /// `a::b::Name<T, 3>`: C, C++, and Rust.
    Angle,

    /// `path/to/pkg.Name[T,U]`.
    Go,

    /// `module.Name(T,null)`.
    Zig,

    /// `package::Name(T:$int,N:$$4)`, and `map[K]V`.
    Odin,

    /// `Name!T` and `Name!(T, 3)`, scoped by the module it is in.
    D,
}

impl NameSyntax {
    const ALL: [Self; 5] = [Self::Angle, Self::Go, Self::Zig, Self::Odin, Self::D];

    pub const fn of(language: SourceLanguage) -> Self {
        match language {
            SourceLanguage::Go => Self::Go,
            SourceLanguage::Zig => Self::Zig,
            SourceLanguage::Odin => Self::Odin,
            SourceLanguage::D => Self::D,
            _ => Self::Angle,
        }
    }
}

/// A type name split into its parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeName<'a> {
    /// The qualifying segments the name spells, outermost first.
    pub path: Vec<&'a str>,

    /// The name without its path or arguments.
    pub base: &'a str,

    /// The argument texts, when the name has an argument list.
    pub arguments: Option<Vec<&'a str>>,
}

impl<'a> TypeName<'a> {
    /// Splits a name. A name the syntax does not describe, such as `&str`,
    /// `[]int`, or `unsigned int`, is all base.
    pub fn parse(name: &'a str, syntax: NameSyntax) -> Self {
        let name = name.trim();
        let parsed = match syntax {
            NameSyntax::Angle => parse_angle(name),
            NameSyntax::Go => parse_go(name),
            NameSyntax::Zig => parse_zig(name),
            NameSyntax::Odin => parse_odin(name),
            NameSyntax::D => parse_d(name),
        };

        parsed.unwrap_or(Self {
            path: Vec::new(),
            base: name,
            arguments: None,
        })
    }
}

fn parse_angle(name: &str) -> Option<TypeName<'_>> {
    let name = name.strip_prefix("::").unwrap_or(name);
    let mut segments = split_top_level(name, "::", NameSyntax::Angle)?;
    let last = segments.pop()?;
    let (base, arguments) = split_arguments(last, '<', '>', NameSyntax::Angle)?;
    let base = base.trim_end();
    let named = is_identifier(base) || is_braced_scope(base);

    (named && segments.iter().all(|segment| is_angle_segment(segment))).then_some(TypeName {
        path: segments,
        base,
        arguments,
    })
}

/// One of Rust's braced scopes, such as `{impl#0}` or a closure's
/// `{closure_env#0}`, which names a type too.
fn is_braced_scope(text: &str) -> bool {
    text.starts_with('{') && text.ends_with('}')
}

fn parse_go(name: &str) -> Option<TypeName<'_>> {
    if !name.starts_with(is_identifier_start)
        || [
            "map[",
            "chan ",
            "chan<-",
            "func(",
            "struct {",
            "struct{",
            "interface {",
            "interface{",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return None;
    }

    let (qualified, arguments) = split_arguments(name, '[', ']', NameSyntax::Go)?;
    if qualified.contains(char::is_whitespace) {
        return None;
    }

    // A package path may hold dots before its last slash, never after.
    let after_slash = qualified.rfind('/').map_or(0, |slash| slash + 1);
    let (path, base) = match qualified[after_slash..].find('.') {
        Some(dot) => {
            let dot = after_slash + dot;
            (vec![&qualified[..dot]], &qualified[dot + 1..])
        }
        None if after_slash == 0 => (Vec::new(), qualified),
        None => return None,
    };

    (!base.is_empty()).then_some(TypeName {
        path,
        base,
        arguments,
    })
}

fn parse_zig(name: &str) -> Option<TypeName<'_>> {
    if !name.starts_with(is_identifier_start) {
        return None;
    }

    let (qualified, arguments) = split_arguments(name, '(', ')', NameSyntax::Zig)?;
    let mut segments = qualified.split('.').collect::<Vec<_>>();
    let base = segments.pop()?;

    (is_identifier(base) && segments.iter().all(|segment| is_identifier(segment))).then_some(
        TypeName {
            path: segments,
            base,
            arguments,
        },
    )
}

fn parse_odin(name: &str) -> Option<TypeName<'_>> {
    // Every map is `map[K]V`, whose arguments are its key and its value.
    if let Some(rest) = name.strip_prefix("map[") {
        let close = top_level_position(rest, ']', NameSyntax::Odin)?;
        let (key, value) = (&rest[..close], &rest[close + 1..]);

        return (!key.is_empty() && !value.is_empty()).then(|| TypeName {
            path: Vec::new(),
            base: "map",
            arguments: Some(vec![key, value]),
        });
    }

    if !name.starts_with(is_identifier_start) || name.starts_with("proc") {
        return None;
    }

    let (qualified, arguments) = split_arguments(name, '(', ')', NameSyntax::Odin)?;
    let mut segments = split_top_level(qualified, "::", NameSyntax::Odin)?;
    let base = segments.pop()?;

    // An argument names its parameter: `T:$int` is the type `int`, and
    // `N:$$4` the constant 4.
    let arguments = arguments
        .map(|arguments| {
            arguments
                .into_iter()
                .map(|argument| {
                    let (_, value) = argument.split_once(':')?;
                    let value = value.trim_start_matches('$');
                    (!value.is_empty()).then_some(value)
                })
                .collect::<Option<Vec<_>>>()
        })
        .map_or(Some(None), |arguments| arguments.map(Some))?;

    (is_identifier(base) && segments.iter().all(|segment| is_identifier(segment))).then_some(
        TypeName {
            path: segments,
            base,
            arguments,
        },
    )
}

/// A template instance's name is its template's, `!`, and its one
/// argument or its parenthesized list of them.
fn parse_d(name: &str) -> Option<TypeName<'_>> {
    let Some(bang) = top_level_position(name, '!', NameSyntax::D) else {
        return is_identifier(name).then_some(TypeName {
            path: Vec::new(),
            base: name,
            arguments: None,
        });
    };

    let (base, argument) = (&name[..bang], &name[bang + 1..]);
    let arguments = match split_arguments(argument, '(', ')', NameSyntax::D)? {
        ("", Some(arguments)) => arguments,
        (argument, None) if !argument.is_empty() => vec![argument],
        _ => return None,
    };

    is_identifier(base).then_some(TypeName {
        path: Vec::new(),
        base,
        arguments: Some(arguments),
    })
}

const fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

fn is_identifier(text: &str) -> bool {
    text.starts_with(is_identifier_start)
        && text
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$'))
}

/// A path segment: an identifier with any arguments, an anonymous
/// namespace, or one of Rust's braced scopes such as `{impl#0}`.
fn is_angle_segment(segment: &str) -> bool {
    segment == ANONYMOUS_NAMESPACE
        || is_braced_scope(segment)
        || split_arguments(segment, '<', '>', NameSyntax::Angle)
            .is_some_and(|(base, _)| is_identifier(base.trim_end()))
}

/// Splits `Name<A, B>` into `Name` and its argument texts. A name without
/// an argument list has none; a list that does not end the name, or that
/// is unbalanced, is not one.
fn split_arguments(
    text: &str,
    open: char,
    close: char,
    syntax: NameSyntax,
) -> Option<(&str, Option<Vec<&str>>)> {
    let Some(start) = top_level_position(text, open, syntax) else {
        return Some((text, None));
    };

    let inner = text[start + open.len_utf8()..].strip_suffix(close)?;

    // The list must close only at the very end, and be balanced, which
    // splitting it checks.
    if top_level_position(inner, close, syntax).is_some() {
        return None;
    }

    let arguments = if inner.trim().is_empty() {
        Vec::new()
    } else {
        split_top_level(inner, ",", syntax)?
            .into_iter()
            .map(str::trim)
            .collect()
    };

    Some((&text[..start], Some(arguments)))
}

/// Splits at every occurrence of `separator` outside brackets.
fn split_top_level<'a>(text: &'a str, separator: &str, syntax: NameSyntax) -> Option<Vec<&'a str>> {
    let mut parts = Vec::new();
    let mut stack = Vec::new();
    let mut start = 0;
    let mut previous = None;
    let first = separator.chars().next();
    let mut characters = text.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if stack.is_empty() && Some(character) == first && text[index..].starts_with(separator) {
            parts.push(&text[start..index]);
            start = index + separator.len();

            // Skip the rest of the separator.
            while characters.peek().is_some_and(|(next, _)| *next < start) {
                characters.next();
            }

            previous = None;
            continue;
        }

        step(&mut stack, character, previous, syntax)?;
        previous = Some(character);
    }

    stack.is_empty().then(|| {
        parts.push(&text[start..]);
        parts
    })
}

/// The first position of `target` outside brackets.
fn top_level_position(text: &str, target: char, syntax: NameSyntax) -> Option<usize> {
    let mut stack = Vec::new();
    let mut previous = None;
    for (index, character) in text.char_indices() {
        if stack.is_empty() && character == target && !(target == '>' && previous == Some('-')) {
            return Some(index);
        }

        step(&mut stack, character, previous, syntax)?;
        previous = Some(character);
    }

    None
}

/// Tracks one character's effect on the open brackets. Angle brackets
/// count only in angle syntax and only outside other brackets, so `->` and
/// a comparison in a parenthesized value do not unbalance a name.
fn step(
    stack: &mut Vec<char>,
    character: char,
    previous: Option<char>,
    syntax: NameSyntax,
) -> Option<()> {
    let angles = syntax == NameSyntax::Angle && stack.last().is_none_or(|top| *top == '<');
    match character {
        '(' | '[' | '{' => stack.push(character),
        '<' if angles => stack.push(character),
        ')' | ']' | '}' => {
            let open = match character {
                ')' => '(',
                ']' => '[',
                _ => '{',
            };

            if stack.pop() != Some(open) {
                return None;
            }
        }
        '>' if angles && previous != Some('-') && stack.pop() != Some('<') => return None,
        _ => {}
    }

    Some(())
}

/// The integer an argument spells, with any C or Rust suffix.
pub fn parse_integer(text: &str) -> Option<IntegerValue> {
    let text = text.trim();

    // C++ names spell a truth value argument as a word.
    match text {
        "false" => return Some(IntegerValue::Unsigned(0)),
        "true" => return Some(IntegerValue::Unsigned(1)),
        _ => {}
    }

    let (negative, digits) = text
        .strip_prefix('-')
        .map_or((false, text), |rest| (true, rest));
    let (radix, digits) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
        .map_or((10, digits), |hex| (16, hex));
    let end = digits
        .find(|character: char| !character.is_digit(radix) && character != '_')
        .unwrap_or(digits.len());
    let (number, suffix) = digits.split_at(end);
    let suffix_valid = suffix.is_empty()
        || suffix
            .chars()
            .all(|character| matches!(character, 'u' | 'U' | 'l' | 'L'))
        || matches!(
            suffix,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
        );

    if number.is_empty() || !suffix_valid {
        return None;
    }

    let magnitude = u128::from_str_radix(&number.replace('_', ""), radix).ok()?;

    if negative {
        let magnitude = i128::try_from(magnitude).ok()?;
        Some(IntegerValue::Signed(-magnitude))
    } else {
        Some(IntegerValue::Unsigned(magnitude))
    }
}

/// Whether two integers are the same number, whatever their signedness.
pub fn same_integer(left: IntegerValue, right: IntegerValue) -> bool {
    match (left, right) {
        (IntegerValue::Signed(left), IntegerValue::Signed(right)) => left == right,
        (IntegerValue::Unsigned(left), IntegerValue::Unsigned(right)) => left == right,
        (IntegerValue::Signed(signed), IntegerValue::Unsigned(unsigned))
        | (IntegerValue::Unsigned(unsigned), IntegerValue::Signed(signed)) => {
            u128::try_from(signed).is_ok_and(|signed| signed == unsigned)
        }
    }
}

/// How patterns reach the types their arguments name.
pub trait TypeLookup {
    fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo>;
}

/// The argument texts one lookup has parsed in one syntax, so that
/// comparing a pattern with many candidates parses each text once.
struct Parses<'t> {
    syntax: NameSyntax,
    parsed: RefCell<HashMap<&'t str, Rc<TypeName<'t>>>>,
}

impl<'t> Parses<'t> {
    fn new(syntax: NameSyntax) -> Self {
        Self {
            syntax,
            parsed: RefCell::default(),
        }
    }

    fn parse(&self, text: &'t str) -> Rc<TypeName<'t>> {
        if let Some(parsed) = self.parsed.borrow().get(text) {
            return Rc::clone(parsed);
        }

        let parsed = Rc::new(TypeName::parse(text, self.syntax));
        self.parsed.borrow_mut().insert(text, Rc::clone(&parsed));
        parsed
    }
}

/// Whether `info` is a type `pattern` names. A pattern's path may omit
/// outer segments and spell or omit the type's inline namespaces, and with
/// `exact` false its arguments may omit trailing ones, as C++ omits
/// defaulted template arguments. Identities keep inline namespaces' names
/// but not their places, so a pattern may spell one anywhere in its path.
fn names_type<'t>(
    pattern: &TypeName<'t>,
    parses: &Parses<'t>,
    info: &TypeInfo,
    types: &dyn TypeLookup,
    exact: bool,
    depth: usize,
) -> bool {
    let syntax = parses.syntax;
    let Some(identity) = info.identity.as_deref() else {
        return false;
    };

    if depth > MAX_ARGUMENT_DEPTH || NameSyntax::of(identity.language) != syntax {
        return false;
    }

    let same_base = identity.base.as_ref() == pattern.base
        || c_type_key_of_name(&identity.base)
            .is_some_and(|key| c_type_key_of_name(pattern.base).is_some_and(|other| other == key));
    if !same_base {
        return false;
    }

    let path = || {
        pattern.path.iter().filter(|segment| {
            !identity
                .inline_namespaces
                .iter()
                .any(|inline| inline.as_ref() == **segment)
        })
    };

    let length = path().count();
    if length > identity.path.len()
        || !identity.path[identity.path.len() - length..]
            .iter()
            .zip(path())
            .all(|(have, want)| have.as_ref() == *want)
    {
        return false;
    }

    let Some(arguments) = &pattern.arguments else {
        return true;
    };

    if arguments.len() > identity.arguments.len()
        || exact && arguments.len() != identity.arguments.len()
    {
        return false;
    }

    arguments
        .iter()
        .zip(identity.arguments.iter())
        .all(|(text, argument)| argument_matches(text, argument, parses, types, depth + 1))
}

fn argument_matches<'t>(
    text: &'t str,
    argument: &TypeArgument,
    parses: &Parses<'t>,
    types: &dyn TypeLookup,
    depth: usize,
) -> bool {
    match argument {
        TypeArgument::Value(value) => {
            parse_integer(text).is_some_and(|parsed| same_integer(parsed, *value))
        }
        TypeArgument::Unknown(spelled) => spelled.as_ref() == text,
        TypeArgument::Type(reference) => types.type_info(*reference).is_some_and(|info| {
            if info.name.as_ref() == text {
                return true;
            }

            // A qualifier may be spelled before or after what it qualifies.
            if let TypeKind::Modified { modifier, target } = &info.kind
                && let Some(word) = match modifier {
                    TypeModifier::Const => Some("const"),
                    TypeModifier::Volatile => Some("volatile"),
                    _ => None,
                }
            {
                let rest = text
                    .strip_prefix(word)
                    .filter(|rest| rest.starts_with(' '))
                    .or_else(|| text.strip_suffix(word).filter(|rest| rest.ends_with(' ')));
                return rest.is_some_and(|rest| {
                    argument_matches(
                        rest.trim(),
                        &TypeArgument::Type(*target),
                        parses,
                        types,
                        depth + 1,
                    )
                });
            }

            names_type(&parses.parse(text), parses, info, types, true, depth)
        }),
    }
}

/// An image's types by identity.
#[derive(Debug, Default)]
pub struct TypeIndex {
    image: Option<ModuleImageId>,

    /// Types with identities, by base, in identifier order. A C base type
    /// is also under C's own spelling of it.
    by_base: HashMap<Arc<str>, Vec<TypeId>>,

    /// Every resolved type, by name.
    by_name: HashMap<Arc<str>, Vec<TypeId>>,

    /// Each type's identity as one string, so that one type in several
    /// units compares equal.
    keys: Vec<Arc<str>>,
}

impl TypeIndex {
    /// Indexes `count` types, which `info` reaches by index.
    #[cfg(any(test, feature = "tools"))]
    pub fn build<'a>(
        image: Option<ModuleImageId>,
        count: usize,
        info: impl Fn(usize) -> Option<&'a TypeInfo> + Sync,
    ) -> Self {
        Self::build_searching(image, count, |_| true, info)
    }

    /// Indexes `count` types, which `info` reaches by index, keying every
    /// one but letting names find only those `searchable` accepts.
    pub fn build_searching<'a>(
        image: Option<ModuleImageId>,
        count: usize,
        searchable: impl Fn(usize) -> bool + Sync,
        info: impl Fn(usize) -> Option<&'a TypeInfo> + Sync,
    ) -> Self {
        // The names and the keys are indexed beside each other: the keys
        // read no name, and each walk is serial.
        let ((by_base, by_name), keys) = rayon::join(
            || names_by_base(count, &searchable, &info),
            || canonical_keys(count, &info),
        );

        Self {
            image,
            by_base,
            by_name,
            keys,
        }
    }

    /// Frees the index on every worker: its keys and names are millions of
    /// allocations, a fifth of a second to free on one thread.
    pub fn free_in_parallel(self) {
        let Self {
            by_base,
            by_name,
            keys,
            ..
        } = self;
        rayon::join(
            || keys.into_par_iter().with_min_len(1 << 14).for_each(drop),
            || rayon::join(|| drop(by_base), || drop(by_name)),
        );
    }

    /// A type's identity as one string, which every type the same as it
    /// shares.
    pub fn key(&self, reference: TypeReference) -> Option<&Arc<str>> {
        if self.image != Some(reference.image) {
            return None;
        }

        self.keys.get(reference.id.index())
    }

    /// Whether two types of this image have the same identity, as one type
    /// defined in several units does.
    pub fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right
            || self.image == Some(left.image)
                && self.image == Some(right.image)
                && self
                    .keys
                    .get(left.id.index())
                    .zip(self.keys.get(right.id.index()))
                    .is_some_and(|(left, right)| left == right)
    }
}

impl NameIndex for TypeIndex {
    fn image(&self) -> Option<ModuleImageId> {
        self.image
    }

    fn by_name(&self, name: &str) -> Vec<TypeId> {
        self.by_name.get(name).cloned().unwrap_or_default()
    }

    fn by_base(&self, base: &str) -> Vec<TypeId> {
        self.by_base.get(base).cloned().unwrap_or_default()
    }
}

/// Types by name and by their identity's base, which name lookups search.
pub trait NameIndex {
    /// The image whose types these are, if it has any.
    fn image(&self) -> Option<ModuleImageId>;

    /// The resolved types named exactly `name`, in identifier order.
    fn by_name(&self, name: &str) -> Vec<TypeId>;

    /// The types whose identity has base `base`, or whose base C spells
    /// so, in identifier order.
    fn by_base(&self, base: &str) -> Vec<TypeId>;

    /// The reference to type `id` of this image.
    fn reference(&self, id: TypeId) -> Option<TypeReference> {
        self.image().map(|image| TypeReference { image, id })
    }

    /// The types with exactly this language, path, and base, whatever
    /// their arguments.
    fn instances(
        &self,
        language: SourceLanguage,
        path: &[&str],
        base: &str,
        types: &dyn TypeLookup,
    ) -> Vec<TypeReference> {
        self.by_base(base)
            .into_iter()
            .filter_map(|id| self.reference(id))
            .filter(|reference| {
                types
                    .type_info(*reference)
                    .and_then(|info| info.identity.as_deref())
                    .is_some_and(|identity| {
                        identity.language == language
                            && identity.base.as_ref() == base
                            && identity.path.len() == path.len()
                            && identity
                                .path
                                .iter()
                                .zip(path)
                                .all(|(have, want)| have.as_ref() == *want)
                    })
            })
            .collect()
    }

    /// The types whose identity has this base, in identifier order.
    fn with_base(&self, base: &str) -> Vec<TypeReference> {
        self.by_base(base)
            .into_iter()
            .filter_map(|id| self.reference(id))
            .collect()
    }

    /// The types a name, as a person or a producer writes it, could mean,
    /// in identifier order: those named exactly so, and those whose
    /// identity the name spells in any language's syntax.
    fn named(&self, text: &str, exact: bool, types: &dyn TypeLookup) -> Vec<TypeReference> {
        let mut found = self.by_name(text);

        for syntax in NameSyntax::ALL {
            let parses = Parses::new(syntax);
            let pattern = TypeName::parse(text, syntax);
            let key = c_type_key_of_name(pattern.base);
            let mut candidates = self.by_base(pattern.base);
            if let Some(key) = key.as_deref() {
                candidates.extend(self.by_base(key));
            }

            crate::count!("type_name_candidates", candidates.len());
            for id in candidates {
                let Some(reference) = self.reference(id) else {
                    continue;
                };

                if types
                    .type_info(reference)
                    .is_some_and(|info| names_type(&pattern, &parses, info, types, exact, 0))
                {
                    found.push(id);
                }
            }
        }

        found.sort_unstable();
        found.dedup();
        found
            .into_iter()
            .filter_map(|id| self.reference(id))
            .collect()
    }
}

/// The types `searchable` lets be found among `count`, by their identity's
/// base names and by their names.
#[expect(clippy::type_complexity, reason = "the two indexes a TypeIndex keeps")]
fn names_by_base<'a>(
    count: usize,
    searchable: &impl Fn(usize) -> bool,
    info: &impl Fn(usize) -> Option<&'a TypeInfo>,
) -> (
    HashMap<Arc<str>, Vec<TypeId>>,
    HashMap<Arc<str>, Vec<TypeId>>,
) {
    let mut by_base = HashMap::<Arc<str>, Vec<TypeId>>::new();
    let mut by_name = HashMap::<Arc<str>, Vec<TypeId>>::new();

    for index in 0..count {
        let Some(type_info) = info(index).filter(|_| searchable(index)) else {
            continue;
        };

        let id = type_info.reference.id;

        by_name
            .entry(Arc::clone(&type_info.name))
            .or_default()
            .push(id);
        if let Some(identity) = &type_info.identity {
            by_base
                .entry(Arc::clone(&identity.base))
                .or_default()
                .push(id);

            // A C base type is also under C's own spelling, whichever
            // words the producer chose: `short int` is `short`.
            if let Some(key) = c_type_key_of_name(&identity.base)
                && key != identity.base.as_ref()
            {
                by_base.entry(Arc::from(key)).or_default().push(id);
            }
        }
    }

    (by_base, by_name)
}

/// Each of `count` types' identity as one string, which every type the
/// same as it shares: [`TypeIndex::key`] without the index.
pub fn canonical_keys<'a>(
    count: usize,
    info: impl Fn(usize) -> Option<&'a TypeInfo>,
) -> Vec<Arc<str>> {
    let mut walk = KeyWalk {
        memo: vec![None; count],
        visiting: vec![false; count],
        depth: 0,
    };

    (0..count)
        .map(|index| canonical_key(index, &info, &mut walk))
        .collect()
}

/// A type's identity as one string: its identity when it has one, and
/// otherwise its shape over its targets' keys. A type in an anonymous
/// namespace is its unit's own, so its key is its own too.
fn canonical_key<'a>(
    index: usize,
    info: &impl Fn(usize) -> Option<&'a TypeInfo>,
    walk: &mut KeyWalk,
) -> Arc<str> {
    if let Some(Some(key)) = walk.memo.get(index) {
        return Arc::clone(key);
    }

    let Some(type_info) = info(index) else {
        return Arc::from(format!("<malformed #{index}>"));
    };

    if index >= walk.memo.len() {
        walk.memo.resize(index + 1, None);
        walk.visiting.resize(index + 1, false);
    }

    if walk.depth > MAX_ARGUMENT_DEPTH * 4 || walk.visiting[index] {
        return Arc::from(format!("<cycle #{index}>"));
    }

    walk.visiting[index] = true;
    walk.depth += 1;
    let mut key_of = |reference: TypeReference| canonical_key(reference.id.index(), info, walk);
    let key = if let Some(identity) = &type_info.identity {
        let mut key = format!("{:?}|", identity.language);
        if identity
            .path
            .iter()
            .any(|segment| segment.as_ref() == ANONYMOUS_NAMESPACE)
        {
            let _ = write!(key, "#{index}|");
        }

        for segment in identity.path.iter() {
            let _ = write!(key, "{segment}::");
        }

        key.push_str(&identity.base);
        if !identity.arguments.is_empty() {
            key.push('<');
            for (position, argument) in identity.arguments.iter().enumerate() {
                if position > 0 {
                    key.push(',');
                }

                match argument {
                    TypeArgument::Type(reference) => key.push_str(&key_of(*reference)),
                    TypeArgument::Value(IntegerValue::Signed(value)) => {
                        let _ = write!(key, "{value}");
                    }
                    TypeArgument::Value(IntegerValue::Unsigned(value)) => {
                        let _ = write!(key, "{value}");
                    }
                    TypeArgument::Unknown(text) => {
                        let _ = write!(key, "?{text}");
                    }
                }
            }

            key.push('>');
        }

        key
    } else {
        match &type_info.kind {
            TypeKind::Pointer { target: None, .. } => "*void".to_owned(),
            TypeKind::Pointer {
                target: Some(target),
                ..
            } => format!("*{}", key_of(*target)),
            TypeKind::Reference { kind, target, .. } => format!("&{kind:?} {}", key_of(*target)),
            TypeKind::Modified { modifier, target } => format!("{modifier:?} {}", key_of(*target)),
            TypeKind::Array {
                element,
                dimensions,
                ordering,
            } => {
                let mut key = String::new();
                if *ordering == ArrayOrdering::ColumnMajor {
                    key.push_str("column-major ");
                }

                for dimension in dimensions.iter() {
                    let _ = write!(key, "[{}]", dimension.count);
                }

                key.push_str(&key_of(*element));
                key
            }
            _ => format!("#{}", type_info.name),
        }
    };

    walk.visiting[index] = false;
    walk.depth -= 1;
    let key: Arc<str> = key.into();
    walk.memo[index] = Some(Arc::clone(&key));
    key
}

/// What [`canonical_key`] knows between types: each finished key, and the
/// types on the path it is walking.
struct KeyWalk {
    memo: Vec<Option<Arc<str>>>,
    visiting: Vec<bool>,
    depth: usize,
}

/// A module image's finalized types, for patterns to reach arguments.
impl TypeLookup for &[TypeNode] {
    fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        match self.get(reference.id.index())? {
            TypeNode::Resolved(info) if info.reference == reference => Some(info),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Names come from debug information, which may hold anything:
        /// parsing one never panics, and returns only pieces of it.
        #[test]
        fn any_name_splits_into_pieces_of_itself(name in "[a-z_:<>()\\[\\],. {}#&*0-9/!é·-]{0,32}") {
            for syntax in NameSyntax::ALL {
                let parsed = TypeName::parse(&name, syntax);
                prop_assert!(name.contains(parsed.base));
                for piece in parsed.path.iter().chain(parsed.arguments.iter().flatten()) {
                    prop_assert!(name.contains(piece));
                }
            }

            let _ = parse_integer(&name);
        }
    }

    fn parts(name: &str, syntax: NameSyntax) -> (Vec<&str>, &str, Option<Vec<&str>>) {
        let parsed = TypeName::parse(name, syntax);
        (parsed.path, parsed.base, parsed.arguments)
    }

    #[test]
    fn names_split_into_path_base_and_arguments_in_each_syntax() {
        use NameSyntax::{Angle, D, Go, Odin, Zig};
        assert_eq!(
            parts("vector<int, std::allocator<int> >", Angle),
            (vec![], "vector", Some(vec!["int", "std::allocator<int>"]))
        );
        assert_eq!(
            parts("alloc::boxed::Box<[u8], alloc::alloc::Global>", Angle),
            (
                vec!["alloc", "boxed"],
                "Box",
                Some(vec!["[u8]", "alloc::alloc::Global"])
            )
        );

        // `->` and a parenthesized comparison do not close the list.
        assert_eq!(
            parts("Vec<fn(i32) -> i32>", Angle),
            (vec![], "Vec", Some(vec!["fn(i32) -> i32"]))
        );
        assert_eq!(
            parts("Fixed<(3 > 2), short int>", Angle),
            (vec![], "Fixed", Some(vec!["(3 > 2)", "short int"]))
        );
        assert_eq!(
            parts("Outer<int>::Inner", Angle),
            (vec!["Outer<int>"], "Inner", None)
        );
        assert_eq!(
            parts("workers::top::{async_fn_env#0}", Angle),
            (vec!["workers", "top"], "{async_fn_env#0}", None)
        );
        assert_eq!(
            parts("{async_fn_env#0}<u32>", Angle),
            (vec![], "{async_fn_env#0}", Some(vec!["u32"]))
        );
        assert_eq!(
            parts("main.Pair[string,main.Point]", Go),
            (vec!["main"], "Pair", Some(vec!["string", "main.Point"]))
        );
        assert_eq!(
            parts("github.com/acme/pkg.Thing", Go),
            (vec!["github.com/acme/pkg"], "Thing", None)
        );
        assert_eq!(
            parts("array_list.Aligned(u32,null)", Zig),
            (vec!["array_list"], "Aligned", Some(vec!["u32", "null"]))
        );
        assert_eq!(
            parts("container_small_array::Small_Array(N:$$4,T:$int)", Odin),
            (
                vec!["container_small_array"],
                "Small_Array",
                Some(vec!["4", "int"])
            )
        );
        assert_eq!(
            parts("map[string][]main::Point", Odin),
            (vec![], "map", Some(vec!["string", "[]main::Point"]))
        );
        assert_eq!(
            parts("Appender!string", D),
            (vec![], "Appender", Some(vec!["string"]))
        );
        assert_eq!(
            parts("Tuple!(int[], Nullable!int, 3)", D),
            (vec![], "Tuple", Some(vec!["int[]", "Nullable!int", "3"]))
        );

        // What the syntax does not describe is all base.
        for (name, syntax) in [
            ("&str", Angle),
            ("*const [i32]", Angle),
            ("unsigned int", Angle),
            ("<lambda(int)>", Angle),
            ("map[string]int", Go),
            ("[]int", Go),
            ("struct { a int }", Go),
            ("[]const u8", Zig),
            ("error{Oops}!u32", Zig),
            ("[dynamic]int", Odin),
            ("proc(x:int)", Odin),
            ("bit_set[0..=int(7)]", Odin),
            ("int[string]", D),
            ("immutable(char)*", D),
            ("Appender!", D),
            ("vector<int", Angle),
        ] {
            assert_eq!(parts(name, syntax), (vec![], name, None), "{name}");
        }
    }

    /// One type defined in several units is one type, but each unit's
    /// anonymous namespace is its own, so types in them are distinct
    /// however alike they are spelled, and so are instances over them.
    #[test]
    fn types_in_anonymous_namespaces_are_distinct_in_each_unit() {
        use crate::{ArgumentOrigin, TypeIdentity};

        let image = ModuleImageId::new(0);
        let reference = |index| TypeReference {
            image,
            id: TypeId::new(index),
        };

        let named = |index, scope: &str, base: &str, argument: Option<u32>| TypeInfo {
            reference: reference(index),
            name: base.into(),
            byte_size: Some(4),
            kind: TypeKind::Unspecified,
            identity: Some(Arc::new(TypeIdentity {
                language: SourceLanguage::Cpp,
                path: [Arc::from(scope)].into(),
                inline_namespaces: Arc::default(),
                base: base.into(),
                arguments: argument
                    .map(|argument| TypeArgument::Type(reference(argument)))
                    .into_iter()
                    .collect(),
                origin: ArgumentOrigin::Dwarf,
                pack: None,
                go: None,
            })),
        };

        let types = [
            named(0, "(anonymous namespace)", "Entry", None),
            named(1, "(anonymous namespace)", "Entry", None),
            named(2, "app", "Entry", None),
            named(3, "app", "Entry", None),
            named(4, "std", "vector", Some(0)),
            named(5, "std", "vector", Some(1)),
        ];

        let index = TypeIndex::build(Some(image), types.len(), |index| types.get(index));
        assert!(!index.same_type(reference(0), reference(1)));
        assert!(index.same_type(reference(2), reference(3)));
        assert!(!index.same_type(reference(4), reference(5)));
    }

    /// GCC spells a qualified argument `int const` and clang `const int`;
    /// either names the qualified type, whatever its own name.
    #[test]
    fn qualified_arguments_match_in_either_order() {
        struct Lookup<'a>(&'a [TypeInfo]);

        impl TypeLookup for Lookup<'_> {
            fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
                self.0.get(reference.id.index())
            }
        }

        use crate::{ArgumentOrigin, TypeIdentity, TypeModifier};

        let image = ModuleImageId::new(0);
        let reference = |index| TypeReference {
            image,
            id: TypeId::new(index),
        };

        let identity = |base: &str, path: &[&str], arguments: Vec<TypeArgument>| {
            Some(Arc::new(TypeIdentity {
                language: SourceLanguage::Cpp,
                path: path.iter().map(|segment| Arc::from(*segment)).collect(),
                inline_namespaces: Arc::default(),
                base: base.into(),
                arguments: arguments.into(),
                origin: ArgumentOrigin::Dwarf,
                pack: None,
                go: None,
            }))
        };

        let types = [
            TypeInfo {
                reference: reference(0),
                name: "int".into(),
                byte_size: Some(4),
                kind: TypeKind::Unspecified,
                identity: identity("int", &[], Vec::new()),
            },
            TypeInfo {
                reference: reference(1),
                name: "const int".into(),
                byte_size: Some(4),
                kind: TypeKind::Modified {
                    modifier: TypeModifier::Const,
                    target: reference(0),
                },
                identity: None,
            },
            TypeInfo {
                reference: reference(2),
                name: "pair<int const, int>".into(),
                byte_size: Some(8),
                kind: TypeKind::Unspecified,
                identity: identity(
                    "pair",
                    &["std"],
                    vec![
                        TypeArgument::Type(reference(1)),
                        TypeArgument::Type(reference(0)),
                    ],
                ),
            },
        ];

        let index = TypeIndex::build(Some(image), types.len(), |index| types.get(index));
        let lookup = Lookup(&types);

        for name in [
            "std::pair<int const, int>",
            "std::pair<const int, int>",
            "pair<const int>",
        ] {
            assert_eq!(index.named(name, false, &lookup), [reference(2)], "{name}");
        }

        assert!(
            index
                .named("std::pair<int, int>", false, &lookup)
                .is_empty()
        );
        assert!(
            index
                .named("std::pair<volatile int, int>", false, &lookup)
                .is_empty()
        );
    }

    #[test]
    fn integer_arguments_parse_with_their_suffixes_and_compare_by_value() {
        assert_eq!(parse_integer("4UL"), Some(IntegerValue::Unsigned(4)));
        assert_eq!(parse_integer("-3"), Some(IntegerValue::Signed(-3)));
        assert_eq!(parse_integer("0x10usize"), Some(IntegerValue::Unsigned(16)));
        assert_eq!(parse_integer("(char)97"), None);
        assert_eq!(parse_integer("3x"), None);

        // A truth value argument, as C++ names spell it, is its number, as
        // its template parameter's DWARF holds it.
        assert_eq!(parse_integer("false"), Some(IntegerValue::Unsigned(0)));
        assert_eq!(parse_integer("true"), Some(IntegerValue::Unsigned(1)));
        assert!(same_integer(
            IntegerValue::Signed(3),
            IntegerValue::Unsigned(3)
        ));
        assert!(!same_integer(
            IntegerValue::Signed(-1),
            IntegerValue::Unsigned(u128::MAX)
        ));
    }
}
