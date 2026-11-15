use std::sync::Arc;

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::locations::ExpressionId;
use super::strings::{StrId, Strings, StringsBuilder};
use super::{Builder, Image, NONE, Record, SharedRecord, TableKind};
use crate::TypeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LayoutChild {
    Member(u32),
    Base(u32),
    Discriminant,
    VariantMember {
        variant: u32,
        member: u32,
    },

    /// A dimension's lower bound, extent, or stride, as `part` says.
    Bound {
        dimension: u32,
        part: BoundPart,
    },

    DataLocation,

    Allocated,

    Associated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BoundPart {
    Lower,
    Extent,
    Stride,
}

impl BoundPart {
    const ALL: [Self; 3] = [Self::Lower, Self::Extent, Self::Stride];
}

#[derive(Debug, Clone, Default)]
pub struct TypeFacts {
    pub dictionary_indices: Vec<(TypeId, u64)>,

    pub passed_by_value: Vec<(TypeId, bool)>,

    pub complex_parts: Vec<(Arc<str>, u64, TypeId)>,

    pub dynamic_layouts: Vec<(TypeId, LayoutChild, ExpressionId)>,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct TypeFactRecord {
    pub ty: U32,
    pub value: U64,
}

impl SharedRecord for TypeFactRecord {
    const NAME: &'static str = "TypeFactRecord";
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct ComplexPartRecord {
    pub name: U32,
    pub size: U64,
    pub ty: U32,
}

impl Record for ComplexPartRecord {
    const KIND: TableKind = TableKind::ComplexParts;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct DynamicLayoutRecord {
    pub aggregate: U32,

    pub first: U32,

    pub second: U32,
    pub expression: U32,
    pub kind: u8,
}

impl Record for DynamicLayoutRecord {
    const KIND: TableKind = TableKind::DynamicLayouts;
}

pub mod children {
    pub const MEMBER: u8 = 0;

    pub const BASE: u8 = 1;

    pub const DISCRIMINANT: u8 = 2;

    pub const VARIANT_MEMBER: u8 = 3;

    pub const BOUND: u8 = 4;

    pub const DATA_LOCATION: u8 = 5;

    pub const ALLOCATED: u8 = 6;

    pub const ASSOCIATED: u8 = 7;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the type facts do not fit an image")]
pub struct TooMany;

const fn encode_child(child: LayoutChild) -> (u8, u32, u32) {
    match child {
        LayoutChild::Member(member) => (children::MEMBER, member, 0),
        LayoutChild::Base(base) => (children::BASE, base, 0),
        LayoutChild::Discriminant => (children::DISCRIMINANT, 0, 0),
        LayoutChild::VariantMember { variant, member } => {
            (children::VARIANT_MEMBER, variant, member)
        }
        LayoutChild::Bound { dimension, part } => (children::BOUND, dimension, part as u32),
        LayoutChild::DataLocation => (children::DATA_LOCATION, 0, 0),
        LayoutChild::Allocated => (children::ALLOCATED, 0, 0),
        LayoutChild::Associated => (children::ASSOCIATED, 0, 0),
    }
}

fn facts(pairs: impl Iterator<Item = (TypeId, u64)>) -> Vec<TypeFactRecord> {
    let mut pairs = pairs.collect::<Vec<_>>();
    pairs.sort_unstable();
    pairs
        .into_iter()
        .map(|(ty, value)| TypeFactRecord {
            ty: ty.get().into(),
            value: value.into(),
        })
        .collect()
}

pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    type_facts: &TypeFacts,
) -> Result<(), TooMany> {
    let dictionary = facts(type_facts.dictionary_indices.iter().copied());
    let passed = facts(
        type_facts
            .passed_by_value
            .iter()
            .map(|(ty, by_value)| (*ty, u64::from(*by_value))),
    );
    let mut complex = type_facts
        .complex_parts
        .iter()
        .map(|(name, size, ty)| (&**name, *size, *ty))
        .collect::<Vec<_>>();
    complex.sort_unstable();
    let complex = complex
        .into_iter()
        .map(|(name, size, ty)| {
            Ok(ComplexPartRecord {
                name: strings.push(name).ok_or(TooMany)?.0.into(),
                size: size.into(),
                ty: ty.get().into(),
            })
        })
        .collect::<Result<Vec<_>, TooMany>>()?;
    let mut layouts = type_facts
        .dynamic_layouts
        .iter()
        .map(|(aggregate, child, expression)| {
            let (kind, first, second) = encode_child(*child);
            ((aggregate.get(), kind, first, second), expression.0)
        })
        .collect::<Vec<_>>();
    layouts.sort_unstable();
    let layouts = layouts
        .into_iter()
        .map(
            |((aggregate, kind, first, second), expression)| DynamicLayoutRecord {
                aggregate: aggregate.into(),
                first: first.into(),
                second: second.into(),
                expression: expression.into(),
                kind,
            },
        )
        .collect::<Vec<_>>();

    builder
        .owned_shared(TableKind::DictionaryIndices, dictionary)
        .owned_shared(TableKind::PassedByValue, passed)
        .owned_table(complex)
        .owned_table(layouts);
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct TypeFactsView<'a> {
    strings: Strings<'a>,
    dictionary: &'a [TypeFactRecord],
    passed: &'a [TypeFactRecord],
    complex: &'a [ComplexPartRecord],
    layouts: &'a [DynamicLayoutRecord],
}

fn fact(facts: &[TypeFactRecord], ty: TypeId) -> Option<u64> {
    facts
        .binary_search_by_key(&ty.get(), |fact| fact.ty.get())
        .ok()
        .and_then(|found| facts.get(found))
        .map(|fact| fact.value.get())
}

impl<'a> TypeFactsView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            dictionary: image.shared(TableKind::DictionaryIndices),
            passed: image.shared(TableKind::PassedByValue),
            complex: image.table(),
            layouts: image.table(),
        }
    }

    pub fn dictionary_index(self, ty: TypeId) -> Option<u64> {
        fact(self.dictionary, ty)
    }

    pub fn passed_by_value(self, ty: TypeId) -> Option<bool> {
        fact(self.passed, ty).map(|value| value != 0)
    }

    pub fn complex_part(self, name: &str, size: u64) -> Option<TypeId> {
        let key =
            |part: &ComplexPartRecord| (self.strings.get(StrId(part.name.get())), part.size.get());
        self.complex
            .binary_search_by(|part| key(part).cmp(&(name, size)))
            .ok()
            .and_then(|found| self.complex.get(found))
            .map(|part| TypeId::new(part.ty.get()))
    }

    pub fn dynamic_layout(self, aggregate: TypeId, child: LayoutChild) -> Option<ExpressionId> {
        let (kind, first, second) = encode_child(child);
        let key = (aggregate.get(), kind, first, second);
        self.layouts
            .binary_search_by_key(&key, |layout| {
                (
                    layout.aggregate.get(),
                    layout.kind,
                    layout.first.get(),
                    layout.second.get(),
                )
            })
            .ok()
            .and_then(|found| self.layouts.get(found))
            .map(|layout| ExpressionId(layout.expression.get()))
    }
}

pub(super) fn validate(image: &Image) -> Result<(), String> {
    let view = TypeFactsView::new(image);
    let types = image.table::<super::types::TypeRecord>().len();
    let expressions = image.table::<super::locations::ExpressionRecord>().len();
    let valid_type = |ty: U32| ty.get() != NONE && (ty.get() as usize) < types;
    let ordered = |facts: &[TypeFactRecord]| {
        facts.is_sorted_by(|earlier, later| earlier.ty.get() < later.ty.get())
            && facts.iter().all(|fact| valid_type(fact.ty))
    };

    if !ordered(view.dictionary)
        || !ordered(view.passed)
        || !view.passed.iter().all(|fact| fact.value.get() <= 1)
    {
        return Err("a type fact is malformed".into());
    }

    let strings = view.strings;
    if !view
        .complex
        .iter()
        .all(|part| strings.contains(StrId(part.name.get())) && valid_type(part.ty))
        || !view.complex.is_sorted_by(|earlier, later| {
            (strings.get(StrId(earlier.name.get())), earlier.size.get())
                < (strings.get(StrId(later.name.get())), later.size.get())
        })
    {
        return Err("a complex number's part is malformed".into());
    }

    let key = |layout: &DynamicLayoutRecord| {
        (
            layout.aggregate.get(),
            layout.kind,
            layout.first.get(),
            layout.second.get(),
        )
    };

    if !view.layouts.iter().all(|layout| {
        valid_type(layout.aggregate)
            && (layout.expression.get() as usize) < expressions
            && match layout.kind {
                children::MEMBER | children::BASE => layout.second.get() == 0,
                children::DISCRIMINANT => layout.first.get() == 0 && layout.second.get() == 0,
                children::VARIANT_MEMBER => true,
                children::BOUND => (layout.second.get() as usize) < BoundPart::ALL.len(),
                children::DATA_LOCATION | children::ALLOCATED | children::ASSOCIATED => {
                    layout.first.get() == 0 && layout.second.get() == 0
                }
                _ => false,
            }
    }) || !view
        .layouts
        .is_sorted_by(|earlier, later| key(earlier) < key(later))
    {
        return Err("a run-time layout is malformed".into());
    }

    Ok(())
}
