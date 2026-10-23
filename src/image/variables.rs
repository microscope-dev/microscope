//! The data objects debug information describes, with the scopes that
//! hold them, their values, and the functions whose frames show them.
//!
//! An object names its scope, whose code, inline instance, and frame base
//! every object declared there shares, so each scope is one row. A
//! function lists its objects in the order a frame shows them; indexes
//! find functions by code and Go entry address, objects by their entry's
//! offset, and DWARF procedures by theirs.

use std::sync::Arc;

use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::functions::{LocationRecord, language_code, language_of, valid_language};
use super::locations::LocationListId;
use super::strings::{StrId, Strings, StringsBuilder};
use super::types::Item;
use super::{Builder, Image, NONE, Record, SharedRecord, Span, TableKind};
use crate::{
    AddressRange, CodeInstanceId, ImageAddress, SourceLanguage, SourceLocation, TypeId,
    VariableKind,
};

/// What debug information says of something, or why it says nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Metadata<T> {
    Value(T),
    Absent(MetadataAbsence),
    Malformed(Arc<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetadataAbsence {
    NoLocation,
    NoFrameBase,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstantValue {
    Unsigned(u128),
    Signed(i128),

    /// A fixed-width form with implicit zero high bits.
    Fixed(u128),
    Bytes(Arc<[u8]>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueDescription {
    Location(LocationListId),

    /// Boxed, as few data objects are constants and a value is wide.
    Constant(Box<ConstantValue>),
}

/// A type, or why it is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeResolution {
    Resolved(TypeId),
    Malformed(Arc<str>),
}

/// A Go local's declaration, and the code instance of the scope declaring
/// it, whose inlined calls stand for their sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoDeclaration {
    pub location: SourceLocation,
    pub instance: Option<CodeInstanceId>,
}

/// One data object, as [`add_to`] takes it.
#[derive(Debug, Clone)]
pub struct DataObject {
    pub debug_info_offset: Option<u64>,
    pub kind: VariableKind,
    pub name: Arc<str>,
    pub declaration: Option<SourceLocation>,

    /// The code of its scope.
    pub ranges: Arc<[AddressRange<ImageAddress>]>,

    /// For a Go local, its declaration, past whose line it is visible;
    /// boxed, as most data objects are not Go's.
    pub go_declaration: Option<Box<GoDeclaration>>,

    /// The inline instance owning this variable, or `None` for the physical
    /// frame. Lookup only sees variables of the selected logical frame.
    pub instance: Option<CodeInstanceId>,
    pub lexical_depth: u32,
    pub order: u64,
    pub type_info: TypeResolution,

    /// For a variable Go moved to the heap, which its debug information
    /// names `&name`, the type of the pointer its location holds; the
    /// variable is what that points to.
    pub escaped: Option<TypeId>,

    /// Whether the compiler made it for itself, such as Go's `.dict` and
    /// `#yield1`: listings leave it out, but its name still reaches it.
    pub hidden: bool,

    /// For `$future`, the future rustc passes the body of an async
    /// function or block, as a pointer it leaves unnamed: its type.
    pub coroutine: Option<TypeId>,
    pub value: Metadata<ValueDescription>,
    pub frame_base: Metadata<LocationListId>,
    pub malformed: Option<Arc<str>>,
}

/// How a function returns its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnConvention {
    /// Go's register ABI on x86-64, which the producer names `regabi`.
    GoRegisters,

    /// The System V x86-64 convention.
    SystemV(Box<SystemV>),
}

/// The one value a function returns by the System V convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemV {
    /// The function's name, which the value is shown by.
    pub name: Arc<str>,
    pub ty: TypeResolution,

    /// The function's language, which says whether the convention is
    /// known for aggregates and how C++ passes a class.
    pub language: SourceLanguage,

    /// Whether the producer says optimization changed how the function
    /// returns, so the convention no longer says where its value is.
    pub rewritten: bool,
}

/// One variable a Go closure captured: a copy of its value, or, when its
/// name begins with `&`, a pointer to the variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    pub name: Arc<str>,

    /// Its offset in the closure's context, past the code pointer.
    pub offset: u64,
    pub type_info: TypeResolution,
}

/// One function whose frames show data objects, as [`add_to`] takes it.
#[derive(Debug, Clone)]
pub struct Function {
    pub ranges: Arc<[AddressRange<ImageAddress>]>,

    /// Its objects, in the order a frame shows them.
    pub objects: Vec<u32>,

    /// The name a Go function value calling it shows.
    pub name: Option<Arc<str>>,

    /// The variables a Go closure captured, in its context, or why they
    /// cannot be known.
    pub captures: Result<Vec<Capture>, Arc<str>>,

    /// How the function returns its values, when that is known.
    pub returns: Option<ReturnConvention>,
}

/// One global, as [`add_to`] takes it. Its object gives its name,
/// declaration, and type.
#[derive(Debug, Clone)]
pub struct Global {
    pub object: u32,

    /// The producer-normalized source qualification.
    pub qualified_name: Arc<str>,

    /// The linker identity, when the debug information gives one.
    pub linkage_name: Option<Arc<str>>,

    /// Whether the producer marks it as visible outside its unit.
    pub external: bool,
}

/// What [`add_to`] encodes.
#[derive(Debug, Default)]
pub struct Variables {
    pub objects: Vec<DataObject>,
    pub functions: Vec<Function>,

    /// The module's globals, in global order.
    pub globals: Vec<Global>,

    /// Go functions by the address their code begins at, which a func
    /// value holds.
    pub go_entries: Vec<(ImageAddress, u32)>,

    /// `DW_TAG_dwarf_procedure`s, which an implicit pointer may point
    /// into, by their entries' offsets.
    pub procedures: Vec<(u64, Metadata<LocationListId>)>,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct CodeRange {
    pub start: U64,
    pub end: U64,
}

impl Record for CodeRange {
    const KIND: TableKind = TableKind::ScopeRanges;
}

impl SharedRecord for CodeRange {
    const NAME: &'static str = "CodeRange";
}

impl CodeRange {
    pub(super) const fn get(&self) -> AddressRange<ImageAddress> {
        AddressRange {
            start: ImageAddress::new(self.start.get()),
            end: ImageAddress::new(self.end.get()),
        }
    }
}

/// A global: its object, and the names it answers to beyond the object's.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct GlobalRecord {
    pub object: U32,
    pub qualified_name: U32,

    /// The linkage name, or [`NONE`].
    pub linkage_name: U32,
    pub external: u8,
}

impl Record for GlobalRecord {
    const KIND: TableKind = TableKind::Globals;
}

/// A global of an image.
#[derive(Debug, Clone, Copy)]
pub struct GlobalEntry<'a> {
    pub object: Object<'a>,
    pub qualified_name: &'a str,
    pub linkage_name: Option<&'a str>,
    pub external: bool,
}

/// The code, instance, and frame base the objects of one scope share.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ScopeRecord {
    pub ranges: Span<CodeRange>,

    /// The inline instance owning the scope, or [`NONE`].
    pub instance: U32,

    /// The code instance a Go local's declaration is in, or [`NONE`].
    pub go_instance: U32,
    pub lexical_depth: U32,
    pub frame_base: U32,
    pub frame_base_kind: u8,
}

impl Record for ScopeRecord {
    const KIND: TableKind = TableKind::Scopes;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ObjectRecord {
    pub name: U32,
    pub declaration: LocationRecord,
    pub scope: U32,

    /// The type, or with [`object_flags::TYPE_MALFORMED`] why it is
    /// unknown.
    pub ty: U32,
    pub escaped: U32,
    pub coroutine: U32,

    /// What [`ObjectRecord::value_kind`] says it is.
    pub value: U32,

    /// Why the object is malformed, or [`NONE`].
    pub malformed: U32,
    pub debug_info_offset: U64,
    pub kind: u8,
    pub value_kind: u8,
    pub flags: u8,
}

impl Record for ObjectRecord {
    const KIND: TableKind = TableKind::DataObjects;
}

pub mod object_flags {
    pub const HIDDEN: u8 = 1 << 0;

    pub const TYPE_MALFORMED: u8 = 1 << 1;

    pub const OFFSET: u8 = 1 << 2;

    pub const GO_DECLARED: u8 = 1 << 3;

    pub const ALL: u8 = HIDDEN | TYPE_MALFORMED | OFFSET | GO_DECLARED;
}

/// What an object's value, or a frame base, is: a location list, a
/// constant, an absence, or why it is malformed.
pub mod value_kinds {
    pub const LOCATION: u8 = 0;

    pub const CONSTANT: u8 = 1;

    pub const NO_LOCATION: u8 = 2;

    pub const NO_FRAME_BASE: u8 = 3;

    pub const NOT_APPLICABLE: u8 = 4;

    pub const MALFORMED: u8 = 5;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ConstantRecord {
    /// The value's low bits, or where a block's bytes begin.
    pub low: U64,

    /// The value's high bits, or a block's length.
    pub high: U64,
    pub kind: u8,
}

impl Record for ConstantRecord {
    const KIND: TableKind = TableKind::Constants;
}

pub mod constant_kinds {
    pub const UNSIGNED: u8 = 0;

    pub const SIGNED: u8 = 1;

    pub const FIXED: u8 = 2;

    pub const BYTES: u8 = 3;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct VariableFunctionRecord {
    pub ranges: Span<CodeRange>,
    pub objects: Span<Item>,

    /// The name a Go function value shows, or [`NONE`].
    pub name: U32,

    /// The first capture, or with [`function_flags::CAPTURES_MALFORMED`]
    /// why they are unknown.
    pub captures: Span<CaptureRecord>,
    pub returned_name: U32,

    /// The returned type, or with [`function_flags::RETURNED_MALFORMED`]
    /// why it is unknown.
    pub returned_type: U32,
    pub other_language: U16,
    pub language: u8,
    pub returns: u8,
    pub flags: u8,
}

impl Record for VariableFunctionRecord {
    const KIND: TableKind = TableKind::VariableFunctions;
}

pub mod function_flags {
    pub const CAPTURES_MALFORMED: u8 = 1 << 0;

    pub const RETURNED_MALFORMED: u8 = 1 << 1;

    pub const REWRITTEN: u8 = 1 << 2;

    pub const ALL: u8 = CAPTURES_MALFORMED | RETURNED_MALFORMED | REWRITTEN;
}

pub mod returns {
    pub const UNKNOWN: u8 = 0;

    pub const GO_REGISTERS: u8 = 1;

    pub const SYSTEM_V: u8 = 2;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct CaptureRecord {
    pub name: U32,
    pub offset: U64,

    /// The type, or when `malformed` is set why it is unknown.
    pub ty: U32,
    pub malformed: u8,
}

impl Record for CaptureRecord {
    const KIND: TableKind = TableKind::Captures;
}

/// Where a function's code begins, ordered by address and then function,
/// with the furthest end of the code beginning there or before.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct FunctionStartRecord {
    pub start: U64,
    pub end: U64,
    pub prefix_max_end: U64,
    pub function: U32,
}

impl Record for FunctionStartRecord {
    const KIND: TableKind = TableKind::FunctionStarts;
}

/// A key of a sorted index, and the row it names.
#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct Keyed {
    pub key: U64,
    pub value: U32,
}

impl SharedRecord for Keyed {
    const NAME: &'static str = "Keyed";
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct DwarfProcedureRecord {
    pub offset: U64,
    pub location: U32,
    pub location_kind: u8,
}

impl Record for DwarfProcedureRecord {
    const KIND: TableKind = TableKind::DwarfProcedures;
}

/// Why data objects could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the data objects do not fit an image")]
pub struct TooMany;

fn number(count: usize) -> Result<u32, TooMany> {
    u32::try_from(count)
        .ok()
        .filter(|count| *count < NONE)
        .ok_or(TooMany)
}

fn text(strings: &mut StringsBuilder, text: &str) -> Result<U32, TooMany> {
    strings.push(text).map(|id| id.0.into()).ok_or(TooMany)
}

fn optional(id: Option<u32>) -> U32 {
    id.unwrap_or(NONE).into()
}

const fn some(value: U32) -> Option<u32> {
    match value.get() {
        NONE => None,
        value => Some(value),
    }
}

/// Encodes a type, or why it is unknown, with whether it is malformed.
fn resolution(
    strings: &mut StringsBuilder,
    resolution: &TypeResolution,
) -> Result<(U32, bool), TooMany> {
    match resolution {
        TypeResolution::Resolved(id) => Ok((id.get().into(), false)),
        TypeResolution::Malformed(why) => Ok((text(strings, why)?, true)),
    }
}

#[derive(Default)]
struct Encoder {
    ranges: Vec<CodeRange>,

    /// Keyed by the ranges themselves, so that the many objects sharing a
    /// scope look theirs up without copying them first.
    pooled_ranges: foldhash::HashMap<Box<[AddressRange<ImageAddress>]>, U32>,
    scopes: Vec<ScopeRecord>,
    pooled_scopes: foldhash::HashMap<ScopeRecord, u32>,
    constants: Vec<ConstantRecord>,
    constant_bytes: Vec<u8>,
}

impl Encoder {
    /// The first of `ranges`, pooled once.
    fn ranges(
        &mut self,
        ranges: &[AddressRange<ImageAddress>],
    ) -> Result<Span<CodeRange>, TooMany> {
        let count = number(ranges.len())?;
        if let Some(first) = self.pooled_ranges.get(ranges) {
            return Ok(Span::new(first.get(), count));
        }

        let first = number(self.ranges.len())?;
        self.ranges.extend(ranges.iter().map(|range| CodeRange {
            start: range.start.get().into(),
            end: range.end.get().into(),
        }));
        number(self.ranges.len())?;
        self.pooled_ranges.insert(ranges.into(), first.into());

        Ok(Span::new(first, count))
    }

    fn scope(&mut self, scope: ScopeRecord) -> Result<U32, TooMany> {
        if let Some(id) = self.pooled_scopes.get(&scope) {
            return Ok((*id).into());
        }

        let id = number(self.scopes.len())?;
        self.scopes.push(scope);
        self.pooled_scopes.insert(scope, id);
        Ok(id.into())
    }

    fn constant(&mut self, constant: &ConstantValue) -> Result<U32, TooMany> {
        let split = |value: u128| {
            let [
                b0,
                b1,
                b2,
                b3,
                b4,
                b5,
                b6,
                b7,
                b8,
                b9,
                b10,
                b11,
                b12,
                b13,
                b14,
                b15,
            ] = value.to_le_bytes();
            let low = u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
            let high = u64::from_le_bytes([b8, b9, b10, b11, b12, b13, b14, b15]);
            (low.into(), high.into())
        };

        let record = match constant {
            ConstantValue::Unsigned(value) => {
                let (low, high) = split(*value);
                ConstantRecord {
                    low,
                    high,
                    kind: constant_kinds::UNSIGNED,
                }
            }
            ConstantValue::Signed(value) => {
                let (low, high) = split(value.cast_unsigned());
                ConstantRecord {
                    low,
                    high,
                    kind: constant_kinds::SIGNED,
                }
            }
            ConstantValue::Fixed(value) => {
                let (low, high) = split(*value);
                ConstantRecord {
                    low,
                    high,
                    kind: constant_kinds::FIXED,
                }
            }
            ConstantValue::Bytes(bytes) => {
                let first = number(self.constant_bytes.len())?;
                self.constant_bytes.extend_from_slice(bytes);
                number(self.constant_bytes.len())?;
                ConstantRecord {
                    low: u64::from(first).into(),
                    high: u64::from(number(bytes.len())?).into(),
                    kind: constant_kinds::BYTES,
                }
            }
        };

        let id = number(self.constants.len())?;
        self.constants.push(record);
        Ok(id.into())
    }

    /// Encodes a value or frame base as its kind and what the kind names.
    fn value(
        &mut self,
        strings: &mut StringsBuilder,
        value: &Metadata<ValueDescription>,
    ) -> Result<(u8, U32), TooMany> {
        match value {
            Metadata::Value(ValueDescription::Constant(constant)) => {
                Ok((value_kinds::CONSTANT, self.constant(constant)?))
            }
            Metadata::Value(ValueDescription::Location(list)) => {
                Ok((value_kinds::LOCATION, list.0.into()))
            }
            Metadata::Absent(absence) => Ok(absent(*absence)),
            Metadata::Malformed(why) => Ok((value_kinds::MALFORMED, text(strings, why)?)),
        }
    }
}

fn absent(absence: MetadataAbsence) -> (u8, U32) {
    let kind = match absence {
        MetadataAbsence::NoLocation => value_kinds::NO_LOCATION,
        MetadataAbsence::NoFrameBase => value_kinds::NO_FRAME_BASE,
        MetadataAbsence::NotApplicable => value_kinds::NOT_APPLICABLE,
    };

    (kind, 0.into())
}

/// Encodes a location, or why there is none, as its kind and what the
/// kind names.
pub(super) fn location(
    strings: &mut StringsBuilder,
    location: &Metadata<LocationListId>,
) -> Result<(u8, U32), TooMany> {
    match location {
        Metadata::Value(list) => Ok((value_kinds::LOCATION, list.0.into())),
        Metadata::Absent(absence) => Ok(absent(*absence)),
        Metadata::Malformed(why) => Ok((value_kinds::MALFORMED, text(strings, why)?)),
    }
}

const fn variable_kind(kind: VariableKind) -> u8 {
    match kind {
        VariableKind::Parameter => 0,
        VariableKind::Result => 1,
        VariableKind::Local => 2,
        VariableKind::Global => 3,
        VariableKind::Returned => 4,
        // Only a JavaScript frame has these, and validation refuses the byte.
        VariableKind::Captured | VariableKind::Module | VariableKind::Unnamed => u8::MAX,
    }
}

/// The kinds a data object can have, by the byte that stores them.
const VARIABLE_KINDS: [VariableKind; 5] = [
    VariableKind::Parameter,
    VariableKind::Result,
    VariableKind::Local,
    VariableKind::Global,
    VariableKind::Returned,
];

/// Adds the data objects, their functions, and their indexes to
/// `builder`, pooling names in `strings`.
///
/// # Panics
///
/// When a Go declaration is not its object's declaration.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    variables: &Variables,
) -> Result<(), TooMany> {
    let mut encoder = Encoder::default();
    let mut objects = Vec::with_capacity(variables.objects.len());

    for object in &variables.objects {
        objects.push(encode_object(&mut encoder, strings, object)?);
    }

    let mut functions = Vec::with_capacity(variables.functions.len());
    let mut lists = FunctionLists::default();

    for function in &variables.functions {
        functions.push(encode_function(
            &mut encoder,
            strings,
            &mut lists,
            function,
        )?);
    }

    let starts = function_starts(&variables.functions)?;
    let go_entries = keyed(
        variables
            .go_entries
            .iter()
            .map(|(address, function)| (address.get(), *function)),
    );
    let offsets = keyed(
        (0_u32..)
            .zip(&variables.objects)
            .filter_map(|(index, object)| Some((object.debug_info_offset?, index))),
    );
    let mut procedures = Vec::<DwarfProcedureRecord>::new();
    let mut sorted = variables.procedures.iter().rev().collect::<Vec<_>>();
    sorted.sort_by_key(|(offset, _)| *offset);
    sorted.dedup_by_key(|(offset, _)| *offset);

    for (offset, list) in sorted {
        let (location_kind, location) = location(strings, list)?;
        procedures.push(DwarfProcedureRecord {
            offset: (*offset).into(),
            location,
            location_kind,
        });
    }

    let globals = variables
        .globals
        .iter()
        .map(|global| {
            Ok(GlobalRecord {
                object: global.object.into(),
                qualified_name: text(strings, &global.qualified_name)?,
                linkage_name: match &global.linkage_name {
                    Some(name) => text(strings, name)?,
                    None => NONE.into(),
                },
                external: u8::from(global.external),
            })
        })
        .collect::<Result<Vec<_>, TooMany>>()?;

    builder
        .owned_table(encoder.ranges)
        .owned_table(encoder.scopes)
        .owned_table(objects)
        .owned_table(encoder.constants)
        .bytes(TableKind::ConstantBytes, encoder.constant_bytes)
        .owned_table(functions)
        .owned_shared(TableKind::FunctionObjects, lists.objects)
        .owned_table(lists.captures)
        .owned_table(starts)
        .owned_shared(TableKind::GoEntries, go_entries)
        .owned_shared(TableKind::ObjectOffsets, offsets)
        .owned_table(procedures)
        .owned_table(globals);

    Ok(())
}

/// The lists functions' rows name spans of.
#[derive(Default)]
struct FunctionLists {
    objects: Vec<Item>,
    captures: Vec<CaptureRecord>,
}

fn encode_function(
    encoder: &mut Encoder,
    strings: &mut StringsBuilder,
    lists: &mut FunctionLists,
    function: &Function,
) -> Result<VariableFunctionRecord, TooMany> {
    let ranges = encoder.ranges(&function.ranges)?;
    let first = number(lists.objects.len())?;
    lists
        .objects
        .extend(function.objects.iter().map(|object| Item {
            value: (*object).into(),
        }));
    let mut flags = 0;

    // A malformed list begins at the string that says why, and is empty.
    let captures = match &function.captures {
        Ok(list) => {
            let first = number(lists.captures.len())?;
            for capture in list {
                let (ty, malformed) = resolution(strings, &capture.type_info)?;
                lists.captures.push(CaptureRecord {
                    name: text(strings, &capture.name)?,
                    offset: capture.offset.into(),
                    ty,
                    malformed: u8::from(malformed),
                });
            }

            Span::new(first, number(list.len())?)
        }
        Err(why) => {
            flags |= function_flags::CAPTURES_MALFORMED;
            Span::new(text(strings, why)?.get(), 0)
        }
    };

    let mut record = VariableFunctionRecord {
        ranges,
        objects: Span::new(first, number(function.objects.len())?),
        name: match &function.name {
            Some(name) => text(strings, name)?,
            None => NONE.into(),
        },
        captures,
        returned_name: NONE.into(),
        returned_type: NONE.into(),
        other_language: 0.into(),
        language: 0,
        returns: returns::UNKNOWN,
        flags,
    };

    match &function.returns {
        None => {}
        Some(ReturnConvention::GoRegisters) => record.returns = returns::GO_REGISTERS,
        Some(ReturnConvention::SystemV(convention)) => {
            let (ty, malformed) = resolution(strings, &convention.ty)?;
            let (language, other) = language_code(convention.language).ok_or(TooMany)?;
            record.returns = returns::SYSTEM_V;
            record.returned_name = text(strings, &convention.name)?;
            record.returned_type = ty;
            record.language = language;
            record.other_language = other.into();
            if malformed {
                record.flags |= function_flags::RETURNED_MALFORMED;
            }

            if convention.rewritten {
                record.flags |= function_flags::REWRITTEN;
            }
        }
    }

    Ok(record)
}

/// Where each function's code begins, by address and then function, with
/// the furthest end of the code beginning there or before.
fn function_starts(functions: &[Function]) -> Result<Vec<FunctionStartRecord>, TooMany> {
    let mut starts = Vec::new();

    for (index, function) in functions.iter().enumerate() {
        let index = number(index)?;
        starts.extend(
            function
                .ranges
                .iter()
                .map(|range| (range.start.get(), index, range.end.get())),
        );
    }

    starts.sort_unstable();
    let mut prefix_max_end = 0;

    Ok(starts
        .into_iter()
        .map(|(start, function, end)| {
            prefix_max_end = prefix_max_end.max(end);
            FunctionStartRecord {
                start: start.into(),
                end: end.into(),
                prefix_max_end: prefix_max_end.into(),
                function: function.into(),
            }
        })
        .collect())
}

/// Of the functions whose code begins as `starts` say, the one `contains`
/// finds holding `address`: among those beginning at the same address,
/// the first; otherwise the one beginning nearest before it. A function is
/// found by any of its ranges, which may overlap another's.
fn function_at(
    starts: &[FunctionStartRecord],
    address: ImageAddress,
    contains: impl Fn(u32) -> bool,
) -> Option<u32> {
    let address = address.get();
    let mut group_end = starts.partition_point(|start| start.start.get() <= address);

    while group_end > 0 {
        let start = starts[group_end - 1].start.get();
        let group_start = starts[..group_end].partition_point(|row| row.start.get() < start);

        if let Some(found) = starts[group_start..group_end]
            .iter()
            .map(|row| row.function.get())
            .find(|function| contains(*function))
        {
            return Some(found);
        }

        if group_start == 0 || starts[group_start - 1].prefix_max_end.get() <= address {
            return None;
        }

        group_end = group_start;
    }

    None
}

/// The functions of a [`Variables`] input by the code they hold, found as
/// an image's [`VariableView::function_at`] finds them.
#[derive(Debug)]
pub struct FunctionIndex<'a> {
    functions: &'a [Function],
    starts: Vec<FunctionStartRecord>,
}

impl Variables {
    /// An index of the functions by the code they hold.
    pub fn function_index(&self) -> Result<FunctionIndex<'_>, TooMany> {
        Ok(FunctionIndex {
            functions: &self.functions,
            starts: function_starts(&self.functions)?,
        })
    }
}

impl<'a> FunctionIndex<'a> {
    /// The function holding `address`.
    pub fn function_at(&self, address: ImageAddress) -> Option<&'a Function> {
        let holds = |id: u32| {
            self.functions[id as usize]
                .ranges
                .iter()
                .any(|range| range.contains(address))
        };

        function_at(&self.starts, address, holds).map(|id| &self.functions[id as usize])
    }
}

/// An index of `pairs` by key, in which a later pair for a key replaces
/// an earlier one, as a map's would.
fn keyed(pairs: impl Iterator<Item = (u64, u32)>) -> Vec<Keyed> {
    let mut sorted = pairs.collect::<Vec<_>>();
    sorted.reverse();
    sorted.sort_by_key(|(key, _)| *key);
    sorted.dedup_by_key(|(key, _)| *key);

    sorted
        .into_iter()
        .map(|(key, value)| Keyed {
            key: key.into(),
            value: value.into(),
        })
        .collect()
}

fn encode_object(
    encoder: &mut Encoder,
    strings: &mut StringsBuilder,
    object: &DataObject,
) -> Result<ObjectRecord, TooMany> {
    let ranges = encoder.ranges(&object.ranges)?;
    let (frame_base_kind, frame_base) = location(strings, &object.frame_base)?;
    let mut flags = 0;
    if let Some(declared) = &object.go_declaration {
        assert_eq!(
            Some(&declared.location),
            object.declaration.as_ref(),
            "a Go local is visible past its declaration"
        );
        flags |= object_flags::GO_DECLARED;
    }

    let go_instance = object
        .go_declaration
        .as_ref()
        .and_then(|declared| declared.instance.map(CodeInstanceId::get));
    let scope = encoder.scope(ScopeRecord {
        ranges,
        instance: optional(object.instance.map(CodeInstanceId::get)),
        go_instance: optional(go_instance),
        lexical_depth: object.lexical_depth.into(),
        frame_base,
        frame_base_kind,
    })?;
    let (ty, malformed) = resolution(strings, &object.type_info)?;
    if malformed {
        flags |= object_flags::TYPE_MALFORMED;
    }

    if object.hidden {
        flags |= object_flags::HIDDEN;
    }

    if object.debug_info_offset.is_some() {
        flags |= object_flags::OFFSET;
    }

    let (value_kind, value) = encoder.value(strings, &object.value)?;

    Ok(ObjectRecord {
        name: text(strings, &object.name)?,
        declaration: LocationRecord::of(object.declaration.as_ref()),
        scope,
        ty,
        escaped: optional(object.escaped.map(TypeId::get)),
        coroutine: optional(object.coroutine.map(TypeId::get)),
        value,
        malformed: match &object.malformed {
            Some(why) => text(strings, why)?,
            None => NONE.into(),
        },
        debug_info_offset: object.debug_info_offset.unwrap_or(0).into(),
        kind: {
            debug_assert!(VARIABLE_KINDS.contains(&object.kind), "{:?}", object.kind);
            variable_kind(object.kind)
        },
        value_kind,
        flags,
    })
}

/// A data object, by its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub u32);

/// A function whose frames show data objects, by its row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VariableFunctionId(pub u32);

/// The data objects of an image.
#[derive(Debug, Clone, Copy)]
pub struct VariableView<'a> {
    strings: Strings<'a>,
    ranges: &'a [CodeRange],
    scopes: &'a [ScopeRecord],
    objects: &'a [ObjectRecord],
    constants: &'a [ConstantRecord],
    constant_bytes: &'a [u8],
    functions: &'a [VariableFunctionRecord],
    function_objects: &'a [Item],
    captures: &'a [CaptureRecord],
    starts: &'a [FunctionStartRecord],
    go_entries: &'a [Keyed],
    offsets: &'a [Keyed],
    procedures: &'a [DwarfProcedureRecord],
    globals: &'a [GlobalRecord],
}

fn find(index: &[Keyed], key: u64) -> Option<u32> {
    index
        .binary_search_by_key(&key, |entry| entry.key.get())
        .ok()
        .map(|found| index[found].value.get())
}

impl<'a> VariableView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            ranges: image.table(),
            scopes: image.table(),
            objects: image.table(),
            constants: image.table(),
            constant_bytes: image.bytes(TableKind::ConstantBytes),
            functions: image.table(),
            function_objects: image.shared(TableKind::FunctionObjects),
            captures: image.table(),
            starts: image.table(),
            go_entries: image.shared(TableKind::GoEntries),
            offsets: image.shared(TableKind::ObjectOffsets),
            procedures: image.table(),
            globals: image.table(),
        }
    }

    /// The object `id` names, which must be one of these.
    pub const fn object(self, id: ObjectId) -> Object<'a> {
        let record = &self.objects[id.0 as usize];
        Object {
            view: self,
            id,
            record,
            scope: &self.scopes[record.scope.get() as usize],
        }
    }

    /// The function `id` names, which must be one of these.
    pub const fn function(self, id: VariableFunctionId) -> VariableFunction<'a> {
        VariableFunction {
            view: self,
            id,
            record: &self.functions[id.0 as usize],
        }
    }

    /// The function whose code contains `address`: of the functions with
    /// code beginning last at or before it whose code contains it, the
    /// first. A function whose own ranges overlap is found by any of them.
    pub fn function_at(self, address: ImageAddress) -> Option<VariableFunction<'a>> {
        function_at(self.starts, address, |id| {
            self.function(VariableFunctionId(id)).contains(address)
        })
        .map(|id| self.function(VariableFunctionId(id)))
    }

    /// The Go function whose code begins at `address`.
    pub fn go_function(self, address: ImageAddress) -> Option<VariableFunction<'a>> {
        find(self.go_entries, address.get()).map(|id| self.function(VariableFunctionId(id)))
    }

    /// The object whose entry is at `offset` in `.debug_info`.
    pub fn object_at_offset(self, offset: u64) -> Option<Object<'a>> {
        find(self.offsets, offset).map(|id| self.object(ObjectId(id)))
    }

    /// The location of the `DW_TAG_dwarf_procedure` at `offset`.
    pub fn procedure(self, offset: u64) -> Option<Metadata<LocationListId>> {
        let found = self
            .procedures
            .binary_search_by_key(&offset, |procedure| procedure.offset.get())
            .ok()?;
        let procedure = &self.procedures[found];
        Some(self.location(procedure.location_kind, procedure.location))
    }

    /// The object of the global `index` numbers.
    pub fn global(self, index: usize) -> Option<Object<'a>> {
        Some(self.global_entry(index)?.object)
    }

    /// The global `index` numbers.
    pub fn global_entry(self, index: usize) -> Option<GlobalEntry<'a>> {
        let record = self.globals.get(index)?;
        Some(GlobalEntry {
            object: self.object(ObjectId(record.object.get())),
            qualified_name: self.strings.get(StrId(record.qualified_name.get())),
            linkage_name: some(record.linkage_name).map(|name| self.strings.get(StrId(name))),
            external: record.external != 0,
        })
    }

    pub const fn global_count(self) -> usize {
        self.globals.len()
    }

    /// The objects of every global, in global order.
    pub fn globals(self) -> impl ExactSizeIterator<Item = Object<'a>> + 'a {
        self.globals
            .iter()
            .map(move |record| self.object(ObjectId(record.object.get())))
    }

    fn text(self, id: U32) -> Arc<str> {
        self.strings.get(StrId(id.get())).into()
    }

    fn resolution(self, ty: U32, malformed: bool) -> TypeResolution {
        if malformed {
            TypeResolution::Malformed(self.text(ty))
        } else {
            TypeResolution::Resolved(TypeId::new(ty.get()))
        }
    }

    fn location(self, kind: u8, value: U32) -> Metadata<LocationListId> {
        decode_location(self.strings, kind, value)
    }

    fn constant(self, id: U32) -> Option<ConstantValue> {
        let record = &self.constants[id.get() as usize];
        let value = u128::from(record.low.get()) | (u128::from(record.high.get()) << 64);
        Some(match record.kind {
            constant_kinds::UNSIGNED => ConstantValue::Unsigned(value),
            constant_kinds::SIGNED => ConstantValue::Signed(value.cast_signed()),
            constant_kinds::FIXED => ConstantValue::Fixed(value),
            _ => ConstantValue::Bytes(self.constant_bytes.get(block(record)?)?.into()),
        })
    }

    fn ranges(self, ranges: Span<CodeRange>) -> &'a [CodeRange] {
        ranges.rows(self.ranges)
    }
}

/// One data object of a [`VariableView`].
#[derive(Debug, Clone, Copy)]
pub struct Object<'a> {
    view: VariableView<'a>,
    id: ObjectId,
    record: &'a ObjectRecord,
    scope: &'a ScopeRecord,
}

impl<'a> Object<'a> {
    pub const fn id(self) -> ObjectId {
        self.id
    }

    pub fn name(self) -> &'a str {
        self.view.strings.get(StrId(self.record.name.get()))
    }

    pub fn kind(self) -> VariableKind {
        VARIABLE_KINDS[usize::from(self.record.kind)]
    }

    pub fn declaration(self) -> Option<SourceLocation> {
        self.record.declaration.get()
    }

    /// The code of its scope.
    pub fn ranges(self) -> impl ExactSizeIterator<Item = AddressRange<ImageAddress>> + 'a {
        self.view
            .ranges(self.scope.ranges)
            .iter()
            .map(CodeRange::get)
    }

    /// Whether its scope's code contains `address`.
    pub fn in_scope(self, address: ImageAddress) -> bool {
        self.ranges().any(|range| range.contains(address))
    }

    /// For a Go local, its declaration, past whose line it is visible.
    pub fn go_declaration(self) -> Option<GoDeclaration> {
        if self.record.flags & object_flags::GO_DECLARED == 0 {
            return None;
        }

        Some(GoDeclaration {
            location: self.declaration()?,
            instance: some(self.scope.go_instance).map(CodeInstanceId::new),
        })
    }

    /// The inline instance owning it, or `None` for the physical frame.
    pub fn instance(self) -> Option<CodeInstanceId> {
        some(self.scope.instance).map(CodeInstanceId::new)
    }

    pub const fn lexical_depth(self) -> u32 {
        self.scope.lexical_depth.get()
    }

    pub fn type_info(self) -> TypeResolution {
        self.view.resolution(
            self.record.ty,
            self.record.flags & object_flags::TYPE_MALFORMED != 0,
        )
    }

    /// The type it is, when it is known.
    pub fn type_id(self) -> Option<TypeId> {
        (self.record.flags & object_flags::TYPE_MALFORMED == 0)
            .then(|| TypeId::new(self.record.ty.get()))
    }

    pub fn escaped(self) -> Option<TypeId> {
        some(self.record.escaped).map(TypeId::new)
    }

    pub const fn hidden(self) -> bool {
        self.record.flags & object_flags::HIDDEN != 0
    }

    pub fn coroutine(self) -> Option<TypeId> {
        some(self.record.coroutine).map(TypeId::new)
    }

    pub fn value(self) -> Metadata<ValueDescription> {
        match self.record.value_kind {
            value_kinds::CONSTANT => self.view.constant(self.record.value).map_or_else(
                || Metadata::Malformed("the constant's bytes lie outside the image".into()),
                |value| Metadata::Value(ValueDescription::Constant(Box::new(value))),
            ),
            kind => match self.view.location(kind, self.record.value) {
                Metadata::Value(list) => Metadata::Value(ValueDescription::Location(list)),
                Metadata::Absent(absence) => Metadata::Absent(absence),
                Metadata::Malformed(why) => Metadata::Malformed(why),
            },
        }
    }

    /// Its location list, when its value has one.
    pub fn location(self) -> Option<LocationListId> {
        (self.record.value_kind == value_kinds::LOCATION)
            .then(|| LocationListId(self.record.value.get()))
    }

    pub fn frame_base(self) -> Metadata<LocationListId> {
        self.view
            .location(self.scope.frame_base_kind, self.scope.frame_base)
    }

    pub fn malformed(self) -> Option<Arc<str>> {
        (self.record.malformed.get() != NONE).then(|| self.view.text(self.record.malformed))
    }

    pub const fn debug_info_offset(self) -> Option<u64> {
        if self.record.flags & object_flags::OFFSET == 0 {
            None
        } else {
            Some(self.record.debug_info_offset.get())
        }
    }
}

/// One function of a [`VariableView`].
#[derive(Debug, Clone, Copy)]
pub struct VariableFunction<'a> {
    view: VariableView<'a>,
    id: VariableFunctionId,
    record: &'a VariableFunctionRecord,
}

impl<'a> VariableFunction<'a> {
    pub const fn id(self) -> VariableFunctionId {
        self.id
    }

    /// Whether its code contains `address`.
    pub fn contains(self, address: ImageAddress) -> bool {
        self.view
            .ranges(self.record.ranges)
            .iter()
            .any(|range| range.get().contains(address))
    }

    /// Its objects, in the order a frame shows them.
    pub fn objects(self) -> impl ExactSizeIterator<Item = Object<'a>> + 'a {
        let view = self.view;
        self.record
            .objects
            .rows(self.view.function_objects)
            .iter()
            .map(move |item| view.object(ObjectId(item.value.get())))
    }

    /// The name a Go function value calling it shows.
    pub fn name(self) -> Option<&'a str> {
        some(self.record.name).map(|name| self.view.strings.get(StrId(name)))
    }

    /// The variables a Go closure captured, in its context, or why they
    /// cannot be known.
    pub fn captures(self) -> Result<Vec<Capture>, Arc<str>> {
        if self.record.flags & function_flags::CAPTURES_MALFORMED != 0 {
            return Err(self.view.text(self.record.captures.first().into()));
        }

        Ok(self
            .record
            .captures
            .rows(self.view.captures)
            .iter()
            .map(|capture| Capture {
                name: self.view.text(capture.name),
                offset: capture.offset.get(),
                type_info: self.view.resolution(capture.ty, capture.malformed != 0),
            })
            .collect())
    }

    /// How the function returns its values, when that is known.
    pub fn returns(self) -> Option<ReturnConvention> {
        let record = self.record;

        match record.returns {
            returns::GO_REGISTERS => Some(ReturnConvention::GoRegisters),
            returns::SYSTEM_V => Some(ReturnConvention::SystemV(Box::new(SystemV {
                name: self.view.text(record.returned_name),
                ty: self.view.resolution(
                    record.returned_type,
                    record.flags & function_flags::RETURNED_MALFORMED != 0,
                ),
                language: language_of(record.language, record.other_language.get()),
                rewritten: record.flags & function_flags::REWRITTEN != 0,
            }))),
            _ => None,
        }
    }
}

/// Where a block constant's bytes are in the pool.
fn block(record: &ConstantRecord) -> Option<std::ops::Range<usize>> {
    let first = usize::try_from(record.low.get()).ok()?;
    let length = usize::try_from(record.high.get()).ok()?;
    Some(first..first.checked_add(length)?)
}

/// The location [`location`] encoded as `kind` and `value`.
pub(super) fn decode_location(
    strings: Strings<'_>,
    kind: u8,
    value: U32,
) -> Metadata<LocationListId> {
    match kind {
        value_kinds::LOCATION => Metadata::Value(LocationListId(value.get())),
        value_kinds::NO_LOCATION => Metadata::Absent(MetadataAbsence::NoLocation),
        value_kinds::NO_FRAME_BASE => Metadata::Absent(MetadataAbsence::NoFrameBase),
        value_kinds::NOT_APPLICABLE => Metadata::Absent(MetadataAbsence::NotApplicable),
        _ => Metadata::Malformed(strings.get(StrId(value.get())).into()),
    }
}

/// Whether `kind` and `value` are a location [`location`] encodes, of an
/// image with `lists` lists and `strings`.
pub(super) fn valid_location(kind: u8, value: U32, lists: usize, strings: Strings<'_>) -> bool {
    match kind {
        value_kinds::LOCATION => (value.get() as usize) < lists,
        value_kinds::NO_LOCATION | value_kinds::NO_FRAME_BASE | value_kinds::NOT_APPLICABLE => {
            value.get() == 0
        }
        value_kinds::MALFORMED => strings.contains(StrId(value.get())),
        _ => false,
    }
}

/// What the variables' rows may name.
struct Bounds<'a> {
    strings: Strings<'a>,
    types: usize,
    instances: usize,
    lists: usize,
    files: usize,
}

impl Bounds<'_> {
    fn string(&self, id: U32) -> bool {
        self.strings.contains(StrId(id.get()))
    }

    fn optional_string(&self, id: U32) -> bool {
        id.get() == NONE || self.string(id)
    }

    const fn optional_type(&self, id: U32) -> bool {
        id.get() == NONE || (id.get() as usize) < self.types
    }

    const fn optional_instance(&self, id: U32) -> bool {
        id.get() == NONE || (id.get() as usize) < self.instances
    }

    /// A type, or with `malformed` why it is unknown.
    fn resolution(&self, ty: U32, malformed: bool) -> bool {
        if malformed {
            self.string(ty)
        } else {
            (ty.get() as usize) < self.types
        }
    }

    fn location(&self, kind: u8, value: U32) -> bool {
        valid_location(kind, value, self.lists, self.strings)
    }
}

/// Checks the data objects, their functions, and their indexes.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let view = VariableView::new(image);
    let bounds = Bounds {
        strings: view.strings,
        types: image.table::<super::types::TypeRecord>().len(),
        instances: image.table::<super::functions::InstanceRecord>().len(),
        lists: image.table::<super::locations::LocationListRecord>().len(),
        files: image.table::<super::lines::FileRecord>().len(),
    };

    validate_objects(view, &bounds)?;
    validate_functions(view, &bounds)?;
    validate_indexes(view, &bounds)
}

fn validate_objects(view: VariableView<'_>, bounds: &Bounds<'_>) -> Result<(), String> {
    if !view
        .ranges
        .iter()
        .all(|range| range.start.get() < range.end.get())
    {
        return Err("a scope's code is empty".into());
    }

    if !view.scopes.iter().all(|scope| {
        scope.ranges.within(view.ranges.len())
            && bounds.optional_instance(scope.instance)
            && bounds.optional_instance(scope.go_instance)
            && scope.frame_base_kind != value_kinds::CONSTANT
            && bounds.location(scope.frame_base_kind, scope.frame_base)
    }) {
        return Err("a scope is malformed".into());
    }

    if !view.constants.iter().all(|constant| match constant.kind {
        constant_kinds::UNSIGNED | constant_kinds::SIGNED | constant_kinds::FIXED => true,
        constant_kinds::BYTES => {
            block(constant).is_some_and(|block| block.end <= view.constant_bytes.len())
        }
        _ => false,
    }) {
        return Err("a constant is malformed".into());
    }

    if !view.objects.iter().all(|object| {
        bounds.string(object.name)
            && object.declaration.valid(bounds.files)
            && (object.scope.get() as usize) < view.scopes.len()
            && usize::from(object.kind) < VARIABLE_KINDS.len()
            && object.flags & !object_flags::ALL == 0
            && bounds.resolution(object.ty, object.flags & object_flags::TYPE_MALFORMED != 0)
            && bounds.optional_type(object.escaped)
            && bounds.optional_type(object.coroutine)
            && bounds.optional_string(object.malformed)
            && (object.flags & object_flags::OFFSET != 0 || object.debug_info_offset.get() == 0)
            && (object.flags & object_flags::GO_DECLARED == 0 || object.declaration.get().is_some())
            && match object.value_kind {
                value_kinds::CONSTANT => (object.value.get() as usize) < view.constants.len(),
                kind => bounds.location(kind, object.value),
            }
    }) {
        return Err("a data object is malformed".into());
    }

    Ok(())
}

fn validate_functions(view: VariableView<'_>, bounds: &Bounds<'_>) -> Result<(), String> {
    if !view.captures.iter().all(|capture| {
        bounds.string(capture.name)
            && capture.malformed <= 1
            && bounds.resolution(capture.ty, capture.malformed != 0)
    }) {
        return Err("a capture is malformed".into());
    }

    let objects = view.objects.len();

    if !view
        .function_objects
        .iter()
        .all(|item| (item.value.get() as usize) < objects)
    {
        return Err("a list names no data object".into());
    }

    if !view.globals.iter().all(|global| {
        (global.object.get() as usize) < objects
            && bounds.string(global.qualified_name)
            && bounds.optional_string(global.linkage_name)
            && global.external <= 1
    }) {
        return Err("a global is malformed".into());
    }

    if !view.functions.iter().all(|function| {
        function.ranges.within(view.ranges.len())
            && function.objects.within(view.function_objects.len())
            && bounds.optional_string(function.name)
            && function.flags & !function_flags::ALL == 0
            && if function.flags & function_flags::CAPTURES_MALFORMED == 0 {
                function.captures.within(view.captures.len())
            } else {
                bounds.string(function.captures.first().into()) && function.captures.count() == 0
            }
            && match function.returns {
                returns::UNKNOWN | returns::GO_REGISTERS => {
                    function.returned_name.get() == NONE
                        && function.returned_type.get() == NONE
                        && function.language == 0
                        && function.other_language.get() == 0
                        && function.flags
                            & (function_flags::RETURNED_MALFORMED | function_flags::REWRITTEN)
                            == 0
                }
                returns::SYSTEM_V => {
                    bounds.string(function.returned_name)
                        && bounds.resolution(
                            function.returned_type,
                            function.flags & function_flags::RETURNED_MALFORMED != 0,
                        )
                        && valid_language(function.language, function.other_language.get())
                }
                _ => false,
            }
    }) {
        return Err("a function's variables are malformed".into());
    }

    Ok(())
}

fn validate_indexes(view: VariableView<'_>, bounds: &Bounds<'_>) -> Result<(), String> {
    let functions = view.functions.len();
    let mut prefix_max_end = 0;
    let mut previous = None;

    for start in view.starts {
        prefix_max_end = prefix_max_end.max(start.end.get());
        let key = (start.start.get(), start.function.get());
        if start.start.get() >= start.end.get()
            || start.prefix_max_end.get() != prefix_max_end
            || (start.function.get() as usize) >= functions
            || previous.is_some_and(|previous| previous > key)
        {
            return Err("the function starts are malformed".into());
        }

        previous = Some(key);
    }

    let keyed = |index: &[Keyed], length: usize| {
        index.is_sorted_by(|earlier, later| earlier.key.get() < later.key.get())
            && index
                .iter()
                .all(|entry| (entry.value.get() as usize) < length)
    };

    if !keyed(view.go_entries, functions) || !keyed(view.offsets, view.objects.len()) {
        return Err("an index of variables is malformed".into());
    }

    if !view
        .procedures
        .is_sorted_by(|earlier, later| earlier.offset.get() < later.offset.get())
        || !view.procedures.iter().all(|procedure| {
            procedure.location_kind != value_kinds::CONSTANT
                && bounds.location(procedure.location_kind, procedure.location)
        })
    {
        return Err("a DWARF procedure is malformed".into());
    }

    Ok(())
}
