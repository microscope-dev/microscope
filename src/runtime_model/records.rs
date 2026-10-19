//! A runtime's data, laid out as the image's debug information describes
//! its types: members found by the names the runtime's source gives them,
//! and sum types read by their own tags. A model reads through these, so
//! no offset, size, or tag value is ever its own guess.

use std::sync::Arc;

use super::{RuntimeImage, RuntimeStop};
use crate::{
    IntegerValue, RecordMember, RecordMemberLayout, TypeInfo, TypeKind, TypeReference,
    VariantDiscriminant, VariantSelection, VariantSelector, VirtualAddress,
};

/// Why a name the model reads is unavailable, naming it.
pub type Missing = Arc<str>;

/// The most wrappers followed to reach a type's representation.
const MAX_WRAPPERS: usize = 16;

/// A member of a type: where it lies within its container, and its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    pub offset: u64,
    pub ty: TypeReference,
}

/// The type a qualified name means, such as
/// `tokio::runtime::task::core::Header`: the first of its definitions
/// whose layout is complete, as a type is described once in every unit
/// that uses it.
pub fn named(image: &dyn RuntimeImage, name: &str) -> Result<TypeReference, Missing> {
    image
        .types_named(name)
        .into_iter()
        .find(|ty| {
            representation(image, *ty).is_some_and(|info| {
                info.byte_size.is_some()
                    && !matches!(
                        info.kind,
                        TypeKind::Record {
                            incomplete: true,
                            ..
                        } | TypeKind::Variant {
                            incomplete: true,
                            ..
                        }
                    )
            })
        })
        .ok_or_else(|| format!("the program describes no type {name}").into())
}

/// The type's name, for reasons that name it.
fn name_of(image: &dyn RuntimeImage, ty: TypeReference) -> Arc<str> {
    image
        .type_info(ty)
        .map_or_else(|| "a type".into(), |info| Arc::clone(&info.name))
}

/// What lays out a value of `ty`, through names and qualifiers.
fn representation(image: &dyn RuntimeImage, mut ty: TypeReference) -> Option<&TypeInfo> {
    for _ in 0..MAX_WRAPPERS {
        let info = image.type_info(ty)?;
        match &info.kind {
            TypeKind::Named { target, .. } => ty = (*target)?,
            TypeKind::Modified { target, .. } => ty = *target,
            _ => return Some(info),
        }
    }

    None
}

/// The member a path of names reaches within `ty`, through nested records
/// and the payloads of a sum type's variants, which a path names by the
/// variant's name.
pub fn field(image: &dyn RuntimeImage, ty: TypeReference, path: &[&str]) -> Result<Field, Missing> {
    let mut found = Field { offset: 0, ty };
    for name in path {
        let container = representation(image, found.ty)
            .ok_or_else(|| format!("{} has no layout", name_of(image, found.ty)))?;
        let member = members(&container.kind)
            .find(|member| member.name.as_deref() == Some(*name))
            .ok_or_else(|| format!("{} has no member {name}", container.name))?;
        let RecordMemberLayout::ByteOffset(offset) = member.layout else {
            return Err(format!("{}.{name} is not at a byte offset", container.name).into());
        };

        found = Field {
            offset: within(found.offset, offset)?,
            ty: member.type_ref,
        };
    }

    Ok(found)
}

/// The members a name may reach directly within a type of this shape.
fn members(kind: &TypeKind) -> Box<dyn Iterator<Item = &RecordMember> + '_> {
    match kind {
        TypeKind::Record { members, .. } | TypeKind::Union { members, .. } => {
            Box::new(members.iter())
        }

        TypeKind::Variant {
            common_members,
            variants,
            ..
        } => Box::new(
            common_members
                .iter()
                .chain(variants.iter().flat_map(|variant| variant.members.iter())),
        ),
        _ => Box::new(std::iter::empty()),
    }
}

/// The offset of a member at `inner` within a member at `outer`.
pub fn within(outer: u64, inner: u64) -> Result<u64, Missing> {
    outer
        .checked_add(inner)
        .ok_or_else(|| "a member's offset overflows".into())
}

/// A type's size in bytes.
pub fn size(image: &dyn RuntimeImage, ty: TypeReference) -> Result<u64, Missing> {
    representation(image, ty)
        .and_then(|info| info.byte_size)
        .ok_or_else(|| format!("{} has no size", name_of(image, ty)).into())
}

/// A member's offset, once it is the size the model reads it as.
pub fn sized(
    image: &dyn RuntimeImage,
    ty: TypeReference,
    path: &[&str],
    bytes: u64,
) -> Result<u64, Missing> {
    let found = field(image, ty, path)?;
    let actual = size(image, found.ty)?;
    if actual != bytes {
        return Err(format!(
            "{}.{} is {actual} bytes, not {bytes}",
            name_of(image, ty),
            path.join(".")
        )
        .into());
    }

    Ok(found.offset)
}

/// What a pointer or reference type points to.
pub fn target(image: &dyn RuntimeImage, ty: TypeReference) -> Result<TypeReference, Missing> {
    match representation(image, ty).map(|info| &info.kind) {
        Some(
            TypeKind::Pointer {
                target: Some(target),
                ..
            }
            | TypeKind::Reference { target, .. },
        ) => Ok(*target),
        _ => Err(format!("{} is no typed pointer", name_of(image, ty)).into()),
    }
}

/// The element type of a slice, whose normalized layout is its data
/// pointer then its length, each a word.
pub fn slice_element(
    image: &dyn RuntimeImage,
    ty: TypeReference,
) -> Result<TypeReference, Missing> {
    match representation(image, ty).map(|info| &info.kind) {
        Some(TypeKind::Slice { element, .. }) => Ok(*element),
        _ => Err(format!("{} is no slice", name_of(image, ty)).into()),
    }
}

/// The value an enumeration gives one of its names.
pub fn enumerator(image: &dyn RuntimeImage, ty: TypeReference, name: &str) -> Result<u64, Missing> {
    let Some(TypeKind::Enumeration { enumerators, .. }) =
        representation(image, ty).map(|info| &info.kind)
    else {
        return Err(format!("{} is no enumeration", name_of(image, ty)).into());
    };

    enumerators
        .iter()
        .find(|enumerator| &*enumerator.name == name)
        .and_then(|enumerator| unsigned(enumerator.value))
        .ok_or_else(|| format!("{} has no value {name}", name_of(image, ty)).into())
}

fn unsigned(value: IntegerValue) -> Option<u64> {
    match value {
        IntegerValue::Signed(value) => u64::try_from(value).ok(),
        IntegerValue::Unsigned(value) => u64::try_from(value).ok(),
    }
}

/// A sum type, such as a Rust `enum`, read by its tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sum {
    name: Arc<str>,

    /// Where the tag is, and its size.
    tag: (u64, usize),
    variants: Vec<SumVariant>,
}

/// One variant of a sum type: its name, which tags select it, and where
/// its payload is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SumVariant {
    pub name: Arc<str>,
    selection: VariantSelection,
    pub payload: Field,
}

/// Whether a value of `ty` is of a sum type, such as a Rust `enum`.
pub fn is_sum(image: &dyn RuntimeImage, ty: TypeReference) -> bool {
    representation(image, ty).is_some_and(|info| matches!(info.kind, TypeKind::Variant { .. }))
}

/// How the sum type `ty` selects its variants.
pub fn sum(image: &dyn RuntimeImage, ty: TypeReference) -> Result<Sum, Missing> {
    let info =
        representation(image, ty).ok_or_else(|| format!("{} has no layout", name_of(image, ty)))?;
    let TypeKind::Variant {
        discriminant,
        variants,
        ..
    } = &info.kind
    else {
        return Err(format!("{} is no sum type", info.name).into());
    };

    let VariantDiscriminant::Stored(tag) = discriminant.as_ref() else {
        return Err(format!("{} stores no tag", info.name).into());
    };

    let RecordMemberLayout::ByteOffset(tag_offset) = tag.layout else {
        return Err(format!("{}'s tag is not at a byte offset", info.name).into());
    };

    let tag_size = size(image, tag.type_ref)?;
    let tag_size = usize::try_from(tag_size)
        .ok()
        .filter(|size| matches!(size, 1 | 2 | 4 | 8))
        .ok_or_else(|| format!("{}'s tag is {tag_size} bytes", info.name))?;

    let variants = variants
        .iter()
        .map(|variant| {
            let [member] = variant.members.as_ref() else {
                return Err(format!("a variant of {} is not one member", info.name).into());
            };

            let RecordMemberLayout::ByteOffset(offset) = member.layout else {
                return Err(format!("a variant of {} is not at a byte offset", info.name).into());
            };

            Ok(SumVariant {
                name: member
                    .name
                    .clone()
                    .ok_or_else(|| format!("a variant of {} has no name", info.name))?,
                selection: variant.selection.clone(),
                payload: Field {
                    offset,
                    ty: member.type_ref,
                },
            })
        })
        .collect::<Result<_, Missing>>()?;

    Ok(Sum {
        name: Arc::clone(&info.name),
        tag: (tag_offset, tag_size),
        variants,
    })
}

impl Sum {
    /// The variant of this name.
    pub fn variant(&self, name: &str) -> Result<&SumVariant, Missing> {
        self.variants
            .iter()
            .find(|variant| &*variant.name == name)
            .ok_or_else(|| format!("{} has no variant {name}", self.name).into())
    }

    /// The variant the value at `address` holds, or why it cannot be read.
    pub fn active(
        &self,
        stop: &dyn RuntimeStop,
        address: VirtualAddress,
    ) -> Result<&SumVariant, Missing> {
        let (offset, size) = self.tag;
        let tag = read(stop, address.wrapping_add(offset), size)
            .ok_or_else(|| format!("the {} at {address:#x} is unreadable", self.name))?;
        let selects = |selector: &VariantSelector| match *selector {
            VariantSelector::Value(value) => unsigned(value) == Some(tag),
            VariantSelector::Range { low, high } => {
                unsigned(low).is_some_and(|low| low <= tag)
                    && unsigned(high).is_some_and(|high| tag <= high)
            }
        };

        self.variants
            .iter()
            .find(|variant| {
                matches!(&variant.selection, VariantSelection::Selectors(selectors)
                    if selectors.iter().any(selects))
            })
            .or_else(|| {
                self.variants
                    .iter()
                    .find(|variant| variant.selection == VariantSelection::Default)
            })
            .ok_or_else(|| format!("the {} at {address:#x} has tag {tag:#x}", self.name).into())
    }
}

/// An unsigned little-endian integer of `size` bytes, at most eight.
pub fn read(stop: &dyn RuntimeStop, address: VirtualAddress, size: usize) -> Option<u64> {
    let mut bytes = [0; 8];
    stop.read(address, bytes.get_mut(..size)?)
        .then(|| u64::from_le_bytes(bytes))
}

/// The address a null pointer holds.
pub const NULL: VirtualAddress = VirtualAddress::new(0);

/// A word at `address`.
pub fn word(stop: &dyn RuntimeStop, address: VirtualAddress) -> Option<u64> {
    read(stop, address, 8)
}

/// The address a pointer at `address` holds.
pub fn pointer(stop: &dyn RuntimeStop, address: VirtualAddress) -> Option<VirtualAddress> {
    word(stop, address).map(VirtualAddress::new)
}

#[cfg(test)]
impl Sum {
    /// Where the tag is, and a value of it that selects the variant
    /// `name`: one of its own, or for the default variant, one no other
    /// variant claims.
    pub fn tag_for(&self, name: &str) -> (u64, usize, u64) {
        let (offset, size) = self.tag;
        let claimed = |tag: u64| {
            self.variants.iter().any(|variant| {
                matches!(&variant.selection, VariantSelection::Selectors(selectors)
                    if selectors.iter().any(|selector| {
                        matches!(*selector, VariantSelector::Value(value) if unsigned(value) == Some(tag))
                    }))
            })
        };

        let variant = self.variant(name).expect("the variant");
        let value = match &variant.selection {
            VariantSelection::Selectors(selectors) => match selectors.first() {
                Some(VariantSelector::Value(value)) => unsigned(*value).expect("an unsigned tag"),
                other => panic!("{name}: {other:?}"),
            },
            _ => (1..=u64::from(u8::MAX))
                .find(|tag| !claimed(*tag))
                .expect("a free tag"),
        };

        (offset, size, value)
    }
}
