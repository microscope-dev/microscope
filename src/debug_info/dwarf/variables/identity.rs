//! Type identities from DWARF: each type's language, the scopes that
//! enclose it, its template parameters, and Go's type attributes.
//!
//! The structure comes from DWARF wherever DWARF has it. Names are parsed
//! only where it does not: GCC omits some templates' parameters, rustc omits
//! const generic parameters, and Go and Zig emit none. A parsed argument
//! names a type only when exactly one type matches it.

use std::sync::Arc;

use foldhash::{HashMap, HashMapExt, HashSet};
use rayon::prelude::*;

use crate::debug_info::dwarf::{DieKey, Reader, die_reference_with_signatures, string_attribute};
use crate::type_identity::{
    ANONYMOUS_NAMESPACE, NameIndex as _, NameSyntax, TypeIndex, TypeLookup, TypeName, parse_integer,
};

use crate::{
    ArgumentOrigin, ArrayBound, ArrayExtent, GoKind, GoTypeAttributes, ModuleImageId,
    RuntimeDimension, SourceLanguage, TypeArgument, TypeId, TypeIdentity, TypeInfo, TypeKind,
    TypeReference,
};

use super::codec::enumeration_constant;
use super::die::strict_flag;
use super::types::{TypeArenaBuilder, TypeEntry};
use super::{MAX_RECORD_CHILDREN, MAX_TYPE_RESOLUTION_DEPTH};

const DW_AT_GO_KIND: gimli::DwAt = gimli::DwAt(0x2900);
const DW_AT_GO_KEY: gimli::DwAt = gimli::DwAt(0x2901);
const DW_AT_GO_ELEM: gimli::DwAt = gimli::DwAt(0x2902);
const DW_AT_GO_EMBEDDED_FIELD: gimli::DwAt = gimli::DwAt(0x2903);
const DW_AT_GO_RUNTIME_TYPE: gimli::DwAt = gimli::DwAt(0x2904);

/// The inline namespaces of C++ standard libraries, for units older than
/// DWARF 5's `DW_AT_export_symbols`. libstdc++'s `__cxx1998` is not one: it
/// holds the containers that debug mode's inline `__debug` replaces.
const INLINE_NAMESPACES: [&str; 6] = ["__1", "__Cr", "__ndk1", "__cxx11", "__8", "__debug"];

/// The scopes enclosing a type DIE, outermost first.
#[derive(Clone, Default)]
pub(super) struct ScopePath {
    /// The named scopes.
    pub(super) path: Arc<[Arc<str>]>,

    /// The inline namespaces among them, which names may spell or omit.
    pub(super) inline: Arc<[Arc<str>]>,
}

impl ScopePath {
    pub(super) fn is_empty(&self) -> bool {
        self.path.is_empty() && self.inline.is_empty()
    }
}

/// What a type's identity is built from, recorded as the type is built.
#[derive(Clone)]
pub(super) struct IdentityParts {
    /// The DIE the type was built from.
    pub(super) die: DieKey,

    /// Arguments from template parameter DIEs, by position.
    pub(super) template: Vec<TypeArgument>,

    /// Where a template parameter pack's arguments begin.
    pub(super) pack: Option<usize>,
}

#[derive(Clone)]
pub(super) struct GoParts {
    attributes: GoTypeAttributes,
    key: Option<TypeReference>,
    element: Option<TypeReference>,
}

impl GoParts {
    /// Replaces each type these name with `map` of it.
    pub(super) fn map_references(&mut self, map: impl Fn(TypeReference) -> TypeReference) {
        for reference in [&mut self.key, &mut self.element].into_iter().flatten() {
            *reference = map(*reference);
        }
    }
}

/// How a DIE contributes to the scope path of the types nested in it.
#[derive(Clone)]
pub(super) enum ScopeSegment {
    /// A named type, whose nested types it scopes.
    Named(Arc<str>),

    /// A namespace not marked inline here, which may still be inline: GCC
    /// copies namespaces into type units without their marks. `listed`
    /// when its unit predates the mark and its name is in
    /// [`INLINE_NAMESPACES`].
    Namespace { name: Arc<str>, listed: bool },

    /// A function, whose name may live on its declaration.
    Function(DieKey),

    /// An inline namespace, which names need not spell.
    Inline(Arc<str>),

    /// A scope that names nothing, such as an anonymous type.
    Transparent,
}

/// The offset of a Go type's runtime type descriptor. Go gives the types it
/// synthesizes for its runtime none.
pub(super) fn go_runtime_type(entry: &gimli::DebuggingInformationEntry<Reader<'_>>) -> Option<u64> {
    match entry.attr_value(DW_AT_GO_RUNTIME_TYPE)? {
        // Go's linker writes the offset from `runtime.types` in the
        // attribute's address form, not the descriptor's address.
        gimli::AttributeValue::Addr(offset) => Some(offset),
        value => value.udata_value(),
    }
    .filter(|offset| *offset != 0)
}

/// Whether Go embeds a struct member.
pub(super) fn go_embedded(entry: &gimli::DebuggingInformationEntry<Reader<'_>>) -> bool {
    entry
        .attr(DW_AT_GO_EMBEDDED_FIELD)
        .is_some_and(|attribute| match attribute.value() {
            gimli::AttributeValue::Flag(value) => value,
            _ => attribute.udata_value().is_some_and(|value| value != 0),
        })
}

/// The language a unit's producer or name proves where its
/// `DW_AT_language` does not: Zig's LLVM backend and Odin say their units
/// are C99, and Nim compiles each module to C in a file named for it, as
/// `@mvalues.nim.c`.
pub(in crate::debug_info) fn produced_language(
    producer: Option<&str>,
    name: Option<&str>,
) -> Option<SourceLanguage> {
    if name.is_some_and(|name| name.ends_with(".nim.c")) {
        return Some(SourceLanguage::Nim);
    }

    let producer = producer?;
    if producer.starts_with("zig ") {
        Some(SourceLanguage::Zig)
    } else if producer == "odin" || producer.starts_with("odin ") {
        Some(SourceLanguage::Odin)
    } else {
        None
    }
}

/// A unit's language: what its producer proves, else what its
/// `DW_AT_language` says.
pub(in crate::debug_info) const fn source_language(
    language: Option<gimli::DwLang>,
    produced: Option<SourceLanguage>,
) -> SourceLanguage {
    if let Some(produced) = produced {
        return produced;
    }

    let Some(language) = language else {
        return SourceLanguage::Unknown;
    };

    match language {
        gimli::DW_LANG_C89
        | gimli::DW_LANG_C
        | gimli::DW_LANG_C99
        | gimli::DW_LANG_C11
        | gimli::DW_LANG_C17 => SourceLanguage::C,
        gimli::DW_LANG_C_plus_plus
        | gimli::DW_LANG_C_plus_plus_03
        | gimli::DW_LANG_C_plus_plus_11
        | gimli::DW_LANG_C_plus_plus_14
        | gimli::DW_LANG_C_plus_plus_17
        | gimli::DW_LANG_C_plus_plus_20 => SourceLanguage::Cpp,
        gimli::DW_LANG_Rust => SourceLanguage::Rust,
        gimli::DW_LANG_Go => SourceLanguage::Go,
        gimli::DW_LANG_Zig => SourceLanguage::Zig,
        gimli::DW_LANG_Fortran77
        | gimli::DW_LANG_Fortran90
        | gimli::DW_LANG_Fortran95
        | gimli::DW_LANG_Fortran03
        | gimli::DW_LANG_Fortran08
        | gimli::DW_LANG_Fortran18 => SourceLanguage::Fortran,
        gimli::DW_LANG_D => SourceLanguage::D,
        gimli::DW_LANG_Ada83
        | gimli::DW_LANG_Ada95
        | gimli::DW_LANG_Ada2005
        | gimli::DW_LANG_Ada2012 => SourceLanguage::Ada,
        other => SourceLanguage::Other(other.0),
    }
}

/// The namespaces a D module scopes what it declares in, one for each
/// part of its name: `std.array` is `std`, then `array`.
pub(super) fn module_segments(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Vec<ScopeSegment> {
    string_attribute(dwarf, unit, entry, gimli::DW_AT_name)
        .ok()
        .flatten()
        .map(|name| {
            name.split('.')
                .map(|part| ScopeSegment::Namespace {
                    name: part.into(),
                    listed: false,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// How a DIE scopes the types nested in it, or `None` when it is not a
/// scope. `cpp` when the DIE's unit is C++.
pub(super) fn scope_segment(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    unit_index: usize,
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    cpp: bool,
) -> Option<ScopeSegment> {
    let name = || {
        string_attribute(dwarf, unit, entry, gimli::DW_AT_name)
            .ok()
            .flatten()
    };

    match entry.tag() {
        gimli::DW_TAG_namespace => {
            let exported = entry.attr_value(gimli::DW_AT_export_symbols);
            let inline = strict_flag(entry, gimli::DW_AT_export_symbols).unwrap_or(false);
            Some(match name() {
                Some(name) if inline => ScopeSegment::Inline(name),
                None if inline => ScopeSegment::Transparent,
                Some(name) => ScopeSegment::Namespace {
                    // Producers older than the mark still use these names
                    // for inline namespaces.
                    listed: cpp
                        && unit.encoding().version < 5
                        && exported.is_none()
                        && INLINE_NAMESPACES.contains(&name.as_ref()),
                    name,
                },
                None => ScopeSegment::Namespace {
                    name: Arc::from(ANONYMOUS_NAMESPACE),
                    listed: false,
                },
            })
        }

        gimli::DW_TAG_structure_type
        | gimli::DW_TAG_class_type
        | gimli::DW_TAG_union_type
        | gimli::DW_TAG_enumeration_type => {
            Some(name().map_or(ScopeSegment::Transparent, ScopeSegment::Named))
        }
        gimli::DW_TAG_subprogram => Some(ScopeSegment::Function(DieKey {
            unit: unit_index,
            offset: entry.offset().0,
        })),
        _ => None,
    }
}

/// The full path of an inline namespace, its own name last, from the
/// segments enclosing it.
pub(super) fn inline_namespace_path(
    segments: &[(isize, ScopeSegment)],
    name: &Arc<str>,
) -> Vec<Arc<str>> {
    segments
        .iter()
        .filter_map(|(_, segment)| match segment {
            ScopeSegment::Named(name)
            | ScopeSegment::Namespace { name, .. }
            | ScopeSegment::Inline(name) => Some(Arc::clone(name)),
            ScopeSegment::Function(_) | ScopeSegment::Transparent => None,
        })
        .chain(std::iter::once(Arc::clone(name)))
        .collect()
}

impl<'data> TypeArenaBuilder<'_, 'data> {
    /// A function's name, from its own DIE or the declaration it completes.
    fn function_name(&self, key: DieKey) -> Option<Arc<str>> {
        let mut current = key;
        for _ in 0..4 {
            let unit = self.units.get(current.unit)?;
            let entry = unit.entry(gimli::UnitOffset(current.offset)).ok()?;
            if entry.attr_value(gimli::DW_AT_name).is_some() {
                return string_attribute(self.dwarf, unit, &entry, gimli::DW_AT_name).ok()?;
            }

            let reference = entry
                .attr_value(gimli::DW_AT_specification)
                .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin));
            current = die_reference_with_signatures(
                reference,
                current.unit,
                self.units,
                self.type_signatures,
            )
            .ok()??;
        }

        None
    }

    /// Resolves a scope stack into a path. A namespace is inline when any
    /// unit marks the namespace with the same full path inline, or when no
    /// unit marks any namespace and its own unit lists it.
    pub(super) fn scope_path(
        &self,
        segments: &[ScopeSegment],
        inline_namespaces: &HashSet<Vec<Arc<str>>>,
    ) -> ScopePath {
        let mut path = Vec::new();
        let mut inline = Vec::new();
        let mut full = Vec::new();

        for segment in segments {
            match segment {
                ScopeSegment::Named(name) => {
                    full.push(Arc::clone(name));
                    path.push(Arc::clone(name));
                }
                ScopeSegment::Namespace { name, listed } => {
                    full.push(Arc::clone(name));
                    if inline_namespaces.contains(&full) || *listed && inline_namespaces.is_empty()
                    {
                        inline.push(Arc::clone(name));
                    } else {
                        path.push(Arc::clone(name));
                    }
                }
                ScopeSegment::Function(key) => {
                    let name = self.function_name(*key);
                    full.extend(name.clone());
                    path.extend(name);
                }
                ScopeSegment::Inline(name) => {
                    full.push(Arc::clone(name));
                    inline.push(Arc::clone(name));
                }
                ScopeSegment::Transparent => {}
            }
        }

        ScopePath {
            path: path.into(),
            inline: inline.into(),
        }
    }

    pub(super) fn language(&self, unit_index: usize) -> SourceLanguage {
        source_language(
            self.context
                .unit_languages
                .get(unit_index)
                .copied()
                .flatten(),
            self.produced_language(unit_index),
        )
    }

    /// Records what a named type's identity is built from.
    pub(super) fn record_identity_parts(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        die: DieKey,
        id: TypeId,
    ) {
        let (template, pack) = match entry.tag() {
            gimli::DW_TAG_structure_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_template_alias => self.template_arguments(entry, die.unit),
            _ => (Vec::new(), None),
        };

        if self.language(die.unit) == SourceLanguage::Go
            && let Some(go) = self.go_parts(entry, die.unit)
        {
            self.go_identity_parts.insert(id, go);
        }

        if self.identity_parts.len() <= id.index() {
            self.identity_parts.resize_with(id.index() + 1, || None);
        }

        self.identity_parts[id.index()] = Some(IdentityParts {
            die,
            template,
            pack,
        });
    }

    /// The arguments a type's template parameter DIEs describe, with packs
    /// flattened, and where its pack begins. A parameter whose value or type
    /// cannot be read is unknown.
    fn template_arguments(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> (Vec<TypeArgument>, Option<usize>) {
        let mut arguments = Vec::new();
        let mut pack = None;

        // Every named aggregate comes here, so its children, mostly its
        // members, are read in place rather than cloned to find the few
        // that are parameters.
        let Ok(mut children) = self.children(unit_index, entry.offset()) else {
            return (arguments, pack);
        };

        // LDC describes an associative array as a record holding typedefs
        // of its key and value types.
        let d = self.language(unit_index) == SourceLanguage::D;
        let mut associative = [None, None];
        let mut read = 0;

        while let Ok(Some(child)) = children.next_child() {
            if read == MAX_RECORD_CHILDREN {
                break;
            }

            read += 1;
            if d && child.tag() == gimli::DW_TAG_typedef {
                let name = self.units.get(unit_index).and_then(|unit| {
                    string_attribute(self.dwarf, unit, child, gimli::DW_AT_name)
                        .ok()
                        .flatten()
                });
                let slot = match name.as_deref() {
                    Some(D_KEY) => 0,
                    Some(D_VALUE) => 1,
                    _ => continue,
                };

                associative[slot] = self.target(child, unit_index).ok().flatten();
            } else if child.tag() == gimli::DW_TAG_GNU_template_parameter_pack {
                pack = pack.or(Some(arguments.len()));
                let Ok(mut parameters) = self.children(unit_index, child.offset()) else {
                    continue;
                };

                let mut read = 0;

                while let Ok(Some(parameter)) = parameters.next_child() {
                    if read == MAX_RECORD_CHILDREN {
                        break;
                    }

                    read += 1;
                    if let Some(argument) = self.parameter_argument(parameter, unit_index) {
                        arguments.push(argument);
                    }
                }
            } else if let Some(argument) = self.parameter_argument(child, unit_index) {
                arguments.push(argument);
            }
        }

        if let ([Some(key), Some(value)], true) = (associative, arguments.is_empty()) {
            arguments = vec![TypeArgument::Type(key), TypeArgument::Type(value)];
        }

        (arguments, pack)
    }

    /// The generic type arguments of the function whose DIE is `key`, each
    /// with its parameter's name: the function's own template type
    /// parameters, or those of the declaration it completes, where rustc
    /// puts a method's.
    pub(super) fn function_generics(&mut self, key: DieKey) -> Vec<(Arc<str>, TypeId)> {
        let mut current = key;
        for _ in 0..4 {
            let Some(unit) = self.units.get(current.unit) else {
                break;
            };

            let Ok(entry) = unit.entry(gimli::UnitOffset(current.offset)) else {
                break;
            };

            let mut generics = Vec::new();
            let Ok(mut children) = self.children(current.unit, entry.offset()) else {
                break;
            };

            let mut read = 0;

            while let Ok(Some(child)) = children.next_child() {
                if read == MAX_RECORD_CHILDREN {
                    break;
                }

                read += 1;
                if child.tag() != gimli::DW_TAG_template_type_parameter {
                    continue;
                }

                let name = string_attribute(self.dwarf, unit, child, gimli::DW_AT_name)
                    .ok()
                    .flatten();
                if let (Some(name), Ok(Some(target))) = (name, self.target(child, current.unit)) {
                    generics.push((name, target.id));
                }
            }

            if !generics.is_empty() {
                return generics;
            }

            let reference = entry
                .attr_value(gimli::DW_AT_specification)
                .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin));

            match die_reference_with_signatures(
                reference,
                current.unit,
                self.units,
                self.type_signatures,
            ) {
                Ok(Some(next)) => current = next,
                _ => break,
            }
        }

        Vec::new()
    }

    /// The argument one template parameter DIE describes.
    fn parameter_argument(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<TypeArgument> {
        Some(match entry.tag() {
            // An unsized array is no type of its own: rustc describes both
            // `str` and `[u8]` as one, so the name's spelling says which.
            gimli::DW_TAG_template_type_parameter
                if self.unsized_array_argument(entry, unit_index) =>
            {
                self.unknown_argument(entry, unit_index)
            }
            gimli::DW_TAG_template_type_parameter => match self.target(entry, unit_index) {
                Ok(Some(target)) => TypeArgument::Type(target),
                // A type parameter without a type is `void`.
                Ok(None) => TypeArgument::Type(self.void_type()),
                Err(_) => self.unknown_argument(entry, unit_index),
            },
            gimli::DW_TAG_template_value_parameter => self.template_value(entry, unit_index),
            gimli::DW_TAG_GNU_template_template_param => {
                let name = self.units.get(unit_index).and_then(|unit| {
                    string_attribute(self.dwarf, unit, entry, gimli::DW_AT_GNU_template_name)
                        .ok()
                        .flatten()
                });
                name.map_or_else(
                    || self.unknown_argument(entry, unit_index),
                    TypeArgument::Unknown,
                )
            }
            _ => return None,
        })
    }

    /// Whether a type parameter DIE's type is an array of unknown length.
    fn unsized_array_argument(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> bool {
        let Ok(Some(target)) = die_reference_with_signatures(
            entry.attr_value(gimli::DW_AT_type),
            unit_index,
            self.units,
            self.type_signatures,
        ) else {
            return false;
        };

        self.units
            .get(target.unit)
            .and_then(|unit| unit.entry(gimli::UnitOffset(target.offset)).ok())
            .is_some_and(|array| array.tag() == gimli::DW_TAG_array_type)
            && self.array_is_unsized(target)
    }

    /// A value parameter's integer, decoded by its type.
    fn template_value(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> TypeArgument {
        let value = entry
            .attr_value(gimli::DW_AT_const_value)
            .and_then(|value| {
                let target = self.target(entry, unit_index).ok()??;
                let base = self.resolved_integer_base(target.id).ok()?;
                enumeration_constant(value, &base, self.byte_order).ok()
            });

        value.map_or_else(
            || self.unknown_argument(entry, unit_index),
            TypeArgument::Value,
        )
    }

    /// An argument known only by its parameter's name.
    fn unknown_argument(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> TypeArgument {
        let name = self.units.get(unit_index).and_then(|unit| {
            string_attribute(self.dwarf, unit, entry, gimli::DW_AT_name)
                .ok()
                .flatten()
        });

        TypeArgument::Unknown(name.unwrap_or_else(|| Arc::from("?")))
    }

    /// Go's kind, runtime type, key, and element for a type DIE.
    fn go_parts(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<GoParts> {
        let kind = Self::go_kind(entry)?;
        let mut reference = |attribute| {
            let key = die_reference_with_signatures(
                entry.attr_value(attribute),
                unit_index,
                self.units,
                self.type_signatures,
            )
            .ok()??;
            Some(TypeReference {
                image: self.image,
                id: self.resolve(key),
            })
        };

        let (key, element) = match kind {
            GoKind::Map => (reference(DW_AT_GO_KEY), reference(DW_AT_GO_ELEM)),
            GoKind::Chan | GoKind::Slice | GoKind::Array | GoKind::Pointer => {
                (None, reference(DW_AT_GO_ELEM))
            }
            _ => (None, None),
        };

        Some(GoParts {
            attributes: GoTypeAttributes {
                kind,
                runtime_type: go_runtime_type(entry),
            },
            key,
            element,
        })
    }

    /// The Go kind a type DIE records, if any.
    pub(super) fn go_kind(
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
    ) -> Option<GoKind> {
        let kind = match entry.attr_value(DW_AT_GO_KIND)? {
            gimli::AttributeValue::Data1(kind) => kind,
            value => u8::try_from(value.udata_value()?).ok()?,
        };

        Some(GoKind::from_abi(kind))
    }

    /// Gives every named type its identity, once names are final.
    pub(super) fn assign_identities(&mut self) {
        // Each identity reads only its own type and the scopes, so they
        // are built in parallel and stored in order.
        // An indexed collect splits alike however the work is stolen.
        // Most identities share their parts with what is already built, so
        // the parts are shared rather than copied: every type in one scope
        // has its path, most names are their own base, and most types take
        // no arguments. Each identity is boxed where it is built: a vector
        // of whole identities beside their boxes held two copies of every
        // one at the load's peak.
        let no_arguments = Arc::<[TypeArgument]>::from([]);
        let built = (0..self.entries.len())
            .into_par_iter()
            .map(|index| {
                let id = TypeId::new(super::row(index));
                if !self.explicit_names.contains(id.index()) {
                    return None;
                }

                let Some(TypeEntry::Resolved(info)) = self.entries.get(index) else {
                    return None;
                };

                let parts = self.identity_parts.get(index)?.as_ref()?;
                let go = self.go_identity_parts.get(&id);
                let language = self.language(parts.die.unit);

                // Nim's C names each seq type for a hash of its element
                // type; it is that type's `seq`.
                if language == SourceLanguage::Nim
                    && let Some(element) = self.nim_seq_element(info)
                {
                    let identity = TypeIdentity {
                        language,
                        path: Arc::from([]),
                        inline_namespaces: Arc::from([]),
                        base: Arc::from("seq"),
                        arguments: Arc::from([TypeArgument::Type(element)]),
                        pack: None,
                        origin: ArgumentOrigin::Dwarf,
                        go: None,
                    };

                    return Some((index, language, Arc::new(identity), Vec::new()));
                }

                // D's associative arrays are each `AssociativeArray` of
                // their key and value types, as druntime once named them.
                if language == SourceLanguage::D && d_associative_array(info, parts) {
                    let identity = TypeIdentity {
                        language,
                        path: Arc::from([]),
                        inline_namespaces: Arc::from([]),
                        base: Arc::from("AssociativeArray"),
                        arguments: Arc::from(parts.template.as_slice()),
                        pack: None,
                        origin: ArgumentOrigin::Dwarf,
                        go: None,
                    };

                    return Some((index, language, Arc::new(identity), Vec::new()));
                }

                let parsed = TypeName::parse(&info.name, NameSyntax::of(language));
                let scopes = self.type_path(parts.die);

                // Only Go, Zig, and Odin names spell their packages and
                // modules.
                let path = if parsed.path.is_empty() {
                    scopes.path
                } else {
                    scopes
                        .path
                        .iter()
                        .cloned()
                        .chain(parsed.path.iter().map(|segment| Arc::<str>::from(*segment)))
                        .collect()
                };

                let base = if parsed.base == &*info.name {
                    Arc::clone(&info.name)
                } else {
                    Arc::from(parsed.base)
                };

                let (arguments, origin, pending) =
                    merge_arguments(parts, go, parsed.arguments.as_deref());
                let pack = parts.pack.filter(|start| *start <= arguments.len());
                let arguments = if arguments.is_empty() {
                    Arc::clone(&no_arguments)
                } else {
                    arguments.into()
                };

                let identity = TypeIdentity {
                    language,
                    path,
                    inline_namespaces: scopes.inline,
                    base,
                    arguments,
                    pack,
                    origin,
                    go: go.map(|go| go.attributes),
                };

                Some((index, language, Arc::new(identity), pending))
            })
            .collect::<Vec<_>>();

        for (index, language, identity, positions) in built.into_iter().flatten() {
            if !positions.is_empty() {
                self.pending_arguments.push(PendingArguments {
                    entry: index,
                    language,
                    positions,
                });
            }

            if let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(index) {
                info.identity = Some(identity);
            }
        }
    }

    /// The element type of a Nim seq, as Nim's C lays one out: a
    /// `tySequence__` record of its `len` and `p`, which points to a
    /// record of its `cap` and its elements, `data`, a flexible array.
    fn nim_seq_element(&self, info: &TypeInfo) -> Option<TypeReference> {
        let members = |info: &TypeInfo, names: [&str; 2]| match &info.kind {
            TypeKind::Record {
                members,
                incomplete: false,
                ..
            } if members.len() == 2
                && members
                    .iter()
                    .zip(names)
                    .all(|(member, name)| member.name.as_deref() == Some(name)) =>
            {
                Some(Arc::clone(members))
            }
            _ => None,
        };

        let resolved = |reference: TypeReference| {
            let mut id = reference.id;
            for _ in 0..MAX_TYPE_RESOLUTION_DEPTH {
                let Some(TypeEntry::Resolved(info)) = self.entries.get(id.index()) else {
                    return None;
                };

                match &info.kind {
                    TypeKind::Named {
                        target: Some(target),
                        ..
                    }
                    | TypeKind::Modified { target, .. } => id = target.id,
                    _ => return Some(info),
                }
            }

            None
        };

        if !info.name.starts_with("tySequence__") {
            return None;
        }

        let seq = members(info, ["len", "p"])?;
        let TypeKind::Pointer {
            target: Some(content),
            ..
        } = resolved(seq[1].type_ref)?.kind
        else {
            return None;
        };

        let content = members(resolved(content)?, ["cap", "data"])?;

        match &resolved(content[1].type_ref)?.kind {
            TypeKind::RuntimeArray {
                element,
                dimensions,
                ..
            } if matches!(
                dimensions.as_ref(),
                [RuntimeDimension {
                    lower_bound: ArrayBound::Constant(0),
                    extent: ArrayExtent::Unknown,
                    byte_stride: None,
                }]
            ) =>
            {
                Some(*element)
            }
            _ => None,
        }
    }

    /// The scopes enclosing the DIE a type was built from, or those of the
    /// declaration it completes.
    fn type_path(&self, die: DieKey) -> ScopePath {
        self.context
            .type_scopes
            .get(&die)
            .or_else(|| {
                self.context
                    .definition_declarations
                    .get(&die)
                    .and_then(|declaration| self.context.type_scopes.get(declaration))
            })
            .cloned()
            .unwrap_or_default()
    }
}

/// The names of the typedefs LDC nests in an associative array's record
/// for its key and value types.
const D_KEY: &str = "__key_t";
const D_VALUE: &str = "__val_t";

/// Whether a D type is an associative array as LDC describes one: `V[K]`,
/// a record of one pointer, `ptr`, to druntime's table, holding typedefs
/// of its key and value types.
fn d_associative_array(info: &TypeInfo, parts: &IdentityParts) -> bool {
    let TypeKind::Record {
        members,
        incomplete: false,
        ..
    } = &info.kind
    else {
        return false;
    };

    info.name.ends_with(']')
        && matches!(
            parts.template.as_slice(),
            [TypeArgument::Type(_), TypeArgument::Type(_)]
        )
        && matches!(members.as_ref(), [member] if member.name.as_deref() == Some("ptr"))
}

/// The positions of a type's identity whose arguments its name spells,
/// which resolve once every identity exists.
#[derive(Debug, Clone)]
pub(super) struct PendingArguments {
    pub(super) entry: usize,
    pub(super) language: SourceLanguage,
    pub(super) positions: Vec<usize>,
}

/// Resolves the arguments `pending` names, parsed from names, once every
/// identity exists: an argument resolves when the types it could name are
/// one, and names the first of them.
///
/// With `classes`, each type's class of types whose fields and references
/// are alike, a name is matched against the first of each class only,
/// since it names every type of a class or none; the types it names are
/// then every member of the classes matched. Those need not all be the
/// same type, as copies of a cyclic type are not.
pub(super) fn resolve_parsed_arguments(
    entries: &mut [TypeEntry],
    image: ModuleImageId,
    pending: &[PendingArguments],
    classes: Option<&[u32]>,
) {
    if pending.is_empty() {
        return;
    }

    let alike = Alike::new(classes);
    let lookup = EntryLookup(entries);
    let index_phase = crate::span!("types.identities.index");
    let index = TypeIndex::build_searching(
        Some(image),
        entries.len(),
        |index| alike.searchable(index),
        |index| match entries.get(index) {
            Some(TypeEntry::Resolved(info)) => Some(info),
            _ => None,
        },
    );
    drop(index_phase);
    let _resolve_phase = crate::span!("types.identities.resolve");

    // The first pointer type to each type, by the target's identity, for
    // arguments spelled as pointers.
    let mut pointers = HashMap::new();
    let spelled_pointer = pending.iter().any(|pending| {
        matches!(
            entries.get(pending.entry),
            Some(TypeEntry::Resolved(TypeInfo { identity: Some(identity), .. }))
                if pending.positions.iter().any(|position| matches!(
                    identity.arguments.get(*position),
                    Some(TypeArgument::Unknown(text)) if text.trim_end().ends_with('*')
                ))
        )
    });
    if spelled_pointer {
        for entry in entries.iter() {
            if let TypeEntry::Resolved(TypeInfo {
                reference,
                kind:
                    crate::TypeKind::Pointer {
                        target: Some(target),
                        ..
                    },
                ..
            }) = entry
                && let Some(key) = index.key(*target)
            {
                pointers.entry(Arc::clone(key)).or_insert(*reference);
            }
        }
    }

    // Many names spell the same argument, which resolves alike each time,
    // so each spelling resolves once. A spelling's answer depends on nothing
    // but the spelling, and finding it searches every type its base names,
    // thousands in a large program, so the spellings resolve in parallel.
    let (spellings, ordered) = spellings(entries, pending);
    crate::count!("type_arguments_spelled", ordered.len());

    // An indexed collect splits alike however the work is stolen.
    let answers = ordered
        .par_iter()
        .map(|(text, language)| {
            resolve_argument(text, *language, &index, &lookup, &pointers, &alike)
        })
        .collect::<Vec<_>>();
    index.free_in_parallel();
    let mut resolved = Vec::new();

    for pending in pending {
        let Some(TypeEntry::Resolved(info)) = entries.get(pending.entry) else {
            continue;
        };

        let Some(identity) = &info.identity else {
            continue;
        };

        let mut arguments = identity.arguments.to_vec();
        for position in &pending.positions {
            let TypeArgument::Unknown(text) = &arguments[*position] else {
                continue;
            };

            if let Some(found) = &answers[spellings[&(Arc::clone(text), pending.language)]] {
                arguments[*position] = found.clone();
            }
        }

        resolved.push((pending.entry, arguments));
    }

    for (entry, arguments) in resolved {
        if let Some(TypeEntry::Resolved(info)) = entries.get_mut(entry)
            && let Some(identity) = &mut info.identity
        {
            Arc::make_mut(identity).arguments = arguments.into();
        }
    }
}

/// A spelling of a type argument, in its language.
type Spelling = (Arc<str>, SourceLanguage);

/// The spellings of the arguments `pending` names, each once in the order
/// first named, and where each is in that order.
fn spellings(
    entries: &[TypeEntry],
    pending: &[PendingArguments],
) -> (HashMap<Spelling, usize>, Vec<Spelling>) {
    let mut spellings = HashMap::new();
    let mut ordered = Vec::new();

    for pending in pending {
        let Some(TypeEntry::Resolved(TypeInfo {
            identity: Some(identity),
            ..
        })) = entries.get(pending.entry)
        else {
            continue;
        };

        for position in &pending.positions {
            if let Some(TypeArgument::Unknown(text)) = identity.arguments.get(*position) {
                spellings
                    .entry((Arc::clone(text), pending.language))
                    .or_insert_with(|| {
                        ordered.push((Arc::clone(text), pending.language));
                        ordered.len() - 1
                    });
            }
        }
    }

    (spellings, ordered)
}

/// Each type's class of types alike to every name, with the members of
/// each class in identifier order; without classes, each type is its own.
struct Alike<'a> {
    classes: Option<&'a [u32]>,
    members: Vec<Vec<TypeId>>,
}

impl<'a> Alike<'a> {
    fn new(classes: Option<&'a [u32]>) -> Self {
        let mut members = Vec::<Vec<TypeId>>::new();
        for (index, class) in classes.unwrap_or_default().iter().enumerate() {
            let class = *class as usize;
            if class >= members.len() {
                members.resize_with(class + 1, Vec::new);
            }

            members[class].push(TypeId::new(super::row(index)));
        }

        Self { classes, members }
    }

    /// Whether names are matched against the type `index` numbers: the
    /// first of its class.
    fn searchable(&self, index: usize) -> bool {
        self.classes
            .is_none_or(|classes| self.members[classes[index] as usize][0].index() == index)
    }

    /// The types alike to `reference`, itself among them.
    fn members(&self, reference: TypeReference) -> Vec<TypeReference> {
        self.classes
            .and_then(|classes| classes.get(reference.id.index()))
            .map_or_else(
                || vec![reference],
                |class| {
                    self.members[*class as usize]
                        .iter()
                        .map(|id| TypeReference {
                            image: reference.image,
                            id: *id,
                        })
                        .collect()
                },
            )
    }
}

struct EntryLookup<'a>(&'a [TypeEntry]);

impl TypeLookup for EntryLookup<'_> {
    fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        match self.0.get(reference.id.index())? {
            TypeEntry::Resolved(info) => Some(info),
            _ => None,
        }
    }
}

/// Combines the arguments DWARF describes with those a name spells. Returns
/// the arguments, their origin, and the positions still to resolve.
fn merge_arguments(
    parts: &IdentityParts,
    go: Option<&GoParts>,
    named: Option<&[&str]>,
) -> (Vec<TypeArgument>, ArgumentOrigin, Vec<usize>) {
    if let Some(go) = go {
        let described = match (go.key, go.element) {
            (Some(key), Some(element)) => vec![key, element],
            (None, Some(element)) => vec![element],
            _ => Vec::new(),
        };

        if !described.is_empty() {
            return (
                described.into_iter().map(TypeArgument::Type).collect(),
                ArgumentOrigin::Dwarf,
                Vec::new(),
            );
        }
    }

    let unknown = |text: &str| TypeArgument::Unknown(Arc::from(text));
    let parsed = |texts: &[&str]| {
        let arguments = texts.iter().map(|text| unknown(text)).collect::<Vec<_>>();
        let pending = (0..arguments.len()).collect();
        (arguments, ArgumentOrigin::ParsedName, pending)
    };

    let template = &parts.template;

    match named {
        _ if template.is_empty() => match named {
            Some(texts) if !texts.is_empty() => parsed(texts),
            _ => (Vec::new(), ArgumentOrigin::None, Vec::new()),
        },
        // The name spells each parameter: an unreadable one takes its
        // spelling, which may still resolve.
        Some(texts) if texts.len() == template.len() => {
            let mut pending = Vec::new();
            let arguments = template
                .iter()
                .zip(texts)
                .enumerate()
                .map(|(position, (argument, text))| match argument {
                    TypeArgument::Unknown(_) => {
                        pending.push(position);
                        unknown(text)
                    }
                    known => known.clone(),
                })
                .collect();

            (arguments, ArgumentOrigin::Dwarf, pending)
        }

        // rustc omits const parameters: the name's integers fill the
        // positions the parameters skip, if that accounts for every one.
        Some(texts) if texts.len() > template.len() => {
            let mut described = template.iter();
            let merged = texts
                .iter()
                .map(|text| {
                    parse_integer(text)
                        .map(TypeArgument::Value)
                        .or_else(|| described.next().cloned())
                })
                .collect::<Option<Vec<_>>>();

            match merged {
                Some(arguments) if described.next().is_none() => {
                    (arguments, ArgumentOrigin::Dwarf, Vec::new())
                }

                // Positions the parameters cannot be placed in come from
                // the name alone, rather than out of place.
                _ => parsed(texts),
            }
        }
        _ => (template.clone(), ArgumentOrigin::Dwarf, Vec::new()),
    }
}

/// What a parsed argument spells: a value, or the one type it names in the
/// type's own language. Types several units define alike are one type;
/// several distinct types, or none, leave it unknown.
fn resolve_argument(
    text: &str,
    language: SourceLanguage,
    index: &TypeIndex,
    lookup: &EntryLookup<'_>,
    pointers: &HashMap<Arc<str>, TypeReference>,
    alike: &Alike<'_>,
) -> Option<TypeArgument> {
    if let Some(value) = parse_integer(text) {
        return Some(TypeArgument::Value(value));
    }

    // A pointer, as GCC spells `int*` in a pack it leaves out: the pointer
    // type whose target is the type its spelling resolves to.
    if let Some(target) = text.trim_end().strip_suffix('*') {
        let Some(TypeArgument::Type(target)) =
            resolve_argument(target.trim_end(), language, index, lookup, pointers, alike)
        else {
            return None;
        };

        return pointers
            .get(index.key(target)?)
            .copied()
            .map(TypeArgument::Type);
    }

    let mut named = index
        .named(text, true, lookup)
        .into_iter()
        .flat_map(|reference| alike.members(reference))
        .collect::<Vec<_>>();
    named.sort_unstable_by_key(|reference| reference.id);
    let candidates = named
        .into_iter()
        .filter_map(|reference| lookup.type_info(reference))
        .filter(|info| {
            info.identity
                .as_deref()
                .is_none_or(|identity| identity.language == language)
        })
        .collect::<Vec<_>>();
    let first = candidates.first()?;

    candidates
        .iter()
        .all(|other| index.same_type(first.reference, other.reference))
        .then_some(TypeArgument::Type(first.reference))
}
