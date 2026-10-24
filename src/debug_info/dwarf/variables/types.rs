//! Normalizing DWARF type DIEs into the platform-neutral type graph.

use crate::image::lines::Files;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, PoisonError};

use foldhash::{HashMap, HashMapExt, HashSet, HashSetExt};
use rayon::prelude::*;

use crate::debug_info::dwarf::{
    DieKey, DieMap, DieWalk, Reader, TypeSignatures, Units, die_reference_with_signatures,
    unit_dwarf,
};

use crate::image::type_facts::BoundPart;
use crate::model::{ArrayDimension, ArrayOrdering};
use crate::{
    Accessibility, ArrayBound, ArrayExtent, BaseClass, BaseClassVirtuality, BaseType,
    BaseTypeEncoding, ByteOrder, EnumerationOrigin, Enumerator, GoKind, IntegerValue,
    ModuleImageId, NamedTypeRelationship, RecordKind, RecordMember, RecordMemberLayout,
    ReferenceKind, RuntimeDimension, SliceWords, SourceLanguage, SourceLocation, TypeId, TypeInfo,
    TypeKind, TypeModifier, TypeReference, Variant, VariantDiscriminant, VariantSelection,
    VariantSelector, VariantStorageKind,
};

use super::codec::{complex_part, enumeration_constant};
use super::die::{
    ByteSize, DW_AT_ZIG_PARENT, DeclaredSource, Seen, UnsignedConstant, array_bound,
    base_type_encoding, byte_size_attribute, constant_member_offset, copy_name, declared_source,
    index_type_is_signed, origin_chain, strict_flag, string_with_origins, type_with_origins,
    unsigned_constant, zig_qualified_name,
};

use super::identity::{
    GoParts, IdentityParts, ScopePath, ScopeSegment, go_embedded, inline_namespace_path,
    module_segments, produced_language, scope_segment, source_language,
};

use super::layered::{Layered, Marks, Rows};
use super::location::copy_expression;
use super::variant::{
    VariantMetadataBudget, VariantMetadataError, copy_variant_selection,
    validate_variant_selections,
};

use super::{MAX_BUILT_TYPES, MAX_RECORD_CHILDREN, MAX_TYPE_RESOLUTION_DEPTH};
use crate::image::locations::ExpressionId;

pub(super) use crate::image::variables::TypeResolution;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TypeEntry {
    Building,
    Resolved(TypeInfo),
    Malformed(Arc<str>),
}

pub(super) struct TypeArenaBuilder<'a, 'data> {
    pub(super) dwarf: &'a gimli::Dwarf<Reader<'data>>,
    pub(super) units: &'a Units<'data>,
    pub(super) type_signatures: &'a TypeSignatures,
    pub(super) image: ModuleImageId,
    pub(super) by_die: Layered<DieMap<TypeId>>,

    /// What the prepass read of every unit, which building types only
    /// reads.
    pub(super) context: Arc<TypeContext>,
    pub(super) entries: Rows<TypeEntry>,
    pub(super) explicit_names: Marks,

    /// The arguments identities spell by name, which resolve once every
    /// identity exists.
    pub(super) pending_arguments: Vec<super::identity::PendingArguments>,
    pub(super) resolution_depth: usize,
    pub(super) byte_order: ByteOrder,
    pub(super) limit_type: Option<TypeId>,

    /// The shared `void` that qualifiers and typedefs without a target name.
    pub(super) void_type: Option<TypeId>,

    /// The character [`Self::character_type`] made, once it has.
    pub(super) character_type: Option<TypeId>,
    pub(super) dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, ExpressionId>,

    /// Where every location is pooled.
    pub(super) pool: &'a std::sync::Mutex<super::location::LocationsBuilder>,

    /// The DIEs [`Children`] read into.
    pub(super) die_buffers: &'a DieBuffers<'data>,
    pub(super) record_member_declarations: Vec<AggregateMemberDeclaration>,

    /// What the records the loader builds may still cost.
    pub(super) budget: crate::debug_info::dwarf::budget::Meter,

    /// What each named type's identity is built from, by its identifier.
    /// Nearly every type is named and identifiers are dense, so a vector
    /// holds them in a third of the hash map's memory: the map's buckets
    /// were mostly empty and every one was touched.
    pub(super) identity_parts: Rows<Option<IdentityParts>>,

    /// The Go parts of each Go type's identity, apart so that no other
    /// type's parts make room for them.
    pub(super) go_identity_parts: Layered<HashMap<TypeId, GoParts>>,

    /// The float type each complex type's parts have, by the part's name
    /// and size.
    pub(super) complex_parts: HashMap<(Arc<str>, u64), TypeId>,

    /// Go's generic type parameters: each typedef of a shape that names
    /// its type argument's entry in the function's dictionary.
    pub(super) go_dict_indices: Layered<HashMap<TypeId, u64>>,

    /// Whether each C++ class whose producer says how calls pass it is
    /// passed by value, in registers where it fits, rather than by
    /// reference to a copy.
    pub(super) passed_by_value: Layered<HashMap<TypeId, bool>>,

    /// The group whose units' types this arena builds, when it builds one
    /// group's apart from the others', and each unit's group.
    pub(super) home: Option<(usize, &'a [usize])>,

    /// Whether it was asked for a type of another group, which a walk of
    /// every unit in order may have built before this one.
    pub(super) foreign: bool,

    /// The DIEs it took for another's type, in order, when it builds one
    /// group's.
    pub(super) aliases: Vec<DieKey>,
}

/// What the type arena's prepass found in every unit.
#[derive(Clone)]
pub(super) struct TypeContext {
    pub(super) type_definitions: DieMap<DieKey>,
    pub(super) ambiguous_type_declarations: DieMap<()>,

    /// Where each unit's DIEs begin. A reference to any other offset
    /// points into the middle of a DIE, whose bytes could decode as
    /// convincing nonsense.
    pub(super) die_offsets: Vec<Bits>,
    pub(super) unit_languages: Vec<Option<gimli::DwLang>>,

    /// The language each unit's producer proves, where it does.
    pub(super) produced_languages: Vec<Option<SourceLanguage>>,

    /// The scopes enclosing each type DIE that has any.
    pub(super) type_scopes: DieMap<ScopePath>,

    /// The declaration each out-of-line type definition completes.
    pub(super) definition_declarations: DieMap<DieKey>,
}

/// A set of small numbers, one bit each: the offsets at which one unit's
/// DIEs begin, one bit per byte of the unit, or the types built with
/// explicit names.
///
/// A set of offsets cost a hash table entry, about 16 bytes, per DIE: some
/// 300 MB for the large benchmark program's 20 million, all live while its
/// types are built. Its DIEs average eight bytes, so a bit per byte, 20 MB,
/// is a sixteenth of that, and a lookup is one load instead of a hash and a
/// probe.
#[derive(Clone, Default)]
pub(super) struct Bits {
    words: Vec<u64>,
}

impl Bits {
    /// Room for the offsets of a unit `length` bytes long.
    fn with_length(length: usize) -> Self {
        Self {
            words: vec![0; length.div_ceil(64)],
        }
    }

    pub(super) fn insert(&mut self, value: usize) {
        let word = value / 64;

        // The set grows to fit: a unit's DIEs lie within its length, but a
        // header that lies about it must not lose one.
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }

        self.words[word] |= 1 << (value % 64);
    }

    pub(super) fn contains(&self, value: usize) -> bool {
        self.words
            .get(value / 64)
            .is_some_and(|word| word & (1 << (value % 64)) != 0)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
    }

    /// Adds every number of `other`.
    pub(super) fn union(&mut self, other: &Self) {
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }

        for (word, other) in self.words.iter_mut().zip(&other.words) {
            *word |= other;
        }
    }

    /// The numbers in the set, in order.
    pub(super) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(index, word)| {
            let mut word = *word;
            std::iter::from_fn(move || {
                (word != 0).then(|| {
                    let bit = word.trailing_zeros() as usize;
                    word &= word - 1;
                    index * 64 + bit
                })
            })
        })
    }
}

/// How many member declarations are read in parallel before their files
/// are interned: enough to keep every worker busy, few enough that their
/// paths take a few megabytes rather than one per member of the program.
const MEMBER_DECLARATION_CHUNK: usize = 1 << 15;

/// What the type arena's prepass reads of one unit.
struct UnitPrepass {
    offsets: Bits,
    language: Option<gimli::DwLang>,

    /// The language the unit's producer or name proves, where it does.
    produced: Option<SourceLanguage>,

    /// Each type DIE declared in a scope, and the scope's segments.
    scoped_types: Vec<(DieKey, Arc<[ScopeSegment]>)>,
    inline_namespaces: Vec<Vec<Arc<str>>>,

    /// Each out-of-line type definition and the declaration it completes.
    definitions: Vec<(DieKey, DieKey)>,
}

impl UnitPrepass {
    fn read(
        dwarf: &gimli::Dwarf<Reader<'_>>,
        units: &Units<'_>,
        type_signatures: &TypeSignatures,
        unit_index: usize,
    ) -> Self {
        let unit = &units[unit_index];
        let mut prepass = Self {
            offsets: Bits::with_length(unit.header.length_including_self()),
            language: None,
            produced: None,
            scoped_types: Vec::new(),
            inline_namespaces: Vec::new(),
            definitions: Vec::new(),
        };

        let mut cpp = false;
        let mut d = false;
        let mut first = true;
        let mut scopes = Vec::<(isize, ScopeSegment)>::new();

        // The path of the first `n` scopes at `n`, shared by the types
        // declared in them. Leaving a scope keeps its parents' paths, so
        // the types after a record share one path with those before it.
        let mut paths_at = Vec::<Option<Arc<[ScopeSegment]>>>::new();
        let Ok(mut walk) = DieWalk::new(unit) else {
            return prepass;
        };

        while let Ok(Some(die)) = walk.next() {
            prepass.offsets.insert(die.offset.0);
            let depth = die.depth;
            while scopes.last().is_some_and(|(scope, _)| *scope >= depth) {
                scopes.pop();
            }

            paths_at.truncate(scopes.len() + 1);
            let key = DieKey {
                unit: unit_index,
                offset: die.offset.0,
            };

            // Paths are resolved once every unit is read, since a
            // function's name may live in another unit.
            if is_type_die_tag(die.tag) && !scopes.is_empty() {
                paths_at.resize(scopes.len() + 1, None);
                let segments = paths_at[scopes.len()].get_or_insert_with(|| {
                    scopes.iter().map(|(_, segment)| segment.clone()).collect()
                });
                prepass.scoped_types.push((key, Arc::clone(segments)));
            }

            // Only the root, namespaces, and types have attributes
            // this reads; a function scopes the types in it by its
            // offset alone.
            if die.tag == gimli::DW_TAG_subprogram && !first {
                scopes.push((depth, ScopeSegment::Function(key)));
                continue;
            }

            // A D module scopes what it declares; Zig's names spell
            // their own modules.
            let scopes_or_names = first
                || die.tag == gimli::DW_TAG_namespace
                || (d && die.tag == gimli::DW_TAG_module)
                || is_type_die_tag(die.tag);
            if !scopes_or_names {
                continue;
            }

            let Ok(entry) = walk.decode() else {
                break;
            };

            if first {
                first = false;
                prepass.language = match entry.attr_value(gimli::DW_AT_language) {
                    Some(gimli::AttributeValue::Language(language)) => Some(language),
                    _ => units.inherited_language(unit_index),
                };

                prepass.produced = unit_produced_language(dwarf, unit, entry);
                cpp = source_language(prepass.language, prepass.produced) == SourceLanguage::Cpp;
                d = source_language(prepass.language, prepass.produced) == SourceLanguage::D;
            }

            if entry.tag() == gimli::DW_TAG_module {
                scopes.extend(
                    module_segments(dwarf, unit, entry)
                        .into_iter()
                        .map(|segment| (depth, segment)),
                );
            } else if let Some(segment) = scope_segment(dwarf, unit, unit_index, entry, cpp) {
                if let ScopeSegment::Inline(name) = &segment {
                    prepass
                        .inline_namespaces
                        .push(inline_namespace_path(&scopes, name));
                }

                scopes.push((depth, segment));
            }

            if !is_type_die_tag(entry.tag()) {
                continue;
            }

            let Ok(Some(declaration)) = die_reference_with_signatures(
                entry.attr_value(gimli::DW_AT_specification),
                unit_index,
                units,
                type_signatures,
            ) else {
                continue;
            };

            prepass.definitions.push((
                DieKey {
                    unit: unit_index,
                    offset: entry.offset().0,
                },
                declaration,
            ));
        }

        prepass
    }
}

/// The language a unit's producer or name proves, from its root DIE.
fn unit_produced_language(
    dwarf: &gimli::Dwarf<Reader<'_>>,
    unit: &gimli::Unit<Reader<'_>>,
    root: &gimli::DebuggingInformationEntry<Reader<'_>>,
) -> Option<SourceLanguage> {
    let text = |attribute| {
        root.attr_value(attribute)
            .and_then(|value| unit_dwarf(dwarf, unit).attr_string(unit, value).ok())
            .map(|text| text.to_string_lossy())
    };

    produced_language(
        text(gimli::DW_AT_producer).as_deref(),
        text(gimli::DW_AT_name).as_deref(),
    )
}

/// What the loader keeps of a finished type graph.
pub(super) struct BuiltTypes {
    pub(super) entries: Vec<TypeEntry>,
    pub(super) dynamic_record_layouts: HashMap<DynamicAggregateLayoutKey, ExpressionId>,
    pub(super) complex_parts: HashMap<(Arc<str>, u64), TypeId>,
    pub(super) go_dict_indices: HashMap<TypeId, u64>,
    pub(super) passed_by_value: HashMap<TypeId, bool>,
    pub(super) image: ModuleImageId,

    /// The arguments identities spell by name, which deduplication
    /// resolves.
    pub(super) pending_arguments: Vec<super::identity::PendingArguments>,
}

#[derive(Clone, Copy)]
pub(super) struct AggregateMemberDeclaration {
    pub(super) aggregate: TypeId,
    pub(super) member: AggregateMemberPath,
    pub(super) die: DieKey,
}

#[derive(Clone, Copy)]
pub(super) enum AggregateMemberPath {
    Direct(usize),
    Discriminant,
    Variant { variant: usize, member: usize },
}

pub(super) enum NamedConstantCollection {
    Enumerators(Vec<Enumerator>),
    Malformed(Arc<str>),
    Limit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct DynamicAggregateLayoutKey {
    pub(super) aggregate: TypeId,
    pub(super) child: DynamicAggregateChild,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum DynamicAggregateChild {
    Member(usize),
    Base(usize),
    Discriminant,
    VariantMember {
        variant: usize,
        member: usize,
    },

    /// A bound of a dimension of an array bounded at run time.
    Bound {
        dimension: usize,
        part: crate::image::type_facts::BoundPart,
    },

    /// Where an array's elements are.
    DataLocation,

    /// Whether an array is allocated.
    Allocated,

    /// Whether an array is associated with storage.
    Associated,
}

/// `DW_OP_push_object_address; DW_OP_deref`: the address the value's
/// first word holds.
const DEREFERENCE_OBJECT: [u8; 2] = [gimli::DW_OP_push_object_address.0, gimli::DW_OP_deref.0];

/// A subrange's bound, count, or stride, as its attribute gives it.
pub(super) enum SubrangeBound<'data> {
    Constant(i128),

    /// The value of an expression.
    Computed {
        expression: gimli::Expression<Reader<'data>>,
        byte_size: u8,
        signed: bool,
    },

    /// What the program stored where an expression says.
    Stored {
        expression: gimli::Expression<Reader<'data>>,
        byte_size: u8,
    },

    /// The value of the variable at this offset in `.debug_info`.
    Variable(u64),
}

/// A subrange's lower bound, where it ends, and its byte stride.
type Subrange<'data> = (
    SubrangeBound<'data>,
    Extent<'data>,
    Option<SubrangeBound<'data>>,
);

/// Where a subrange ends.
pub(super) enum Extent<'data> {
    Upper(SubrangeBound<'data>),
    Count(SubrangeBound<'data>),
    Unknown,
}

/// Whether an aggregate's child DIE describes its scope, such as a method,
/// nested type, static member, or template parameter, rather than bytes of
/// an instance. Any type may be declared in a scope: GCC nests the
/// qualified types a class's methods use in the class.
const fn is_scope_only_child(tag: gimli::DwTag) -> bool {
    is_type_die_tag(tag)
        || matches!(
            tag,
            gimli::DW_TAG_subprogram
                | gimli::DW_TAG_variable
                | gimli::DW_TAG_template_type_parameter
                | gimli::DW_TAG_template_value_parameter
                | gimli::DW_TAG_GNU_template_parameter_pack
                | gimli::DW_TAG_GNU_template_template_param
                | gimli::DW_TAG_friend
                | gimli::DW_TAG_imported_declaration
                | gimli::DW_TAG_imported_module
                | gimli::DW_TAG_access_declaration
        )
}

pub(super) fn zig_optional_payload_name(name: &str) -> Option<&str> {
    name.strip_prefix('?').filter(|payload| !payload.is_empty())
}

pub(super) fn zig_error_union_type_names(name: &str) -> Option<(&str, &str)> {
    let (error, payload) = name.split_once('!')?;
    if payload.is_empty()
        || !(error == "anyerror" || error.starts_with("error{") && error.ends_with('}'))
    {
        return None;
    }

    Some((error, payload))
}

impl<'a, 'data> TypeArenaBuilder<'a, 'data> {
    #[expect(
        clippy::too_many_arguments,
        reason = "the module's debug information and what building shares"
    )]
    pub(super) fn new(
        dwarf: &'a gimli::Dwarf<Reader<'data>>,
        units: &'a Units<'data>,
        type_signatures: &'a TypeSignatures,
        image: ModuleImageId,
        byte_order: ByteOrder,
        pool: &'a std::sync::Mutex<super::location::LocationsBuilder>,
        die_buffers: &'a DieBuffers<'data>,
        budget: crate::debug_info::dwarf::budget::Meter,
    ) -> Self {
        // Each unit is read on its own in parallel and merged in unit order,
        // which is the order a serial walk recorded everything in.
        let prepasses = (0..units.len())
            .into_par_iter()
            .map(|unit_index| UnitPrepass::read(dwarf, units, type_signatures, unit_index))
            .collect::<Vec<_>>();
        let mut die_offsets = Vec::with_capacity(units.len());
        let mut unit_languages = Vec::with_capacity(units.len());
        let mut produced_languages = Vec::with_capacity(units.len());
        let mut type_definitions = DieMap::default();
        let mut definition_declarations = DieMap::default();
        let mut ambiguous_type_declarations = DieMap::default();
        let mut scoped_types = Vec::new();
        let mut inline_namespaces = HashSet::new();

        for prepass in prepasses {
            die_offsets.push(prepass.offsets);
            unit_languages.push(prepass.language);
            produced_languages.push(prepass.produced);
            scoped_types.extend(prepass.scoped_types);
            inline_namespaces.extend(prepass.inline_namespaces);

            for (definition, declaration) in prepass.definitions {
                definition_declarations.insert(definition, declaration);
                if type_definitions
                    .insert(declaration, definition)
                    .is_some_and(|existing| existing != definition)
                {
                    ambiguous_type_declarations.insert(declaration, ());
                }
            }
        }

        let mut builder = Self {
            dwarf,
            units,
            type_signatures,
            image,
            by_die: Layered::default(),
            context: Arc::new(TypeContext {
                type_definitions,
                ambiguous_type_declarations,
                die_offsets,
                unit_languages,
                produced_languages,
                type_scopes: DieMap::default(),
                definition_declarations,
            }),
            entries: Rows::default(),
            explicit_names: Marks::default(),
            pending_arguments: Vec::new(),
            resolution_depth: 0,
            byte_order,
            limit_type: None,
            void_type: None,
            character_type: None,
            dynamic_record_layouts: HashMap::new(),
            pool,
            die_buffers,
            record_member_declarations: Vec::new(),
            budget,
            identity_parts: Rows::default(),
            go_identity_parts: Layered::default(),
            complex_parts: HashMap::new(),
            passed_by_value: Layered::default(),
            go_dict_indices: Layered::default(),
            home: None,
            foreign: false,
            aliases: Vec::new(),
        };

        // Types in one scope share its segments, so each distinct list is
        // resolved once, in parallel, as a path may name another unit's
        // function.
        let mut distinct = HashMap::<*const ScopeSegment, usize>::new();
        let mut lists = Vec::new();
        let scoped_types = scoped_types
            .into_iter()
            .map(|(key, segments)| {
                let index = *distinct.entry(segments.as_ptr()).or_insert_with(|| {
                    lists.push(Arc::clone(&segments));
                    lists.len() - 1
                });
                (key, index)
            })
            .collect::<Vec<_>>();
        let paths = lists
            .par_iter()
            .map(|segments| builder.scope_path(segments, &inline_namespaces))
            .collect::<Vec<_>>();
        let context = Arc::make_mut(&mut builder.context);

        for (key, index) in scoped_types {
            let path = &paths[index];
            if !path.is_empty() {
                context.type_scopes.insert(key, path.clone());
            }
        }

        builder
    }

    /// Makes everything built so far the base that the walks of units
    /// share.
    pub(super) fn freeze(&mut self) {
        self.by_die.freeze();
        self.entries.freeze();
        self.explicit_names.freeze();
        self.identity_parts.freeze();
        self.go_identity_parts.freeze();
        self.go_dict_indices.freeze();
        self.passed_by_value.freeze();
    }

    pub(super) fn variable_type(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> TypeResolution {
        let key = match die_reference_with_signatures(
            value,
            unit_index,
            self.units,
            self.type_signatures,
        ) {
            Ok(Some(key)) => key,
            Ok(None) => return TypeResolution::Malformed("variable has no type".into()),
            Err(error) => return TypeResolution::Malformed(error.to_string().into()),
        };

        let id = self.resolve(key);

        match self.entries.get(id.index()) {
            Some(TypeEntry::Malformed(reason)) => TypeResolution::Malformed(Arc::clone(reason)),
            Some(TypeEntry::Building) => {
                TypeResolution::Malformed("type graph did not finish building".into())
            }
            Some(TypeEntry::Resolved(_)) => TypeResolution::Resolved(id),
            None => TypeResolution::Malformed("type ID is outside the arena".into()),
        }
    }

    /// The DIE an attribute of a DIE in unit `unit_index` refers to.
    pub(super) fn reference(
        &self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> Option<DieKey> {
        die_reference_with_signatures(value, unit_index, self.units, self.type_signatures)
            .ok()
            .flatten()
    }

    /// Builds the type an attribute refers to, if any, so that the type
    /// index knows it; a malformed reference only goes unbuilt.
    pub(super) fn reach(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) {
        let _ = self.type_reference(unit_index, value);
    }

    /// Builds the type an attribute of a DIE in unit `unit_index` refers to.
    fn type_reference(
        &mut self,
        unit_index: usize,
        value: Option<gimli::AttributeValue<Reader<'data>>>,
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        let key =
            die_reference_with_signatures(value, unit_index, self.units, self.type_signatures)
                .map_err(malformed)?;

        Ok(key.map(|key| TypeReference {
            image: self.image,
            id: self.resolve(key),
        }))
    }

    /// The direct children of the DIE at `offset` in unit `unit_index`.
    pub(super) fn children(
        &self,
        unit_index: usize,
        offset: gimli::UnitOffset,
    ) -> std::result::Result<Children<'a, 'data>, Arc<str>> {
        let units: &'a Units<'data> = self.units;
        let unit = units.get(unit_index).ok_or("DIE unit is unavailable")?;

        Ok(Children::new(
            unit.entries_raw(Some(offset)).map_err(malformed)?,
            self.die_buffers,
        ))
    }

    /// The language unit `unit_index`'s producer proves, if it does.
    pub(super) fn produced_language(&self, unit_index: usize) -> Option<SourceLanguage> {
        self.context
            .produced_languages
            .get(unit_index)
            .copied()
            .flatten()
    }

    /// The language a unit's `DW_AT_language` names.
    pub(super) fn dwarf_language(&self, unit_index: usize) -> Option<gimli::DwLang> {
        self.context
            .unit_languages
            .get(unit_index)
            .copied()
            .flatten()
    }

    pub(super) fn is_zig(&self, unit_index: usize) -> bool {
        self.produced_language(unit_index) == Some(SourceLanguage::Zig)
    }

    /// The id of the next type, unless the types fill every id but those
    /// [`Self::reserved_id`] gives.
    fn next_id(&self) -> Option<TypeId> {
        (self.entries.len() < MAX_BUILT_TYPES).then(|| self.reserved_id())
    }

    /// The id of the next type, for the few types made once each: void,
    /// the character, and the type past the work limit.
    fn reserved_id(&self) -> TypeId {
        TypeId::new(super::row(self.entries.len()))
    }

    /// A built type through the names typedefs give it.
    fn unnamed(&self, mut id: TypeId) -> Option<&TypeInfo> {
        for _ in 0..MAX_TYPE_RESOLUTION_DEPTH {
            match self.entries.get(id.index())? {
                TypeEntry::Resolved(TypeInfo {
                    kind:
                        TypeKind::Named {
                            target: Some(target),
                            ..
                        },
                    ..
                }) => id = target.id,
                TypeEntry::Resolved(info) => return Some(info),
                TypeEntry::Building | TypeEntry::Malformed(_) => return None,
            }
        }

        None
    }

    /// The byte size of a built type, if it has one.
    fn byte_size_of(&self, id: TypeId) -> Option<u64> {
        match self.entries.get(id.index())? {
            TypeEntry::Resolved(info) => info.byte_size,
            TypeEntry::Building | TypeEntry::Malformed(_) => None,
        }
    }

    pub(super) fn resolve(&mut self, key: DieKey) -> TypeId {
        if let Some(id) = self.by_die.get(&key) {
            return *id;
        }

        if let Some((home, groups)) = self.home
            && groups.get(key.unit) != Some(&home)
        {
            self.foreign = true;
        }

        let canonical = self.canonical_type_key(key);
        if let Ok(canonical) = canonical
            && canonical != key
        {
            let id = self.resolve(canonical);
            self.by_die.insert(key, id);
            if self.home.is_some() {
                self.aliases.push(key);
            }

            return id;
        }

        let Some(id) = self.next_id() else {
            return self.type_limit(key);
        };

        if self.budget.charge("types", size_of::<TypeEntry>()).is_err() {
            return self.type_limit(key);
        }

        self.by_die.insert(key, id);
        self.entries.push(TypeEntry::Building);
        let entry = match canonical {
            Err(reason) => TypeEntry::Malformed(reason),
            Ok(_) if self.resolution_depth >= MAX_TYPE_RESOLUTION_DEPTH => {
                TypeEntry::Malformed("type wrapper depth exceeds its limit".into())
            }
            Ok(_) => {
                self.resolution_depth += 1;
                let entry = self.build(key, id).unwrap_or_else(TypeEntry::Malformed);
                self.resolution_depth -= 1;
                entry
            }
        };

        self.entries[id.index()] = entry;

        id
    }

    fn type_limit(&mut self, key: DieKey) -> TypeId {
        let id = if let Some(id) = self.limit_type {
            id
        } else {
            let id = self.reserved_id();
            self.entries.push(TypeEntry::Malformed(
                "type graph exceeds its work limit".into(),
            ));
            self.limit_type = Some(id);
            id
        };

        self.by_die.insert(key, id);
        id
    }

    /// Returns `void`. Producers omit `DW_AT_type` to mean it, so a
    /// `const void` or `typedef void T` has no target DIE to resolve.
    pub(super) fn void_type(&mut self) -> TypeReference {
        let reference = TypeReference {
            image: self.image,
            id: self.void_type.unwrap_or_else(|| self.reserved_id()),
        };

        if self.void_type.is_none() {
            self.entries.push(resolved(
                reference,
                "void".into(),
                None,
                TypeKind::Unspecified,
            ));
            self.void_type = Some(reference.id);
        }

        reference
    }

    /// The one-byte character of a string type that names none.
    fn character_type(&mut self) -> TypeReference {
        let reference = TypeReference {
            image: self.image,
            id: self.character_type.unwrap_or_else(|| self.reserved_id()),
        };

        if self.character_type.is_none() {
            let name: Arc<str> = "character".into();
            self.entries.push(resolved(
                reference,
                Arc::clone(&name),
                Some(1),
                TypeKind::Base(BaseType {
                    base_name: Arc::clone(&name),
                    name,
                    encoding: BaseTypeEncoding::UnsignedCharacter,
                    byte_size: 1,
                    bit_size: None,
                }),
            ));
            self.explicit_names.insert(reference.id.index());
            self.character_type = Some(reference.id);
        }

        reference
    }

    fn canonical_type_key(&self, key: DieKey) -> std::result::Result<DieKey, Arc<str>> {
        let mut current = key;
        let mut visited = Seen::default();

        while visited.insert(current) {
            let unit = self
                .units
                .get(current.unit)
                .ok_or_else(|| Arc::from("type reference is outside loaded units"))?;
            if !self
                .context
                .die_offsets
                .get(current.unit)
                .is_some_and(|offsets| offsets.contains(current.offset))
            {
                return Err("type reference does not identify a DIE".into());
            }

            let offset = gimli::UnitOffset(current.offset);
            let entry = unit
                .entries_raw(Some(offset))
                .and_then(|raw| self.die_buffers.read(raw, offset))
                .map_err(|error| Arc::from(error.to_string()))?;
            if !is_type_die_tag(entry.tag()) {
                return Err(format!("DW_AT_type target has non-type tag {:?}", entry.tag()).into());
            }

            if self
                .context
                .ambiguous_type_declarations
                .contains_key(&current)
            {
                return Err("type declaration has multiple definitions".into());
            }

            if let Some(definition) = self.context.type_definitions.get(&current).copied() {
                current = definition;
                continue;
            }

            let Some(signature) = entry.attr_value(gimli::DW_AT_signature) else {
                return Ok(current);
            };

            current = die_reference_with_signatures(
                Some(signature),
                current.unit,
                self.units,
                self.type_signatures,
            )
            .map_err(|error| Arc::from(error.to_string()))?
            .ok_or_else(|| Arc::from("type declaration signature has no definition"))?;
        }

        Err("type declaration/definition references form a cycle".into())
    }

    /// Builds the type at a DIE that [`Self::canonical_type_key`] validated
    /// as a type DIE.
    fn build(&mut self, key: DieKey, id: TypeId) -> Built {
        let units: &'a Units<'data> = self.units;
        let unit = units
            .get(key.unit)
            .ok_or("type reference is outside loaded units")?;
        let buffers: &'a DieBuffers<'data> = self.die_buffers;
        let offset = gimli::UnitOffset(key.offset);
        let entry = unit
            .entries_raw(Some(offset))
            .and_then(|raw| buffers.read(raw, offset))
            .map_err(malformed)?;
        let reference = TypeReference {
            image: self.image,
            id,
        };

        let origins = origin_chain(units, key.unit, &entry).map_err(malformed)?;
        let explicit_name =
            string_with_origins(self.dwarf, units, unit, &entry, &origins, gimli::DW_AT_name)
                .map_err(malformed)?;

        // Self-hosted Zig names a type declared in another by its own name,
        // and says which one it is in.
        let explicit_name = match explicit_name {
            Some(name) if self.is_zig(key.unit) && entry.attr_value(DW_AT_ZIG_PARENT).is_some() => {
                Some(
                    zig_qualified_name(self.dwarf, units, key.unit, &entry, name)
                        .map_err(malformed)?,
                )
            }
            name => name,
        };

        if explicit_name.is_some() {
            self.explicit_names.insert(id.index());
        }

        // Validate the tag's mandatory attributes first: an unusable size
        // returns an opaque type early, which must not mask a defect.
        if let Some(defect) = self.mandatory_attribute_defect(&entry, key.unit) {
            return Err(defect);
        }

        let address_class = match resolve_address_class(&entry, reference, explicit_name.clone()) {
            Ok(address_class) => address_class,
            Err(entry) => return Ok(*entry),
        };

        let explicit_size = match resolve_explicit_size(&entry, reference, explicit_name.clone()) {
            Ok(size) => size,
            Err(entry) => return Ok(*entry),
        };

        let pointer_size = explicit_size
            .or_else(|| (address_class == 0).then_some(u64::from(unit.encoding().address_size)));
        let named = explicit_name.is_some();
        let slice_layout = if entry.tag() == gimli::DW_TAG_structure_type {
            self.slice_layout(&entry, key, explicit_name.as_deref())
        } else {
            None
        };

        // GNAT passes an array of an unconstrained type as a record of
        // pointers to its elements and bounds, which is the array.
        if slice_layout.is_none()
            && let Some(array) = self.ada_fat_pointer(&entry, key.unit)
        {
            let array = self.units[key.unit]
                .entry(array)
                .map_err(|_| Arc::<str>::from("an array's record names no array"))?;
            let endian = gimli::Reader::endian(gimli::Section::reader(&self.dwarf.debug_info));
            let dereference_object = gimli::Expression(Reader::new(&DEREFERENCE_OBJECT, endian));
            let built = self.build_array(
                &array,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                Some(dereference_object),
            )?;

            if named && matches!(built, TypeEntry::Resolved(_)) {
                self.record_identity_parts(&entry, key, id);
            }

            return Ok(built);
        }

        let built = match slice_layout {
            Some(layout) => self.build_slice_type(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                layout,
            ),
            None => self.build_kind(
                &entry,
                key.unit,
                reference,
                explicit_name,
                explicit_size,
                pointer_size,
                address_class,
            ),
        }?;
        if named && matches!(built, TypeEntry::Resolved(_)) {
            self.record_identity_parts(&entry, key, id);
        }

        Ok(built)
    }

    /// Builds a type by its tag.
    #[expect(
        clippy::too_many_arguments,
        reason = "the tag's builders take the attributes validated for every type"
    )]
    fn build_kind(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        pointer_size: Option<u64>,
        address_class: u64,
    ) -> Built {
        match entry.tag() {
            gimli::DW_TAG_base_type => {
                Self::build_base_type(entry, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_enumeration_type => self.build_enumeration_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                explicit_size,
            ),
            gimli::DW_TAG_pointer_type => self.build_pointer_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                pointer_size,
                address_class,
            ),
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => self
                .build_reference_type(
                    entry,
                    unit_index,
                    reference,
                    explicit_name,
                    pointer_size,
                    address_class,
                ),
            gimli::DW_TAG_array_type => {
                self.build_array_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_string_type => {
                self.build_string_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_subrange_type => {
                self.build_subrange_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => {
                self.build_record_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_union_type => {
                self.build_union_type(entry, unit_index, reference, explicit_name, explicit_size)
            }

            gimli::DW_TAG_typedef
            | gimli::DW_TAG_template_alias
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type => {
                self.build_wrapper_type(entry, unit_index, reference, explicit_name, explicit_size)
            }
            gimli::DW_TAG_unspecified_type => Ok(resolved(
                reference,
                explicit_name.unwrap_or_else(|| Arc::from("void")),
                explicit_size,
                TypeKind::Unspecified,
            )),
            // A Go func is a pointer to its closure context.
            gimli::DW_TAG_subroutine_type
                if self.language(unit_index) == SourceLanguage::Go
                    && Self::go_kind(entry) == Some(GoKind::Func)
                    && explicit_size == Some(8) =>
            {
                Ok(resolved(
                    reference,
                    explicit_name.unwrap_or_else(|| Arc::from("func")),
                    explicit_size,
                    TypeKind::Function,
                ))
            }
            gimli::DW_TAG_subroutine_type => {
                self.build_signature_type(entry, unit_index, reference, explicit_name)
            }

            // A type this backend does not model is opaque, not defective.
            tag => Ok(opaque(
                reference,
                explicit_name.unwrap_or_else(|| Arc::from(format!("{tag:?}"))),
                explicit_size,
                format!("type tag {tag:?} is unsupported"),
            )),
        }
    }

    fn record_accessibility(
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
        record_kind: RecordKind,
    ) -> std::result::Result<Accessibility, Arc<str>> {
        match entry.attr_value(gimli::DW_AT_accessibility) {
            Some(gimli::AttributeValue::Accessibility(value))
                if value == gimli::DW_ACCESS_public =>
            {
                Ok(Accessibility::Public)
            }

            Some(gimli::AttributeValue::Accessibility(value))
                if value == gimli::DW_ACCESS_protected =>
            {
                Ok(Accessibility::Protected)
            }

            Some(gimli::AttributeValue::Accessibility(value))
                if value == gimli::DW_ACCESS_private =>
            {
                Ok(Accessibility::Private)
            }
            None if record_kind == RecordKind::Class => Ok(Accessibility::Private),
            None => Ok(Accessibility::Public),
            Some(_) => Err("record accessibility has an invalid encoding".into()),
        }
    }

    fn record_byte_layout(
        entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    ) -> RecordMemberLayout {
        entry
            .attr(gimli::DW_AT_data_member_location)
            .and_then(constant_member_offset)
            .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
    }

    /// A defect in the attributes a type's tag requires.
    fn mandatory_attribute_defect(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<Arc<str>> {
        match entry.tag() {
            gimli::DW_TAG_base_type => base_type_encoding(entry).err(),
            gimli::DW_TAG_reference_type | gimli::DW_TAG_rvalue_reference_type => self
                .target_defect(
                    entry,
                    unit_index,
                    "reference type",
                    TargetRequirement::Required,
                ),
            gimli::DW_TAG_typedef | gimli::DW_TAG_template_alias => {
                strict_flag(entry, gimli::DW_AT_declaration)
                    .err()
                    .or_else(|| {
                        self.target_defect(
                            entry,
                            unit_index,
                            "named type",
                            TargetRequirement::Optional,
                        )
                    })
            }

            gimli::DW_TAG_array_type
            | gimli::DW_TAG_coarray_type
            | gimli::DW_TAG_set_type
            | gimli::DW_TAG_file_type
            | gimli::DW_TAG_dynamic_type
            | gimli::DW_TAG_ptr_to_member_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type => {
                self.target_defect(entry, unit_index, "type", TargetRequirement::Required)
            }

            // A pointer's target is optional (`void *`), but must be a type.
            _ => self.target_defect(entry, unit_index, "type", TargetRequirement::Optional),
        }
    }

    /// A defect in a `DW_AT_type` target: absent though required, or not a
    /// type DIE.
    fn target_defect(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        kind: &str,
        requirement: TargetRequirement,
    ) -> Option<Arc<str>> {
        match die_reference_with_signatures(
            entry.attr_value(gimli::DW_AT_type),
            unit_index,
            self.units,
            self.type_signatures,
        ) {
            Ok(Some(key)) => {
                let target = self
                    .context
                    .die_offsets
                    .get(key.unit)
                    .filter(|offsets| offsets.contains(key.offset))
                    .and_then(|_| self.units.get(key.unit))
                    .and_then(|unit| unit.entry(gimli::UnitOffset(key.offset)).ok());
                match target {
                    None => Some(format!("{kind} target does not identify a DIE").into()),
                    Some(target) if !is_type_die_tag(target.tag()) => {
                        Some(format!("{kind} target has non-type tag {:?}", target.tag()).into())
                    }
                    Some(_) => None,
                }
            }
            Ok(None) => match requirement {
                TargetRequirement::Required => Some(format!("{kind} has no target").into()),
                TargetRequirement::Optional => None,
            },
            Err(error) => Some(error.to_string().into()),
        }
    }

    fn build_base_type(
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let name = explicit_name.unwrap_or_else(|| Arc::from("<unnamed base type>"));
        let byte_size = explicit_size.ok_or("base type has no byte size")?;
        if byte_size == 0 {
            // Zig's storage-less `void` payload and Rust's unit type `()` are
            // zero-byte base types with no bits to decode, whatever their
            // encoding: `void` is unspecified, and the others hold nothing,
            // as an empty structure does.
            let kind = if name.as_ref() == "void" {
                TypeKind::Unspecified
            } else {
                TypeKind::Record {
                    kind: RecordKind::Struct,
                    members: Arc::from([]),
                    bases: Arc::from([]),
                    incomplete: false,
                }
            };

            return Ok(resolved(reference, name, Some(0), kind));
        }

        let raw_encoding = gimli::DwAte(base_type_encoding(entry)?);
        let encoding = match raw_encoding {
            gimli::DW_ATE_float => Some(BaseTypeEncoding::Floating),
            gimli::DW_ATE_complex_float if byte_size % 2 == 0 => {
                Some(BaseTypeEncoding::ComplexFloating)
            }

            // `char8_t`, `char16_t`, `char32_t`, and Rust's `char` are code
            // units of UTF-8, UTF-16, and UTF-32.
            gimli::DW_ATE_UTF if matches!(byte_size, 1 | 2 | 4) => {
                Some(BaseTypeEncoding::UnsignedCharacter)
            }

            // C++'s `wchar_t` is a type of its own, which compilers encode
            // as an integer; on Linux it holds UTF-32.
            gimli::DW_ATE_signed if name.as_ref() == "wchar_t" => {
                Some(BaseTypeEncoding::SignedCharacter)
            }
            gimli::DW_ATE_unsigned if name.as_ref() == "wchar_t" => {
                Some(BaseTypeEncoding::UnsignedCharacter)
            }
            _ => integer_encoding(raw_encoding),
        };

        let Some(encoding) = encoding else {
            return Ok(opaque(
                reference,
                name,
                Some(byte_size),
                format!("base type encoding {raw_encoding:?} is unsupported"),
            ));
        };

        let bit_size = match entry.attr(gimli::DW_AT_bit_size).map(unsigned_constant) {
            None => None,
            Some(UnsignedConstant::Value(0)) => {
                return Err("base type has a zero bit size".into());
            }
            Some(UnsignedConstant::Value(bit_size)) if bit_size <= byte_size.saturating_mul(8) => {
                Some(bit_size)
            }
            Some(UnsignedConstant::Value(_) | UnsignedConstant::Oversized) => {
                return Err("base type bit size exceeds its byte storage".into());
            }
            Some(UnsignedConstant::NonConstant) => {
                return Err("DW_AT_bit_size is not an unsigned integer constant".into());
            }
        };

        let base = BaseType {
            name: Arc::clone(&name),
            base_name: Arc::clone(&name),
            encoding,
            byte_size,
            bit_size,
        };

        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Base(base),
        ))
    }

    pub(super) fn resolved_integer_base(
        &self,
        id: TypeId,
    ) -> std::result::Result<BaseType, Arc<str>> {
        let mut current = id;
        let mut visited = HashSet::new();

        loop {
            if !visited.insert(current) {
                return Err("enumeration underlying type contains a wrapper cycle".into());
            }

            let entry = self
                .entries
                .get(current.index())
                .ok_or_else(|| Arc::from("enumeration underlying type is outside the arena"))?;
            let info = match entry {
                TypeEntry::Resolved(info) => info,
                TypeEntry::Malformed(reason) => return Err(Arc::clone(reason)),
                TypeEntry::Building => {
                    return Err("enumeration underlying type did not finish building".into());
                }
            };

            match &info.kind {
                TypeKind::Base(base) if !is_floating(base.encoding) => {
                    return Ok(base.clone());
                }
                TypeKind::Enumeration { representation, .. } => {
                    return Ok(representation.clone());
                }

                TypeKind::Modified { target, .. }
                | TypeKind::Named {
                    target: Some(target),
                    ..
                } => {
                    current = target.id;
                }
                TypeKind::Base(_) => {
                    return Err("enumeration underlying type is not integral".into());
                }
                _ => return Err("enumeration underlying type is not an integer type".into()),
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "enumeration normalization validates representation and ordered symbols together"
    )]
    fn build_enumeration_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let name = explicit_name.unwrap_or_else(|| {
            Arc::from(format!("<anonymous enumeration@0x{:x}>", entry.offset().0))
        });
        let underlying = self.target(entry, unit_index)?;
        let mut representation = if let Some(underlying) = underlying {
            self.resolved_integer_base(underlying.id)?
        } else {
            let byte_size = explicit_size
                .ok_or("enumeration has neither an underlying type nor a byte size")?;
            if byte_size == 0 {
                return Err("enumeration has a zero byte size".into());
            }

            let Ok(raw_encoding) = base_type_encoding(entry) else {
                return Err("enumeration without an underlying type has no encoding".into());
            };

            BaseType {
                name: Arc::clone(&name),
                base_name: Arc::clone(&name),
                encoding: integer_encoding(gimli::DwAte(raw_encoding))
                    .ok_or("enumeration encoding is not an integral encoding")?,
                byte_size,
                bit_size: None,
            }
        };

        let byte_size = explicit_size.unwrap_or(representation.byte_size);
        if byte_size == 0 {
            return Err("enumeration has a zero byte size".into());
        }

        if byte_size != representation.byte_size {
            return Err("enumeration byte size differs from its underlying type".into());
        }

        if let Some(attribute) = entry.attr(gimli::DW_AT_encoding) {
            let enum_encoding = match unsigned_constant(attribute) {
                UnsignedConstant::Value(value) => u8::try_from(value).ok(),
                UnsignedConstant::Oversized | UnsignedConstant::NonConstant => None,
            };

            let compatible = enum_encoding.is_some_and(|encoding| {
                matches!(
                    (gimli::DwAte(encoding), representation.encoding),
                    (gimli::DW_ATE_boolean, BaseTypeEncoding::Boolean)
                        | (
                            gimli::DW_ATE_signed | gimli::DW_ATE_signed_char,
                            BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                        )
                        | (
                            gimli::DW_ATE_unsigned | gimli::DW_ATE_unsigned_char,
                            BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter
                        )
                )
            });
            if !compatible {
                return Err("enumeration encoding differs from its underlying type".into());
            }
        }

        if let Some(attribute) = entry.attr(gimli::DW_AT_bit_size) {
            representation.bit_size = match unsigned_constant(attribute) {
                UnsignedConstant::Value(0) => {
                    return Err("enumeration has a zero bit size".into());
                }
                UnsignedConstant::Value(value) if value <= byte_size.saturating_mul(8) => {
                    Some(value)
                }

                UnsignedConstant::Value(_)
                | UnsignedConstant::Oversized
                | UnsignedConstant::NonConstant => {
                    return Err("enumeration bit size is not valid for its byte storage".into());
                }
            };
        }

        representation.name = Arc::clone(&name);

        let unit = &self.units[unit_index];
        let mut enumerators = Vec::new();
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            // rustc declares an enumeration's methods inside it, and its
            // generic arguments, none of which change its values.
            if matches!(
                child.tag(),
                gimli::DW_TAG_subprogram
                    | gimli::DW_TAG_template_type_parameter
                    | gimli::DW_TAG_template_value_parameter
            ) {
                continue;
            }

            if child.tag() != gimli::DW_TAG_enumerator {
                return Err(format!(
                    "enumeration contains unsupported direct child {:?}",
                    child.tag()
                )
                .into());
            }

            if enumerators.len() >= MAX_RECORD_CHILDREN
                || self
                    .budget
                    .charge("symbolic names", size_of::<Enumerator>())
                    .is_err()
            {
                return Ok(opaque(
                    reference,
                    name,
                    Some(byte_size),
                    "enumerator metadata exceeds its resource limit",
                ));
            }

            let enumerator_name = copy_name(self.dwarf, unit, child)
                .map_err(malformed)?
                .ok_or("enumerator has no name")?;
            let value = child
                .attr_value(gimli::DW_AT_const_value)
                .ok_or("enumerator has no constant value")?;
            enumerators.push(Enumerator {
                name: enumerator_name,
                value: enumeration_constant(value, &representation, self.byte_order)?,
            });
        }

        let scoped = strict_flag(entry, gimli::DW_AT_enum_class)?;

        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Enumeration {
                representation,
                underlying,
                enumerators: enumerators.into(),
                origin: EnumerationOrigin::Language,
                scoped,
            },
        ))
    }

    /// A function's type: what it returns and what its parameters are.
    fn build_signature_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
    ) -> Built {
        let returns = self.target(entry, unit_index)?;
        let mut parameters = Vec::new();
        let mut variadic = false;
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            match child.tag() {
                gimli::DW_TAG_formal_parameter => {
                    if parameters.len() >= MAX_RECORD_CHILDREN {
                        return Ok(opaque(
                            reference,
                            explicit_name.unwrap_or_else(|| Arc::from("<function>")),
                            None,
                            "parameter metadata exceeds its resource limit",
                        ));
                    }

                    parameters.push(
                        self.target(child, unit_index)?
                            .ok_or("a function type's parameter has no type")?,
                    );
                }
                gimli::DW_TAG_unspecified_parameters => variadic = true,
                // Producers may describe more, such as template parameters,
                // which a signature does not need.
                _ => {}
            }
        }

        let prototyped = strict_flag(entry, gimli::DW_AT_prototyped)?;
        let kind = TypeKind::Signature {
            returns,
            parameters: parameters.into(),
            variadic,
            prototyped,
        };

        // Named from its parts once every type is built.
        let name = explicit_name.unwrap_or_else(|| Arc::from("<function>"));

        Ok(resolved(reference, name, None, kind))
    }

    pub(super) fn target(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<TypeReference>, Arc<str>> {
        self.type_reference(unit_index, entry.attr_value(gimli::DW_AT_type))
    }

    /// Every integer constant a unit declares at its top level, such as a
    /// Go package's `const`s, by name. Constants of other types are left
    /// out; values come from each constant's own type, read before typed Go
    /// constants turn their types into enumerations.
    pub(super) fn named_constants(&mut self) -> BTreeMap<Arc<str>, IntegerValue> {
        // Each unit is searched for its constants in parallel; their types
        // are built in unit order, which numbers them.
        let units: &Units<'data> = self.units;
        let found = units
            .par_iter()
            .map(|unit| {
                let mut found = Vec::new();
                let Ok(mut walk) = DieWalk::new(unit) else {
                    return found;
                };

                while let Ok(Some(die)) = walk.next() {
                    if die.depth != 1 || die.tag != gimli::DW_TAG_constant {
                        continue;
                    }

                    let Ok(entry) = walk.decode() else {
                        break;
                    };

                    found.push(entry.clone());
                }

                found
            })
            .collect::<Vec<_>>();
        let mut constants = BTreeMap::new();

        for (unit_index, found) in found.iter().enumerate() {
            let unit = &units[unit_index];
            for entry in found {
                let (Ok(Some(name)), Some(value)) = (
                    copy_name(self.dwarf, unit, entry),
                    entry.attr_value(gimli::DW_AT_const_value),
                ) else {
                    continue;
                };

                let Ok(Some(target)) = self.target(entry, unit_index) else {
                    continue;
                };

                let Some(TypeEntry::Resolved(TypeInfo {
                    kind: TypeKind::Base(base),
                    ..
                })) = self.entries.get(target.id.index())
                else {
                    continue;
                };

                if matches!(base.encoding, BaseTypeEncoding::Floating) {
                    continue;
                }

                if let Ok(value) = enumeration_constant(value, base, self.byte_order) {
                    constants.insert(name, value);
                }
            }
        }

        constants
    }

    pub(super) fn populate_go_named_constants(&mut self) {
        let mut constants = BTreeMap::<TypeId, NamedConstantCollection>::new();
        for (unit_index, unit) in self.units.iter().enumerate() {
            if self.language(unit_index) != SourceLanguage::Go {
                continue;
            }

            let Ok(mut walk) = DieWalk::new(unit) else {
                continue;
            };

            while let Ok(Some(die)) = walk.next() {
                if die.tag != gimli::DW_TAG_constant {
                    continue;
                }

                let Ok(entry) = walk.decode() else {
                    break;
                };

                let Ok(Some(target)) = self.target(entry, unit_index) else {
                    continue;
                };

                let target = target.id;
                let Some(TypeEntry::Resolved(info)) = self.entries.get(target.index()) else {
                    continue;
                };

                let TypeKind::Base(base) = &info.kind else {
                    continue;
                };

                if !info.name.contains('.') || is_floating(base.encoding) {
                    continue;
                }

                let representation = base.clone();
                let enumerator = copy_name(self.dwarf, unit, entry)
                    .map_err(|error| Arc::from(error.to_string()))
                    .and_then(|name| name.ok_or_else(|| Arc::from("typed Go constant has no name")))
                    .and_then(|name| {
                        entry
                            .attr_value(gimli::DW_AT_const_value)
                            .ok_or_else(|| Arc::from("typed Go constant has no value"))
                            .and_then(|value| {
                                enumeration_constant(value, &representation, self.byte_order)
                            })
                            .map(|value| Enumerator { name, value })
                    });

                let symbolic_limit = enumerator.is_ok()
                    && self
                        .budget
                        .charge("symbolic names", size_of::<Enumerator>())
                        .is_err();
                let collection = constants
                    .entry(target)
                    .or_insert_with(|| NamedConstantCollection::Enumerators(Vec::new()));

                if symbolic_limit {
                    *collection = NamedConstantCollection::Limit;
                    continue;
                }

                match (collection, enumerator) {
                    (NamedConstantCollection::Enumerators(values), Ok(enumerator))
                        if values.len() < MAX_RECORD_CHILDREN =>
                    {
                        values.push(enumerator);
                    }
                    (collection @ NamedConstantCollection::Enumerators(_), Ok(_)) => {
                        *collection = NamedConstantCollection::Limit;
                    }
                    (collection @ NamedConstantCollection::Enumerators(_), Err(reason)) => {
                        *collection = NamedConstantCollection::Malformed(reason);
                    }
                    (NamedConstantCollection::Malformed(_) | NamedConstantCollection::Limit, _) => {
                    }
                }
            }
        }

        self.declare_go_named_constants(constants);
    }

    /// Makes each typed Go constant's type an enumeration of its constants.
    fn declare_go_named_constants(&mut self, constants: BTreeMap<TypeId, NamedConstantCollection>) {
        for (target, collection) in constants {
            let index = target.index();
            match collection {
                NamedConstantCollection::Malformed(reason) => {
                    self.entries[index] = TypeEntry::Malformed(reason);
                }
                NamedConstantCollection::Limit => {
                    let TypeEntry::Resolved(info) = &mut self.entries[index] else {
                        continue;
                    };

                    info.kind = TypeKind::Opaque {
                        description: "typed Go constant count exceeds its resource limit".into(),
                    };
                }
                NamedConstantCollection::Enumerators(enumerators) if !enumerators.is_empty() => {
                    let TypeEntry::Resolved(info) = &mut self.entries[index] else {
                        continue;
                    };

                    let TypeKind::Base(base) = &info.kind else {
                        continue;
                    };

                    let mut representation = base.clone();
                    representation.name = Arc::clone(&info.name);
                    info.kind = TypeKind::Enumeration {
                        representation,
                        underlying: None,
                        enumerators: enumerators.into(),
                        origin: EnumerationOrigin::NamedConstants,
                        scoped: false,
                    };
                }
                NamedConstantCollection::Enumerators(_) => {}
            }
        }
    }

    /// Gives each aggregate's members their declarations, or makes an
    /// aggregate malformed when one of its members' cannot be read.
    pub(super) fn populate_record_member_declarations(&mut self, files: &mut Files) {
        let pending = std::mem::take(&mut self.record_member_declarations);

        // Each declaration is read on its own in parallel, and their files
        // interned in the order noted, which numbers them as a serial pass.
        // A chunk at a time, so that only a chunk's paths are held at once.
        let mut declarations = Vec::with_capacity(pending.len());

        for chunk in pending.chunks(MEMBER_DECLARATION_CHUNK) {
            let read = chunk
                .par_iter()
                .map(|metadata| self.member_declaration(metadata.die))
                .collect::<Vec<_>>();

            declarations.extend(read.into_iter().map(
                |declaration| -> std::result::Result<_, Arc<str>> {
                    Ok(declaration?.map(|declared| declared.intern(files)))
                },
            ));
        }

        let mut order = (0..pending.len()).collect::<Vec<_>>();
        order.par_sort_by_key(|index| pending[*index].aggregate);

        for members in
            order.chunk_by(|left, right| pending[*left].aggregate == pending[*right].aggregate)
        {
            let record = pending[members[0]].aggregate;
            if let Some(Err(reason)) = members
                .iter()
                .map(|index| &declarations[*index])
                .find(|declaration| declaration.is_err())
            {
                self.entries[record.index()] = TypeEntry::Malformed(Arc::clone(reason));
                continue;
            }

            if let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(record.index()) {
                declare_members(
                    &mut info.kind,
                    members.iter().map(|index| {
                        let declaration = declarations[*index].as_ref().ok().cloned().flatten();
                        (pending[*index].member, declaration)
                    }),
                );
            }
        }
    }

    /// Where the member whose DIE is `key` is declared.
    fn member_declaration(
        &self,
        key: DieKey,
    ) -> std::result::Result<Option<DeclaredSource>, Arc<str>> {
        let unit = self
            .units
            .get(key.unit)
            .ok_or_else(|| Arc::from("record member unit is unavailable"))?;
        let entry = unit
            .entry(gimli::UnitOffset(key.offset))
            .map_err(|error| Arc::from(error.to_string()))?;
        let chain = origin_chain(self.units, key.unit, &entry)
            .map_err(|error| Arc::from(error.to_string()))?;

        declared_source(self.dwarf, self.units, unit, &entry, &chain)
            .map_err(|error| Arc::from(error.to_string()))
    }

    fn target_name(&self, target: TypeReference) -> Arc<str> {
        self.entries
            .get(target.id.index())
            .and_then(|entry| match entry {
                TypeEntry::Resolved(info) => Some(Arc::clone(&info.name)),
                TypeEntry::Building | TypeEntry::Malformed(_) => None,
            })
            .unwrap_or_else(|| Arc::from("<recursive type>"))
    }

    pub(super) fn finalize_type_graph(&mut self) {
        for (index, entry) in self.entries.flat().iter_mut().enumerate() {
            if matches!(entry, TypeEntry::Building) {
                *entry = TypeEntry::Malformed(
                    format!("type graph node {index} did not finish building").into(),
                );
            }
        }

        self.add_complex_parts();

        propagate_wrapper_sizes(self.entries.flat());

        self.reject_inline_storage_cycles();

        let names_phase = crate::span!("types.names");

        // Most types are named explicitly and keep their names, so only the
        // others are rendered, and with one set of the types being visited,
        // which every rendering leaves empty.
        let mut visiting = HashSet::new();
        let names = (0..self.entries.len())
            .map(|index| {
                let id = TypeId::new(super::row(index));
                if self.explicit_names.contains(id.index()) {
                    return None;
                }

                visiting.clear();
                Some(self.render_type_name(id, &mut visiting))
            })
            .collect::<Vec<_>>();

        for (index, name) in names.into_iter().enumerate() {
            if let Some(name) = name
                && let Some(TypeEntry::Resolved(info)) = self.entries.get_mut(index)
            {
                info.name = name;
            }
        }

        drop(names_phase);
        let _phase = crate::span!("types.identities");
        self.assign_identities();
    }

    /// The finished graph, once [`Self::finalize_type_graph`] has run.
    pub(super) fn finish(self) -> BuiltTypes {
        BuiltTypes {
            entries: self.entries.into_flat(),
            dynamic_record_layouts: self.dynamic_record_layouts,
            complex_parts: self.complex_parts,
            go_dict_indices: self.go_dict_indices.into_flat(),
            passed_by_value: self.passed_by_value.into_flat(),
            image: self.image,
            pending_arguments: self.pending_arguments,
        }
    }

    /// The type a built pointer type points to.
    pub(super) fn pointee(&self, pointer: TypeId) -> Option<TypeId> {
        match self.entries.get(pointer.index()) {
            Some(TypeEntry::Resolved(TypeInfo {
                kind:
                    TypeKind::Pointer {
                        target: Some(target),
                        address_class: 0,
                    },
                ..
            })) => Some(target.id),
            _ => None,
        }
    }

    /// Gives each complex type's real and imaginary parts a float type:
    /// the program's own float of that name and size, or one made for it.
    fn add_complex_parts(&mut self) {
        let mut parts = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let TypeEntry::Resolved(TypeInfo {
                kind: TypeKind::Base(base),
                ..
            }) = entry
            else {
                continue;
            };

            match base.encoding {
                BaseTypeEncoding::Floating => {
                    let id = TypeId::new(super::row(index));
                    self.complex_parts
                        .entry((Arc::clone(&base.base_name), base.byte_size))
                        .or_insert(id);
                }
                BaseTypeEncoding::ComplexFloating => parts.push(complex_part(base)),
                _ => {}
            }
        }

        for part in parts {
            let key = (Arc::clone(&part.base_name), part.byte_size);
            let Some(id) = self.next_id() else {
                continue;
            };

            if self.complex_parts.contains_key(&key)
                || self.budget.charge("types", size_of::<TypeEntry>()).is_err()
            {
                continue;
            }

            let reference = TypeReference {
                image: self.image,
                id,
            };

            self.entries.push(resolved(
                reference,
                Arc::clone(&part.name),
                Some(part.byte_size),
                TypeKind::Base(part),
            ));
            self.explicit_names.insert(reference.id.index());
            self.complex_parts.insert(key, reference.id);
        }
    }

    fn reject_inline_storage_cycles(&mut self) {
        let description: Arc<str> = "type graph contains an inline-storage cycle".into();
        for node in inline_storage_cycle_nodes(self.entries.flat()) {
            self.entries[node] = TypeEntry::Malformed(Arc::clone(&description));
        }
    }

    /// Renders a type's source-style name from its targets' names. Cycles
    /// and chains deeper than the resolution limit, which only malformed
    /// metadata builds, render as `<type #N>` instead of recursing without
    /// bound on the controller thread's stack.
    fn render_type_name(&self, id: TypeId, visiting: &mut HashSet<TypeId>) -> Arc<str> {
        if visiting.len() >= MAX_TYPE_RESOLUTION_DEPTH || !visiting.insert(id) {
            return Arc::from(format!("<type #{}>", id.get()));
        }

        let Some(TypeEntry::Resolved(info)) = self.entries.get(id.index()) else {
            visiting.remove(&id);
            return Arc::from(format!("<type #{}>", id.get()));
        };

        if self.explicit_names.contains(id.index()) {
            visiting.remove(&id);
            return Arc::clone(&info.name);
        }

        let rendered = match &info.kind {
            TypeKind::Pointer { .. }
            | TypeKind::Reference { .. }
            | TypeKind::Array { .. }
            | TypeKind::Signature { .. } => {
                // `declare` guards against cycles itself.
                visiting.remove(&id);
                let declared = self.declare(id, String::new(), visiting);
                visiting.insert(id);
                Arc::from(declared)
            }

            // A qualified pointer qualifies its declarator, as in
            // `int (* const)(int)`.
            TypeKind::Modified { modifier, target }
                if modifier_keyword(*modifier).is_some()
                    && self.modified_target_is_indirection(target.id) =>
            {
                visiting.remove(&id);
                let declared = self.declare(id, String::new(), visiting);
                visiting.insert(id);
                Arc::from(declared)
            }
            TypeKind::Modified { modifier, target } => {
                let target_name = self.render_type_name(target.id, visiting);
                let target_is_indirection = self.modified_target_is_indirection(target.id);
                Arc::from(modifier_type_name(
                    *modifier,
                    &target_name,
                    target_is_indirection,
                ))
            }

            TypeKind::Named {
                target: Some(target),
                ..
            } => self.render_type_name(target.id, visiting),
            _ => Arc::clone(&info.name),
        };

        visiting.remove(&id);
        rendered
    }

    fn modified_target_is_indirection(&self, start: TypeId) -> bool {
        let mut current = start;
        let mut visited = Seen::default();
        while visited.insert(current) {
            let Some(TypeEntry::Resolved(info)) = self.entries.get(current.index()) else {
                return false;
            };

            match info.kind {
                TypeKind::Pointer { .. } | TypeKind::Reference { .. } => return true,
                TypeKind::Modified { target, .. } => current = target.id,
                _ => return false,
            }
        }

        false
    }

    /// Renders a type as C declares it around `inner`, the declarator
    /// built so far: `int (*)(int)` is a pointer to `int (int)`, whose
    /// parameter list binds tighter than its `*`.
    fn declare(&self, id: TypeId, inner: String, visiting: &mut HashSet<TypeId>) -> String {
        let join = |base: &str, inner: String| {
            if inner.is_empty() {
                base.to_owned()
            } else if inner.starts_with('[') {
                format!("{base}{inner}")
            } else {
                format!("{base} {inner}")
            }
        };

        let info = match self.entries.get(id.index()) {
            Some(TypeEntry::Resolved(
                info @ TypeInfo {
                    kind:
                        TypeKind::Pointer { .. }
                        | TypeKind::Reference { .. }
                        | TypeKind::Array { .. }
                        | TypeKind::Signature { .. },
                    ..
                },
            )) if !self.explicit_names.contains(id.index()) => info,
            Some(TypeEntry::Resolved(
                info @ TypeInfo {
                    kind: TypeKind::Modified { modifier, target },
                    ..
                },
            )) if !self.explicit_names.contains(id.index())
                && modifier_keyword(*modifier).is_some()
                && self.modified_target_is_indirection(target.id) =>
            {
                info
            }
            _ => return join(&self.render_type_name(id, visiting), inner),
        };

        if visiting.len() >= MAX_TYPE_RESOLUTION_DEPTH || !visiting.insert(id) {
            return join(&format!("<type #{}>", id.get()), inner);
        }

        let declared = match &info.kind {
            TypeKind::Pointer { target: None, .. } => join("void", format!("*{inner}")),
            TypeKind::Pointer {
                target: Some(target),
                ..
            } => self.declare_indirection(target.id, "*", &inner, visiting),
            TypeKind::Reference { kind, target, .. } => {
                let symbol = if *kind == ReferenceKind::Lvalue {
                    "&"
                } else {
                    "&&"
                };

                self.declare_indirection(target.id, symbol, &inner, visiting)
            }

            TypeKind::Array {
                element,
                dimensions,
                ..
            } => {
                use std::fmt::Write;
                let mut inner = inner;
                for dimension in dimensions.iter() {
                    let _ = write!(inner, "[{}]", dimension.count);
                }

                self.declare(element.id, inner, visiting)
            }

            TypeKind::Signature {
                returns,
                parameters,
                variadic,
                prototyped,
            } => {
                let mut list = parameters
                    .iter()
                    .map(|parameter| self.render_type_name(parameter.id, visiting).to_string())
                    .collect::<Vec<_>>();

                // An unprototyped C function's `()` says nothing of its
                // parameters, though producers describe them as variadic.
                let unprototyped = list.is_empty() && !*prototyped;
                if *variadic && !unprototyped {
                    list.push("...".to_owned());
                } else if list.is_empty() && *prototyped {
                    list.push("void".to_owned());
                }

                let inner = format!("{inner}({})", list.join(", "));

                match returns {
                    Some(returns) => self.declare(returns.id, inner, visiting),
                    None => join("void", inner),
                }
            }
            TypeKind::Modified { modifier, target } => match modifier_keyword(*modifier) {
                Some(keyword) => {
                    let inner = if inner.starts_with(['*', '&']) {
                        format!(" {keyword} {inner}")
                    } else {
                        format!(" {keyword}{inner}")
                    };

                    self.declare(target.id, inner, visiting)
                }
                None => join(&format!("<type #{}>", id.get()), inner),
            },
            _ => join(&format!("<type #{}>", id.get()), inner),
        };

        visiting.remove(&id);
        declared
    }

    /// Declares a pointer or reference to `target`, parenthesized where
    /// the target's own declarator binds tighter, as an array's or a
    /// function's does.
    fn declare_indirection(
        &self,
        target: TypeId,
        symbol: &str,
        inner: &str,
        visiting: &mut HashSet<TypeId>,
    ) -> String {
        let binds_tighter = !self.explicit_names.contains(target.index())
            && matches!(
                self.entries.get(target.index()),
                Some(TypeEntry::Resolved(TypeInfo {
                    kind: TypeKind::Array { .. } | TypeKind::Signature { .. },
                    ..
                }))
            );
        let inner = if binds_tighter {
            format!("({symbol}{inner})")
        } else {
            format!("{symbol}{inner}")
        };

        self.declare(target, inner, visiting)
    }

    fn build_pointer_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        byte_size: Option<u64>,
        address_class: u64,
    ) -> Built {
        let target = self.target(entry, unit_index)?;
        let name = explicit_name.unwrap_or_else(|| {
            target.map_or_else(
                || Arc::from("void *"),
                |target| Arc::from(format!("{} *", self.target_name(target))),
            )
        });

        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Pointer {
                target,
                address_class,
            },
        ))
    }

    fn build_reference_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        byte_size: Option<u64>,
        address_class: u64,
    ) -> Built {
        let target = self
            .target(entry, unit_index)?
            .ok_or("reference type has no target")?;
        let (kind, suffix) = if entry.tag() == gimli::DW_TAG_reference_type {
            (ReferenceKind::Lvalue, "&")
        } else {
            (ReferenceKind::Rvalue, "&&")
        };

        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{} {suffix}", self.target_name(target))));

        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Reference {
                kind,
                target,
                address_class,
            },
        ))
    }

    fn build_wrapper_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let target = self.target(entry, unit_index)?;

        // An absent target means `void`, except on a typedef declaration,
        // which is incomplete.
        let declaration = strict_flag(entry, gimli::DW_AT_declaration).unwrap_or(false);
        let target = match target {
            None if !declaration => Some(self.void_type()),
            target => target,
        };

        let byte_size =
            explicit_size.or_else(|| target.and_then(|target| self.byte_size_of(target.id)));
        if matches!(
            entry.tag(),
            gimli::DW_TAG_typedef | gimli::DW_TAG_template_alias
        ) {
            let name = explicit_name.unwrap_or_else(|| {
                target.map_or_else(
                    || Arc::from("<incomplete named type>"),
                    |target| self.target_name(target),
                )
            });
            let relationship = named_type_relationship(entry.tag(), self.language(unit_index));
            if let Some(index) = entry
                .attr_value(DW_AT_GO_DICT_INDEX)
                .and_then(|value| value.udata_value())
            {
                self.go_dict_indices.insert(reference.id, index);
            }

            return Ok(resolved(
                reference,
                name,
                byte_size,
                TypeKind::Named {
                    target,
                    relationship,
                },
            ));
        }

        // Only a declaration keeps an absent target; a qualifier cannot be one.
        let target = target.ok_or("type modifier has no target")?;
        let Some(modifier) = type_modifier(entry.tag()) else {
            return Err("type modifier has no modifier kind".into());
        };

        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("{modifier:?} {}", self.target_name(target))));

        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Modified { modifier, target },
        ))
    }

    fn has_direct_variant_part(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<bool, Arc<str>> {
        let mut found = false;
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            if child.tag() == gimli::DW_TAG_variant_part {
                if found {
                    return Err("aggregate contains multiple direct variant parts".into());
                }

                found = true;
            }
        }

        Ok(found)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Zig tagged-union recognition and remapping form one fail-closed structural validation"
    )]
    fn normalize_zig_tagged_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<std::result::Result<TypeKind, Arc<str>>> {
        if incomplete || !self.is_zig(unit_index) || members.len() != 2 {
            return None;
        }

        let payload_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("payload"))?;
        let tag_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("tag"))?;
        let payload = &members[payload_index];
        let tag = &members[tag_index];
        let payload_info = self.entries.get(payload.type_ref.id.index())?;
        let tag_info = self.entries.get(tag.type_ref.id.index())?;
        let (
            TypeEntry::Resolved(TypeInfo {
                name: payload_name,
                kind:
                    TypeKind::Union {
                        members: payload_members,
                        incomplete: false,
                    },
                ..
            }),
            TypeEntry::Resolved(TypeInfo {
                name: tag_name,
                kind:
                    TypeKind::Enumeration {
                        enumerators,
                        origin: EnumerationOrigin::Language,
                        ..
                    },
                ..
            }),
        ) = (payload_info, tag_info)
        else {
            return None;
        };

        if !payload_name.ends_with(":Payload")
            || !tag_name.starts_with("@typeInfo(")
            || !tag_name.contains(".@\"union\".tag_type")
        {
            return None;
        }

        let payload_members = Arc::clone(payload_members);
        let enumerators = Arc::clone(enumerators);
        let payload_offset = match payload.layout {
            RecordMemberLayout::ByteOffset(offset) => offset,
            RecordMemberLayout::BitRange { .. } | RecordMemberLayout::Runtime => {
                return Some(Err(
                    "Zig tagged-union payload has a non-byte-static location".into(),
                ));
            }
        };

        let mut variants = Vec::with_capacity(enumerators.len());

        for enumerator in enumerators.iter() {
            let matching = payload_members
                .iter()
                .enumerate()
                .filter(|(_, member)| member.name.as_deref() == Some(enumerator.name.as_ref()))
                .collect::<Vec<_>>();
            let [(payload_member_index, payload_member)] = matching.as_slice() else {
                return Some(Err(format!(
                    "Zig tagged-union arm '{}' does not map uniquely to its payload union",
                    enumerator.name
                )
                .into()));
            };

            let payload_type = self
                .entries
                .get(payload_member.type_ref.id.index())
                .and_then(|entry| match entry {
                    TypeEntry::Resolved(info) => Some(&info.kind),
                    TypeEntry::Building | TypeEntry::Malformed(_) => None,
                });
            let variant_members = if matches!(payload_type, Some(TypeKind::Unspecified)) {
                Arc::from([])
            } else {
                let mut member = (*payload_member).clone();
                member.layout = match member.layout {
                    RecordMemberLayout::ByteOffset(offset) => {
                        let Some(offset) = payload_offset.checked_add(offset) else {
                            return Some(Err(
                                "Zig tagged-union payload member offset overflows".into()
                            ));
                        };

                        RecordMemberLayout::ByteOffset(offset)
                    }

                    RecordMemberLayout::BitRange {
                        bit_offset,
                        bit_size,
                    } => {
                        let Some(bit_offset) = payload_offset
                            .checked_mul(8)
                            .and_then(|offset| offset.checked_add(bit_offset))
                        else {
                            return Some(Err(
                                "Zig tagged-union payload bit offset overflows".into()
                            ));
                        };

                        RecordMemberLayout::BitRange {
                            bit_offset,
                            bit_size,
                        }
                    }
                    RecordMemberLayout::Runtime => {
                        return Some(Err(
                            "Zig tagged-union payload member has a runtime location".into(),
                        ));
                    }
                };

                if let Some(metadata) = self.record_member_declarations.iter().find(|metadata| {
                    metadata.aggregate == payload.type_ref.id
                        && matches!(
                            metadata.member,
                            AggregateMemberPath::Direct(index)
                                if index == *payload_member_index
                        )
                }) {
                    self.record_member_declarations
                        .push(AggregateMemberDeclaration {
                            aggregate,
                            member: AggregateMemberPath::Variant {
                                variant: variants.len(),
                                member: 0,
                            },
                            die: metadata.die,
                        });
                }

                Arc::from([member])
            };

            variants.push(Variant {
                name: Some(Arc::clone(&enumerator.name)),
                selection: VariantSelection::Selectors(Arc::from([VariantSelector::Value(
                    enumerator.value,
                )])),
                members: variant_members,
            });
        }

        for metadata in &mut self.record_member_declarations {
            if metadata.aggregate == aggregate
                && matches!(
                    metadata.member,
                    AggregateMemberPath::Direct(index) if index == tag_index
                )
            {
                metadata.member = AggregateMemberPath::Discriminant;
            }
        }

        self.record_member_declarations.retain(|metadata| {
            metadata.aggregate != aggregate
                || !matches!(
                    metadata.member,
                    AggregateMemberPath::Direct(index) if index == payload_index
                )
        });

        Some(Ok(TypeKind::Variant {
            storage: VariantStorageKind::Struct,
            common_members: Arc::from([]),
            bases: Arc::from([]),
            discriminant: Box::new(VariantDiscriminant::Stored(tag.clone())),
            variants: variants.into(),
            incomplete: false,
        }))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Zig optional/error-union recognition remaps layout, declarations, and dynamic expressions atomically"
    )]
    fn normalize_zig_optional_or_error_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        name: &str,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<std::result::Result<TypeKind, Arc<str>>> {
        if incomplete || !self.is_zig(unit_index) || members.len() != 2 {
            return None;
        }

        let payload_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("payload"))?;
        let optional_payload = zig_optional_payload_name(name);
        let error_union_types = zig_error_union_type_names(name);
        let (discriminant_index, variants, duplicate_discriminant_member) =
            if let Some(expected_payload) = optional_payload {
                if self.target_name(members[payload_index].type_ref).as_ref() != expected_payload {
                    return None;
                }

                let some_index = members
                    .iter()
                    .position(|member| member.name.as_deref() == Some("some"))?;
                let some_type = self
                    .resolved_integer_base(members[some_index].type_ref.id)
                    .ok()?;
                if some_type.byte_size != 1
                    || !matches!(
                        some_type.encoding,
                        BaseTypeEncoding::Boolean
                            | BaseTypeEncoding::Unsigned
                            | BaseTypeEncoding::UnsignedCharacter
                    )
                {
                    return None;
                }

                (
                    some_index,
                    vec![
                        Variant {
                            name: Some("null".into()),
                            selection: VariantSelection::Selectors(Arc::from([
                                VariantSelector::Value(IntegerValue::Unsigned(0)),
                            ])),
                            members: Arc::from([]),
                        },
                        Variant {
                            name: Some("some".into()),
                            selection: VariantSelection::Selectors(Arc::from([
                                VariantSelector::Value(IntegerValue::Unsigned(1)),
                            ])),
                            members: Arc::from([members[payload_index].clone()]),
                        },
                    ],
                    false,
                )
            } else if let Some((expected_error, expected_payload)) = error_union_types {
                let error_index = members
                    .iter()
                    .position(|member| member.name.as_deref() == Some("error"))?;
                if self.target_name(members[error_index].type_ref).as_ref() != expected_error
                    || self.target_name(members[payload_index].type_ref).as_ref()
                        != expected_payload
                {
                    return None;
                }

                let error_type = self
                    .resolved_integer_base(members[error_index].type_ref.id)
                    .ok()?;
                if matches!(
                    error_type.encoding,
                    BaseTypeEncoding::Signed
                        | BaseTypeEncoding::SignedCharacter
                        | BaseTypeEncoding::Floating
                        | BaseTypeEncoding::ComplexFloating
                ) {
                    return None;
                }

                (
                    error_index,
                    vec![
                        Variant {
                            name: Some("success".into()),
                            selection: VariantSelection::Selectors(Arc::from([
                                VariantSelector::Value(IntegerValue::Unsigned(0)),
                            ])),
                            members: Arc::from([members[payload_index].clone()]),
                        },
                        Variant {
                            name: Some("error".into()),
                            selection: VariantSelection::Default,
                            members: Arc::from([members[error_index].clone()]),
                        },
                    ],
                    true,
                )
            } else {
                return None;
            };
        if let Err(reason) = validate_variant_selections(&variants) {
            return Some(Err(reason));
        }

        let payload_variant = usize::from(optional_payload.is_some());
        let declaration_snapshot = self.record_member_declarations.clone();

        for metadata in &mut self.record_member_declarations {
            if metadata.aggregate != aggregate {
                continue;
            }

            match metadata.member {
                AggregateMemberPath::Direct(index) if index == discriminant_index => {
                    metadata.member = AggregateMemberPath::Discriminant;
                }
                AggregateMemberPath::Direct(index) if index == payload_index => {
                    metadata.member = AggregateMemberPath::Variant {
                        variant: payload_variant,
                        member: 0,
                    };
                }
                _ => {}
            }
        }

        if duplicate_discriminant_member
            && let Some(metadata) = declaration_snapshot.iter().find(|metadata| {
                metadata.aggregate == aggregate
                    && matches!(
                        metadata.member,
                        AggregateMemberPath::Direct(index) if index == discriminant_index
                    )
            })
        {
            self.record_member_declarations
                .push(AggregateMemberDeclaration {
                    aggregate,
                    member: AggregateMemberPath::Variant {
                        variant: 1,
                        member: 0,
                    },
                    die: metadata.die,
                });
        }

        for (index, child) in [
            (
                payload_index,
                DynamicAggregateChild::VariantMember {
                    variant: payload_variant,
                    member: 0,
                },
            ),
            (discriminant_index, DynamicAggregateChild::Discriminant),
        ] {
            if let Some(expression) =
                self.dynamic_record_layouts
                    .remove(&DynamicAggregateLayoutKey {
                        aggregate,
                        child: DynamicAggregateChild::Member(index),
                    })
            {
                self.dynamic_record_layouts
                    .insert(DynamicAggregateLayoutKey { aggregate, child }, expression);
            }
        }

        if duplicate_discriminant_member
            && let Some(expression) = self
                .dynamic_record_layouts
                .get(&DynamicAggregateLayoutKey {
                    aggregate,
                    child: DynamicAggregateChild::Discriminant,
                })
                .copied()
        {
            self.dynamic_record_layouts.insert(
                DynamicAggregateLayoutKey {
                    aggregate,
                    child: DynamicAggregateChild::VariantMember {
                        variant: 1,
                        member: 0,
                    },
                },
                expression,
            );
        }

        Some(Ok(TypeKind::Variant {
            storage: VariantStorageKind::Struct,
            common_members: Arc::from([]),
            bases: Arc::from([]),
            discriminant: Box::new(VariantDiscriminant::Stored(
                members[discriminant_index].clone(),
            )),
            variants: variants.into(),
            incomplete: false,
        }))
    }

    /// Builds one member of the aggregate `aggregate`, recording its
    /// declaration and any location computed at run time.
    #[expect(
        clippy::too_many_arguments,
        reason = "a member keeps its owner, location identity, access, and declaration provenance"
    )]
    fn build_member(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        dynamic_child: DynamicAggregateChild,
        declaration: AggregateMemberPath,
        absent_byte_offset: Option<u64>,
        record_kind: RecordKind,
        what: &str,
    ) -> std::result::Result<RecordMember, Arc<str>> {
        let units: &'a Units<'data> = self.units;
        let chain = origin_chain(units, unit_index, child).map_err(malformed)?;
        let (owner, value) = type_with_origins(unit_index, child, &chain);
        let target = self
            .type_reference(owner, value)?
            .ok_or_else(|| Arc::from(format!("{what} has no type")))?;
        let name = string_with_origins(
            self.dwarf,
            units,
            &units[unit_index],
            child,
            &chain,
            gimli::DW_AT_name,
        )
        .map_err(malformed)?;
        let layout = self.record_member_layout(child, target, absent_byte_offset)?;
        if layout == RecordMemberLayout::Runtime {
            self.record_dynamic_layout(child, unit_index, aggregate, dynamic_child)?;
        }

        let member = RecordMember {
            name,
            type_ref: target,
            layout,
            accessibility: Self::record_accessibility(child, record_kind)?,
            artificial: strict_flag(child, gimli::DW_AT_artificial)?,
            embedded: go_embedded(child),
            declaration: None,
        };

        self.record_member_declarations
            .push(AggregateMemberDeclaration {
                aggregate,
                member: declaration,
                die: DieKey {
                    unit: unit_index,
                    offset: child.offset().0,
                },
            });

        Ok(member)
    }

    /// Builds the `index`th base class of the aggregate `aggregate`.
    fn build_base(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        index: usize,
        record_kind: RecordKind,
        what: &str,
    ) -> std::result::Result<BaseClass, Arc<str>> {
        let type_ref = self
            .target(child, unit_index)?
            .ok_or_else(|| Arc::from(format!("{what} has no type")))?;
        let layout = Self::record_byte_layout(child);
        if layout == RecordMemberLayout::Runtime {
            self.record_dynamic_layout(
                child,
                unit_index,
                aggregate,
                DynamicAggregateChild::Base(index),
            )?;
        }

        let virtuality = match child.attr_value(gimli::DW_AT_virtuality) {
            None | Some(gimli::AttributeValue::Virtuality(gimli::DW_VIRTUALITY_none)) => {
                BaseClassVirtuality::None
            }

            Some(gimli::AttributeValue::Virtuality(value))
                if value == gimli::DW_VIRTUALITY_virtual
                    || value == gimli::DW_VIRTUALITY_pure_virtual =>
            {
                BaseClassVirtuality::Virtual
            }
            _ => return Err("base-class virtuality has an invalid encoding".into()),
        };

        Ok(BaseClass {
            type_ref,
            layout,
            accessibility: Self::record_accessibility(child, record_kind)?,
            virtuality,
        })
    }

    /// Records the location expression of a member whose offset is computed
    /// at run time.
    fn record_dynamic_layout(
        &mut self,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        dynamic_child: DynamicAggregateChild,
    ) -> std::result::Result<(), Arc<str>> {
        let Some(expression) = child
            .attr_value(gimli::DW_AT_data_member_location)
            .and_then(|value| value.exprloc_value())
        else {
            return Ok(());
        };

        let unit = &self.units[unit_index];
        let expression = copy_expression(
            self.dwarf,
            &mut self.pool.lock().unwrap_or_else(PoisonError::into_inner),
            unit_index,
            unit,
            expression,
        )
        .map_err(malformed)?;
        self.dynamic_record_layouts.insert(
            DynamicAggregateLayoutKey {
                aggregate,
                child: dynamic_child,
            },
            expression,
        );

        Ok(())
    }

    fn build_variant_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        storage: VariantStorageKind,
    ) -> Built {
        let incomplete = strict_flag(entry, gimli::DW_AT_declaration)?;
        if !incomplete && explicit_size.is_none() {
            return Err("complete variant aggregate has no byte size".into());
        }

        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("<anonymous variant@0x{:x}>", entry.offset().0)));
        let limit = || {
            opaque(
                reference,
                Arc::clone(&name),
                explicit_size,
                "variant metadata exceeds its resource limit",
            )
        };

        let mut budget = VariantMetadataBudget::default();
        let record_kind = if storage == VariantStorageKind::Class {
            RecordKind::Class
        } else {
            RecordKind::Struct
        };

        let mut common_members = Vec::new();
        let mut bases = Vec::new();
        let mut part = None;
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            match child.tag() {
                gimli::DW_TAG_member => {
                    if budget.consume().is_err() {
                        return Ok(limit());
                    }

                    let index = common_members.len();
                    common_members.push(self.build_member(
                        child,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        AggregateMemberPath::Direct(index),
                        (storage == VariantStorageKind::Union).then_some(0),
                        record_kind,
                        "variant component",
                    )?);
                }
                gimli::DW_TAG_inheritance if storage != VariantStorageKind::Union => {
                    if budget.consume().is_err() {
                        return Ok(limit());
                    }

                    bases.push(self.build_base(
                        child,
                        unit_index,
                        reference.id,
                        bases.len(),
                        record_kind,
                        "variant base class",
                    )?);
                }
                gimli::DW_TAG_variant_part => {
                    if part.is_some() {
                        return Err("variant aggregate contains multiple variant parts".into());
                    }

                    part = match self.variant_part(
                        child,
                        unit_index,
                        reference.id,
                        record_kind,
                        &mut budget,
                    ) {
                        Ok(part) => Some(part),
                        Err(VariantMetadataError::Malformed(reason)) => return Err(reason),
                        Err(VariantMetadataError::Limit) => return Ok(limit()),
                    };
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        format!("variant aggregate contains unsupported direct child {tag:?}"),
                    ));
                }
            }
        }

        let (discriminant, variants) = part.ok_or("variant aggregate has no variant part")?;

        Ok(resolved(
            reference,
            name,
            explicit_size,
            TypeKind::Variant {
                storage,
                common_members: common_members.into(),
                bases: bases.into(),
                discriminant: Box::new(discriminant),
                variants: variants.into(),
                incomplete,
            },
        ))
    }

    /// A variant part's discriminant and variants.
    #[expect(
        clippy::too_many_lines,
        reason = "a variant part's discriminator and arms validate one nested DIE contract"
    )]
    fn variant_part(
        &mut self,
        part: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        aggregate: TypeId,
        record_kind: RecordKind,
        budget: &mut VariantMetadataBudget,
    ) -> std::result::Result<(VariantDiscriminant, Vec<Variant>), VariantMetadataError> {
        let discriminator = die_reference_with_signatures(
            part.attr_value(gimli::DW_AT_discr),
            unit_index,
            self.units,
            self.type_signatures,
        )
        .map_err(malformed)?;
        let tag_type = self.target(part, unit_index)?;
        let discriminant = if let Some(discriminator) = discriminator {
            let mut stored = None;
            let mut children = self.children(unit_index, part.offset())?;
            while let Some(child) = children.next_child()? {
                if child.offset().0 != discriminator.offset || discriminator.unit != unit_index {
                    continue;
                }

                if child.tag() != gimli::DW_TAG_member {
                    return Err(VariantMetadataError::Malformed(
                        "DW_AT_discr does not reference a member child".into(),
                    ));
                }

                budget.consume()?;
                stored = Some(self.build_member(
                    child,
                    unit_index,
                    aggregate,
                    DynamicAggregateChild::Discriminant,
                    AggregateMemberPath::Discriminant,
                    None,
                    record_kind,
                    "variant component",
                )?);
            }

            let stored = stored
                .ok_or_else(|| Arc::from("DW_AT_discr references a non-child discriminator"))?;
            if let Some(tag_type) = tag_type
                && tag_type.id != stored.type_ref.id
            {
                return Err(VariantMetadataError::Malformed(
                    "variant tag type differs from its discriminator member".into(),
                ));
            }

            VariantDiscriminant::Stored(stored)
        } else {
            tag_type.map_or(VariantDiscriminant::Absent, VariantDiscriminant::TagType)
        };

        let representation = match &discriminant {
            VariantDiscriminant::Stored(member) => Some(member.type_ref.id),
            VariantDiscriminant::TagType(tag_type) => Some(tag_type.id),
            VariantDiscriminant::Absent => None,
        }
        .map(|id| self.resolved_integer_base(id))
        .transpose()?;

        let unit = &self.units[unit_index];
        let mut variants = Vec::new();
        let mut variant_entries = self.children(unit_index, part.offset())?;

        while let Some(variant) = variant_entries.next_child()? {
            if variant.tag() == gimli::DW_TAG_member {
                continue;
            }

            if variant.tag() != gimli::DW_TAG_variant {
                return Err(VariantMetadataError::Malformed(
                    format!(
                        "variant part contains unsupported direct child {:?}",
                        variant.tag()
                    )
                    .into(),
                ));
            }

            budget.consume()?;
            let selection = match &representation {
                Some(representation) => {
                    copy_variant_selection(variant, representation, self.byte_order, budget)?
                }

                None if variant.attr_value(gimli::DW_AT_discr_value).is_some()
                    || variant.attr_value(gimli::DW_AT_discr_list).is_some() =>
                {
                    return Err(VariantMetadataError::Malformed(
                        "a variant selects a discriminant its part does not have".into(),
                    ));
                }
                None => VariantSelection::Default,
            };

            let name = copy_name(self.dwarf, unit, variant).map_err(malformed)?;
            let variant_index = variants.len();
            let mut members = Vec::new();
            let mut member_entries = self.children(unit_index, variant.offset())?;

            while let Some(member) = member_entries.next_child()? {
                if member.tag() != gimli::DW_TAG_member {
                    return Err(VariantMetadataError::Malformed(
                        format!("variant contains unsupported component {:?}", member.tag()).into(),
                    ));
                }

                budget.consume()?;
                let member_index = members.len();
                members.push(self.build_member(
                    member,
                    unit_index,
                    aggregate,
                    DynamicAggregateChild::VariantMember {
                        variant: variant_index,
                        member: member_index,
                    },
                    AggregateMemberPath::Variant {
                        variant: variant_index,
                        member: member_index,
                    },
                    None,
                    record_kind,
                    "variant component",
                )?);
            }

            variants.push(Variant {
                name,
                selection,
                members: members.into(),
            });
        }

        // A sum with no stored tag selects among its variants by which can
        // hold a value, not by selectors; one with none holds no value.
        if !matches!(discriminant, VariantDiscriminant::Absent) {
            if variants.is_empty() {
                return Err(VariantMetadataError::Malformed(
                    "variant part has no variants".into(),
                ));
            }

            validate_variant_selections(&variants)?;
        }

        Ok((discriminant, variants))
    }

    /// Records whether calls pass a C++ class by value, when its producer
    /// says.
    fn note_calling_convention(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        id: TypeId,
    ) {
        match entry.attr_value(gimli::DW_AT_calling_convention) {
            Some(gimli::AttributeValue::CallingConvention(gimli::DW_CC_pass_by_value)) => {
                self.passed_by_value.insert(id, true);
            }
            Some(gimli::AttributeValue::CallingConvention(gimli::DW_CC_pass_by_reference)) => {
                self.passed_by_value.insert(id, false);
            }
            _ => {}
        }
    }

    /// The size of a Zig packed struct that Zig's own backend describes by
    /// the integer that backs it.
    fn packed_size(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> std::result::Result<Option<u64>, Arc<str>> {
        if self.language(unit_index) != SourceLanguage::Zig {
            return Ok(None);
        }

        Ok(self
            .target(entry, unit_index)?
            .and_then(|backing| self.byte_size_of(backing.id)))
    }

    fn build_record_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let kind = if entry.tag() == gimli::DW_TAG_class_type {
            RecordKind::Class
        } else {
            RecordKind::Struct
        };

        let incomplete = strict_flag(entry, gimli::DW_AT_declaration)?;
        let explicit_size = match explicit_size {
            None if !incomplete => self.packed_size(entry, unit_index)?,
            size => size,
        };

        if !incomplete && explicit_size.is_none() {
            return Err("complete record type has no byte size".into());
        }

        self.note_calling_convention(entry, reference.id);
        if self.has_direct_variant_part(entry, unit_index)? {
            let storage = if kind == RecordKind::Class {
                VariantStorageKind::Class
            } else {
                VariantStorageKind::Struct
            };

            return self.build_variant_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                explicit_size,
                storage,
            );
        }

        let name = explicit_name.unwrap_or_else(|| {
            Arc::from(format!("<anonymous {:?}@0x{:x}>", kind, entry.offset().0))
        });
        let mut members = Vec::new();
        let mut bases = Vec::new();
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            match child.tag() {
                // A DWARF 4 static data member is a declaration with no bytes
                // in an instance.
                gimli::DW_TAG_member
                    if strict_flag(child, gimli::DW_AT_declaration).unwrap_or(false) => {}
                gimli::DW_TAG_member | gimli::DW_TAG_inheritance
                    if members.len().saturating_add(bases.len()) >= MAX_RECORD_CHILDREN =>
                {
                    return Err("record child count exceeds its limit".into());
                }
                gimli::DW_TAG_member => {
                    let index = members.len();
                    members.push(self.build_member(
                        child,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        AggregateMemberPath::Direct(index),
                        None,
                        kind,
                        "record member",
                    )?);
                }
                gimli::DW_TAG_inheritance => {
                    bases.push(self.build_base(
                        child,
                        unit_index,
                        reference.id,
                        bases.len(),
                        kind,
                        "base class",
                    )?);
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        format!("record contains unsupported direct child {tag:?}"),
                    ));
                }
            }
        }

        let normalized = self
            .normalize_zig_optional_or_error_union(
                unit_index,
                reference.id,
                &name,
                &members,
                incomplete,
            )
            .or_else(|| {
                self.normalize_zig_tagged_union(unit_index, reference.id, &members, incomplete)
            });
        let kind = match normalized {
            Some(kind) => kind?,
            None => TypeKind::Record {
                kind,
                members: members.into(),
                bases: bases.into(),
                incomplete,
            },
        };

        Ok(resolved(reference, name, explicit_size, kind))
    }

    /// A self-hosted Zig optional pointer, `?*T` or `?[*]T`, as the pointer
    /// it is: a union of a pointer's size whose discriminant is the
    /// pointer's own bits, `null` when they are 0, and whose other variant
    /// is the pointer, as the LLVM backend describes it. Any other type is
    /// itself.
    fn zig_nullable_pointer(&self, info: TypeInfo) -> TypeInfo {
        let info_of = |reference: TypeReference| match self.entries.get(reference.id.index()) {
            Some(TypeEntry::Resolved(info)) => Some(info),
            _ => None,
        };

        let TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            ..
        } = &info.kind
        else {
            return info;
        };

        let VariantDiscriminant::Stored(discriminant) = discriminant.as_ref() else {
            return info;
        };

        let whole = |member: &RecordMember| {
            member.layout == RecordMemberLayout::ByteOffset(0)
                && info_of(member.type_ref).and_then(|member| member.byte_size) == info.byte_size
        };

        let pointer = match variants.as_ref() {
            [null, some]
                if info.name.starts_with('?')
                    && common_members.is_empty()
                    && bases.is_empty()
                    && whole(discriminant)
                    && null.selection
                        == VariantSelection::Selectors(Arc::from([VariantSelector::Value(
                            IntegerValue::Unsigned(0),
                        )]))
                    && some.selection == VariantSelection::Default =>
            {
                match some.members.as_ref() {
                    [payload] if whole(payload) => info_of(payload.type_ref)
                        .filter(|payload| matches!(payload.kind, TypeKind::Pointer { .. })),
                    _ => None,
                }
            }
            _ => None,
        };

        match pointer {
            Some(pointer) => TypeInfo {
                kind: pointer.kind.clone(),
                ..info
            },
            None => info,
        }
    }

    fn build_union_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let incomplete = strict_flag(entry, gimli::DW_AT_declaration)?;
        if !incomplete && explicit_size.is_none() {
            return Err("complete union type has no byte size".into());
        }

        if self.has_direct_variant_part(entry, unit_index)? {
            let built = self.build_variant_type(
                entry,
                unit_index,
                reference,
                explicit_name,
                explicit_size,
                VariantStorageKind::Union,
            )?;
            return Ok(match built {
                TypeEntry::Resolved(info) if self.is_zig(unit_index) => {
                    TypeEntry::Resolved(self.zig_nullable_pointer(info))
                }
                built => built,
            });
        }

        let name = explicit_name
            .unwrap_or_else(|| Arc::from(format!("<anonymous union@0x{:x}>", entry.offset().0)));
        let mut members = Vec::new();
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            match child.tag() {
                gimli::DW_TAG_member
                    if strict_flag(child, gimli::DW_AT_declaration).unwrap_or(false) => {}
                gimli::DW_TAG_member if members.len() >= MAX_RECORD_CHILDREN => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        "union member count exceeds its resource limit",
                    ));
                }
                gimli::DW_TAG_member => {
                    let index = members.len();
                    members.push(self.build_member(
                        child,
                        unit_index,
                        reference.id,
                        DynamicAggregateChild::Member(index),
                        AggregateMemberPath::Direct(index),
                        Some(0),
                        RecordKind::Struct,
                        "union member",
                    )?);
                }
                tag if is_scope_only_child(tag) => {}
                tag => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        format!("union contains unsupported direct child {tag:?}"),
                    ));
                }
            }
        }

        let kind = self
            .normalize_odin_union(unit_index, reference.id, &members, incomplete)
            .unwrap_or_else(|| TypeKind::Union {
                members: members.into(),
                incomplete,
            });

        Ok(resolved(reference, name, explicit_size, kind))
    }

    /// An Odin union as the variant its tag selects. Odin describes one as
    /// a union of its `tag` and of each variant, named `v` and the tag value
    /// that selects it; a tag no variant is named for, 0 unless the union is
    /// `#no_nil`, is nil. A union without a tag, which holds one pointer
    /// whose null is nil, stays a union.
    fn normalize_odin_union(
        &mut self,
        unit_index: usize,
        aggregate: TypeId,
        members: &[RecordMember],
        incomplete: bool,
    ) -> Option<TypeKind> {
        if incomplete || self.language(unit_index) != SourceLanguage::Odin {
            return None;
        }

        let tag_index = members
            .iter()
            .position(|member| member.name.as_deref() == Some("tag"))?;
        let tag = &members[tag_index];
        let TypeKind::Base(base) = &self.unnamed(tag.type_ref.id)?.kind else {
            return None;
        };

        let signed = match base.encoding {
            BaseTypeEncoding::Signed => true,
            BaseTypeEncoding::Unsigned => false,
            _ => return None,
        };

        let value = |number: u64| {
            if signed {
                IntegerValue::Signed(i128::from(number))
            } else {
                IntegerValue::Unsigned(u128::from(number))
            }
        };

        let mut variants = Vec::with_capacity(members.len());
        let mut tags = Vec::with_capacity(members.len());

        for (index, member) in members.iter().enumerate() {
            if index == tag_index {
                continue;
            }

            let number = member
                .name
                .as_deref()?
                .strip_prefix('v')?
                .parse::<u64>()
                .ok()?;
            if member.layout != RecordMemberLayout::ByteOffset(0) || tags.contains(&number) {
                return None;
            }

            let TypeEntry::Resolved(info) = self.entries.get(member.type_ref.id.index())? else {
                return None;
            };

            tags.push(number);
            variants.push((
                index,
                Variant {
                    name: Some(Arc::clone(&info.name)),
                    selection: VariantSelection::Selectors(Arc::from([VariantSelector::Value(
                        value(number),
                    )])),
                    members: Arc::from([member.clone()]),
                },
            ));
        }

        // Every member's path is found before any is changed, so a union
        // that cannot be normalized keeps its declarations as they were.
        let mut paths = Vec::new();
        for (position, metadata) in self.record_member_declarations.iter().enumerate() {
            if metadata.aggregate != aggregate {
                continue;
            }

            if let AggregateMemberPath::Direct(index) = metadata.member {
                let path = if index == tag_index {
                    AggregateMemberPath::Discriminant
                } else {
                    let variant = variants.iter().position(|(member, _)| *member == index)?;
                    AggregateMemberPath::Variant { variant, member: 0 }
                };

                paths.push((position, path));
            }
        }

        for (position, path) in paths {
            self.record_member_declarations[position].member = path;
        }

        let mut variants = variants
            .into_iter()
            .map(|(_, variant)| variant)
            .collect::<Vec<_>>();
        if !tags.contains(&0) {
            variants.push(Variant {
                name: Some(Arc::from("nil")),
                selection: VariantSelection::Selectors(Arc::from([VariantSelector::Value(value(
                    0,
                ))])),
                members: Arc::from([]),
            });
        }

        Some(TypeKind::Variant {
            storage: VariantStorageKind::Union,
            common_members: Arc::from([]),
            bases: Arc::from([]),
            discriminant: Box::new(VariantDiscriminant::Stored(tag.clone())),
            variants: variants.into(),
            incomplete: false,
        })
    }

    fn record_member_layout(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        target: TypeReference,
        absent_byte_offset: Option<u64>,
    ) -> std::result::Result<RecordMemberLayout, Arc<str>> {
        let bit_size = entry
            .attr(gimli::DW_AT_bit_size)
            .and_then(gimli::Attribute::udata_value);
        if let Some(bit_size) = bit_size {
            if bit_size == 0 {
                return Err("record bit-field has zero width".into());
            }

            if let Some(bit_offset) = entry
                .attr(gimli::DW_AT_data_bit_offset)
                .and_then(gimli::Attribute::udata_value)
            {
                bit_offset
                    .checked_add(bit_size)
                    .ok_or_else(|| Arc::from("record bit-field range overflows"))?;
                return Ok(RecordMemberLayout::BitRange {
                    bit_offset,
                    bit_size,
                });
            }

            if let Some(legacy_offset) = entry
                .attr(gimli::DW_AT_bit_offset)
                .and_then(gimli::Attribute::udata_value)
            {
                let byte_offset = entry
                    .attr(gimli::DW_AT_data_member_location)
                    .and_then(gimli::Attribute::udata_value)
                    .or(absent_byte_offset)
                    .ok_or_else(|| Arc::from("record member location is not a constant"))?;
                let storage_bytes = entry
                    .attr(gimli::DW_AT_byte_size)
                    .and_then(gimli::Attribute::udata_value)
                    .or_else(|| self.byte_size_of(target.id))
                    .ok_or_else(|| Arc::from("legacy bit-field has no storage size"))?;
                let storage_bits = storage_bytes
                    .checked_mul(8)
                    .ok_or_else(|| Arc::from("legacy bit-field storage size overflows"))?;
                let within = match self.byte_order {
                    ByteOrder::Big => legacy_offset,
                    ByteOrder::Little => storage_bits
                        .checked_sub(legacy_offset)
                        .and_then(|value| value.checked_sub(bit_size))
                        .ok_or_else(|| Arc::from("legacy bit-field range exceeds storage"))?,
                };

                let bit_offset = byte_offset
                    .checked_mul(8)
                    .and_then(|value| value.checked_add(within))
                    .ok_or_else(|| Arc::from("legacy bit-field range overflows"))?;
                return Ok(RecordMemberLayout::BitRange {
                    bit_offset,
                    bit_size,
                });
            }

            return Err("bit-field has no bit offset".into());
        }

        // A member placed by its first bit alone, as Zig's own backend
        // places a packed struct's, spans its type's bits.
        if let Some(bit_offset) = entry
            .attr(gimli::DW_AT_data_bit_offset)
            .and_then(gimli::Attribute::udata_value)
        {
            if bit_offset % 8 == 0 {
                return Ok(RecordMemberLayout::ByteOffset(bit_offset / 8));
            }

            let bit_size = match self.entries.get(target.id.index()) {
                Some(TypeEntry::Resolved(TypeInfo {
                    kind: TypeKind::Base(base),
                    ..
                })) => base.bit_size.or_else(|| base.byte_size.checked_mul(8)),
                _ => None,
            }
            .ok_or_else(|| Arc::from("member placed by bit has no bit width"))?;
            bit_offset
                .checked_add(bit_size)
                .ok_or_else(|| Arc::from("record bit-field range overflows"))?;
            return Ok(RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            });
        }

        Ok(entry.attr(gimli::DW_AT_data_member_location).map_or_else(
            || {
                absent_byte_offset
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
            |attribute| {
                constant_member_offset(attribute)
                    .map_or(RecordMemberLayout::Runtime, RecordMemberLayout::ByteOffset)
            },
        ))
    }

    fn build_array_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        self.build_array(
            entry,
            unit_index,
            reference,
            explicit_name,
            explicit_size,
            None,
        )
    }

    /// Whether `entry` is the record GNAT passes an array of an
    /// unconstrained type as: artificial, with only a pointer to the
    /// elements, `P_ARRAY`, first, and one to the bounds, `P_BOUNDS`. Its
    /// array's bounds are found from the record. Returns the array's entry.
    fn ada_fat_pointer(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<gimli::UnitOffset> {
        if self.language(unit_index) != SourceLanguage::Ada
            || entry.attr_value(gimli::DW_AT_artificial) != Some(gimli::AttributeValue::Flag(true))
        {
            return None;
        }

        let unit = &self.units[unit_index];
        let mut children = self.children(unit_index, entry.offset()).ok()?;
        let mut members = Vec::new();

        while let Ok(Some(child)) = children.next_child() {
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }

            let name = child
                .attr_value(gimli::DW_AT_name)
                .and_then(|name| self.dwarf.attr_string(unit, name).ok())?;
            members.push((
                name.slice().to_vec(),
                child
                    .attr(gimli::DW_AT_data_member_location)
                    .and_then(constant_member_offset),
                child.attr_value(gimli::DW_AT_type),
            ));
        }

        let [
            (array, Some(0), Some(gimli::AttributeValue::UnitRef(pointer))),
            (bounds, ..),
        ] = members.as_slice()
        else {
            return None;
        };

        if array.as_slice() != b"P_ARRAY" || bounds.as_slice() != b"P_BOUNDS" {
            return None;
        }

        let pointer = unit.entry(*pointer).ok()?;
        if pointer.tag() != gimli::DW_TAG_pointer_type {
            return None;
        }

        let Some(gimli::AttributeValue::UnitRef(array)) = pointer.attr_value(gimli::DW_AT_type)
        else {
            return None;
        };

        (unit.entry(array).ok()?.tag() == gimli::DW_TAG_array_type).then_some(array)
    }

    /// An array, whose elements are where `data_location` finds them from
    /// the value's place when it is given.
    fn build_array(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        data_location: Option<gimli::Expression<Reader<'data>>>,
    ) -> Built {
        let element = self
            .target(entry, unit_index)?
            .ok_or("array type has no element type")?;
        let name = || {
            explicit_name
                .clone()
                .unwrap_or_else(|| Arc::from("<dynamic array>"))
        };

        // A stride for the whole array, or one in bits, spaces elements as
        // nothing here reads.
        if entry.attr(gimli::DW_AT_byte_stride).is_some()
            || entry.attr(gimli::DW_AT_bit_stride).is_some()
        {
            return Ok(opaque(
                reference,
                name(),
                explicit_size,
                "arrays whose elements a stride of the whole array spaces are unsupported",
            ));
        }

        let default_lower =
            default_lower_bound(self.dwarf_language(unit_index), self.language(unit_index));
        let mut subranges = Vec::new();
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            if child.tag() != gimli::DW_TAG_subrange_type {
                continue;
            }

            match self.read_subrange(unit_index, child, default_lower) {
                Ok(subrange) => subranges.push(subrange),
                Err(description) => {
                    return Ok(opaque(reference, name(), explicit_size, description));
                }
            }
        }

        if subranges.is_empty() {
            return Err("array type has no subrange dimensions".into());
        }

        let ordering = self.array_ordering(entry, unit_index, subranges.len())?;
        let located = [
            gimli::DW_AT_data_location,
            gimli::DW_AT_allocated,
            gimli::DW_AT_associated,
        ]
        .into_iter()
        .any(|attribute| entry.attr(attribute).is_some())
            || data_location.is_some();
        let statically = subranges
            .iter()
            .map(|(lower, extent, stride)| {
                let SubrangeBound::Constant(lower) = *lower else {
                    return None;
                };

                let count = match extent {
                    Extent::Count(SubrangeBound::Constant(count)) => u64::try_from(*count).ok()?,
                    Extent::Upper(SubrangeBound::Constant(upper)) => {
                        u64::try_from(upper.checked_sub(lower)?.checked_add(1)?).ok()?
                    }
                    _ => return None,
                };

                stride.is_none().then_some(ArrayDimension {
                    lower_bound: lower,
                    count,
                })
            })
            .collect::<Option<Vec<_>>>()
            .filter(|_| !located);
        let name =
            explicit_name.unwrap_or_else(|| Arc::from(format!("{}[]", self.target_name(element))));
        let Some(dimensions) = statically else {
            return self.build_runtime_array(
                entry,
                unit_index,
                reference,
                (name, explicit_size),
                element,
                subranges,
                ordering,
                data_location,
            );
        };

        // Producers rarely give a C array a size of its own: it is its
        // elements', laid end to end.
        let byte_size = explicit_size.or_else(|| {
            let element_size = self.byte_size_of(element.id)?;
            dimensions.iter().try_fold(element_size, |size, dimension| {
                size.checked_mul(dimension.count)
            })
        });

        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Array {
                element,
                dimensions: dimensions.into(),
                ordering,
            },
        ))
    }

    /// An array bounded at run time, recording the expressions that find
    /// its bounds and where its elements are.
    #[expect(
        clippy::too_many_arguments,
        reason = "the array's entry, identity, and the subranges already read"
    )]
    fn build_runtime_array(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        (name, explicit_size): (Arc<str>, Option<u64>),
        element: TypeReference,
        subranges: Vec<Subrange<'data>>,
        ordering: ArrayOrdering,
        data_location: Option<gimli::Expression<Reader<'data>>>,
    ) -> Built {
        let mut expressions = data_location
            .map(|expression| (DynamicAggregateChild::DataLocation, expression))
            .into_iter()
            .collect::<Vec<_>>();

        for (attribute, child) in [
            (
                gimli::DW_AT_data_location,
                DynamicAggregateChild::DataLocation,
            ),
            (gimli::DW_AT_allocated, DynamicAggregateChild::Allocated),
            (gimli::DW_AT_associated, DynamicAggregateChild::Associated),
        ] {
            match entry.attr_value(attribute) {
                None => {}
                Some(gimli::AttributeValue::Exprloc(expression)) => {
                    expressions.push((child, expression));
                }

                // An array that says it is always allocated is.
                Some(gimli::AttributeValue::Flag(true))
                    if attribute != gimli::DW_AT_data_location => {}
                Some(_) => {
                    return Ok(opaque(
                        reference,
                        name,
                        explicit_size,
                        "array's place or allocation is not an expression",
                    ));
                }
            }
        }

        let mut bound = |bound: SubrangeBound<'data>, child| match bound {
            SubrangeBound::Constant(value) => ArrayBound::Constant(value),
            SubrangeBound::Computed {
                expression,
                byte_size,
                signed,
            } => {
                expressions.push((child, expression));
                ArrayBound::Computed { byte_size, signed }
            }

            SubrangeBound::Stored {
                expression,
                byte_size,
            } => {
                expressions.push((child, expression));
                ArrayBound::Stored {
                    byte_size,
                    signed: false,
                }
            }
            SubrangeBound::Variable(debug_info_offset) => {
                ArrayBound::Variable { debug_info_offset }
            }
        };

        let dimensions = subranges
            .into_iter()
            .enumerate()
            .map(|(dimension, (lower, extent, stride))| {
                let part = |part| DynamicAggregateChild::Bound { dimension, part };
                RuntimeDimension {
                    lower_bound: bound(lower, part(BoundPart::Lower)),
                    extent: match extent {
                        Extent::Upper(upper) => {
                            ArrayExtent::Upper(bound(upper, part(BoundPart::Extent)))
                        }
                        Extent::Count(count) => {
                            ArrayExtent::Count(bound(count, part(BoundPart::Extent)))
                        }
                        Extent::Unknown => ArrayExtent::Unknown,
                    },
                    byte_stride: stride.map(|stride| bound(stride, part(BoundPart::Stride))),
                }
            })
            .collect::<Arc<[_]>>();
        self.record_array_layouts(unit_index, reference.id, expressions)?;

        Ok(resolved(
            reference,
            name,
            explicit_size,
            TypeKind::RuntimeArray {
                element,
                dimensions,
                ordering,
            },
        ))
    }

    /// Records the expressions that find an array's parts at run time.
    fn record_array_layouts(
        &mut self,
        unit_index: usize,
        array: TypeId,
        expressions: Vec<(DynamicAggregateChild, gimli::Expression<Reader<'data>>)>,
    ) -> std::result::Result<(), Arc<str>> {
        let unit = &self.units[unit_index];
        for (child, expression) in expressions {
            let expression = copy_expression(
                self.dwarf,
                &mut self.pool.lock().unwrap_or_else(PoisonError::into_inner),
                unit_index,
                unit,
                expression,
            )
            .map_err(malformed)?;
            self.dynamic_record_layouts.insert(
                DynamicAggregateLayoutKey {
                    aggregate: array,
                    child,
                },
                expression,
            );
        }

        Ok(())
    }

    /// How an array of `rank` dimensions lays out its elements: row by row,
    /// but Fortran's go column by column unless the array says otherwise.
    fn array_ordering(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        rank: usize,
    ) -> std::result::Result<ArrayOrdering, Arc<str>> {
        let ordering = match entry.attr_value(gimli::DW_AT_ordering) {
            Some(gimli::AttributeValue::Ordering(gimli::DW_ORD_col_major)) => {
                ArrayOrdering::ColumnMajor
            }
            Some(gimli::AttributeValue::Ordering(gimli::DW_ORD_row_major)) => {
                ArrayOrdering::RowMajor
            }
            Some(_) => return Err("array ordering has an invalid encoding".into()),
            None if self.language(unit_index) == SourceLanguage::Fortran => {
                ArrayOrdering::ColumnMajor
            }
            None => ArrayOrdering::RowMajor,
        };

        // One dimension has one order.
        Ok(if rank == 1 {
            ArrayOrdering::RowMajor
        } else {
            ordering
        })
    }

    /// A subrange's lower bound, where it ends, and its stride, or why
    /// they are unsupported.
    fn read_subrange(
        &self,
        unit_index: usize,
        child: &gimli::DebuggingInformationEntry<Reader<'data>>,
        default_lower: Option<i128>,
    ) -> std::result::Result<Subrange<'data>, &'static str> {
        if child.attr(gimli::DW_AT_bit_stride).is_some() {
            return Err("arrays whose elements a stride in bits spaces are unsupported");
        }

        let index = self.index_type(unit_index, child);
        let bound = |attribute: &gimli::Attribute<Reader<'data>>| {
            self.subrange_bound(unit_index, attribute, index)?
                .ok_or("array bound has an invalid encoding")
        };

        let lower = child.attr(gimli::DW_AT_lower_bound).map_or_else(
            || {
                default_lower
                    .map(SubrangeBound::Constant)
                    .ok_or("array lower bound is not stated and not its language's")
            },
            bound,
        )?;
        let extent = match (
            child.attr(gimli::DW_AT_count),
            child.attr(gimli::DW_AT_upper_bound),
        ) {
            (Some(count), _) => Extent::Count(bound(count)?),
            (None, Some(upper)) => Extent::Upper(bound(upper)?),
            (None, None) => Extent::Unknown,
        };

        let stride = child
            .attr(gimli::DW_AT_byte_stride)
            .map(bound)
            .transpose()?;

        Ok((lower, extent, stride))
    }

    /// The size and signedness of a subrange's index type, which give its
    /// computed bounds' values: a target word, signed, when it names none.
    fn index_type(
        &self,
        unit_index: usize,
        subrange: &gimli::DebuggingInformationEntry<Reader<'data>>,
    ) -> (u8, bool) {
        let unit = &self.units[unit_index];
        let word = (unit.encoding().address_size, true);
        let Some(gimli::AttributeValue::UnitRef(offset)) = subrange.attr_value(gimli::DW_AT_type)
        else {
            return word;
        };

        let Ok(index_type) = unit.entry(offset) else {
            return word;
        };

        let size = index_type
            .attr(gimli::DW_AT_byte_size)
            .and_then(gimli::Attribute::udata_value)
            .and_then(|size| u8::try_from(size).ok())
            .filter(|size| (1..=16).contains(size));
        size.map_or(word, |size| (size, index_type_is_signed(unit, subrange)))
    }

    /// One bound, count, or stride of a subrange, as its attribute gives
    /// it, or why it is unsupported.
    fn subrange_bound(
        &self,
        unit_index: usize,
        attribute: &gimli::Attribute<Reader<'data>>,
        (byte_size, signed): (u8, bool),
    ) -> std::result::Result<Option<SubrangeBound<'data>>, &'static str> {
        if let Some(value) = array_bound(attribute, signed) {
            return Ok(Some(SubrangeBound::Constant(value)));
        }

        match attribute.value() {
            gimli::AttributeValue::Exprloc(expression) => Ok(Some(SubrangeBound::Computed {
                expression,
                byte_size,
                signed,
            })),
            gimli::AttributeValue::UnitRef(offset) => {
                let unit = &self.units[unit_index];
                let referenced = unit
                    .entry(offset)
                    .map_err(|_| "array bound refers to no entry")?;
                if !matches!(
                    referenced.tag(),
                    gimli::DW_TAG_variable | gimli::DW_TAG_formal_parameter
                ) {
                    return Err("array bounds a record's member gives are unsupported");
                }

                Ok(self
                    .units
                    .debug_info_offset(DieKey {
                        unit: unit_index,
                        offset: offset.0,
                    })
                    .map(SubrangeBound::Variable))
            }

            gimli::AttributeValue::Sdata(_)
            | gimli::AttributeValue::Udata(_)
            | gimli::AttributeValue::Data1(_)
            | gimli::AttributeValue::Data2(_)
            | gimli::AttributeValue::Data4(_)
            | gimli::AttributeValue::Data8(_) => Ok(None),
            _ => Err("array bound's form is unsupported"),
        }
    }

    /// A Fortran `character(len=N)`: N characters, counted from one, of
    /// the type it names, or else of one byte each. A length the program
    /// decides at run time is where the string says.
    fn build_string_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        if let Some(length) = entry.attr(gimli::DW_AT_string_length) {
            let name = explicit_name.unwrap_or_else(|| Arc::from("character(len=:)"));
            let unit = &self.units[unit_index];
            let length = match length.value() {
                gimli::AttributeValue::Exprloc(expression) => SubrangeBound::Stored {
                    expression,
                    byte_size: [gimli::DW_AT_string_length_byte_size, gimli::DW_AT_byte_size]
                        .into_iter()
                        .find_map(|attribute| entry.attr(attribute))
                        .map_or_else(
                            || Some(unit.encoding().address_size),
                            |size| {
                                size.udata_value()
                                    .and_then(|size| u8::try_from(size).ok())
                                    .filter(|size| (1..=16).contains(size))
                            },
                        )
                        .ok_or("string length's size is invalid")?,
                },
                _ => match self.subrange_bound(unit_index, length, (0, false)) {
                    Ok(Some(length @ SubrangeBound::Variable(_))) => length,
                    Ok(_) => return Err("string length has an invalid encoding".into()),
                    Err(description) => {
                        return Ok(opaque(reference, name, None, description));
                    }
                },
            };

            let element = self
                .target(entry, unit_index)?
                .unwrap_or_else(|| self.character_type());
            self.explicit_names.insert(reference.id.index());
            return self.build_runtime_array(
                entry,
                unit_index,
                reference,
                (name, None),
                element,
                vec![(SubrangeBound::Constant(1), Extent::Count(length), None)],
                ArrayOrdering::RowMajor,
                None,
            );
        }

        // With a length of its own, a string's byte size is the length's.
        let Some(byte_size) = explicit_size else {
            return Ok(opaque(
                reference,
                explicit_name.unwrap_or_else(|| Arc::from("character(len=?)")),
                None,
                "string has no length",
            ));
        };

        let element = self
            .target(entry, unit_index)?
            .unwrap_or_else(|| self.character_type());
        let count = self
            .byte_size_of(element.id)
            .filter(|&size| size > 0 && byte_size % size == 0)
            .map(|size| byte_size / size)
            .ok_or("string length is not a whole number of its characters")?;
        let name = explicit_name.unwrap_or_else(|| Arc::from(format!("character(len={count})")));
        self.explicit_names.insert(reference.id.index());

        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Array {
                element,
                dimensions: Arc::from([ArrayDimension {
                    lower_bound: 1,
                    count,
                }]),
                ordering: ArrayOrdering::RowMajor,
            },
        ))
    }

    /// A subrange type, as Ada's `range -128 .. 127`, is a type of its own
    /// that holds its base type's values within its bounds.
    fn build_subrange_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
    ) -> Built {
        let Some(target) = self.target(entry, unit_index)? else {
            return Ok(opaque(
                reference,
                explicit_name.unwrap_or_else(|| Arc::from("<subrange>")),
                explicit_size,
                "subrange type names no base type",
            ));
        };

        let byte_size = explicit_size.or_else(|| self.byte_size_of(target.id));
        let name = explicit_name.unwrap_or_else(|| self.target_name(target));

        Ok(resolved(
            reference,
            name,
            byte_size,
            TypeKind::Named {
                target: Some(target),
                relationship: NamedTypeRelationship::Distinct,
            },
        ))
    }

    fn build_slice_type(
        &mut self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
        reference: TypeReference,
        explicit_name: Option<Arc<str>>,
        explicit_size: Option<u64>,
        layout: SliceLayout,
    ) -> Built {
        let name = explicit_name.unwrap_or_else(|| Arc::from("<slice>"));
        let byte_size = explicit_size.ok_or("slice descriptor has no byte size")?;
        let unit = &self.units[unit_index];
        let address_size = u64::from(unit.encoding().address_size);
        let field_names = layout.members();
        let word_size = address_size;
        if word_size == 0 || byte_size != word_size * layout.word_count() {
            return Ok(opaque(
                reference,
                name,
                Some(byte_size),
                "slice descriptor does not use target-sized words",
            ));
        }

        let mut fields = Vec::new();
        let mut children = self.children(unit_index, entry.offset())?;

        while let Some(child) = children.next_child()? {
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }

            let field_name = copy_name(self.dwarf, unit, child)
                .map_err(malformed)?
                .ok_or("slice member has no name")?;
            let offset = child
                .attr(gimli::DW_AT_data_member_location)
                .and_then(gimli::Attribute::udata_value)
                .ok_or("slice member has no constant offset")?;
            let field_type = self
                .target(child, unit_index)?
                .ok_or("slice member has no type")?;
            fields.push((field_name, offset, field_type));
        }

        if fields.len() != field_names.len()
            || fields.iter().zip(field_names).enumerate().any(
                |(index, ((name, offset, _), expected_name))| {
                    name.as_ref() != *expected_name || *offset != index as u64 * word_size
                },
            )
        {
            return Ok(opaque(
                reference,
                name,
                Some(byte_size),
                "unrecognized slice descriptor layout",
            ));
        }

        let words = layout.words();
        let element = self.slice_element(layout, fields[usize::from(words.data)].2)?;
        let counts = [Some(words.length), words.capacity];

        for (_, _, field_type) in counts
            .into_iter()
            .flatten()
            .map(|word| &fields[usize::from(word)])
        {
            // Odin names its integers, as `int`, by typedefs.
            let valid = matches!(self.unnamed(field_type.id), Some(TypeInfo {
                kind: TypeKind::Base(BaseType {
                    encoding: BaseTypeEncoding::Unsigned | BaseTypeEncoding::Signed,
                    byte_size: size,
                    ..
                }),
                ..
            }) if *size == word_size);
            if !valid {
                return Err(
                    "slice length and capacity members must be target-sized unsigned integers"
                        .into(),
                );
            }
        }

        let text = layout.is_text(&name);

        Ok(resolved(
            reference,
            name,
            Some(byte_size),
            TypeKind::Slice {
                element,
                words,
                text,
            },
        ))
    }
}

impl TypeArenaBuilder<'_, '_> {
    /// A slice's element type: what its data member points to, or a string
    /// type's bytes.
    fn slice_element(
        &mut self,
        layout: SliceLayout,
        data: TypeReference,
    ) -> std::result::Result<TypeReference, Arc<str>> {
        if let SliceLayout::RustBytes(bytes) = layout {
            let array = self.units[bytes.unit]
                .entry(gimli::UnitOffset(bytes.offset))
                .map_err(malformed)?;
            return self
                .type_reference(bytes.unit, array.attr_value(gimli::DW_AT_type))?
                .ok_or_else(|| "string type's bytes have no type".into());
        }

        match self.entries.get(data.id.index()) {
            Some(TypeEntry::Resolved(TypeInfo {
                kind:
                    TypeKind::Pointer {
                        target: Some(target),
                        ..
                    },
                ..
            })) => Ok(*target),
            _ => Err("slice data member is not a typed pointer".into()),
        }
    }
}

/// Which language's slice descriptor a structure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SliceLayout {
    /// `{data_ptr, length}`: a pointer to a slice or `str`.
    Rust,

    /// `{data_ptr, length}`: a pointer to a string type, such as `Path`,
    /// `OsStr`, or `CStr`, which wraps the unsized array of bytes it holds.
    RustBytes(DieKey),

    /// `{ptr, len}`.
    Zig,

    /// `{array, len, cap}`.
    Go,

    /// `{data, len}`: Odin's slices and strings.
    Odin,

    /// `{data, len, cap, allocator}`: Odin's dynamic arrays, whose
    /// allocator is two words.
    OdinDynamic,

    /// `{length, ptr}`: D's slices and strings.
    D,
}

impl SliceLayout {
    /// The descriptor's members, in order, each at the word of its index.
    const fn members(self) -> &'static [&'static str] {
        match self {
            Self::Rust | Self::RustBytes(_) => &["data_ptr", "length"],
            Self::Zig => &["ptr", "len"],
            Self::Go => &["array", "len", "cap"],
            Self::Odin => &["data", "len"],
            Self::OdinDynamic => &["data", "len", "cap", "allocator"],
            Self::D => &["length", "ptr"],
        }
    }

    /// How many words the descriptor is.
    const fn word_count(self) -> u64 {
        match self {
            Self::OdinDynamic => 5,
            layout => layout.members().len() as u64,
        }
    }

    /// Which of the descriptor's words hold its parts.
    const fn words(self) -> SliceWords {
        match self {
            Self::Rust | Self::RustBytes(_) | Self::Zig | Self::Odin => SliceWords::POINTER_LENGTH,
            Self::Go | Self::OdinDynamic => SliceWords::POINTER_LENGTH_CAPACITY,
            Self::D => SliceWords::LENGTH_POINTER,
        }
    }

    /// Whether a slice so named is the language's text: Rust's `str`, and
    /// Zig's `[]const u8` and its sentinel-terminated forms.
    fn is_text(self, name: &str) -> bool {
        match self {
            Self::Rust => {
                matches!(name, "&str" | "&mut str" | "*const str" | "*mut str")
                    || name
                        .strip_prefix("alloc::boxed::Box<str")
                        .is_some_and(|rest| rest.starts_with([',', '>']))
            }
            Self::RustBytes(_) => true,
            Self::Zig => matches!(name, "[]const u8" | "[:0]const u8" | "[:0]u8"),
            Self::Odin => name == "string",
            // D's strings, and any slice of its characters, however
            // qualified: `char[]`, `const(wchar)[]`.
            Self::D => {
                matches!(name, "string" | "wstring" | "dstring")
                    || name.strip_suffix("[]").is_some_and(|element| {
                        let element = ["const(", "immutable(", "shared(", "inout("]
                            .into_iter()
                            .find_map(|qualifier| {
                                element.strip_prefix(qualifier)?.strip_suffix(')')
                            })
                            .unwrap_or(element);
                        matches!(element, "char" | "wchar" | "dchar")
                    })
            }
            Self::Go | Self::OdinDynamic => false,
        }
    }
}

impl<'data> TypeArenaBuilder<'_, 'data> {
    /// Which language's slice descriptor a structure DIE is, judged by its
    /// shape and by what its language says, never by a library's names.
    ///
    /// rustc emits each pointer to a slice or `str` as a structure of a data
    /// pointer and a length outside every module, whatever it names it:
    /// `&[T]`, `*const [T]` when optimizing, `&mut [T]`, `Box<[T]>`. A
    /// pointer to a type with an unsized tail, such as `&Path`, has the same
    /// shape, but its length counts the tail, so it is not a slice, unless
    /// the tail is bytes that wrappers alone hold, as in `Path`. Go marks
    /// slices by kind, and Zig spells them `[]T` and `[:s]T`.
    fn slice_layout(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        key: DieKey,
        name: Option<&str>,
    ) -> Option<SliceLayout> {
        match self.language(key.unit) {
            SourceLanguage::Go => Self::go_kind(entry)
                .map_or_else(
                    || name.is_some_and(|name| name.starts_with("[]")),
                    |kind| kind == GoKind::Slice,
                )
                .then_some(SliceLayout::Go),
            SourceLanguage::Zig => name
                .is_some_and(|name| name.starts_with("[]") || name.starts_with("[:"))
                .then_some(SliceLayout::Zig),
            SourceLanguage::Odin => match name? {
                "string" => Some(SliceLayout::Odin),
                name if name.starts_with("[]") => Some(SliceLayout::Odin),
                name if name.starts_with("[dynamic]") => Some(SliceLayout::OdinDynamic),
                _ => None,
            },
            // A D slice is named for its element, and a string for its
            // characters.
            SourceLanguage::D => {
                let name = name?;
                if !(name.ends_with("[]") || matches!(name, "string" | "wstring" | "dstring")) {
                    return None;
                }

                let members = self.member_types(entry, key.unit)?;
                let [(length, _), (pointer, _)] = members.as_slice() else {
                    return None;
                };

                (length.as_ref() == "length" && pointer.as_ref() == "ptr").then_some(SliceLayout::D)
            }
            SourceLanguage::Rust if !self.context.type_scopes.contains_key(&key) => {
                let members = self.member_types(entry, key.unit)?;
                let [(first, data), (second, _)] = members.as_slice() else {
                    return None;
                };

                if first.as_ref() != "data_ptr" || second.as_ref() != "length" {
                    return None;
                }

                let data = (*data)?;
                if !self.points_to_unsized(data) {
                    return Some(SliceLayout::Rust);
                }

                self.string_bytes(data).map(SliceLayout::RustBytes)
            }
            _ => None,
        }
    }

    /// A structure's members' names and type DIEs, in order.
    fn member_types(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> Option<Vec<(Arc<str>, Option<DieKey>)>> {
        let unit = self.units.get(unit_index)?;
        let mut members = Vec::new();
        let mut children = self.children(unit_index, entry.offset()).ok()?;

        while let Some(child) = children.next_child().ok()? {
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }

            if members.len() >= MAX_RECORD_CHILDREN {
                return None;
            }

            let name = copy_name(self.dwarf, unit, child).ok()??;
            let target = die_reference_with_signatures(
                child.attr_value(gimli::DW_AT_type),
                unit_index,
                self.units,
                self.type_signatures,
            )
            .ok()?;
            members.push((name, target));
        }

        Some(members)
    }

    /// Whether a pointer DIE points to a type whose last member is unsized,
    /// such as `Path` or `RcInner<str>`. Anything unreadable counts as
    /// unsized, so that a doubtful pointer is never presented as a slice.
    fn points_to_unsized(&self, pointer: DieKey) -> bool {
        const MAX_DEPTH: usize = 16;
        let target = |key: DieKey| -> Option<(
            gimli::DebuggingInformationEntry<Reader<'data>>,
            Option<DieKey>,
        )> {
            let unit = self.units.get(key.unit)?;
            let entry = unit.entry(gimli::UnitOffset(key.offset)).ok()?;
            let target = die_reference_with_signatures(
                entry.attr_value(gimli::DW_AT_type),
                key.unit,
                self.units,
                self.type_signatures,
            )
            .ok()?;
            Some((entry, target))
        };

        let Some((pointer_entry, Some(mut current))) = target(pointer) else {
            return true;
        };

        if pointer_entry.tag() != gimli::DW_TAG_pointer_type {
            return true;
        }

        for _ in 0..MAX_DEPTH {
            let Some((entry, next)) = target(current) else {
                return true;
            };

            match entry.tag() {
                gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type => {
                    match next {
                        Some(next) => current = next,
                        None => return false,
                    }
                }
                gimli::DW_TAG_array_type => return self.array_is_unsized(current),
                gimli::DW_TAG_structure_type => {
                    if self.tail_overruns(&entry, current.unit) {
                        return true;
                    }

                    let Some(members) = self.member_types(&entry, current.unit) else {
                        return true;
                    };

                    match members.last() {
                        Some((_, Some(last))) => current = *last,
                        Some((_, None)) => return true,
                        None => return false,
                    }
                }
                _ => return false,
            }
        }

        true
    }

    /// Whether a record's last member runs past the record's end, as an
    /// unsized tail does when rustc describes it by its element alone:
    /// `struct Tail { head: u32, tail: [u16] }` is 4 bytes whose `tail` is a
    /// `u16` at offset 4. A member of no size may sit at the end of a sized
    /// record, and a size that cannot be read proves nothing.
    fn tail_overruns(
        &self,
        entry: &gimli::DebuggingInformationEntry<Reader<'data>>,
        unit_index: usize,
    ) -> bool {
        let Some(size) = entry
            .attr(gimli::DW_AT_byte_size)
            .and_then(gimli::Attribute::udata_value)
        else {
            return false;
        };

        let Ok(mut children) = self.children(unit_index, entry.offset()) else {
            return false;
        };

        let mut last = None;

        while let Ok(Some(child)) = children.next_child() {
            if child.tag() != gimli::DW_TAG_member {
                continue;
            }

            let offset = child
                .attr(gimli::DW_AT_data_member_location)
                .and_then(gimli::Attribute::udata_value);
            let target = die_reference_with_signatures(
                child.attr_value(gimli::DW_AT_type),
                unit_index,
                self.units,
                self.type_signatures,
            )
            .ok()
            .flatten();
            last = Some((offset, target));
        }

        let Some((Some(offset), Some(target))) = last else {
            return false;
        };

        self.die_byte_size(target)
            .and_then(|member| offset.checked_add(member))
            .is_some_and(|end| end > size)
    }

    /// The size of the type a DIE describes, through typedefs and
    /// qualifiers, when the DIE states it.
    fn die_byte_size(&self, mut key: DieKey) -> Option<u64> {
        const MAX_DEPTH: usize = 16;

        for _ in 0..MAX_DEPTH {
            let entry = self
                .units
                .get(key.unit)?
                .entry(gimli::UnitOffset(key.offset))
                .ok()?;
            if let Some(size) = entry
                .attr(gimli::DW_AT_byte_size)
                .and_then(gimli::Attribute::udata_value)
            {
                return Some(size);
            }

            if !matches!(
                entry.tag(),
                gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type
            ) {
                return None;
            }

            key = die_reference_with_signatures(
                entry.attr_value(gimli::DW_AT_type),
                key.unit,
                self.units,
                self.type_signatures,
            )
            .ok()??;
        }

        None
    }

    /// The unsized array of bytes a pointer DIE's target holds, when that
    /// target is records each of a single member at offset zero, ending in
    /// the array, as `Path`, `OsStr`, and `CStr` are.
    fn string_bytes(&self, pointer: DieKey) -> Option<DieKey> {
        const MAX_DEPTH: usize = 16;
        let entry = |key: DieKey| {
            self.units
                .get(key.unit)?
                .entry(gimli::UnitOffset(key.offset))
                .ok()
        };

        let target = |entry: &gimli::DebuggingInformationEntry<Reader<'data>>, unit| {
            die_reference_with_signatures(
                entry.attr_value(gimli::DW_AT_type),
                unit,
                self.units,
                self.type_signatures,
            )
            .ok()
            .flatten()
        };

        let pointer_entry = entry(pointer)?;
        let mut current = target(&pointer_entry, pointer.unit)?;
        let mut wrapped = false;

        for _ in 0..MAX_DEPTH {
            let current_entry = entry(current)?;

            match current_entry.tag() {
                gimli::DW_TAG_structure_type => {
                    // The one member: whether it is at offset zero, and
                    // its type. A second member, or an error before one,
                    // ends the search.
                    let mut children = self.children(current.unit, current_entry.offset()).ok()?;
                    let mut only = None;

                    while let Ok(Some(child)) = children.next_child() {
                        if child.tag() != gimli::DW_TAG_member {
                            continue;
                        }

                        if only.is_some() {
                            return None;
                        }

                        let at_start = child
                            .attr(gimli::DW_AT_data_member_location)
                            .and_then(gimli::Attribute::udata_value)
                            == Some(0);
                        only = Some((at_start, target(child, current.unit)));
                    }

                    let (at_start, member_type) = only?;
                    if !at_start {
                        return None;
                    }

                    current = member_type?;
                    wrapped = true;
                }
                gimli::DW_TAG_array_type if wrapped && self.array_is_unsized(current) => {
                    let element = entry(target(&current_entry, current.unit)?)?;
                    let byte = element.tag() == gimli::DW_TAG_base_type
                        && element
                            .attr(gimli::DW_AT_byte_size)
                            .and_then(gimli::Attribute::udata_value)
                            == Some(1);

                    return byte.then_some(current);
                }
                _ => return None,
            }
        }

        None
    }

    /// Whether an array type DIE has a dimension with no count.
    pub(super) fn array_is_unsized(&self, array: DieKey) -> bool {
        let Ok(mut children) = self.children(array.unit, gimli::UnitOffset(array.offset)) else {
            return true;
        };

        while let Ok(Some(child)) = children.next_child() {
            if child.tag() == gimli::DW_TAG_subrange_type
                && child.attr(gimli::DW_AT_count).is_none()
                && child.attr(gimli::DW_AT_upper_bound).is_none()
            {
                return true;
            }
        }

        false
    }
}

/// Where an array of a unit's language begins when its subrange does not
/// say: DWARF's default lower bound for the language, which a language not
/// in the standard's table does not have.
fn default_lower_bound(language: Option<gimli::DwLang>, source: SourceLanguage) -> Option<i128> {
    if matches!(
        source,
        SourceLanguage::C
            | SourceLanguage::Cpp
            | SourceLanguage::Rust
            | SourceLanguage::Go
            | SourceLanguage::Zig
            | SourceLanguage::Odin
            | SourceLanguage::Nim
    ) {
        return Some(0);
    }

    let language = language?;

    match language {
        gimli::DW_LANG_Kotlin | gimli::DW_LANG_Crystal => Some(0),
        gimli::DW_LANG_Fortran18 | gimli::DW_LANG_Ada2005 | gimli::DW_LANG_Ada2012 => Some(1),
        language => language
            .default_lower_bound()
            .and_then(|bound| i128::try_from(bound).ok()),
    }
}

/// Resolves a type DIE's `DW_AT_address_class`, which defaults to zero. A
/// class too large to represent is unsupported, and any other non-constant
/// form defective: either read as the default class would use semantics the
/// producer never specified.
fn resolve_address_class(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    reference: TypeReference,
    explicit_name: Option<Arc<str>>,
) -> std::result::Result<u64, Box<TypeEntry>> {
    let Some(attribute) = entry.attr(gimli::DW_AT_address_class) else {
        return Ok(0);
    };

    match unsigned_constant(attribute) {
        UnsignedConstant::Value(value) => Ok(value),
        UnsignedConstant::Oversized => Err(Box::new(opaque(
            reference,
            explicit_name.unwrap_or_else(|| Arc::from("<unsupported type>")),
            None,
            "DW_AT_address_class exceeds the supported u64 range",
        ))),
        UnsignedConstant::NonConstant => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_address_class is not an unsigned integer constant".into(),
        ))),
    }
}

/// Resolves a type DIE's `DW_AT_byte_size`: `None` when it is absent, and
/// otherwise the size or the terminal entry for a size that is unusable or
/// defective. Only an absent size may fall back to a default.
fn resolve_explicit_size(
    entry: &gimli::DebuggingInformationEntry<Reader<'_>>,
    reference: TypeReference,
    explicit_name: Option<Arc<str>>,
) -> std::result::Result<Option<u64>, Box<TypeEntry>> {
    match byte_size_attribute(entry) {
        ByteSize::Absent => Ok(None),
        ByteSize::Constant(size) => Ok(Some(size)),
        ByteSize::Unsupported(description) => Err(Box::new(opaque(
            reference,
            explicit_name.unwrap_or_else(|| Arc::from("<oversized type>")),
            None,
            description,
        ))),
        ByteSize::Dynamic => Err(Box::new(opaque(
            reference,
            explicit_name.unwrap_or_else(|| Arc::from("<dynamically sized type>")),
            None,
            "dynamic DW_AT_byte_size is unsupported",
        ))),
        ByteSize::Malformed => Err(Box::new(TypeEntry::Malformed(
            "DW_AT_byte_size is neither a constant nor a supported dynamic form".into(),
        ))),
    }
}

/// A type built, or why its metadata is malformed.
type Built = std::result::Result<TypeEntry, Arc<str>>;

fn malformed(error: impl std::fmt::Display) -> Arc<str> {
    error.to_string().into()
}

/// Gives the members `declarations` names their declarations.
fn declare_members(
    kind: &mut TypeKind,
    declarations: impl Iterator<Item = (AggregateMemberPath, Option<SourceLocation>)>,
) {
    // A record's lists are its own until types merge, so each is written
    // in place; copying a list for each member it declares made a variant
    // part's cost grow with the square of its members.
    let (mut direct, mut discriminant, mut variants) = match kind {
        TypeKind::Record { members, .. } | TypeKind::Union { members, .. } => {
            (Some(members), None, None)
        }

        TypeKind::Variant {
            common_members,
            discriminant,
            variants,
            ..
        } => (Some(common_members), Some(discriminant), Some(variants)),
        _ => (None, None, None),
    };

    for (path, declaration) in declarations {
        match path {
            AggregateMemberPath::Direct(member) => {
                if let Some(members) = &mut direct
                    && member < members.len()
                {
                    Arc::make_mut(members)[member].declaration = declaration;
                }
            }
            AggregateMemberPath::Discriminant => {
                if let Some(discriminant) = &mut discriminant
                    && let VariantDiscriminant::Stored(member) = discriminant.as_mut()
                {
                    member.declaration = declaration;
                }
            }
            AggregateMemberPath::Variant { variant, member } => {
                if let Some(variants) = &mut variants
                    && variants
                        .get(variant)
                        .is_some_and(|variant| member < variant.members.len())
                {
                    let members = &mut Arc::make_mut(variants)[variant].members;
                    Arc::make_mut(members)[member].declaration = declaration;
                }
            }
        }
    }
}

/// A type with no identity yet.
const fn resolved(
    reference: TypeReference,
    name: Arc<str>,
    byte_size: Option<u64>,
    kind: TypeKind,
) -> TypeEntry {
    TypeEntry::Resolved(TypeInfo {
        reference,
        name,
        byte_size,
        kind,
        identity: None,
    })
}

/// A type whose representation is unsupported.
fn opaque(
    reference: TypeReference,
    name: Arc<str>,
    byte_size: Option<u64>,
    description: impl Into<Arc<str>>,
) -> TypeEntry {
    resolved(
        reference,
        name,
        byte_size,
        TypeKind::Opaque {
            description: description.into(),
        },
    )
}

/// Go's `DW_AT_go_dict_index`: which entry of a generic function's
/// dictionary holds a type parameter's argument.
const DW_AT_GO_DICT_INDEX: gimli::DwAt = gimli::DwAt(0x2906);

/// Whether an encoding is a float or a pair of them.
const fn is_floating(encoding: BaseTypeEncoding) -> bool {
    matches!(
        encoding,
        BaseTypeEncoding::Floating | BaseTypeEncoding::ComplexFloating
    )
}

/// The integral `BaseTypeEncoding` of a `DW_ATE_*` encoding.
const fn integer_encoding(encoding: gimli::DwAte) -> Option<BaseTypeEncoding> {
    Some(match encoding {
        gimli::DW_ATE_boolean => BaseTypeEncoding::Boolean,
        gimli::DW_ATE_signed => BaseTypeEncoding::Signed,
        gimli::DW_ATE_signed_char => BaseTypeEncoding::SignedCharacter,
        gimli::DW_ATE_unsigned => BaseTypeEncoding::Unsigned,
        gimli::DW_ATE_unsigned_char => BaseTypeEncoding::UnsignedCharacter,
        _ => return None,
    })
}

/// Spare DIEs for [`Children`] to read into, kept for the whole load.
///
/// A DIE owns a vector of its decoded attributes, so reading children into
/// a DIE of their own allocated once for every aggregate, enumeration,
/// array, and signature a load builds, several times for each record. Each
/// [`Children`] takes a DIE from here and gives it back when dropped, so a
/// load allocates only as many as it nests. The builder is shared with the
/// threads that build identities, so the spares are behind a lock, which
/// costs less uncontended than the allocation it saves.
#[derive(Default)]
pub(super) struct DieBuffers<'data>(
    std::sync::Mutex<Vec<gimli::DebuggingInformationEntry<Reader<'data>>>>,
);

impl<'data> DieBuffers<'data> {
    fn take(&self) -> gimli::DebuggingInformationEntry<Reader<'data>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
            .unwrap_or_else(gimli::DebuggingInformationEntry::null)
    }

    fn give_back(&self, entry: gimli::DebuggingInformationEntry<Reader<'data>>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
    }

    /// The DIE at `offset`, which `raw` starts at, read into a spare as
    /// `gimli::Unit::entry` reads it. Resolving a type reads its DIE twice,
    /// once to find its definition and once to build it, which allocated
    /// the DIE's attributes each time.
    fn read(
        &self,
        mut raw: gimli::EntriesRaw<'_, Reader<'data>>,
        offset: gimli::UnitOffset,
    ) -> gimli::Result<SpareEntry<'_, 'data>> {
        let mut spare = SpareEntry {
            entry: self.take(),
            buffers: self,
        };

        raw.read_entry(&mut spare.entry)?;
        if spare.entry.is_null() {
            return Err(gimli::Error::NoEntryAtGivenOffset(offset.0 as u64));
        }

        Ok(spare)
    }
}

/// A DIE read into a spare from [`DieBuffers`], which it goes back to when
/// dropped.
pub(super) struct SpareEntry<'a, 'data> {
    entry: gimli::DebuggingInformationEntry<Reader<'data>>,
    buffers: &'a DieBuffers<'data>,
}

impl<'data> std::ops::Deref for SpareEntry<'_, 'data> {
    type Target = gimli::DebuggingInformationEntry<Reader<'data>>;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl Drop for SpareEntry<'_, '_> {
    fn drop(&mut self) {
        let entry = std::mem::replace(&mut self.entry, gimli::DebuggingInformationEntry::null());
        self.buffers.give_back(entry);
    }
}

/// The direct children of one DIE, read one at a time into one reused DIE
/// and lent from it.
pub(super) struct Children<'a, 'data> {
    raw: gimli::EntriesRaw<'a, Reader<'data>>,
    entry: gimli::DebuggingInformationEntry<Reader<'data>>,
    buffers: &'a DieBuffers<'data>,
    started: bool,
    done: bool,
}

impl<'a, 'data> Children<'a, 'data> {
    fn new(raw: gimli::EntriesRaw<'a, Reader<'data>>, buffers: &'a DieBuffers<'data>) -> Self {
        Self {
            entry: buffers.take(),
            raw,
            buffers,
            started: false,
            done: false,
        }
    }

    /// The next child, or `None` after the last. After an error there are
    /// no more.
    pub(super) fn next_child(
        &mut self,
    ) -> std::result::Result<Option<&gimli::DebuggingInformationEntry<Reader<'data>>>, Arc<str>>
    {
        if self.done {
            return Ok(None);
        }

        match self.advance() {
            Ok(true) => Ok(Some(&self.entry)),
            Ok(false) => {
                self.done = true;
                Ok(None)
            }
            Err(error) => {
                self.done = true;
                Err(malformed(error))
            }
        }
    }

    /// Reads the next child into the DIE, returning whether there is one.
    fn advance(&mut self) -> gimli::Result<bool> {
        if !self.started {
            self.started = true;

            // The parent itself, whose attributes nothing here reads.
            let Some(parent) = self.raw.read_abbreviation()? else {
                return Ok(false);
            };

            self.raw.skip_attributes(parent.attributes())?;
            if !parent.has_children() {
                return Ok(false);
            }
        }

        // The parent is at depth zero and its children at one. Their own
        // children are skipped by their abbreviations, without decoding
        // the attributes nothing here reads.
        while !self.raw.is_empty() {
            if self.raw.next_depth() <= 1 {
                // A null entry ends the children.
                return self.raw.read_entry(&mut self.entry);
            }

            if let Some(abbreviation) = self.raw.read_abbreviation()? {
                self.raw.skip_attributes(abbreviation.attributes())?;
            }
        }

        Ok(false)
    }
}

impl Drop for Children<'_, '_> {
    fn drop(&mut self) {
        let entry = std::mem::replace(&mut self.entry, gimli::DebuggingInformationEntry::null());
        self.buffers.give_back(entry);
    }
}

/// Whether a type DIE's `DW_AT_type` must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetRequirement {
    Required,
    Optional,
}

fn named_type_relationship(tag: gimli::DwTag, language: SourceLanguage) -> NamedTypeRelationship {
    if tag == gimli::DW_TAG_template_alias {
        return NamedTypeRelationship::Synonym;
    }

    match language {
        // Nim's are the C typedefs it compiles to.
        SourceLanguage::C | SourceLanguage::Cpp | SourceLanguage::Nim => {
            NamedTypeRelationship::Synonym
        }

        // A Rust type alias is the type it names: Rust has no typedef of a
        // distinct type. Releases after 1.97 describe `usize` and `isize`
        // as typedefs of `u64` and `i64`, which must stay integers.
        SourceLanguage::Rust => NamedTypeRelationship::Synonym,

        // Odin's typedefs are its `distinct` types, and wrappers that name
        // its own types, as `int`; GNAT's name Ada's types, as an access
        // type.
        SourceLanguage::Go | SourceLanguage::Odin | SourceLanguage::Ada => {
            NamedTypeRelationship::Distinct
        }
        SourceLanguage::Zig => NamedTypeRelationship::Encoding,
        _ => NamedTypeRelationship::Unspecified,
    }
}

const fn type_modifier(tag: gimli::DwTag) -> Option<TypeModifier> {
    match tag {
        gimli::DW_TAG_const_type => Some(TypeModifier::Const),
        gimli::DW_TAG_volatile_type => Some(TypeModifier::Volatile),
        gimli::DW_TAG_restrict_type => Some(TypeModifier::Restrict),
        gimli::DW_TAG_atomic_type => Some(TypeModifier::Atomic),
        gimli::DW_TAG_immutable_type => Some(TypeModifier::Immutable),
        gimli::DW_TAG_packed_type => Some(TypeModifier::Packed),
        gimli::DW_TAG_shared_type => Some(TypeModifier::Shared),
        _ => None,
    }
}

/// Whether a DIE tag denotes a type. A `DW_AT_type` edge must name one of
/// these; a reference to any other tag is defective metadata. Tags this backend
/// does not model still count as types and are surfaced as opaque.
pub(super) const fn is_type_die_tag(tag: gimli::DwTag) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_base_type
            | gimli::DW_TAG_pointer_type
            | gimli::DW_TAG_reference_type
            | gimli::DW_TAG_rvalue_reference_type
            | gimli::DW_TAG_ptr_to_member_type
            | gimli::DW_TAG_array_type
            | gimli::DW_TAG_structure_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_typedef
            | gimli::DW_TAG_template_alias
            | gimli::DW_TAG_const_type
            | gimli::DW_TAG_volatile_type
            | gimli::DW_TAG_restrict_type
            | gimli::DW_TAG_atomic_type
            | gimli::DW_TAG_immutable_type
            | gimli::DW_TAG_packed_type
            | gimli::DW_TAG_shared_type
            | gimli::DW_TAG_subroutine_type
            | gimli::DW_TAG_string_type
            | gimli::DW_TAG_set_type
            | gimli::DW_TAG_subrange_type
            | gimli::DW_TAG_file_type
            | gimli::DW_TAG_interface_type
            | gimli::DW_TAG_unspecified_type
            | gimli::DW_TAG_coarray_type
            | gimli::DW_TAG_dynamic_type
    )
}

pub(super) trait TypeMetadataEntry {
    fn type_info(&self) -> std::result::Result<&TypeInfo, Arc<str>>;
}

impl TypeMetadataEntry for TypeEntry {
    fn type_info(&self) -> std::result::Result<&TypeInfo, Arc<str>> {
        match self {
            Self::Resolved(info) => Ok(info),
            Self::Malformed(reason) => Err(Arc::clone(reason)),
            Self::Building => Err("type graph did not finish building".into()),
        }
    }
}

/// A module's types, as the loader builds them or an image keeps them.
pub(super) trait TypeEntries {
    /// The type `id` names, or why its metadata is unusable.
    fn entry(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>>;
}

impl<T: TypeMetadataEntry> TypeEntries for [T] {
    fn entry(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
        self.get(id.index()).map_or_else(
            || Err("type ID is outside the module arena".into()),
            TypeMetadataEntry::type_info,
        )
    }
}

impl TypeEntries for crate::image::types::TypeTable {
    fn entry(&self, id: TypeId) -> std::result::Result<&TypeInfo, Arc<str>> {
        match self.node(id) {
            Some(crate::TypeNode::Resolved(info)) => Ok(info),
            Some(crate::TypeNode::Malformed { description, .. }) => Err(Arc::clone(description)),
            None => Err("type ID is outside the module arena".into()),
        }
    }
}

pub(super) fn type_info_from(
    types: &(impl TypeEntries + ?Sized),
    id: TypeId,
) -> std::result::Result<&TypeInfo, Arc<str>> {
    types.entry(id)
}

pub(super) fn propagate_wrapper_sizes(types: &mut [TypeEntry]) {
    let mut dependents = vec![Vec::new(); types.len()];
    let mut ready = VecDeque::new();

    for (index, entry) in types.iter().enumerate() {
        let TypeEntry::Resolved(info) = entry else {
            continue;
        };

        if info.byte_size.is_some() {
            ready.push_back(index);
            continue;
        }

        let (TypeKind::Modified { target, .. }
        | TypeKind::Named {
            target: Some(target),
            ..
        }) = info.kind
        else {
            continue;
        };

        if let Some(target_dependents) = dependents.get_mut(target.id.index()) {
            target_dependents.push(index);
        }
    }

    while let Some(target) = ready.pop_front() {
        let Some(size) = types
            .get(target)
            .and_then(|entry| entry.type_info().ok())
            .and_then(|info| info.byte_size)
        else {
            continue;
        };

        for dependent in std::mem::take(&mut dependents[target]) {
            let Some(TypeEntry::Resolved(info)) = types.get_mut(dependent) else {
                continue;
            };

            if info.byte_size.is_none() {
                info.byte_size = Some(size);
                ready.push_back(dependent);
            }
        }
    }
}

pub(super) fn inline_storage_cycle_nodes(types: &[TypeEntry]) -> Vec<usize> {
    // The edges are kept flat, each node's after the one before: a vector
    // of their own for each node, and another of predecessors, allocated
    // twice for every type a load builds.
    let mut targets = Vec::new();
    let mut edges = Edges::default();

    for entry in types {
        if let TypeEntry::Resolved(info) = entry {
            targets.clear();
            inline_storage_targets(&info.kind, &mut targets);
            edges
                .targets
                .extend(targets.iter().filter(|target| **target < types.len()));
        }

        edges.ends.push(edges.targets.len());
    }

    let reverse = edges.reversed();

    let mut visited = vec![false; types.len()];
    let mut finish = Vec::with_capacity(types.len());
    let mut stack = Vec::new();
    for start in 0..types.len() {
        if visited[start] {
            continue;
        }

        visited[start] = true;
        stack.push((start, 0_usize));

        while let Some((node, edge_index)) = stack.last_mut() {
            if let Some(next) = edges.of(*node).get(*edge_index).copied() {
                *edge_index += 1;
                if !visited[next] {
                    visited[next] = true;
                    stack.push((next, 0));
                }
            } else {
                finish.push(*node);
                stack.pop();
            }
        }
    }

    let mut assigned = vec![false; types.len()];
    let mut cyclic_nodes = Vec::new();
    let mut component = Vec::new();
    let mut stack = Vec::new();

    for start in finish.into_iter().rev() {
        if assigned[start] {
            continue;
        }

        assigned[start] = true;
        component.clear();
        stack.push(start);

        while let Some(node) = stack.pop() {
            component.push(node);
            for predecessor in reverse.of(node) {
                if !assigned[*predecessor] {
                    assigned[*predecessor] = true;
                    stack.push(*predecessor);
                }
            }
        }

        let cyclic = component.len() > 1
            || component
                .first()
                .is_some_and(|node| edges.of(*node).contains(node));
        if cyclic {
            cyclic_nodes.extend_from_slice(&component);
        }
    }

    cyclic_nodes
}

/// A graph's edges, each node's targets after the node before's.
#[derive(Default)]
struct Edges {
    targets: Vec<usize>,

    /// Where each node's targets end.
    ends: Vec<usize>,
}

impl Edges {
    fn of(&self, node: usize) -> &[usize] {
        let start = node.checked_sub(1).map_or(0, |before| self.ends[before]);
        &self.targets[start..self.ends[node]]
    }

    /// The same graph with every edge turned around, each node's
    /// predecessors in the order their edges were added.
    fn reversed(&self) -> Self {
        let mut ends = vec![0; self.ends.len()];
        for target in &self.targets {
            ends[*target] += 1;
        }

        let mut total = 0;

        for end in &mut ends {
            total += *end;
            *end = total;
        }

        // Each node's predecessors fill its slot from its start.
        let mut next = ends.clone();
        let mut targets = vec![0; self.targets.len()];

        for source in (0..self.ends.len()).rev() {
            for target in self.of(source).iter().rev() {
                next[*target] -= 1;
                targets[next[*target]] = source;
            }
        }

        Self { targets, ends }
    }
}

fn inline_storage_targets(kind: &TypeKind, targets: &mut Vec<usize>) {
    let mut push = |reference: TypeReference| {
        targets.push(reference.id.index());
    };

    match kind {
        TypeKind::Enumeration {
            underlying: Some(target),
            ..
        }
        | TypeKind::Modified { target, .. }
        | TypeKind::Named {
            target: Some(target),
            ..
        } => push(*target),
        TypeKind::Array { element, .. } => push(*element),
        TypeKind::Record { members, bases, .. } => {
            for member in members.iter() {
                push(member.type_ref);
            }

            for base in bases.iter() {
                push(base.type_ref);
            }
        }
        TypeKind::Union { members, .. } => {
            for member in members.iter() {
                push(member.type_ref);
            }
        }

        TypeKind::Variant {
            common_members,
            bases,
            discriminant,
            variants,
            ..
        } => {
            for member in common_members.iter() {
                push(member.type_ref);
            }

            for base in bases.iter() {
                push(base.type_ref);
            }

            if let VariantDiscriminant::Stored(member) = discriminant.as_ref() {
                push(member.type_ref);
            }

            for variant in variants.iter() {
                for member in variant.members.iter() {
                    push(member.type_ref);
                }
            }
        }

        TypeKind::Signature {
            returns,
            parameters,
            ..
        } => {
            if let Some(returns) = returns {
                push(*returns);
            }

            for parameter in parameters.iter() {
                push(*parameter);
            }
        }

        TypeKind::Base(_)
        | TypeKind::Function
        | TypeKind::Enumeration {
            underlying: None, ..
        }
        | TypeKind::Pointer { .. }
        | TypeKind::Reference { .. }
        | TypeKind::Slice { .. }

        // A Fortran type may hold an allocatable array of itself, whose
        // elements are elsewhere.
        | TypeKind::RuntimeArray { .. }
        | TypeKind::Named { target: None, .. }
        | TypeKind::Unspecified
        | TypeKind::Opaque { .. } => {}
    }
}

/// The word a qualifier is written with, or `None` for `_Atomic`, which
/// wraps its type instead.
const fn modifier_keyword(modifier: TypeModifier) -> Option<&'static str> {
    Some(match modifier {
        TypeModifier::Const => "const",
        TypeModifier::Volatile => "volatile",
        TypeModifier::Restrict => "restrict",
        TypeModifier::Immutable => "immutable",
        TypeModifier::Packed => "packed",
        TypeModifier::Shared => "shared",
        TypeModifier::Atomic => return None,
    })
}

fn modifier_type_name(modifier: TypeModifier, target: &str, indirection: bool) -> String {
    let Some(keyword) = modifier_keyword(modifier) else {
        return format!("_Atomic({target})");
    };

    if indirection {
        format!("{target} {keyword}")
    } else {
        format!("{keyword} {target}")
    }
}
