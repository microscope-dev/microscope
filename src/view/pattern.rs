//! Matching a view's pattern against a type's identity.
//!
//! A pattern names what a type is, never how a producer spelled it: its
//! language, its path from the root with inline namespaces optional and
//! `**` standing for any run of segments, its base name, and its leading
//! arguments by position.

use crate::eval::types::TypeSource;
use crate::type_identity::same_integer;
use crate::{GoKind, IntegerValue, SourceLanguage, TypeArgument, TypeIdentity, TypeReference};

use super::syntax::{ArgumentPattern, Language, Pattern, Segment};

/// How deeply argument patterns may nest.
const MAX_DEPTH: usize = 16;

/// An argument a pattern captured by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Captured {
    Type(TypeReference),
    Value(IntegerValue),
}

/// The arguments a pattern captured, by name.
pub type Captures = Vec<(String, Captured)>;

/// Whether a view of `language` applies to a type of `actual`. A view of
/// any language presents values of compiled languages' types, whose
/// members it names, never Python objects.
#[must_use]
pub const fn language_matches(language: Language, actual: SourceLanguage) -> bool {
    matches!(
        (language, actual),
        (Language::Any, actual) if !matches!(actual, SourceLanguage::Python)
    ) || matches!(
        (language, actual),
        (Language::Python, SourceLanguage::Python)
            | (Language::C, SourceLanguage::C)
            | (Language::Cpp, SourceLanguage::Cpp)
            | (Language::Rust, SourceLanguage::Rust)
            | (Language::Go, SourceLanguage::Go)
            | (Language::Zig, SourceLanguage::Zig)
            | (Language::Odin, SourceLanguage::Odin)
            | (Language::D, SourceLanguage::D)
            | (Language::Nim, SourceLanguage::Nim)
            | (Language::Ada, SourceLanguage::Ada)
    )
}

/// The arguments `pattern` captures from a type of `identity`, or `None`
/// when the pattern does not name the type.
pub fn matches(
    pattern: &Pattern,
    identity: &TypeIdentity,
    types: &dyn TypeSource,
) -> Option<Captures> {
    matches_with(pattern, identity, types, Captures::new())
}

/// As [`matches`], with names already captured, which the type's arguments
/// must then equal.
pub fn matches_with(
    pattern: &Pattern,
    identity: &TypeIdentity,
    types: &dyn TypeSource,
    mut captures: Captures,
) -> Option<Captures> {
    matches_into(pattern, identity, types, &mut captures, 0).then_some(captures)
}

/// The word a Go pattern names a kind of type with, whatever its name:
/// `go map<K, V>` is every map.
#[must_use]
pub fn go_kind_word(identity: &TypeIdentity) -> Option<&'static str> {
    if identity.language != SourceLanguage::Go {
        return None;
    }

    match identity.go?.kind {
        GoKind::Map => Some("map"),
        GoKind::Chan => Some("chan"),
        GoKind::Interface => Some("interface"),
        _ => None,
    }
}

fn matches_into(
    pattern: &Pattern,
    identity: &TypeIdentity,
    types: &dyn TypeSource,
    captures: &mut Captures,
    depth: usize,
) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }

    let by_kind = pattern.path.is_empty() && go_kind_word(identity) == Some(&pattern.base);
    if !by_kind && identity.base.as_ref() != pattern.base {
        return false;
    }

    if by_kind {
        return arguments_match(pattern, identity, types, captures, depth);
    }

    // A pattern may spell the inline namespaces a path omits, anywhere.
    let path = pattern
        .path
        .iter()
        .filter(|segment| {
            !matches!(segment, Segment::Name(name)
                if identity.inline_namespaces.iter().any(|inline| inline.as_ref() == name))
        })
        .collect::<Vec<_>>();
    let actual = identity
        .path
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<&str>>();
    path_matches(&path, &actual) && arguments_match(pattern, identity, types, captures, depth)
}

/// Whether a pattern's leading arguments match the type's.
fn arguments_match(
    pattern: &Pattern,
    identity: &TypeIdentity,
    types: &dyn TypeSource,
    captures: &mut Captures,
    depth: usize,
) -> bool {
    let Some(arguments) = &pattern.arguments else {
        return true;
    };

    // Trailing arguments may be left out, as C++ fills them with defaults,
    // but a pattern that reaches a parameter pack spells all of it:
    // `std::tuple<A, B>` names only pairs.
    let whole = identity.pack.is_some_and(|start| arguments.len() >= start);
    (arguments.len() == identity.arguments.len() || !whole)
        && arguments.len() <= identity.arguments.len()
        && arguments
            .iter()
            .zip(identity.arguments.iter())
            .all(|(pattern, argument)| {
                argument_matches(pattern, argument, identity.language, types, captures, depth)
            })
}

/// Whether segments match a path from its root, `**` matching any run.
fn path_matches(pattern: &[&Segment], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((Segment::AnyRun, rest)) => {
            (0..=path.len()).any(|skipped| path_matches(rest, &path[skipped..]))
        }
        Some((Segment::Name(name), rest)) => path
            .split_first()
            .is_some_and(|(first, remaining)| first == name && path_matches(rest, remaining)),
    }
}

fn argument_matches(
    pattern: &ArgumentPattern,
    argument: &TypeArgument,
    language: SourceLanguage,
    types: &dyn TypeSource,
    captures: &mut Captures,
    depth: usize,
) -> bool {
    match (pattern, argument) {
        (ArgumentPattern::Wildcard, _) => true,
        (ArgumentPattern::Value(expected), TypeArgument::Value(value)) => {
            same_integer(*expected, *value)
        }
        (ArgumentPattern::Capture(name), TypeArgument::Type(reference)) => {
            capture(captures, name, Captured::Type(*reference), types)
        }
        (ArgumentPattern::Capture(name), TypeArgument::Value(value)) => {
            capture(captures, name, Captured::Value(*value), types)
        }
        (ArgumentPattern::Type(pattern), TypeArgument::Type(reference)) => {
            let Some(info) = types.type_info(*reference) else {
                return false;
            };

            info.identity.as_deref().is_some_and(|identity| {
                identity.language == language
                    && matches_into(pattern, identity, types, captures, depth + 1)
            })
        }

        // An argument known only by how the type's name spells it, such
        // as rustc's `str`, matches a pattern that spells it so.
        (ArgumentPattern::Type(pattern), TypeArgument::Unknown(spelled)) => {
            pattern.path.is_empty()
                && pattern.arguments.is_none()
                && pattern.base == spelled.as_ref()
        }

        // Any other argument the debugger could not resolve matches only
        // `_`.
        _ => false,
    }
}

/// Records a capture; a name captured twice must capture the same thing.
fn capture(captures: &mut Captures, name: &str, found: Captured, types: &dyn TypeSource) -> bool {
    match captures.iter().find(|(existing, _)| existing == name) {
        None => {
            captures.push((name.to_owned(), found));
            true
        }
        Some((_, Captured::Value(previous))) => {
            matches!(found, Captured::Value(value) if same_integer(*previous, value))
        }
        Some((_, Captured::Type(previous))) => match found {
            Captured::Type(reference) => {
                types.same_type(*previous, reference)
                    || types
                        .type_info(*previous)
                        .zip(types.type_info(reference))
                        .is_some_and(|(left, right)| {
                            left.identity.is_some() && left.identity == right.identity
                        })
            }
            Captured::Value(_) => false,
        },
    }
}
