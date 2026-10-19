//! The type graph: every type with its members, bases, variants,
//! enumerators, dimensions, parameters and identity, and the indexes that
//! find types by name, base, identity class, enumerator, and Go runtime
//! descriptor.
//!
//! Each type is one record whose kind says what its fields mean; lists
//! live in shared tables, and each type's spans follow the previous type's,
//! so that every graph has exactly one encoding. A type's references are
//! local identifiers; [`TypeTable`] binds them to a session's image.

use std::sync::{Arc, OnceLock};

use zerocopy::little_endian::{I128, U16, U32, U64, U128};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::functions::{
    FunctionRecord, GenericRecord, LocationRecord, language_code, language_of, valid_language,
};

use super::index::{self, NameEntry};
use super::strings::{StrId, Strings, StringsBuilder};
use super::{Builder, Image, NONE, Record, SharedRecord, Span, TableKind};
use crate::{
    Accessibility, ArgumentOrigin, ArrayBound, ArrayDimension, ArrayExtent, ArrayOrdering,
    BaseClass, BaseClassVirtuality, BaseType, BaseTypeEncoding, EnumerationOrigin, Enumerator,
    GoKind, GoTypeAttributes, IntegerValue, ModuleImageId, NamedTypeRelationship, RecordKind,
    RecordMember, RecordMemberLayout, ReferenceKind, RuntimeDimension, SliceWords, TypeArgument,
    TypeId, TypeIdentity, TypeInfo, TypeKind, TypeModifier, TypeNode, TypeReference, Variant,
    VariantDiscriminant, VariantSelection, VariantSelector, VariantStorageKind,
};

/// What a type's parts are: its members, enumerators, dimensions, or
/// parameters, by its kind, each in its own table.
pub enum TypeParts {}

/// One type. What each field holds depends on [`TypeRecord::kind`]; a
/// field its kind does not use is zero, or [`NONE`] for references.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct TypeRecord {
    /// The type's name; a malformed type's description.
    pub name: U32,

    /// A base type's or an enumeration's source name, or an opaque type's
    /// description.
    pub text: U32,

    /// A base type's or an enumeration's underlying base-type name.
    pub base_name: U32,

    /// The type's identity, or [`NONE`].
    pub identity: U32,

    /// The type a pointer, reference, array, slice, modifier or name
    /// stands for; an enumeration's underlying type; what a signature
    /// returns; a variant type's tag type. [`NONE`] when absent.
    pub target: U32,

    /// Members, enumerators, dimensions, or parameters.
    pub parts: Span<TypeParts>,

    /// Base classes.
    pub bases: Span<BaseRecord>,

    /// A variant type's variants.
    pub variants: Span<VariantRecord>,

    /// A variant type's stored discriminant member, or [`NONE`].
    pub discriminant: U32,
    pub byte_size: U64,

    /// A base type's or an enumeration's storage size, or a pointer's or
    /// reference's address class.
    pub value: U64,

    /// A base type's or an enumeration's bit size.
    pub bit_size: U64,
    pub flags: U16,
    pub kind: u8,

    /// The kind's own category: an encoding, modifier, relationship, or
    /// record, reference, or storage kind.
    pub detail: u8,
}

impl Record for TypeRecord {
    const KIND: TableKind = TableKind::Types;
}

impl TypeRecord {
    fn set(&mut self, flag: u16) {
        self.flags = (self.flags.get() | flag).into();
    }
}

/// [`TypeRecord::kind`].
pub mod kinds {
    pub const MALFORMED: u8 = 0;

    pub const BASE: u8 = 1;

    pub const ENUMERATION: u8 = 2;

    pub const POINTER: u8 = 3;

    pub const REFERENCE: u8 = 4;

    pub const ARRAY: u8 = 5;

    pub const SLICE: u8 = 6;

    pub const RECORD: u8 = 7;

    pub const UNION: u8 = 8;

    pub const VARIANT: u8 = 9;

    pub const MODIFIED: u8 = 10;

    pub const NAMED: u8 = 11;

    pub const UNSPECIFIED: u8 = 12;

    pub const FUNCTION: u8 = 13;

    pub const SIGNATURE: u8 = 14;

    pub const OPAQUE: u8 = 15;

    pub const RUNTIME_ARRAY: u8 = 16;
}

/// [`TypeRecord::flags`].
pub mod type_flags {
    pub const SIZED: u16 = 1 << 0;

    pub const BIT_SIZED: u16 = 1 << 1;

    pub const INCOMPLETE: u16 = 1 << 2;

    pub const SCOPED: u16 = 1 << 3;

    /// An enumeration of a named integer's associated constants.
    pub const NAMED_CONSTANTS: u16 = 1 << 4;

    pub const TEXT: u16 = 1 << 6;

    pub const VARIADIC: u16 = 1 << 7;

    pub const PROTOTYPED: u16 = 1 << 8;

    /// A variant type's discriminant is stored; with [`TAG_TYPE`], only
    /// its type is described; with neither, it is absent.
    pub const STORED: u16 = 1 << 9;

    pub const TAG_TYPE: u16 = 1 << 10;
}

/// An exact integer, signed or not.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct IntegerRecord {
    pub bits: U128,
    pub signed: u8,
}

impl IntegerRecord {
    const ZERO: Self = Self {
        bits: U128::ZERO,
        signed: 0,
    };

    fn of(value: IntegerValue) -> Self {
        match value {
            IntegerValue::Signed(value) => Self {
                bits: value.cast_unsigned().into(),
                signed: 1,
            },
            IntegerValue::Unsigned(value) => Self {
                bits: value.into(),
                signed: 0,
            },
        }
    }

    const fn get(self) -> IntegerValue {
        if self.signed == 0 {
            IntegerValue::Unsigned(self.bits.get())
        } else {
            IntegerValue::Signed(self.bits.get().cast_signed())
        }
    }
}

/// One member of a record, union, or variant.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct MemberRecord {
    /// The member's name, or [`NONE`] when anonymous.
    pub name: U32,
    pub ty: U32,

    /// The byte offset, or the bit offset of a bit range.
    pub offset: U64,
    pub bit_size: U64,
    pub declaration: LocationRecord,
    pub layout: u8,
    pub accessibility: u8,
    pub flags: u8,
}

impl Record for MemberRecord {
    const KIND: TableKind = TableKind::TypeMembers;
}

/// [`MemberRecord::flags`].
pub mod member_flags {
    pub const ARTIFICIAL: u8 = 1 << 0;

    pub const EMBEDDED: u8 = 1 << 1;

    pub const ALL: u8 = ARTIFICIAL | EMBEDDED;
}

const BYTE_OFFSET: u8 = 0;
const BIT_RANGE: u8 = 1;
const RUNTIME: u8 = 2;

/// One base class.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct BaseRecord {
    pub ty: U32,
    pub offset: U64,
    pub bit_size: U64,
    pub layout: u8,
    pub accessibility: u8,
    pub virtual_base: u8,
}

impl Record for BaseRecord {
    const KIND: TableKind = TableKind::TypeBases;
}

/// One variant of a variant type.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct VariantRecord {
    /// The variant's name, or [`NONE`].
    pub name: U32,
    pub selectors: Span<SelectorRecord>,
    pub members: Span<MemberRecord>,

    /// Whether the variant is the default, which has no selectors.
    pub default: u8,
}

impl Record for VariantRecord {
    const KIND: TableKind = TableKind::TypeVariants;
}

/// One value or inclusive range that selects a variant.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct SelectorRecord {
    pub low: IntegerRecord,

    /// A range's last value; zero for one value.
    pub high: IntegerRecord,
    pub range: u8,
}

impl Record for SelectorRecord {
    const KIND: TableKind = TableKind::TypeSelectors;
}

/// One enumerator.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct EnumeratorRecord {
    pub name: U32,
    pub value: IntegerRecord,
}

impl Record for EnumeratorRecord {
    const KIND: TableKind = TableKind::TypeEnumerators;
}

/// One array dimension.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct DimensionRecord {
    pub lower_bound: I128,
    pub count: U64,
}

impl Record for DimensionRecord {
    const KIND: TableKind = TableKind::TypeDimensions;
}

/// One bound of an array dimension bounded at run time.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct BoundRecord {
    /// A constant's value, or a variable's offset in `.debug_info`.
    pub value: I128,

    /// What [`bound_kinds`] says the bound is.
    pub kind: u8,

    /// How many bytes a computed or stored bound has.
    pub byte_size: u8,
    pub signed: u8,
}

/// [`BoundRecord::kind`].
pub mod bound_kinds {
    pub const NONE: u8 = 0;

    pub const CONSTANT: u8 = 1;

    pub const COMPUTED: u8 = 2;

    pub const STORED: u8 = 3;

    pub const VARIABLE: u8 = 4;
}

/// [`RuntimeDimensionRecord::ends`].
pub mod ends {
    pub const UPPER: u8 = 0;

    pub const COUNT: u8 = 1;

    pub const UNKNOWN: u8 = 2;
}

/// One dimension of an array bounded at run time.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct RuntimeDimensionRecord {
    pub lower: BoundRecord,
    pub extent: BoundRecord,

    /// The byte stride, or [`bound_kinds::NONE`].
    pub stride: BoundRecord,

    /// What [`ends`] says the extent is.
    pub ends: u8,
}

impl Record for RuntimeDimensionRecord {
    const KIND: TableKind = TableKind::RuntimeDimensions;
}

impl BoundRecord {
    const ABSENT: Self = Self {
        value: I128::ZERO,
        kind: bound_kinds::NONE,
        byte_size: 0,
        signed: 0,
    };

    fn of(bound: ArrayBound) -> Self {
        let sized = |kind, byte_size, signed: bool| Self {
            value: I128::ZERO,
            kind,
            byte_size,
            signed: u8::from(signed),
        };

        match bound {
            ArrayBound::Constant(value) => Self {
                value: value.into(),
                kind: bound_kinds::CONSTANT,
                byte_size: 0,
                signed: 0,
            },
            ArrayBound::Computed { byte_size, signed } => {
                sized(bound_kinds::COMPUTED, byte_size, signed)
            }
            ArrayBound::Stored { byte_size, signed } => {
                sized(bound_kinds::STORED, byte_size, signed)
            }
            ArrayBound::Variable { debug_info_offset } => Self {
                value: i128::from(debug_info_offset).into(),
                kind: bound_kinds::VARIABLE,
                byte_size: 0,
                signed: 0,
            },
        }
    }

    /// The bound, when the record is a valid one.
    fn bound(self) -> Option<ArrayBound> {
        let value = self.value.get();
        let sized = (1..=16).contains(&self.byte_size) && self.signed <= 1;
        let plain = self.byte_size == 0 && self.signed == 0;
        match self.kind {
            bound_kinds::CONSTANT if plain => Some(ArrayBound::Constant(value)),
            bound_kinds::COMPUTED if sized && value == 0 => Some(ArrayBound::Computed {
                byte_size: self.byte_size,
                signed: self.signed == 1,
            }),
            bound_kinds::STORED if sized && value == 0 => Some(ArrayBound::Stored {
                byte_size: self.byte_size,
                signed: self.signed == 1,
            }),
            bound_kinds::VARIABLE if plain => Some(ArrayBound::Variable {
                debug_info_offset: u64::try_from(value).ok()?,
            }),
            _ => None,
        }
    }

    fn absent(self) -> bool {
        self == Self::ABSENT
    }
}

impl RuntimeDimensionRecord {
    fn of(dimension: &RuntimeDimension) -> Self {
        let (ends, extent) = match dimension.extent {
            ArrayExtent::Upper(bound) => (ends::UPPER, BoundRecord::of(bound)),
            ArrayExtent::Count(bound) => (ends::COUNT, BoundRecord::of(bound)),
            ArrayExtent::Unknown => (ends::UNKNOWN, BoundRecord::ABSENT),
        };

        Self {
            lower: BoundRecord::of(dimension.lower_bound),
            extent,
            stride: dimension
                .byte_stride
                .map_or(BoundRecord::ABSENT, BoundRecord::of),
            ends,
        }
    }

    /// The dimension, when the record is a valid one.
    fn dimension(self) -> Option<RuntimeDimension> {
        let extent = match self.ends {
            ends::UPPER => ArrayExtent::Upper(self.extent.bound()?),
            ends::COUNT => ArrayExtent::Count(self.extent.bound()?),
            ends::UNKNOWN if self.extent.absent() => ArrayExtent::Unknown,
            _ => return None,
        };

        let byte_stride = if self.stride.absent() {
            None
        } else {
            Some(self.stride.bound()?)
        };

        Some(RuntimeDimension {
            lower_bound: self.lower.bound()?,
            extent,
            byte_stride,
        })
    }
}

/// One value of a list of numbers: a signature's parameter types, an
/// identity's path segments, or a type's identity class.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Item {
    pub value: U32,
}

impl SharedRecord for Item {
    const NAME: &'static str = "Item";
}

/// What a named type is.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct IdentityRecord {
    /// The enclosing scopes and the inline namespaces among them, as
    /// strings in [`TableKind::IdentityStrings`].
    pub path: Span<Item>,
    pub inline: Span<Item>,
    pub base: U32,
    pub arguments: Span<ArgumentRecord>,

    /// Where a template parameter pack begins among the arguments, or
    /// [`NONE`].
    pub pack: U32,
    pub runtime_type: U64,
    pub other_language: U16,
    pub language: u8,
    pub origin: u8,

    /// Go's kind by `internal/abi.Kind`'s number.
    pub go_kind: u8,
    pub flags: u8,
}

impl Record for IdentityRecord {
    const KIND: TableKind = TableKind::TypeIdentities;
}

/// [`IdentityRecord::flags`].
pub mod identity_flags {
    pub const GO: u8 = 1 << 0;

    pub const RUNTIME_TYPE: u8 = 1 << 1;

    pub const ALL: u8 = GO | RUNTIME_TYPE;
}

/// One template or generic argument.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ArgumentRecord {
    pub value: IntegerRecord,

    /// A type argument's type, or an unknown argument's text.
    pub reference: U32,
    pub kind: u8,
}

impl Record for ArgumentRecord {
    const KIND: TableKind = TableKind::TypeArguments;
}

const TYPE_ARGUMENT: u8 = 0;
const VALUE_ARGUMENT: u8 = 1;
const UNKNOWN_ARGUMENT: u8 = 2;

/// The first type, in identifier order, a Go runtime type descriptor's
/// offset describes.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct RuntimeTypeRecord {
    pub offset: U64,
    pub ty: U32,
}

impl Record for RuntimeTypeRecord {
    const KIND: TableKind = TableKind::GoRuntimeTypes;
}

const ENCODINGS: [BaseTypeEncoding; 7] = [
    BaseTypeEncoding::Boolean,
    BaseTypeEncoding::Signed,
    BaseTypeEncoding::SignedCharacter,
    BaseTypeEncoding::Unsigned,
    BaseTypeEncoding::UnsignedCharacter,
    BaseTypeEncoding::Floating,
    BaseTypeEncoding::ComplexFloating,
];
const MODIFIERS: [TypeModifier; 7] = [
    TypeModifier::Const,
    TypeModifier::Volatile,
    TypeModifier::Restrict,
    TypeModifier::Atomic,
    TypeModifier::Immutable,
    TypeModifier::Packed,
    TypeModifier::Shared,
];
const RELATIONSHIPS: [NamedTypeRelationship; 4] = [
    NamedTypeRelationship::Synonym,
    NamedTypeRelationship::Distinct,
    NamedTypeRelationship::Encoding,
    NamedTypeRelationship::Unspecified,
];
const REFERENCES: [ReferenceKind; 2] = [ReferenceKind::Lvalue, ReferenceKind::Rvalue];
const RECORDS: [RecordKind; 2] = [RecordKind::Struct, RecordKind::Class];
const ORDERINGS: [ArrayOrdering; 2] = [ArrayOrdering::RowMajor, ArrayOrdering::ColumnMajor];
const STORAGE: [VariantStorageKind; 3] = [
    VariantStorageKind::Struct,
    VariantStorageKind::Class,
    VariantStorageKind::Union,
];
const ACCESSIBILITY: [Accessibility; 3] = [
    Accessibility::Public,
    Accessibility::Protected,
    Accessibility::Private,
];
const ORIGINS: [ArgumentOrigin; 3] = [
    ArgumentOrigin::Dwarf,
    ArgumentOrigin::ParsedName,
    ArgumentOrigin::None,
];

fn code<T: PartialEq>(known: &[T], value: &T) -> Result<u8, TooMany> {
    super::functions::code_of(known, value).ok_or(TooMany)
}

/// What [`add_to`] encodes.
#[derive(Debug)]
pub struct Types<'a> {
    /// Every type, each at its identifier's index.
    pub nodes: &'a [TypeNode],

    /// Each type's identity class: two types have one class when they are
    /// the same type, as one type defined in several units is.
    pub classes: &'a [u32],
}

/// Why types could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the types do not fit an image")]
pub struct TooMany;

fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

/// The tables [`add_to`] fills, as it fills them.
#[derive(Default)]
struct Encoder {
    types: Vec<TypeRecord>,
    members: Vec<MemberRecord>,
    bases: Vec<BaseRecord>,
    variants: Vec<VariantRecord>,
    selectors: Vec<SelectorRecord>,
    enumerators: Vec<EnumeratorRecord>,
    dimensions: Vec<DimensionRecord>,
    runtime_dimensions: Vec<RuntimeDimensionRecord>,
    parameters: Vec<Item>,
    identities: Vec<IdentityRecord>,
    identity_strings: Vec<Item>,
    arguments: Vec<ArgumentRecord>,
}

fn reference(reference: Option<TypeReference>) -> U32 {
    reference
        .map_or(NONE, |reference| reference.id.get())
        .into()
}

impl Encoder {
    fn text(strings: &mut StringsBuilder, text: &str) -> Result<U32, TooMany> {
        Ok(strings.push(text).ok_or(TooMany)?.0.into())
    }

    fn member(
        &mut self,
        strings: &mut StringsBuilder,
        member: &RecordMember,
    ) -> Result<(), TooMany> {
        let (layout, offset, bit_size) = layout(member.layout);
        let mut flags = 0;

        if member.artificial {
            flags |= member_flags::ARTIFICIAL;
        }

        if member.embedded {
            flags |= member_flags::EMBEDDED;
        }

        self.members.push(MemberRecord {
            name: match &member.name {
                Some(name) => Self::text(strings, name)?,
                None => NONE.into(),
            },
            ty: member.type_ref.id.get().into(),
            offset: offset.into(),
            bit_size: bit_size.into(),
            declaration: LocationRecord::of(member.declaration.as_ref()),
            layout,
            accessibility: code(&ACCESSIBILITY, &member.accessibility)?,
            flags,
        });
        Ok(())
    }

    /// Adds members, returning their span.
    fn members(
        &mut self,
        strings: &mut StringsBuilder,
        members: &[RecordMember],
    ) -> Result<Span<MemberRecord>, TooMany> {
        let first = number(self.members.len())?;
        for member in members {
            self.member(strings, member)?;
        }

        Ok(Span::new(first, number(members.len())?))
    }

    fn bases(&mut self, bases: &[BaseClass]) -> Result<Span<BaseRecord>, TooMany> {
        let first = number(self.bases.len())?;
        for base in bases {
            let (layout, offset, bit_size) = layout(base.layout);
            self.bases.push(BaseRecord {
                ty: base.type_ref.id.get().into(),
                offset: offset.into(),
                bit_size: bit_size.into(),
                layout,
                accessibility: code(&ACCESSIBILITY, &base.accessibility)?,
                virtual_base: u8::from(base.virtuality == BaseClassVirtuality::Virtual),
            });
        }

        Ok(Span::new(first, number(bases.len())?))
    }

    fn base_type(
        record: &mut TypeRecord,
        strings: &mut StringsBuilder,
        base: &BaseType,
    ) -> Result<(), TooMany> {
        record.text = Self::text(strings, &base.name)?;
        record.base_name = Self::text(strings, &base.base_name)?;
        record.detail = code(&ENCODINGS, &base.encoding)?;
        record.value = base.byte_size.into();
        if let Some(bits) = base.bit_size {
            record.bit_size = bits.into();
            record.set(type_flags::BIT_SIZED);
        }

        Ok(())
    }

    fn identity(
        &mut self,
        strings: &mut StringsBuilder,
        identity: &TypeIdentity,
    ) -> Result<u32, TooMany> {
        let mut segments =
            |segments: &[Arc<str>], this: &mut Self| -> Result<Span<Item>, TooMany> {
                let first = number(this.identity_strings.len())?;
                for segment in segments {
                    this.identity_strings.push(Item {
                        value: Self::text(strings, segment)?,
                    });
                }

                Ok(Span::new(first, number(segments.len())?))
            };
        let path = segments(&identity.path, self)?;
        let inline = segments(&identity.inline_namespaces, self)?;
        let arguments = number(self.arguments.len())?;

        for argument in identity.arguments.iter() {
            self.arguments.push(match argument {
                TypeArgument::Type(ty) => ArgumentRecord {
                    value: IntegerRecord::ZERO,
                    reference: ty.id.get().into(),
                    kind: TYPE_ARGUMENT,
                },
                TypeArgument::Value(value) => ArgumentRecord {
                    value: IntegerRecord::of(*value),
                    reference: NONE.into(),
                    kind: VALUE_ARGUMENT,
                },
                TypeArgument::Unknown(text) => ArgumentRecord {
                    value: IntegerRecord::ZERO,
                    reference: Self::text(strings, text)?,
                    kind: UNKNOWN_ARGUMENT,
                },
            });
        }

        let (language, other_language) = language_code(identity.language).ok_or(TooMany)?;
        let mut flags = 0;
        let (go_kind, runtime_type) = match identity.go {
            Some(go) => {
                flags |= identity_flags::GO;
                if go.runtime_type.is_some() {
                    flags |= identity_flags::RUNTIME_TYPE;
                }

                (go_kind_code(go.kind)?, go.runtime_type.unwrap_or(0))
            }
            None => (0, 0),
        };

        let index = number(self.identities.len())?;
        self.identities.push(IdentityRecord {
            path,
            inline,
            base: Self::text(strings, &identity.base)?,
            arguments: Span::new(arguments, number(identity.arguments.len())?),
            pack: match identity.pack {
                Some(pack) => number(pack)?,
                None => NONE,
            }
            .into(),
            runtime_type: runtime_type.into(),
            other_language: other_language.into(),
            language,
            origin: code(&ORIGINS, &identity.origin)?,
            go_kind,
            flags,
        });
        Ok(index)
    }

    #[expect(clippy::too_many_lines, reason = "one arm for each kind of type")]
    fn add(&mut self, strings: &mut StringsBuilder, node: &TypeNode) -> Result<(), TooMany> {
        let mut record = TypeRecord {
            name: NONE.into(),
            text: NONE.into(),
            base_name: NONE.into(),
            identity: NONE.into(),
            target: NONE.into(),
            parts: Span::new(0, 0),
            bases: Span::new(0, 0),
            variants: Span::new(0, 0),
            discriminant: NONE.into(),
            byte_size: 0.into(),
            value: 0.into(),
            bit_size: 0.into(),
            flags: 0.into(),
            kind: kinds::MALFORMED,
            detail: 0,
        };

        let info = match node {
            TypeNode::Malformed { description, .. } => {
                record.name = Self::text(strings, description)?;
                self.types.push(record);
                return Ok(());
            }
            TypeNode::Resolved(info) => info,
        };

        record.name = Self::text(strings, &info.name)?;
        if let Some(size) = info.byte_size {
            record.byte_size = size.into();
            record.set(type_flags::SIZED);
        }

        // A list starts where the previous type's ended, even when empty.
        let flag = |record: &mut TypeRecord, set: bool, flag: u16| {
            if set {
                record.set(flag);
            }
        };

        match &info.kind {
            TypeKind::Base(base) => {
                record.kind = kinds::BASE;
                Self::base_type(&mut record, strings, base)?;
            }

            TypeKind::Enumeration {
                representation,
                underlying,
                enumerators,
                origin,
                scoped,
            } => {
                record.kind = kinds::ENUMERATION;
                Self::base_type(&mut record, strings, representation)?;
                record.target = reference(*underlying);
                record.parts =
                    Span::new(number(self.enumerators.len())?, number(enumerators.len())?);
                for enumerator in enumerators.iter() {
                    self.enumerators.push(EnumeratorRecord {
                        name: Self::text(strings, &enumerator.name)?,
                        value: IntegerRecord::of(enumerator.value),
                    });
                }

                flag(
                    &mut record,
                    *origin == EnumerationOrigin::NamedConstants,
                    type_flags::NAMED_CONSTANTS,
                );
                flag(&mut record, *scoped, type_flags::SCOPED);
            }

            TypeKind::Pointer {
                target,
                address_class,
            } => {
                record.kind = kinds::POINTER;
                record.target = reference(*target);
                record.value = (*address_class).into();
            }

            TypeKind::Reference {
                kind,
                target,
                address_class,
            } => {
                record.kind = kinds::REFERENCE;
                record.detail = code(&REFERENCES, kind)?;
                record.target = reference(Some(*target));
                record.value = (*address_class).into();
            }

            TypeKind::Array {
                element,
                dimensions,
                ordering,
            } => {
                record.kind = kinds::ARRAY;
                record.detail = code(&ORDERINGS, ordering)?;
                record.target = reference(Some(*element));
                record.parts = Span::new(number(self.dimensions.len())?, number(dimensions.len())?);
                self.dimensions
                    .extend(dimensions.iter().map(|dimension| DimensionRecord {
                        lower_bound: dimension.lower_bound.into(),
                        count: dimension.count.into(),
                    }));
            }

            TypeKind::RuntimeArray {
                element,
                dimensions,
                ordering,
            } => {
                record.kind = kinds::RUNTIME_ARRAY;
                record.detail = code(&ORDERINGS, ordering)?;
                record.target = reference(Some(*element));
                record.parts = Span::new(
                    number(self.runtime_dimensions.len())?,
                    number(dimensions.len())?,
                );
                self.runtime_dimensions
                    .extend(dimensions.iter().map(RuntimeDimensionRecord::of));
            }

            TypeKind::Slice {
                element,
                words,
                text,
            } => {
                record.kind = kinds::SLICE;
                record.target = reference(Some(*element));
                record.value = slice_words(*words).into();
                flag(&mut record, *text, type_flags::TEXT);
            }

            TypeKind::Record {
                kind,
                members,
                bases,
                incomplete,
            } => {
                record.kind = kinds::RECORD;
                record.detail = code(&RECORDS, kind)?;
                record.parts = self.members(strings, members)?.cast();
                record.bases = self.bases(bases)?;
                flag(&mut record, *incomplete, type_flags::INCOMPLETE);
            }

            TypeKind::Union {
                members,
                incomplete,
            } => {
                record.kind = kinds::UNION;
                record.parts = self.members(strings, members)?.cast();
                flag(&mut record, *incomplete, type_flags::INCOMPLETE);
            }

            TypeKind::Variant {
                storage,
                common_members,
                bases,
                discriminant,
                variants,
                incomplete,
            } => {
                record.kind = kinds::VARIANT;
                record.detail = code(&STORAGE, storage)?;
                record.parts = self.members(strings, common_members)?.cast();
                record.bases = self.bases(bases)?;
                record.variants = Span::new(number(self.variants.len())?, number(variants.len())?);
                for variant in variants.iter() {
                    let selectors = number(self.selectors.len())?;
                    let (default, selector_count) = match &variant.selection {
                        VariantSelection::Default => (1, 0),
                        VariantSelection::Selectors(selectors) => {
                            for selector in selectors.iter() {
                                self.selectors.push(match selector {
                                    VariantSelector::Value(value) => SelectorRecord {
                                        low: IntegerRecord::of(*value),
                                        high: IntegerRecord::ZERO,
                                        range: 0,
                                    },
                                    VariantSelector::Range { low, high } => SelectorRecord {
                                        low: IntegerRecord::of(*low),
                                        high: IntegerRecord::of(*high),
                                        range: 1,
                                    },
                                });
                            }

                            (0, number(selectors.len())?)
                        }
                    };

                    // A variant's members follow the previous variant's.
                    let members = self.members(strings, &variant.members)?;
                    self.variants.push(VariantRecord {
                        name: match &variant.name {
                            Some(name) => Self::text(strings, name)?,
                            None => NONE.into(),
                        },
                        selectors: Span::new(selectors, selector_count),
                        members,
                        default,
                    });
                }

                match discriminant.as_ref() {
                    VariantDiscriminant::Stored(member) => {
                        record.discriminant = number(self.members.len())?.into();
                        self.member(strings, member)?;
                        record.set(type_flags::STORED);
                    }
                    VariantDiscriminant::TagType(tag) => {
                        record.target = reference(Some(*tag));
                        record.set(type_flags::TAG_TYPE);
                    }
                    VariantDiscriminant::Absent => {}
                }

                flag(&mut record, *incomplete, type_flags::INCOMPLETE);
            }
            TypeKind::Modified { modifier, target } => {
                record.kind = kinds::MODIFIED;
                record.detail = code(&MODIFIERS, modifier)?;
                record.target = reference(Some(*target));
            }

            TypeKind::Named {
                target,
                relationship,
            } => {
                record.kind = kinds::NAMED;
                record.detail = code(&RELATIONSHIPS, relationship)?;
                record.target = reference(*target);
            }
            TypeKind::Unspecified => record.kind = kinds::UNSPECIFIED,
            TypeKind::Function => record.kind = kinds::FUNCTION,
            TypeKind::Signature {
                returns,
                parameters,
                variadic,
                prototyped,
            } => {
                record.kind = kinds::SIGNATURE;
                record.target = reference(*returns);
                record.parts = Span::new(number(self.parameters.len())?, number(parameters.len())?);
                self.parameters
                    .extend(parameters.iter().map(|parameter| Item {
                        value: parameter.id.get().into(),
                    }));
                flag(&mut record, *variadic, type_flags::VARIADIC);
                flag(&mut record, *prototyped, type_flags::PROTOTYPED);
            }
            TypeKind::Opaque { description } => {
                record.kind = kinds::OPAQUE;
                record.text = Self::text(strings, description)?;
            }
        }

        if let Some(identity) = &info.identity {
            record.identity = self.identity(strings, identity)?.into();
        }

        self.types.push(record);
        Ok(())
    }
}

/// Where a slice descriptor keeps its parts, as a type record's value
/// holds it: the data's word, the length's, and the capacity's, a byte
/// each, the capacity's [`NO_SLICE_WORD`] when it has none.
fn slice_words(words: SliceWords) -> u64 {
    u64::from(words.data)
        | u64::from(words.length) << 8
        | u64::from(words.capacity.unwrap_or(NO_SLICE_WORD)) << 16
}

/// The words [`slice_words`] encodes.
fn slice_words_of(value: u64) -> SliceWords {
    let [data, length, capacity, ..] = value.to_le_bytes();
    SliceWords {
        data,
        length,
        capacity: Some(capacity).filter(|word| *word != NO_SLICE_WORD),
    }
}

/// Whether a type record's value is slice words [`slice_words`] made.
fn valid_slice_words(value: u64) -> bool {
    value >> 24 == 0 && slice_words(slice_words_of(value)) == value
}

/// The capacity's word of a slice descriptor that has none.
const NO_SLICE_WORD: u8 = 0xff;

const fn layout(layout: RecordMemberLayout) -> (u8, u64, u64) {
    match layout {
        RecordMemberLayout::ByteOffset(offset) => (BYTE_OFFSET, offset, 0),
        RecordMemberLayout::BitRange {
            bit_offset,
            bit_size,
        } => (BIT_RANGE, bit_offset, bit_size),
        RecordMemberLayout::Runtime => (RUNTIME, 0, 0),
    }
}

/// Go's kind as `internal/abi.Kind` numbers it. A kind this version
/// knows by name is never another kind's number.
fn go_kind_code(kind: GoKind) -> Result<u8, TooMany> {
    let code = match kind {
        GoKind::Other(code) => code,
        kind => (1..=26)
            .find(|code| GoKind::from_abi(*code) == kind)
            .ok_or(TooMany)?,
    };

    // An unknown kind's number decodes to it again.
    (GoKind::from_abi(code) == kind)
        .then_some(code)
        .ok_or(TooMany)
}

/// Adds `types` and their indexes to `builder`, pooling text in
/// `strings`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    types: &Types<'_>,
) -> Result<(), TooMany> {
    assert_eq!(
        types.nodes.len(),
        types.classes.len(),
        "one class for each type"
    );
    let mut encoder = Encoder::default();
    for node in types.nodes {
        encoder.add(strings, node)?;
    }

    let mut names = Vec::new();
    let mut bases = Vec::new();
    let mut enumerators = Vec::new();
    let mut runtime_types = Vec::<RuntimeTypeRecord>::new();
    let mut base_keys = Vec::new();

    for (index, node) in types.nodes.iter().enumerate() {
        let TypeNode::Resolved(info) = node else {
            continue;
        };

        let id = number(index)?;
        let record = &encoder.types[index];
        names.push((&*info.name, StrId(record.name.get()), id));
        if let Some(identity) = &info.identity {
            let base = encoder.identities[record.identity.get() as usize].base;
            bases.push((&*identity.base, StrId(base.get()), id));
            if let Some(key) = crate::eval::types::c_type_key_of_name(&identity.base)
                && key != identity.base.as_ref()
            {
                base_keys.push((key, id));
            }

            if let Some(offset) = identity.go.and_then(|go| go.runtime_type) {
                runtime_types.push(RuntimeTypeRecord {
                    offset: offset.into(),
                    ty: id.into(),
                });
            }
        }

        if let TypeKind::Enumeration {
            enumerators: listed,
            ..
        } = &info.kind
        {
            let first = record.parts.first() as usize;
            let mut seen = foldhash::HashSet::default();
            for (enumerator, encoded) in listed.iter().zip(&encoder.enumerators[first..]) {
                if seen.insert(&enumerator.name) {
                    enumerators.push((&*enumerator.name, StrId(encoded.name.get()), id));
                }
            }
        }
    }

    let base_keys = base_keys
        .iter()
        .map(|(key, id)| Ok((key.as_str(), StrId(Encoder::text(strings, key)?.get()), *id)))
        .collect::<Result<Vec<_>, TooMany>>()?;
    bases.extend(base_keys);

    // The first type for each offset, in identifier order.
    runtime_types.sort_by_key(|record| record.offset.get());
    runtime_types.dedup_by_key(|record| record.offset.get());

    let classes = types
        .classes
        .iter()
        .map(|class| Item {
            value: (*class).into(),
        })
        .collect::<Vec<_>>();

    builder
        .owned_table(encoder.types)
        .owned_table(encoder.members)
        .owned_table(encoder.bases)
        .owned_table(encoder.variants)
        .owned_table(encoder.selectors)
        .owned_table(encoder.enumerators)
        .owned_table(encoder.dimensions)
        .owned_table(encoder.runtime_dimensions)
        .owned_shared(TableKind::TypeParameters, encoder.parameters)
        .owned_table(encoder.identities)
        .owned_shared(TableKind::IdentityStrings, encoder.identity_strings)
        .owned_table(encoder.arguments)
        .owned_shared(TableKind::TypeNames, index::names(names))
        .owned_shared(TableKind::TypeBaseNames, index::names(bases))
        .owned_shared(TableKind::TypeClasses, classes)
        .owned_shared(TableKind::EnumeratorNames, index::names(enumerators))
        .owned_table(runtime_types);
    Ok(())
}

/// The types of a validated image.
#[derive(Debug, Clone, Copy)]
pub struct TypeView<'a> {
    strings: Strings<'a>,
    types: &'a [TypeRecord],
    members: &'a [MemberRecord],
    bases: &'a [BaseRecord],
    variants: &'a [VariantRecord],
    selectors: &'a [SelectorRecord],
    enumerators: &'a [EnumeratorRecord],
    dimensions: &'a [DimensionRecord],
    runtime_dimensions: &'a [RuntimeDimensionRecord],
    parameters: &'a [Item],
    identities: &'a [IdentityRecord],
    identity_strings: &'a [Item],
    arguments: &'a [ArgumentRecord],
    names: &'a [NameEntry],
    base_names: &'a [NameEntry],
    classes: &'a [Item],
    enumerator_names: &'a [NameEntry],
    runtime_types: &'a [RuntimeTypeRecord],
}

impl<'a> TypeView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            types: image.table(),
            members: image.table(),
            bases: image.table(),
            variants: image.table(),
            selectors: image.table(),
            enumerators: image.table(),
            dimensions: image.table(),
            runtime_dimensions: image.table(),
            parameters: image.shared(TableKind::TypeParameters),
            identities: image.table(),
            identity_strings: image.shared(TableKind::IdentityStrings),
            arguments: image.table(),
            names: image.shared(TableKind::TypeNames),
            base_names: image.shared(TableKind::TypeBaseNames),
            classes: image.shared(TableKind::TypeClasses),
            enumerator_names: image.shared(TableKind::EnumeratorNames),
            runtime_types: image.table(),
        }
    }

    /// How many types there are.
    pub const fn len(self) -> usize {
        self.types.len()
    }

    /// The resolved types named exactly `name`, in identifier order.
    pub fn named(self, name: &str) -> impl Iterator<Item = TypeId> + 'a {
        index::named(self.strings, self.names, name).map(TypeId::new)
    }

    /// The types whose identity has base `base`, or whose base C spells
    /// so, in identifier order.
    pub fn with_base(self, base: &str) -> impl Iterator<Item = TypeId> + 'a {
        index::named(self.strings, self.base_names, base).map(TypeId::new)
    }

    /// The enumerations with an enumerator named `name`, in identifier
    /// order.
    pub fn with_enumerator(self, name: &str) -> impl Iterator<Item = TypeId> + 'a {
        index::named(self.strings, self.enumerator_names, name).map(TypeId::new)
    }

    /// The type's identity class.
    pub fn class(self, id: TypeId) -> Option<u32> {
        Some(self.classes.get(id.index())?.value.get())
    }

    /// The first type a Go runtime type descriptor's offset describes.
    pub fn go_runtime_type(self, offset: u64) -> Option<TypeId> {
        let index = self
            .runtime_types
            .binary_search_by_key(&offset, |record| record.offset.get())
            .ok()?;
        Some(TypeId::new(self.runtime_types[index].ty.get()))
    }

    /// Every Go runtime type descriptor offset, with the first type it
    /// describes, in offset order.
    #[cfg(feature = "tools")]
    pub fn go_runtime_types(self) -> impl Iterator<Item = (u64, TypeId)> + 'a {
        self.runtime_types
            .iter()
            .map(|record| (record.offset.get(), TypeId::new(record.ty.get())))
    }

    fn text(self, id: U32) -> Arc<str> {
        self.strings.get(StrId(id.get())).into()
    }

    /// The records and variants whose names `keep` accepts, in identifier
    /// order, without decoding them.
    pub fn aggregates_named(
        self,
        keep: impl Fn(&str) -> bool + 'a,
    ) -> impl Iterator<Item = TypeId> + 'a {
        (0_u32..)
            .zip(self.types)
            .filter(move |(_, record)| {
                matches!(record.kind, kinds::RECORD | kinds::VARIANT)
                    && keep(self.strings.get(StrId(record.name.get())))
            })
            .map(|(id, _)| TypeId::new(id))
    }

    fn optional_text(self, id: U32) -> Option<Arc<str>> {
        (id.get() != NONE).then(|| self.text(id))
    }

    /// The type `id` names, its references in image `image`.
    pub fn node(self, image: ModuleImageId, id: TypeId) -> Option<TypeNode> {
        let record = self.types.get(id.index())?;
        let reference = TypeReference { image, id };
        if record.kind == kinds::MALFORMED {
            return Some(TypeNode::Malformed {
                reference,
                description: self.text(record.name),
            });
        }

        let Some(kind) = self.kind(image, record) else {
            debug_assert!(
                false,
                "validation checked type {id:?}'s targets and dimensions"
            );
            return Some(TypeNode::Malformed {
                reference,
                description: "the image's record of the type does not decode".into(),
            });
        };

        Some(TypeNode::Resolved(TypeInfo {
            reference,
            name: self.text(record.name),
            byte_size: has(record, type_flags::SIZED).then(|| record.byte_size.get()),
            kind,
            identity: (record.identity.get() != NONE).then(|| {
                Arc::new(self.identity(image, &self.identities[record.identity.get() as usize]))
            }),
        }))
    }

    fn reference(image: ModuleImageId, id: U32) -> Option<TypeReference> {
        (id.get() != NONE).then(|| TypeReference {
            image,
            id: TypeId::new(id.get()),
        })
    }

    fn base_type(self, record: &TypeRecord) -> BaseType {
        BaseType {
            name: self.text(record.text),
            base_name: self.text(record.base_name),
            encoding: ENCODINGS[usize::from(record.detail)],
            byte_size: record.value.get(),
            bit_size: has(record, type_flags::BIT_SIZED).then(|| record.bit_size.get()),
        }
    }

    fn member(self, image: ModuleImageId, member: &MemberRecord) -> RecordMember {
        RecordMember {
            name: self.optional_text(member.name),
            type_ref: TypeReference {
                image,
                id: TypeId::new(member.ty.get()),
            },
            layout: member_layout(member.layout, member.offset.get(), member.bit_size.get()),
            accessibility: ACCESSIBILITY[usize::from(member.accessibility)],
            artificial: member.flags & member_flags::ARTIFICIAL != 0,
            embedded: member.flags & member_flags::EMBEDDED != 0,
            declaration: member.declaration.get(),
        }
    }

    fn members(self, image: ModuleImageId, members: Span<MemberRecord>) -> Arc<[RecordMember]> {
        members
            .rows(self.members)
            .iter()
            .map(|member| self.member(image, member))
            .collect()
    }

    fn bases(self, image: ModuleImageId, record: &TypeRecord) -> Arc<[BaseClass]> {
        record
            .bases
            .rows(self.bases)
            .iter()
            .map(|base| BaseClass {
                type_ref: TypeReference {
                    image,
                    id: TypeId::new(base.ty.get()),
                },
                layout: member_layout(base.layout, base.offset.get(), base.bit_size.get()),
                accessibility: ACCESSIBILITY[usize::from(base.accessibility)],
                virtuality: if base.virtual_base == 0 {
                    BaseClassVirtuality::None
                } else {
                    BaseClassVirtuality::Virtual
                },
            })
            .collect()
    }

    #[expect(clippy::too_many_lines, reason = "one arm for each kind of type")]
    fn kind(self, image: ModuleImageId, record: &TypeRecord) -> Option<TypeKind> {
        let target = Self::reference(image, record.target);
        let required = || target;
        let parts = record.parts.range();
        let incomplete = has(record, type_flags::INCOMPLETE);
        Some(match record.kind {
            kinds::BASE => TypeKind::Base(self.base_type(record)),
            kinds::ENUMERATION => TypeKind::Enumeration {
                representation: self.base_type(record),
                underlying: target,
                enumerators: self.enumerators[parts]
                    .iter()
                    .map(|enumerator| Enumerator {
                        name: self.text(enumerator.name),
                        value: enumerator.value.get(),
                    })
                    .collect(),
                origin: if has(record, type_flags::NAMED_CONSTANTS) {
                    EnumerationOrigin::NamedConstants
                } else {
                    EnumerationOrigin::Language
                },
                scoped: has(record, type_flags::SCOPED),
            },
            kinds::POINTER => TypeKind::Pointer {
                target,
                address_class: record.value.get(),
            },
            kinds::REFERENCE => TypeKind::Reference {
                kind: REFERENCES[usize::from(record.detail)],
                target: required()?,
                address_class: record.value.get(),
            },
            kinds::ARRAY => TypeKind::Array {
                element: required()?,
                dimensions: self.dimensions[parts]
                    .iter()
                    .map(|dimension| ArrayDimension {
                        lower_bound: dimension.lower_bound.get(),
                        count: dimension.count.get(),
                    })
                    .collect(),
                ordering: ORDERINGS[usize::from(record.detail)],
            },
            kinds::RUNTIME_ARRAY => TypeKind::RuntimeArray {
                element: required()?,
                dimensions: self.runtime_dimensions[parts]
                    .iter()
                    .map(|dimension| dimension.dimension())
                    .collect::<Option<_>>()?,
                ordering: ORDERINGS[usize::from(record.detail)],
            },
            kinds::SLICE => TypeKind::Slice {
                element: required()?,
                words: slice_words_of(record.value.get()),
                text: has(record, type_flags::TEXT),
            },
            kinds::RECORD => TypeKind::Record {
                kind: RECORDS[usize::from(record.detail)],
                members: self.members(image, record.parts.cast()),
                bases: self.bases(image, record),
                incomplete,
            },
            kinds::UNION => TypeKind::Union {
                members: self.members(image, record.parts.cast()),
                incomplete,
            },
            kinds::VARIANT => {
                let variants = record
                    .variants
                    .rows(self.variants)
                    .iter()
                    .map(|variant| Variant {
                        name: self.optional_text(variant.name),
                        selection: if variant.default == 0 {
                            VariantSelection::Selectors(
                                variant
                                    .selectors
                                    .rows(self.selectors)
                                    .iter()
                                    .map(|selector| {
                                        if selector.range == 0 {
                                            VariantSelector::Value(selector.low.get())
                                        } else {
                                            VariantSelector::Range {
                                                low: selector.low.get(),
                                                high: selector.high.get(),
                                            }
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            VariantSelection::Default
                        },
                        members: self.members(image, variant.members),
                    })
                    .collect();
                let discriminant = if has(record, type_flags::STORED) {
                    VariantDiscriminant::Stored(
                        self.member(image, &self.members[record.discriminant.get() as usize]),
                    )
                } else if has(record, type_flags::TAG_TYPE) {
                    VariantDiscriminant::TagType(required()?)
                } else {
                    VariantDiscriminant::Absent
                };

                TypeKind::Variant {
                    storage: STORAGE[usize::from(record.detail)],
                    common_members: self.members(image, record.parts.cast()),
                    bases: self.bases(image, record),
                    discriminant: Box::new(discriminant),
                    variants,
                    incomplete,
                }
            }
            kinds::MODIFIED => TypeKind::Modified {
                modifier: MODIFIERS[usize::from(record.detail)],
                target: required()?,
            },
            kinds::NAMED => TypeKind::Named {
                target,
                relationship: RELATIONSHIPS[usize::from(record.detail)],
            },
            kinds::UNSPECIFIED => TypeKind::Unspecified,
            kinds::FUNCTION => TypeKind::Function,
            kinds::SIGNATURE => TypeKind::Signature {
                returns: target,
                parameters: self.parameters[parts]
                    .iter()
                    .map(|parameter| TypeReference {
                        image,
                        id: TypeId::new(parameter.value.get()),
                    })
                    .collect(),
                variadic: has(record, type_flags::VARIADIC),
                prototyped: has(record, type_flags::PROTOTYPED),
            },
            _ => TypeKind::Opaque {
                description: self.text(record.text),
            },
        })
    }

    fn identity(self, image: ModuleImageId, record: &IdentityRecord) -> TypeIdentity {
        let segments = |segments: Span<Item>| -> Arc<[Arc<str>]> {
            segments
                .rows(self.identity_strings)
                .iter()
                .map(|segment| self.text(segment.value))
                .collect()
        };

        TypeIdentity {
            language: language_of(record.language, record.other_language.get()),
            path: segments(record.path),
            inline_namespaces: segments(record.inline),
            base: self.text(record.base),
            arguments: record
                .arguments
                .rows(self.arguments)
                .iter()
                .map(|argument| match argument.kind {
                    TYPE_ARGUMENT => TypeArgument::Type(TypeReference {
                        image,
                        id: TypeId::new(argument.reference.get()),
                    }),
                    VALUE_ARGUMENT => TypeArgument::Value(argument.value.get()),
                    _ => TypeArgument::Unknown(self.text(argument.reference)),
                })
                .collect(),
            pack: (record.pack.get() != NONE).then(|| record.pack.get() as usize),
            origin: ORIGINS[usize::from(record.origin)],
            go: (record.flags & identity_flags::GO != 0).then(|| GoTypeAttributes {
                kind: GoKind::from_abi(record.go_kind),
                runtime_type: (record.flags & identity_flags::RUNTIME_TYPE != 0)
                    .then(|| record.runtime_type.get()),
            }),
        }
    }
}

const fn has(record: &TypeRecord, flag: u16) -> bool {
    record.flags.get() & flag != 0
}

const fn member_layout(layout: u8, offset: u64, bit_size: u64) -> RecordMemberLayout {
    match layout {
        BYTE_OFFSET => RecordMemberLayout::ByteOffset(offset),
        BIT_RANGE => RecordMemberLayout::BitRange {
            bit_offset: offset,
            bit_size,
        },
        _ => RecordMemberLayout::Runtime,
    }
}

/// A validated image's types, decoded once each as they are asked for.
#[derive(Debug)]
pub struct TypeTable {
    image: Arc<Image>,
    id: ModuleImageId,
    decoded: Box<[OnceLock<Box<TypeNode>>]>,

    /// The coroutines, read from their types on the first question.
    coroutines:
        OnceLock<std::collections::BTreeMap<TypeId, Result<crate::CoroutineInfo, Arc<str>>>>,
}

impl TypeTable {
    /// The types of `image`, whose references name image `id`.
    pub fn new(image: Arc<Image>, id: ModuleImageId) -> Self {
        let count = TypeView::new(&image).len();
        Self {
            image,
            id,
            decoded: (0..count).map(|_| OnceLock::new()).collect(),
            coroutines: OnceLock::new(),
        }
    }

    /// The image holding the types, and the rest of its module's tables.
    pub fn tables(&self) -> &Image {
        &self.image
    }

    pub fn view(&self) -> TypeView<'_> {
        TypeView::new(&self.image)
    }

    /// The image whose types these are.
    pub const fn image(&self) -> ModuleImageId {
        self.id
    }

    pub fn len(&self) -> usize {
        self.decoded.len()
    }

    /// The type `id` names.
    pub fn node(&self, id: TypeId) -> Option<&TypeNode> {
        Some(self.decoded_node(self.decoded.get(id.index())?, id))
    }

    /// The type `id` names, decoded into `slot`, its slot.
    fn decoded_node<'s>(&'s self, slot: &'s OnceLock<Box<TypeNode>>, id: TypeId) -> &'s TypeNode {
        slot.get_or_init(|| {
            Box::new(self.view().node(self.id, id).unwrap_or_else(|| {
                debug_assert!(false, "type {id:?} has a slot but no record");
                TypeNode::Malformed {
                    reference: TypeReference { image: self.id, id },
                    description: "the image has no record of the type".into(),
                }
            }))
        })
    }

    /// The resolved type `reference` names, when it is this image's.
    pub fn info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        if reference.image != self.id {
            return None;
        }

        match self.node(reference.id)? {
            TypeNode::Resolved(info) => Some(info),
            TypeNode::Malformed { .. } => None,
        }
    }

    /// What the coroutine of type `id` is, or why its layout cannot be read
    /// as one; `None` for a type that is no coroutine.
    pub fn coroutine(&self, id: TypeId) -> Option<Result<&crate::CoroutineInfo, &Arc<str>>> {
        self.coroutines
            .get_or_init(|| crate::debug_info::coroutines::in_table(self))
            .get(&id)
            .map(Result::as_ref)
    }

    /// Every type, in identifier order.
    pub fn nodes(&self) -> impl ExactSizeIterator<Item = &TypeNode> + '_ {
        self.decoded
            .iter()
            .zip(super::ids(self.decoded.len()))
            .map(|(slot, id)| self.decoded_node(slot, TypeId::new(id)))
    }
}

impl crate::type_identity::TypeLookup for TypeTable {
    fn type_info(&self, reference: TypeReference) -> Option<&TypeInfo> {
        self.info(reference)
    }
}

/// Checks the types and their indexes.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    validate_types(image)?;
    validate_lists(image)?;
    validate_indexes(image)
}

/// Where each of a type's lists must begin: each list follows the one
/// before it in type order.
#[derive(Default)]
struct Cursors {
    members: u32,
    bases: u32,
    variants: u32,
    selectors: u32,
    enumerators: u32,
    dimensions: u32,
    runtime_dimensions: u32,
    parameters: u32,
    identities: u32,
}

/// Advances `cursor` past a list, failing unless the list begins at it.
const fn follows<T>(cursor: &mut u32, list: Span<T>) -> bool {
    let fits = list.first() == *cursor;
    *cursor = cursor.saturating_add(list.count());
    fits
}

const fn valid_type(id: U32, types: usize) -> bool {
    (id.get() as usize) < types
}

const fn valid_optional_type(id: U32, types: usize) -> bool {
    id.get() == NONE || valid_type(id, types)
}

#[expect(clippy::too_many_lines, reason = "one rule for each kind of type")]
fn validate_types(image: &Image) -> Result<(), String> {
    use type_flags as f;
    let strings = image.strings();
    let types = image.table::<TypeRecord>();
    let members = image.table::<MemberRecord>();
    let variants = image.table::<VariantRecord>();
    let runtime_dimensions = image.table::<RuntimeDimensionRecord>();
    let count = types.len();
    let text = |id: U32| strings.contains(StrId(id.get()));
    let mut cursors = Cursors::default();

    for record in types {
        let flags = record.flags.get();
        let only = |allowed: u16| flags & !allowed == 0;
        let unused = |id: U32| id.get() == NONE;
        let none = |value: U64| value.get() == 0;
        let no_text = unused(record.text) && unused(record.base_name);
        let base_type = || {
            text(record.text)
                && text(record.base_name)
                && usize::from(record.detail) < ENCODINGS.len()
                && (flags & f::BIT_SIZED != 0 || none(record.bit_size))
        };

        let plain = no_text
            && none(record.value)
            && none(record.bit_size)
            && record.detail == 0
            && record.parts == Span::EMPTY;
        let no_bases = record.bases == Span::EMPTY;
        let no_variants = record.variants == Span::EMPTY && unused(record.discriminant);

        if !text(record.name) {
            return Err("a type is malformed".into());
        }

        let valid = match record.kind {
            kinds::MALFORMED => {
                plain
                    && flags == 0
                    && unused(record.target)
                    && unused(record.identity)
                    && none(record.byte_size)
                    && no_bases
                    && no_variants
            }
            kinds::BASE => {
                base_type()
                    && only(f::SIZED | f::BIT_SIZED)
                    && unused(record.target)
                    && record.parts == Span::EMPTY
                    && no_bases
                    && no_variants
            }
            kinds::ENUMERATION => {
                base_type()
                    && only(f::SIZED | f::BIT_SIZED | f::NAMED_CONSTANTS | f::SCOPED)
                    && valid_optional_type(record.target, count)
                    && follows(&mut cursors.enumerators, record.parts)
                    && no_bases
                    && no_variants
            }
            kinds::POINTER => {
                no_text
                    && record.detail == 0
                    && none(record.bit_size)
                    && record.parts == Span::EMPTY
                    && only(f::SIZED)
                    && valid_optional_type(record.target, count)
                    && no_bases
                    && no_variants
            }
            kinds::REFERENCE => {
                no_text
                    && usize::from(record.detail) < REFERENCES.len()
                    && none(record.bit_size)
                    && record.parts == Span::EMPTY
                    && only(f::SIZED)
                    && valid_type(record.target, count)
                    && no_bases
                    && no_variants
            }
            kinds::ARRAY => {
                no_text
                    && usize::from(record.detail) < ORDERINGS.len()
                    && none(record.value)
                    && none(record.bit_size)
                    && only(f::SIZED)
                    && valid_type(record.target, count)
                    && follows(&mut cursors.dimensions, record.parts)
                    && no_bases
                    && no_variants
            }
            kinds::RUNTIME_ARRAY => {
                no_text
                    && usize::from(record.detail) < ORDERINGS.len()
                    && none(record.value)
                    && none(record.bit_size)
                    && only(f::SIZED)
                    && valid_type(record.target, count)
                    && record.parts.within(runtime_dimensions.len())
                    && runtime_dimensions[record.parts.range()]
                        .iter()
                        .all(|dimension| dimension.dimension().is_some())
                    && follows(&mut cursors.runtime_dimensions, record.parts)
                    && no_bases
                    && no_variants
            }
            kinds::SLICE => {
                no_text
                    && valid_slice_words(record.value.get())
                    && none(record.bit_size)
                    && record.detail == 0
                    && record.parts == Span::EMPTY
                    && only(f::SIZED | f::TEXT)
                    && valid_type(record.target, count)
                    && no_bases
                    && no_variants
            }
            kinds::RECORD => {
                no_text
                    && usize::from(record.detail) < RECORDS.len()
                    && none(record.value)
                    && none(record.bit_size)
                    && only(f::SIZED | f::INCOMPLETE)
                    && unused(record.target)
                    && follows(&mut cursors.members, record.parts)
                    && follows(&mut cursors.bases, record.bases)
                    && no_variants
            }
            kinds::UNION => {
                no_text
                    && record.detail == 0
                    && none(record.value)
                    && none(record.bit_size)
                    && only(f::SIZED | f::INCOMPLETE)
                    && unused(record.target)
                    && follows(&mut cursors.members, record.parts)
                    && no_bases
                    && no_variants
            }
            kinds::VARIANT => {
                let stored = flags & f::STORED != 0;
                let tag = flags & f::TAG_TYPE != 0;
                let ok = no_text
                    && usize::from(record.detail) < STORAGE.len()
                    && none(record.value)
                    && none(record.bit_size)
                    && only(f::SIZED | f::INCOMPLETE | f::STORED | f::TAG_TYPE)
                    && !(stored && tag)
                    && if tag {
                        valid_type(record.target, count)
                    } else {
                        unused(record.target)
                    }
                    && follows(&mut cursors.members, record.parts)
                    && follows(&mut cursors.bases, record.bases)
                    && follows(&mut cursors.variants, record.variants);

                // Each variant's selectors and members follow the last's,
                // then the stored discriminant.
                ok && record.variants.within(variants.len())
                    && record.variants.rows(variants).iter().all(|variant| {
                        (variant.name.get() == NONE || text(variant.name))
                            && variant.default <= 1
                            && (variant.default == 0 || variant.selectors.count() == 0)
                            && follows(&mut cursors.selectors, variant.selectors)
                            && follows(&mut cursors.members, variant.members)
                    })
                    && if stored {
                        follows(
                            &mut cursors.members,
                            Span::<MemberRecord>::new(record.discriminant.get(), 1),
                        )
                    } else {
                        unused(record.discriminant)
                    }
            }
            kinds::MODIFIED => {
                no_text
                    && usize::from(record.detail) < MODIFIERS.len()
                    && none(record.value)
                    && none(record.bit_size)
                    && record.parts == Span::EMPTY
                    && only(f::SIZED)
                    && valid_type(record.target, count)
                    && no_bases
                    && no_variants
            }
            kinds::NAMED => {
                no_text
                    && usize::from(record.detail) < RELATIONSHIPS.len()
                    && none(record.value)
                    && none(record.bit_size)
                    && record.parts == Span::EMPTY
                    && only(f::SIZED)
                    && valid_optional_type(record.target, count)
                    && no_bases
                    && no_variants
            }
            kinds::UNSPECIFIED | kinds::FUNCTION => {
                plain && only(f::SIZED) && unused(record.target) && no_bases && no_variants
            }
            kinds::SIGNATURE => {
                no_text
                    && record.detail == 0
                    && none(record.value)
                    && none(record.bit_size)
                    && only(f::SIZED | f::VARIADIC | f::PROTOTYPED)
                    && valid_optional_type(record.target, count)
                    && follows(&mut cursors.parameters, record.parts)
                    && no_bases
                    && no_variants
            }
            kinds::OPAQUE => {
                text(record.text)
                    && unused(record.base_name)
                    && record.detail == 0
                    && none(record.value)
                    && none(record.bit_size)
                    && record.parts == Span::EMPTY
                    && only(f::SIZED)
                    && unused(record.target)
                    && no_bases
                    && no_variants
            }
            _ => false,
        };

        let sized = flags & f::SIZED != 0 || none(record.byte_size);

        // Identities follow in type order too.
        let identity = unused(record.identity)
            || follows(
                &mut cursors.identities,
                Span::<IdentityRecord>::new(record.identity.get(), 1),
            );

        if !valid || !sized || !identity {
            return Err("a type is malformed".into());
        }
    }

    // The lists hold exactly what the types name.
    let lengths = [
        (cursors.members, members.len()),
        (cursors.bases, image.table::<BaseRecord>().len()),
        (cursors.variants, variants.len()),
        (cursors.selectors, image.table::<SelectorRecord>().len()),
        (cursors.enumerators, image.table::<EnumeratorRecord>().len()),
        (cursors.dimensions, image.table::<DimensionRecord>().len()),
        (cursors.runtime_dimensions, runtime_dimensions.len()),
        (
            cursors.parameters,
            image.shared::<Item>(TableKind::TypeParameters).len(),
        ),
        (cursors.identities, image.table::<IdentityRecord>().len()),
    ];

    if !lengths
        .iter()
        .all(|(cursor, length)| *cursor as usize == *length)
    {
        return Err("the type lists do not match their types".into());
    }

    Ok(())
}

const fn valid_integer(value: &IntegerRecord) -> bool {
    value.signed <= 1
}

const fn valid_layout(layout: u8, offset: U64, bit_size: U64) -> bool {
    match layout {
        BYTE_OFFSET => bit_size.get() == 0,
        BIT_RANGE => true,
        RUNTIME => offset.get() == 0 && bit_size.get() == 0,
        _ => false,
    }
}

fn validate_lists(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let text = |id: U32| strings.contains(StrId(id.get()));
    let types = image.table::<TypeRecord>().len();
    let files = image.table::<super::lines::FileRecord>().len();

    if !image.table::<MemberRecord>().iter().all(|member| {
        (member.name.get() == NONE || text(member.name))
            && valid_type(member.ty, types)
            && valid_layout(member.layout, member.offset, member.bit_size)
            && usize::from(member.accessibility) < ACCESSIBILITY.len()
            && member.flags & !member_flags::ALL == 0
            && member.declaration.valid(files)
    }) || !image.table::<BaseRecord>().iter().all(|base| {
        valid_type(base.ty, types)
            && valid_layout(base.layout, base.offset, base.bit_size)
            && usize::from(base.accessibility) < ACCESSIBILITY.len()
            && base.virtual_base <= 1
    }) || !image.table::<SelectorRecord>().iter().all(|selector| {
        valid_integer(&selector.low)
            && valid_integer(&selector.high)
            && match selector.range {
                0 => selector.high == IntegerRecord::ZERO,
                1 => true,
                _ => false,
            }
    }) || !image
        .table::<EnumeratorRecord>()
        .iter()
        .all(|enumerator| text(enumerator.name) && valid_integer(&enumerator.value))
        || !image
            .shared::<Item>(TableKind::TypeParameters)
            .iter()
            .all(|parameter| valid_type(parameter.value, types))
    {
        return Err("a type's member, base, selector, enumerator or parameter is malformed".into());
    }

    let identity_strings = image.shared::<Item>(TableKind::IdentityStrings);
    let arguments = image.table::<ArgumentRecord>();
    let mut strings_cursor = 0;
    let mut arguments_cursor = 0;

    for identity in image.table::<IdentityRecord>() {
        let go = identity.flags & identity_flags::GO != 0;
        let runtime = identity.flags & identity_flags::RUNTIME_TYPE != 0;
        if !follows(&mut strings_cursor, identity.path)
            || !follows(&mut strings_cursor, identity.inline)
            || !follows(&mut arguments_cursor, identity.arguments)
            || !text(identity.base)
            || (identity.pack.get() != NONE && identity.pack.get() > identity.arguments.count())
            || usize::from(identity.origin) >= ORIGINS.len()
            || !valid_language(identity.language, identity.other_language.get())
            || identity.flags & !identity_flags::ALL != 0
            || (runtime && !go)
            || (!runtime && identity.runtime_type.get() != 0)
            || (!go && identity.go_kind != 0)
        {
            return Err("a type identity is malformed".into());
        }
    }

    if strings_cursor as usize != identity_strings.len()
        || arguments_cursor as usize != arguments.len()
        || !identity_strings.iter().all(|segment| text(segment.value))
        || !arguments.iter().all(|argument| {
            valid_integer(&argument.value)
                && match argument.kind {
                    TYPE_ARGUMENT => {
                        argument.value == IntegerRecord::ZERO
                            && valid_type(argument.reference, types)
                    }
                    VALUE_ARGUMENT => argument.reference.get() == NONE,
                    UNKNOWN_ARGUMENT => {
                        argument.value == IntegerRecord::ZERO && text(argument.reference)
                    }
                    _ => false,
                }
        })
    {
        return Err("a type identity's segments or arguments are malformed".into());
    }

    // What functions say of types names types there are.
    if !image
        .table::<GenericRecord>()
        .iter()
        .all(|generic| valid_type(generic.argument, types))
        || !image
            .table::<FunctionRecord>()
            .iter()
            .all(|function| valid_optional_type(function.coroutine, types))
    {
        return Err("a function names a type the image lacks".into());
    }

    Ok(())
}

/// Whether no two entries of a name index name the same record by the
/// same text: entries for one record and hash are adjacent, so only
/// those are compared.
fn distinct_entries(strings: Strings<'_>, entries: &[NameEntry]) -> bool {
    entries
        .chunk_by(|earlier, later| (earlier.hash, earlier.value) == (later.hash, later.value))
        .all(|run| {
            run.len() == 1 || {
                let mut texts = run
                    .iter()
                    .map(|entry| strings.bytes(StrId(entry.name.get())))
                    .collect::<Vec<_>>();
                texts.sort_unstable();
                texts.windows(2).all(|pair| pair[0] != pair[1])
            }
        })
}

#[expect(clippy::too_many_lines, reason = "one check for each index")]
fn validate_indexes(image: &Image) -> Result<(), String> {
    let strings = image.strings();
    let types = image.table::<TypeRecord>();
    let identities = image.table::<IdentityRecord>();
    let enumerators = image.table::<EnumeratorRecord>();
    let resolved = |entry: &NameEntry| types[entry.value.get() as usize].kind != kinds::MALFORMED;
    let names = image.shared::<NameEntry>(TableKind::TypeNames);

    if !index::valid_names(&strings, names, types.len())
        || names.len()
            != types
                .iter()
                .filter(|record| record.kind != kinds::MALFORMED)
                .count()
        || !names
            .iter()
            .all(|entry| resolved(entry) && types[entry.value.get() as usize].name == entry.name)
    {
        return Err("the type name index disagrees with the types".into());
    }

    // Each identity's base names its type, and so does C's spelling of
    // it where that differs: every entry is one of these, no two are the
    // same, and there are as many as these.
    let base_names = image.shared::<NameEntry>(TableKind::TypeBaseNames);
    let mut expected = 0;
    let mut keys = Vec::with_capacity(types.len());

    for record in types {
        let key = if record.kind == kinds::MALFORMED || record.identity.get() == NONE {
            None
        } else {
            let base = strings.get(StrId(identities[record.identity.get() as usize].base.get()));
            let key = crate::eval::types::c_type_key_of_name(base).filter(|key| key != base);
            expected += 1 + usize::from(key.is_some());
            Some((base, key))
        };

        keys.push(key);
    }

    let names_its_type = |entry: &NameEntry| {
        let name = strings.get(StrId(entry.name.get()));
        keys[entry.value.get() as usize]
            .as_ref()
            .is_some_and(|(base, key)| *base == name || key.as_deref() == Some(name))
    };

    if !index::valid_names(&strings, base_names, types.len())
        || base_names.len() != expected
        || !base_names.iter().all(names_its_type)
        || !distinct_entries(strings, base_names)
    {
        return Err("the type base index disagrees with the types".into());
    }

    // Classes are numbered in the order their first types come.
    let classes = image.shared::<Item>(TableKind::TypeClasses);
    let mut next_class = 0;

    if classes.len() != types.len()
        || !classes.iter().all(|class| {
            let class = class.value.get();
            if class == next_class {
                next_class += 1;
                true
            } else {
                class < next_class
            }
        })
    {
        return Err("the type classes are malformed".into());
    }

    // Each enumeration is under each of its enumerators' names once.
    let enumerator_names = image.shared::<NameEntry>(TableKind::EnumeratorNames);
    let mut expected = 0;
    let mut listed = vec![Vec::new(); types.len()];

    for (index, record) in types.iter().enumerate() {
        if record.kind != kinds::ENUMERATION {
            continue;
        }

        let mut names = enumerators[record.parts.range()]
            .iter()
            .map(|enumerator| enumerator.name.get())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        expected += names.len();
        listed[index] = names;
    }

    if !index::valid_names(&strings, enumerator_names, types.len())
        || enumerator_names.len() != expected
        || !enumerator_names.iter().all(|entry| {
            listed[entry.value.get() as usize]
                .binary_search(&entry.name.get())
                .is_ok()
        })
        || !distinct_entries(strings, enumerator_names)
    {
        return Err("the enumerator index disagrees with the types".into());
    }

    // The first type with each runtime descriptor, by offset.
    let runtime_types = image.table::<RuntimeTypeRecord>();
    let mut first = std::collections::BTreeMap::new();

    for (index, record) in types.iter().enumerate() {
        if record.kind == kinds::MALFORMED || record.identity.get() == NONE {
            continue;
        }

        let identity = &identities[record.identity.get() as usize];
        if identity.flags & identity_flags::RUNTIME_TYPE != 0 {
            first.entry(identity.runtime_type.get()).or_insert(index);
        }
    }

    if runtime_types.len() != first.len()
        || !runtime_types
            .iter()
            .zip(&first)
            .all(|(record, (offset, index))| {
                record.offset.get() == *offset && record.ty.get() as usize == *index
            })
    {
        return Err("the Go runtime type index disagrees with the types".into());
    }

    Ok(())
}
