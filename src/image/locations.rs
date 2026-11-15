use std::hash::BuildHasher as _;

use gimli::ValueType;
use zerocopy::little_endian::{U16, U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::{Builder, Image, NONE, Record, Span, TableKind};
use crate::{AddressRange, ImageAddress};

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ExpressionRecord {
    pub bytes: U32,
    pub length: U32,

    pub unit: U32,

    pub addresses: Span<IndexedAddressRecord>,

    pub procedures: Span<ProcedureRecord>,

    pub version: U16,
    pub address_size: u8,

    pub dwarf64: u8,
}

impl Record for ExpressionRecord {
    const KIND: TableKind = TableKind::Expressions;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct IndexedAddressRecord {
    pub index: U64,
    pub address: U64,
}

impl Record for IndexedAddressRecord {
    const KIND: TableKind = TableKind::IndexedAddresses;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ProcedureRecord {
    pub offset: U64,

    pub list: U32,
}

impl Record for ProcedureRecord {
    const KIND: TableKind = TableKind::Procedures;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LocationListRecord {
    pub entries: Span<LocationEntryRecord>,
}

impl Record for LocationListRecord {
    const KIND: TableKind = TableKind::LocationLists;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct LocationEntryRecord {
    pub start: U64,
    pub end: U64,
    pub expression: U32,

    pub flags: u8,
}

impl Record for LocationEntryRecord {
    const KIND: TableKind = TableKind::LocationEntries;
}

pub const DEFAULT: u8 = 1;

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct EvaluationUnitRecord {
    pub offset: U64,

    pub base_types: Span<BaseTypeRecord>,

    pub language: U16,
    pub flags: u8,
}

impl Record for EvaluationUnitRecord {
    const KIND: TableKind = TableKind::EvaluationUnits;
}

pub mod unit_flags {
    pub const OFFSET: u8 = 1 << 0;

    pub const LANGUAGE: u8 = 1 << 1;

    pub const ALL: u8 = OFFSET | LANGUAGE;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct BaseTypeRecord {
    pub offset: U64,
    pub value_type: u8,
}

impl Record for BaseTypeRecord {
    const KIND: TableKind = TableKind::BaseTypes;
}

const VALUE_TYPES: [ValueType; 10] = [
    ValueType::I8,
    ValueType::U8,
    ValueType::I16,
    ValueType::U16,
    ValueType::I32,
    ValueType::U32,
    ValueType::I64,
    ValueType::U64,
    ValueType::F32,
    ValueType::F64,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExpressionId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocationListId(pub u32);

#[derive(Debug, Clone, Default)]
pub struct EvaluationUnit {
    pub offset: Option<u64>,
    pub language: Option<gimli::DwLang>,

    pub base_types: Vec<(u64, ValueType)>,
}

#[derive(Debug, Default, Clone)]
pub struct LocationsBuilder {
    bytes: Vec<u8>,
    expressions: Vec<ExpressionRecord>,
    addresses: Vec<IndexedAddressRecord>,
    procedures: Vec<ProcedureRecord>,
    lists: Vec<LocationListRecord>,
    entries: Vec<LocationEntryRecord>,
    units: Vec<EvaluationUnitRecord>,
    base_types: Vec<BaseTypeRecord>,
    pooled_expressions: super::strings::Pooled,
    pooled_lists: super::strings::Pooled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the locations do not fit an image")]
pub struct TooLarge;

fn index(length: usize) -> Result<u32, TooLarge> {
    u32::try_from(length)
        .ok()
        .filter(|index| *index != NONE)
        .ok_or(TooLarge)
}

impl LocationsBuilder {
    pub fn unit(&mut self, unit: &EvaluationUnit) -> Result<u32, TooLarge> {
        let mut base_types = unit.base_types.clone();
        base_types.sort_unstable_by_key(|(offset, _)| *offset);
        base_types.dedup_by_key(|(offset, _)| *offset);
        let first = index(self.base_types.len())?;

        for (offset, value_type) in &base_types {
            let code = VALUE_TYPES
                .iter()
                .position(|known| known == value_type)
                .ok_or(TooLarge)?;
            self.base_types.push(BaseTypeRecord {
                offset: (*offset).into(),
                value_type: u8::try_from(code + 1).map_err(|_| TooLarge)?,
            });
        }

        let mut flags = 0;
        if unit.offset.is_some() {
            flags |= unit_flags::OFFSET;
        }

        if unit.language.is_some() {
            flags |= unit_flags::LANGUAGE;
        }

        self.units.push(EvaluationUnitRecord {
            offset: unit.offset.unwrap_or(0).into(),
            base_types: Span::new(first, index(base_types.len())?),
            language: unit.language.map_or(0, |language| language.0).into(),
            flags,
        });
        index(self.units.len() - 1)
    }

    pub fn expression(
        &mut self,
        bytes: &[u8],
        unit: u32,
        encoding: gimli::Encoding,
        addresses: &[(u64, u64)],
        procedures: &[(u64, Option<LocationListId>)],
    ) -> Result<ExpressionId, TooLarge> {
        let mut addresses = addresses.to_vec();
        addresses.sort_unstable();
        addresses.dedup_by_key(|(index, _)| *index);
        let mut procedures = procedures.to_vec();
        procedures.sort_unstable_by_key(|(offset, _)| *offset);
        procedures.dedup_by_key(|(offset, _)| *offset);
        let hash = foldhash::fast::FixedState::default().hash_one((
            bytes,
            unit,
            encoding,
            &addresses,
            &procedures,
        ));
        let same = |tables: LocationTables<'_>, id: u32| {
            let found = tables.expression(ExpressionId(id));
            found.bytes() == bytes
                && found.unit() == unit
                && found.encoding() == encoding
                && found.addresses().eq(addresses.iter().copied())
                && found.procedures().eq(procedures.iter().copied())
        };

        if let Some(id) = self
            .pooled_expressions
            .find(hash, |id| same(self.tables(), id))
        {
            return Ok(ExpressionId(id));
        }

        let id = index(self.expressions.len())?;
        let record = ExpressionRecord {
            bytes: index(self.bytes.len())?.into(),
            length: index(bytes.len())?.into(),
            unit: unit.into(),
            addresses: Span::new(index(self.addresses.len())?, index(addresses.len())?),
            procedures: Span::new(index(self.procedures.len())?, index(procedures.len())?),
            version: encoding.version.into(),
            address_size: encoding.address_size,
            dwarf64: u8::from(encoding.format == gimli::Format::Dwarf64),
        };

        self.bytes.extend_from_slice(bytes);
        index(self.bytes.len())?;
        self.addresses.extend(
            addresses
                .iter()
                .map(|(index, address)| IndexedAddressRecord {
                    index: (*index).into(),
                    address: (*address).into(),
                }),
        );
        self.procedures
            .extend(procedures.iter().map(|(offset, list)| ProcedureRecord {
                offset: (*offset).into(),
                list: list.map_or(NONE, |list| list.0).into(),
            }));
        self.expressions.push(record);
        self.pooled_expressions.insert(hash, id);
        Ok(ExpressionId(id))
    }

    pub fn list(
        &mut self,
        entries: &[(Option<AddressRange<ImageAddress>>, ExpressionId)],
    ) -> Result<LocationListId, TooLarge> {
        let hash = foldhash::fast::FixedState::default().hash_one(entries);
        let same = |tables: LocationTables<'_>, id: u32| {
            tables
                .list(LocationListId(id))
                .entries()
                .map(|(range, expression)| (range, expression.id()))
                .eq(entries.iter().copied())
        };

        if let Some(id) = self.pooled_lists.find(hash, |id| same(self.tables(), id)) {
            return Ok(LocationListId(id));
        }

        let id = index(self.lists.len())?;
        self.lists.push(LocationListRecord {
            entries: Span::new(index(self.entries.len())?, index(entries.len())?),
        });
        self.entries.extend(
            entries
                .iter()
                .map(|(range, expression)| LocationEntryRecord {
                    start: range.map_or(0, |range| range.start.get()).into(),
                    end: range.map_or(0, |range| range.end.get()).into(),
                    expression: expression.0.into(),
                    flags: if range.is_none() { DEFAULT } else { 0 },
                }),
        );
        index(self.entries.len())?;
        self.pooled_lists.insert(hash, id);
        Ok(LocationListId(id))
    }

    pub fn largest_table(&self) -> usize {
        [
            self.bytes.len(),
            self.expressions.len(),
            self.addresses.len(),
            self.procedures.len(),
            self.lists.len(),
            self.entries.len(),
        ]
        .into_iter()
        .max()
        .unwrap_or(0)
    }

    pub fn absorb(
        &mut self,
        other: &Self,
    ) -> Result<(Vec<ExpressionId>, Vec<LocationListId>), TooLarge> {
        let tables = other.tables();
        let mut expressions = Vec::with_capacity(other.expressions.len());
        let mut lists = Vec::<LocationListId>::with_capacity(other.lists.len());
        let mut procedures = Vec::new();
        let mut entries = Vec::new();

        while expressions.len() < other.expressions.len() || lists.len() < other.lists.len() {
            if let Some(record) = other.expressions.get(expressions.len()) {
                let called = record.procedures.rows(&other.procedures);
                if called.iter().all(|procedure| {
                    procedure.list.get() == NONE || (procedure.list.get() as usize) < lists.len()
                }) {
                    let expression = tables.expression(ExpressionId(index(expressions.len())?));
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "the check above found every list the expression names pooled"
                    )]
                    let pooled = |list: LocationListId| lists[list.0 as usize];
                    procedures.clear();
                    procedures.extend(
                        expression
                            .procedures()
                            .map(|(offset, list)| (offset, list.map(pooled))),
                    );
                    let addresses = expression.addresses().collect::<Vec<_>>();
                    expressions.push(self.expression(
                        expression.bytes(),
                        expression.unit(),
                        expression.encoding(),
                        &addresses,
                        &procedures,
                    )?);
                    continue;
                }
            }

            let list = tables.list(LocationListId(index(lists.len())?));
            #[expect(
                clippy::indexing_slicing,
                reason = "a builder pools a list only after every expression it names, and \
                          those are pooled here first"
            )]
            let pooled = |expression: Expression<'_>| expressions[expression.id().0 as usize];
            entries.clear();
            entries.extend(
                list.entries()
                    .map(|(range, expression)| (range, pooled(expression))),
            );
            lists.push(self.list(&entries)?);
        }

        Ok((expressions, lists))
    }

    pub fn tables(&self) -> LocationTables<'_> {
        LocationTables {
            bytes: &self.bytes,
            expressions: &self.expressions,
            addresses: &self.addresses,
            procedures: &self.procedures,
            lists: &self.lists,
            entries: &self.entries,
            units: &self.units,
            base_types: &self.base_types,
        }
    }

    pub fn add_to<'a>(&'a self, builder: &mut Builder<'a>) {
        builder
            .bytes(TableKind::ExpressionBytes, self.bytes.clone())
            .table(&self.expressions)
            .table(&self.addresses)
            .table(&self.procedures)
            .table(&self.lists)
            .table(&self.entries)
            .table(&self.units)
            .table(&self.base_types);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LocationTables<'a> {
    bytes: &'a [u8],
    expressions: &'a [ExpressionRecord],
    addresses: &'a [IndexedAddressRecord],
    procedures: &'a [ProcedureRecord],
    lists: &'a [LocationListRecord],
    entries: &'a [LocationEntryRecord],
    units: &'a [EvaluationUnitRecord],
    base_types: &'a [BaseTypeRecord],
}

impl<'a> LocationTables<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            bytes: image.bytes(TableKind::ExpressionBytes),
            expressions: image.table(),
            addresses: image.table(),
            procedures: image.table(),
            lists: image.table(),
            entries: image.table(),
            units: image.table(),
            base_types: image.table(),
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass ids from records whose validation checks every expression id \
                  against this table"
    )]
    pub const fn expression(self, id: ExpressionId) -> Expression<'a> {
        debug_assert!(
            (id.0 as usize) < self.expressions.len(),
            "an expression id is outside its table"
        );
        Expression {
            tables: self,
            id,
            record: &self.expressions[id.0 as usize],
        }
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "callers pass ids from records whose validation checks every list id against \
                  this table"
    )]
    pub fn list(self, id: LocationListId) -> LocationList<'a> {
        debug_assert!(
            (id.0 as usize) < self.lists.len(),
            "list {} is not one of {}",
            id.0,
            self.lists.len()
        );
        let record = &self.lists[id.0 as usize];
        LocationList {
            tables: self,
            entries: record.entries.rows(self.entries),
        }
    }

    pub fn unit_offset(self, unit: u32) -> Option<u64> {
        let record = self.units.get(unit as usize)?;
        (record.flags & unit_flags::OFFSET != 0).then(|| record.offset.get())
    }

    pub fn unit_language(self, unit: u32) -> Option<gimli::DwLang> {
        let record = self.units.get(unit as usize)?;
        (record.flags & unit_flags::LANGUAGE != 0).then(|| gimli::DwLang(record.language.get()))
    }

    pub fn base_type(self, unit: u32, offset: u64) -> Option<ValueType> {
        let record = self.units.get(unit as usize)?;
        let types = record.base_types.rows(self.base_types);
        let found = types
            .binary_search_by_key(&offset, |record| record.offset.get())
            .ok()?;
        let code = usize::from(types.get(found)?.value_type);
        VALUE_TYPES.get(code.checked_sub(1)?).copied()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Expression<'a> {
    tables: LocationTables<'a>,
    id: ExpressionId,
    record: &'a ExpressionRecord,
}

impl<'a> Expression<'a> {
    pub const fn id(self) -> ExpressionId {
        self.id
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "expression validation checks every record's bytes lie within the pool"
    )]
    pub fn bytes(self) -> &'a [u8] {
        let start = self.record.bytes.get() as usize;
        &self.tables.bytes[start..start + self.record.length.get() as usize]
    }

    pub const fn unit(self) -> u32 {
        self.record.unit.get()
    }

    pub fn unit_offset(self) -> Option<u64> {
        self.tables.unit_offset(self.unit())
    }

    pub fn base_type(self, offset: u64) -> Option<ValueType> {
        self.tables.base_type(self.unit(), offset)
    }

    pub const fn encoding(self) -> gimli::Encoding {
        gimli::Encoding {
            format: if self.record.dwarf64 == 0 {
                gimli::Format::Dwarf32
            } else {
                gimli::Format::Dwarf64
            },
            version: self.record.version.get(),
            address_size: self.record.address_size,
        }
    }

    fn address_records(self) -> &'a [IndexedAddressRecord] {
        self.record.addresses.rows(self.tables.addresses)
    }

    fn procedure_records(self) -> &'a [ProcedureRecord] {
        self.record.procedures.rows(self.tables.procedures)
    }

    pub fn addresses(self) -> impl Iterator<Item = (u64, u64)> + 'a {
        self.address_records()
            .iter()
            .map(|record| (record.index.get(), record.address.get()))
    }

    pub fn procedures(self) -> impl Iterator<Item = (u64, Option<LocationListId>)> + 'a {
        self.procedure_records().iter().map(|record| {
            (
                record.offset.get(),
                (record.list.get() != NONE).then(|| LocationListId(record.list.get())),
            )
        })
    }

    pub fn indexed_address(self, index: u64) -> Option<u64> {
        let records = self.address_records();
        let found = records
            .binary_search_by_key(&index, |record| record.index.get())
            .ok()?;
        Some(records.get(found)?.address.get())
    }

    pub fn procedure(self, offset: u64) -> Option<Procedure<'a>> {
        let records = self.procedure_records();
        let found = records
            .binary_search_by_key(&offset, |record| record.offset.get())
            .ok()?;
        Some(match records.get(found)?.list.get() {
            NONE => Procedure::Unlocated,
            list => Procedure::Located(self.tables.list(LocationListId(list))),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Procedure<'a> {
    Located(LocationList<'a>),

    Unlocated,
}

impl<'a> Procedure<'a> {
    pub const fn locations(self) -> Option<LocationList<'a>> {
        match self {
            Self::Located(list) => Some(list),
            Self::Unlocated => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LocationList<'a> {
    tables: LocationTables<'a>,
    entries: &'a [LocationEntryRecord],
}

impl<'a> LocationList<'a> {
    pub fn entries(
        self,
    ) -> impl ExactSizeIterator<Item = (Option<AddressRange<ImageAddress>>, Expression<'a>)> + 'a
    {
        self.entries.iter().map(move |entry| {
            let range = (entry.flags & DEFAULT == 0).then(|| AddressRange {
                start: ImageAddress::new(entry.start.get()),
                end: ImageAddress::new(entry.end.get()),
            });
            (
                range,
                self.tables.expression(ExpressionId(entry.expression.get())),
            )
        })
    }

    pub const fn is_empty(self) -> bool {
        self.entries.is_empty()
    }
}

pub(super) fn validate(image: &Image) -> Result<(), String> {
    let tables = LocationTables::new(image);
    if !tables.units.iter().all(|unit| {
        unit.flags & !unit_flags::ALL == 0
            && (unit.flags & unit_flags::OFFSET != 0 || unit.offset.get() == 0)
            && (unit.flags & unit_flags::LANGUAGE != 0 || unit.language.get() == 0)
            && unit.base_types.within(tables.base_types.len())
            && {
                let types = unit.base_types.rows(tables.base_types);
                types
                    .iter()
                    .all(|record| (1..=VALUE_TYPES.len()).contains(&usize::from(record.value_type)))
                    && types
                        .is_sorted_by(|earlier, later| earlier.offset.get() < later.offset.get())
            }
    }) {
        return Err("an evaluation unit is malformed".into());
    }

    if !tables
        .lists
        .iter()
        .all(|list| list.entries.within(tables.entries.len()))
        || !tables.entries.iter().all(|entry| {
            (entry.expression.get() as usize) < tables.expressions.len()
                && match entry.flags {
                    DEFAULT => entry.start.get() == 0 && entry.end.get() == 0,
                    0 => entry.start.get() < entry.end.get(),
                    _ => false,
                }
        })
    {
        return Err("a location list is malformed".into());
    }

    if !tables.expressions.iter().all(|record| {
        (record.unit.get() as usize) < tables.units.len()
            && (2..=5).contains(&record.version.get())
            && matches!(record.address_size, 1 | 2 | 4 | 8)
            && record.dwarf64 <= 1
            && (record.bytes.get() as usize)
                .checked_add(record.length.get() as usize)
                .is_some_and(|end| end <= tables.bytes.len())
            && record.addresses.within(tables.addresses.len())
            && record.procedures.within(tables.procedures.len())
            && {
                record
                    .addresses
                    .rows(tables.addresses)
                    .is_sorted_by(|earlier, later| earlier.index.get() < later.index.get())
            }
            && {
                record
                    .procedures
                    .rows(tables.procedures)
                    .iter()
                    .all(|procedure| {
                        procedure.list.get() == NONE
                            || (procedure.list.get() as usize) < tables.lists.len()
                    })
                    && record
                        .procedures
                        .rows(tables.procedures)
                        .is_sorted_by(|earlier, later| earlier.offset.get() < later.offset.get())
            }
    }) {
        return Err("an expression is malformed".into());
    }

    Ok(())
}
