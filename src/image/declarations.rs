//! What the debug information declares of a module beyond its types and
//! variables: integer constants by name, such as a Go package's `const`s,
//! the concrete type each Rust trait object's vtable is for, and the
//! compilers that produced it.

use std::sync::Arc;

use zerocopy::little_endian::{U32, U64};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout, Unaligned};

use super::strings::{StrId, Strings, StringsBuilder};
use super::types::Item;
use super::{Builder, Image, NONE, Record, TableKind};
use crate::{ImageAddress, IntegerValue, TypeId};

/// What [`add_to`] encodes.
#[derive(Debug, Clone, Default)]
pub struct Declarations {
    /// Integer constants by name; a later one replaces an earlier one of
    /// the same name.
    pub constants: Vec<(Arc<str>, IntegerValue)>,

    /// Rust trait objects' vtables, by address, with the concrete type
    /// each is for; a later one replaces an earlier one at its address.
    pub vtables: Vec<(ImageAddress, TypeId)>,

    /// The producers, in the order they were first named.
    pub producers: Vec<Arc<str>>,
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct NamedConstantRecord {
    pub name: U32,

    /// The value's two's-complement bytes, little-endian.
    pub value: [u8; 16],

    /// Whether the value is signed.
    pub signed: u8,
}

impl Record for NamedConstantRecord {
    const KIND: TableKind = TableKind::NamedConstants;
}

#[repr(C)]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable, Unaligned,
)]
pub struct VtableRecord {
    pub address: U64,
    pub ty: U32,
}

impl Record for VtableRecord {
    const KIND: TableKind = TableKind::Vtables;
}

/// Why declarations could not be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the declarations do not fit an image")]
pub struct TooMany;

/// Adds the declarations to `builder`, pooling names in `strings`.
pub fn add_to(
    builder: &mut Builder<'_>,
    strings: &mut StringsBuilder,
    declarations: &Declarations,
) -> Result<(), TooMany> {
    let mut constants = declarations.constants.iter().rev().collect::<Vec<_>>();
    constants.sort_by(|(left, _), (right, _)| left.cmp(right));
    constants.dedup_by(|(later, _), (earlier, _)| later == earlier);
    let constants = constants
        .into_iter()
        .map(|(name, value)| {
            let (value, signed) = match *value {
                IntegerValue::Signed(value) => (value.to_le_bytes(), 1),
                IntegerValue::Unsigned(value) => (value.to_le_bytes(), 0),
            };

            Ok(NamedConstantRecord {
                name: strings.push(name).ok_or(TooMany)?.0.into(),
                value,
                signed,
            })
        })
        .collect::<Result<Vec<_>, TooMany>>()?;

    let mut vtables = declarations.vtables.iter().rev().collect::<Vec<_>>();
    vtables.sort_by_key(|(address, _)| *address);
    vtables.dedup_by_key(|(address, _)| *address);
    let vtables = vtables
        .into_iter()
        .map(|(address, ty)| VtableRecord {
            address: address.get().into(),
            ty: ty.get().into(),
        })
        .collect::<Vec<_>>();

    let mut named = std::collections::HashSet::new();
    let mut producers = Vec::new();
    for producer in &declarations.producers {
        let id = strings.push(producer).ok_or(TooMany)?.0;
        if named.insert(id) {
            producers.push(Item { value: id.into() });
        }
    }

    builder
        .owned_table(constants)
        .owned_table(vtables)
        .owned_shared(TableKind::Producers, producers);

    Ok(())
}

/// The declarations of an image.
#[derive(Debug, Clone, Copy)]
pub struct DeclarationView<'a> {
    strings: Strings<'a>,
    constants: &'a [NamedConstantRecord],
    vtables: &'a [VtableRecord],
    producers: &'a [Item],
}

const fn value(record: &NamedConstantRecord) -> IntegerValue {
    if record.signed == 0 {
        IntegerValue::Unsigned(u128::from_le_bytes(record.value))
    } else {
        IntegerValue::Signed(i128::from_le_bytes(record.value))
    }
}

impl<'a> DeclarationView<'a> {
    pub fn new(image: &'a Image) -> Self {
        Self {
            strings: image.strings(),
            constants: image.table(),
            vtables: image.table(),
            producers: image.shared(TableKind::Producers),
        }
    }

    fn name(self, record: &NamedConstantRecord) -> &'a str {
        self.strings.get(StrId(record.name.get()))
    }

    /// The value of the integer constant `name`.
    pub fn constant(self, name: &str) -> Option<IntegerValue> {
        let found = self
            .constants
            .binary_search_by(|record| self.name(record).cmp(name))
            .ok()?;
        Some(value(&self.constants[found]))
    }

    /// Every integer constant, by name.
    #[cfg(any(test, feature = "tools", feature = "fuzzing"))]
    pub fn constants(self) -> impl ExactSizeIterator<Item = (&'a str, IntegerValue)> + 'a {
        self.constants
            .iter()
            .map(move |record| (self.name(record), value(record)))
    }

    /// The concrete type the vtable at `address` is for.
    pub fn vtable(self, address: ImageAddress) -> Option<TypeId> {
        let found = self
            .vtables
            .binary_search_by_key(&address.get(), |record| record.address.get())
            .ok()?;
        Some(TypeId::new(self.vtables[found].ty.get()))
    }

    /// Every vtable, by address.
    #[cfg(any(test, feature = "tools", feature = "fuzzing"))]
    pub fn vtables(self) -> impl ExactSizeIterator<Item = (ImageAddress, TypeId)> + 'a {
        self.vtables.iter().map(|record| {
            (
                ImageAddress::new(record.address.get()),
                TypeId::new(record.ty.get()),
            )
        })
    }

    /// The distinct producers, in the order they were first named.
    pub fn producers(self) -> impl ExactSizeIterator<Item = &'a str> + 'a {
        self.producers
            .iter()
            .map(move |item| self.strings.get(StrId(item.value.get())))
    }
}

/// Checks the declarations.
pub(super) fn validate(image: &Image) -> Result<(), String> {
    let view = DeclarationView::new(image);
    let strings = view.strings;
    if !view
        .constants
        .iter()
        .all(|record| strings.contains(StrId(record.name.get())) && record.signed <= 1)
        || !view
            .constants
            .is_sorted_by(|earlier, later| view.name(earlier) < view.name(later))
    {
        return Err("a named constant is malformed".into());
    }

    let types = image.table::<super::types::TypeRecord>().len();
    if !view
        .vtables
        .iter()
        .all(|record| record.ty.get() != NONE && (record.ty.get() as usize) < types)
        || !view
            .vtables
            .is_sorted_by(|earlier, later| earlier.address.get() < later.address.get())
    {
        return Err("a vtable is malformed".into());
    }

    let mut producers = view
        .producers
        .iter()
        .map(|item| item.value.get())
        .collect::<Vec<_>>();
    producers.sort_unstable();
    if !producers.iter().all(|id| strings.contains(StrId(*id)))
        || producers.windows(2).any(|pair| pair[0] == pair[1])
    {
        return Err("a producer is malformed".into());
    }

    Ok(())
}
