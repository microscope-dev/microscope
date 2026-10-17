//! The types expressions compute with: the program's, seen through to their
//! representation, and the few the language names itself.

use std::sync::Arc;

use super::number::{FloatFormat, IntType};
use super::syntax::ast::CWord;
use crate::{
    ArrayDimension, BaseType, BaseTypeEncoding, CBaseType, Enumerator, ModuleImageId,
    NamedTypeRelationship, TypeId, TypeInfo, TypeKind, TypeReference,
};

/// A type an expression's value has.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    /// A type the program's debug information describes.
    Program(TypeReference),

    /// An exact integer: unsuffixed literals and arithmetic results.
    Exact,

    /// A built-in integer type, `iN` or `uN`.
    Int(IntType),

    /// A built-in float type.
    Float(FloatFormat),

    /// A C base type the program does not describe, as the target lays it
    /// out.
    C(CBaseType),
    Bool,

    /// A pointer the expression derived, by `&` or a cast.
    Pointer(Arc<Self>),

    /// What `void*` points to.
    Void,

    /// The type of `null`.
    Null,

    /// The type of a string literal.
    Text,

    /// A type only a value knows, as a Python variable's is: each
    /// operation on the value finds it when it runs. Such values are
    /// always places, or numbers and truth values computed from them.
    Dynamic,
}

/// What can be done with a value of a type.
#[derive(Debug, Clone)]
pub enum Category {
    /// An integer: exact when `int` is `None`. Enumerations keep their
    /// enumerators.
    Integer {
        int: Option<IntType>,
        enumerators: Option<Arc<[Enumerator]>>,
    },
    Float(FloatFormat),
    Bool,

    /// A pointer, to `void` when the pointee is `None`.
    Pointer(Option<Ty>),

    /// A language reference, which stands for what it refers to.
    Reference,
    Array {
        element: TypeReference,
        dimensions: Arc<[ArrayDimension]>,
    },
    Slice(TypeReference),

    /// An array bounded at run time, whose value finds its bounds.
    RuntimeArray,

    /// A record, union, or variant, whose members are reached with `.`.
    Record,
    Void,
    Null,
    Text,

    /// A type only a value knows, which each operation finds when it runs.
    Dynamic,

    /// A type the debugger cannot compute with, and why.
    Opaque(Arc<str>),
}

/// The types a scope describes, by reference.
pub trait TypeSource {
    /// One program type, or `None` when its metadata is malformed.
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo>;

    /// The size of the target's addresses, in bytes.
    fn pointer_size(&self) -> u8;

    /// The target's byte order.
    fn byte_order(&self) -> crate::ByteOrder;

    /// The target's layout of a C base type, or `None` when microscope does not
    /// know the target's C data model.
    fn c_base_type(&self, ty: CBaseType) -> Option<BaseType>;

    /// Whether two types have one identity, as one type defined in several
    /// units does.
    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right
    }
}

/// How deep a chain of typedefs and qualifiers may go.
const MAX_WRAPPERS: usize = 64;

/// The type a chain of typedefs and qualifiers stands for, and its
/// metadata.
pub fn representation(
    types: &dyn TypeSource,
    mut ty: TypeReference,
) -> Result<(TypeReference, &TypeInfo), Arc<str>> {
    for _ in 0..MAX_WRAPPERS {
        let info = types
            .type_info(ty)
            .ok_or_else(|| Arc::<str>::from("the type's debug information is malformed"))?;
        match &info.kind {
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                relationship:
                    NamedTypeRelationship::Synonym
                    | NamedTypeRelationship::Distinct
                    | NamedTypeRelationship::Encoding,
            } => ty = *target,
            TypeKind::Named { target: None, .. } => {
                return Err(format!("`{}` is declared but not defined", info.name).into());
            }
            _ => return Ok((ty, info)),
        }
    }

    Err("the type's typedefs nest too deeply".into())
}

/// Classifies a type by what can be done with its values.
pub fn category(types: &dyn TypeSource, ty: &Ty) -> Category {
    match ty {
        Ty::Program(reference) => program_category(types, *reference),
        Ty::Exact => Category::Integer {
            int: None,
            enumerators: None,
        },
        Ty::Int(int) => Category::Integer {
            int: Some(*int),
            enumerators: None,
        },
        Ty::Float(format) => Category::Float(*format),
        Ty::C(c) => types.c_base_type(*c).map_or_else(
            || Category::Opaque(format!("the target's `{}` is unknown", c.name()).into()),
            |base| base_category(&base),
        ),
        Ty::Bool => Category::Bool,
        Ty::Pointer(pointee) => Category::Pointer(match pointee.as_ref() {
            Ty::Void => None,
            pointee => Some(pointee.clone()),
        }),
        Ty::Void => Category::Void,
        Ty::Null => Category::Null,
        Ty::Text => Category::Text,
        Ty::Dynamic => Category::Dynamic,
    }
}

fn program_category(types: &dyn TypeSource, ty: TypeReference) -> Category {
    let (_, info) = match representation(types, ty) {
        Ok(found) => found,
        Err(reason) => return Category::Opaque(reason),
    };

    match &info.kind {
        TypeKind::Base(base) => base_category(base),
        TypeKind::Enumeration {
            representation,
            enumerators,
            ..
        } => match base_category(representation) {
            Category::Integer { int, .. } => Category::Integer {
                int,
                enumerators: Some(Arc::clone(enumerators)),
            },
            _ => Category::Opaque("the enumeration is not an integer".into()),
        },
        TypeKind::Pointer { address_class, .. } | TypeKind::Reference { address_class, .. }
            if *address_class != 0 =>
        {
            Category::Opaque(
                format!("the pointer is into another address space ({address_class})").into(),
            )
        }
        TypeKind::Pointer { target: None, .. } => Category::Pointer(None),
        TypeKind::Pointer {
            target: Some(target),
            ..
        } => match program_category(types, *target) {
            Category::Void => Category::Pointer(None),
            _ => Category::Pointer(Some(Ty::Program(*target))),
        },
        TypeKind::Reference { .. } => Category::Reference,
        TypeKind::Array {
            element,
            dimensions,
            ..
        } => Category::Array {
            element: *element,
            dimensions: Arc::clone(dimensions),
        },
        TypeKind::Slice { element, .. } => Category::Slice(*element),
        TypeKind::RuntimeArray { .. } => Category::RuntimeArray,
        TypeKind::Record { .. } | TypeKind::Union { .. } | TypeKind::Variant { .. } => {
            Category::Record
        }
        TypeKind::Unspecified => Category::Void,
        TypeKind::Opaque { description } => Category::Opaque(Arc::clone(description)),
        _ => Category::Opaque(format!("`{}` is not a type values have", info.name).into()),
    }
}

fn base_category(base: &BaseType) -> Category {
    let bits = base
        .bit_size
        .unwrap_or_else(|| base.byte_size.saturating_mul(8));
    let int = |signed| {
        u8::try_from(bits)
            .ok()
            .and_then(|width| IntType::new(width, signed))
            .map_or_else(
                || Category::Opaque(format!("`{}` has {bits} bits", base.name).into()),
                |int| Category::Integer {
                    int: Some(int),
                    enumerators: None,
                },
            )
    };

    match base.encoding {
        BaseTypeEncoding::Boolean => Category::Bool,
        BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => int(true),
        BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => int(false),
        BaseTypeEncoding::Floating => base.float_layout().map_or_else(
            || {
                Category::Opaque(
                    format!(
                        "`{}` is not a float format microscope computes with",
                        base.name
                    )
                    .into(),
                )
            },
            |layout| Category::Float(FloatFormat::of(layout)),
        ),
        BaseTypeEncoding::ComplexFloating => Category::Opaque(
            format!(
                "`{}` is a complex number, which microscope does not compute with",
                base.name
            )
            .into(),
        ),
    }
}

/// Whether values of a type are characters, whose pointers point at text.
pub fn is_character(types: &dyn TypeSource, ty: &Ty) -> bool {
    if let Ty::C(c) = ty {
        return types.c_base_type(*c).is_some_and(|base| {
            matches!(
                base.encoding,
                BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter
            )
        });
    }

    let Ty::Program(reference) = ty else {
        return false;
    };

    matches!(
        representation(types, *reference),
        Ok((
            _,
            TypeInfo {
                kind: TypeKind::Base(BaseType {
                    encoding: BaseTypeEncoding::SignedCharacter
                        | BaseTypeEncoding::UnsignedCharacter,
                    byte_size: 1,
                    ..
                }),
                ..
            }
        ))
    )
}

/// A type's size in bytes, which exact integers, `null`, and strings lack.
pub fn size_of(types: &dyn TypeSource, ty: &Ty) -> Option<u64> {
    match ty {
        Ty::Program(reference) => types.type_info(*reference)?.byte_size,
        Ty::Int(int) => Some(u64::from(int.width()).div_ceil(8)),
        Ty::Float(format) => Some(format.size()),
        Ty::C(c) => types.c_base_type(*c).map(|base| base.byte_size),
        Ty::Bool => Some(1),
        Ty::Pointer(_) => Some(u64::from(types.pointer_size())),
        Ty::Exact | Ty::Void | Ty::Null | Ty::Text | Ty::Dynamic => None,
    }
}

/// The name a type is shown by.
pub fn type_name(types: &dyn TypeSource, ty: &Ty) -> String {
    match ty {
        Ty::Program(reference) => types
            .type_info(*reference)
            .map_or_else(|| "<malformed>".to_owned(), |info| info.name.to_string()),
        Ty::Exact => "integer".to_owned(),
        Ty::Int(int) => int_name(*int),
        Ty::Float(format) => format.name().to_owned(),
        Ty::C(c) => c.name().to_owned(),
        Ty::Bool => "bool".to_owned(),
        Ty::Pointer(pointee) => pointer_name(&type_name(types, pointee)),
        Ty::Void => "void".to_owned(),
        Ty::Null => "null".to_owned(),
        Ty::Text => "string".to_owned(),
        Ty::Dynamic => "dynamic".to_owned(),
    }
}

/// The name of a pointer to a type named `pointee`. A pointer to a
/// pointer to a function or an array adds its `*` inside the declarator's
/// parentheses, as `int (**)(int)` points to `int (*)(int)`, and a pointer
/// to an array parenthesizes its `*`, as in `int (*)[2]`.
fn pointer_name(pointee: &str) -> String {
    let hole = pointee.match_indices("(*").find(|(index, _)| {
        pointee[index + 1..]
            .trim_start_matches('*')
            .starts_with([')', ' '])
    });
    if let Some((index, _)) = hole {
        return format!("{}*{}", &pointee[..=index], &pointee[index + 1..]);
    }

    if pointee.ends_with(']')
        && let Some(index) = pointee.find('[')
    {
        return format!("{} (*){}", pointee[..index].trim_end(), &pointee[index..]);
    }

    format!("{pointee}*")
}

fn int_name(int: IntType) -> String {
    format!("{}{}", if int.is_signed() { 'i' } else { 'u' }, int.width())
}

/// The image that types the language itself defines claim.
pub const LANGUAGE_IMAGE: ModuleImageId = ModuleImageId::new(u32::MAX);

/// The identity of a type without one of its own, which no language or
/// program type has.
pub const UNIDENTIFIED: TypeReference = TypeReference {
    image: LANGUAGE_IMAGE,
    id: TypeId::new(u32::MAX),
};

/// The image ids at and above this one belong to the language: no loaded
/// image has one. Pointers to a program type claim those below
/// [`LANGUAGE_IMAGE`], one per pointee image and pointer depth.
const LANGUAGE_IMAGES: u32 = 1 << 31;

/// How deep a pointer to a program type may be and keep its own identity,
/// past the casts and `&`s an expression can nest.
const POINTER_DEPTHS: u32 = 512;

/// A stable identity within [`LANGUAGE_IMAGE`] for a type the language
/// defines, `depth` pointers to `leaf`. Each pointer level adds `1 << 20`.
fn language_type_id(leaf: &Ty, depth: u32) -> Option<u32> {
    let leaf = match leaf {
        Ty::Exact => 0,
        Ty::Bool => 1,
        Ty::Null => 2,
        Ty::Text => 3,
        Ty::Void => 4,
        Ty::Float(FloatFormat::Binary32) => 5,
        Ty::Float(FloatFormat::Binary64) => 6,
        Ty::Float(FloatFormat::X87Extended) => 7,
        Ty::Float(FloatFormat::Binary16) => 8,
        Ty::Float(FloatFormat::BFloat16) => 9,
        Ty::Float(FloatFormat::Binary128) => 10,
        Ty::Int(int) => 16 + u32::from(int.width()) * 2 + u32::from(int.is_signed()),
        Ty::C(c) => 512 + *c as u32,
        Ty::Program(_) | Ty::Pointer(_) | Ty::Dynamic => return None,
    };

    Some((depth << 20) + leaf)
}

/// The reference a type is identified by: its own for a program type.
pub fn type_reference(ty: &Ty) -> TypeReference {
    let mut depth = 0_u32;
    let mut leaf = ty;
    while let Ty::Pointer(pointee) = leaf {
        depth += 1;
        leaf = pointee;
    }

    debug_assert!(depth < POINTER_DEPTHS, "a pointer {depth} deep");

    let unidentified = UNIDENTIFIED;

    let Ty::Program(reference) = leaf else {
        return language_type_id(leaf, depth).map_or(unidentified, |id| TypeReference {
            image: LANGUAGE_IMAGE,
            id: TypeId::new(id),
        });
    };

    if depth == 0 {
        return *reference;
    }

    let image = u64::from(reference.image.get()) * u64::from(POINTER_DEPTHS) + u64::from(depth);
    u32::try_from(image)
        .ok()
        .and_then(|image| LANGUAGE_IMAGE.get().checked_sub(image))
        .filter(|&image| image >= LANGUAGE_IMAGES)
        .map_or(unidentified, |image| TypeReference {
            image: ModuleImageId::new(image),
            id: reference.id,
        })
}

/// Metadata describing a type, as results carry it.
pub fn type_info(types: &dyn TypeSource, ty: &Ty) -> TypeInfo {
    if let Ty::Program(reference) = ty
        && let Some(info) = types.type_info(*reference)
    {
        return info.clone();
    }

    let name: Arc<str> = type_name(types, ty).into();
    let base = |encoding, byte_size| {
        TypeKind::Base(BaseType {
            name: Arc::clone(&name),
            base_name: Arc::clone(&name),
            encoding,
            byte_size,
            bit_size: None,
        })
    };

    let kind = match ty {
        Ty::C(c) => types
            .c_base_type(*c)
            .map_or(TypeKind::Unspecified, TypeKind::Base),
        Ty::Int(int) => base(
            if int.is_signed() {
                BaseTypeEncoding::Signed
            } else {
                BaseTypeEncoding::Unsigned
            },
            size_of(types, ty).unwrap_or_default(),
        ),
        Ty::Exact => base(BaseTypeEncoding::Signed, 16),
        Ty::Float(_) => base(
            BaseTypeEncoding::Floating,
            size_of(types, ty).unwrap_or_default(),
        ),
        Ty::Bool => base(BaseTypeEncoding::Boolean, 1),
        Ty::Pointer(pointee) => TypeKind::Pointer {
            target: match pointee.as_ref() {
                Ty::Void => None,
                pointee => Some(type_reference(pointee)),
            },
            address_class: 0,
        },
        Ty::Program(_) | Ty::Void | Ty::Null | Ty::Text | Ty::Dynamic => TypeKind::Unspecified,
    };

    TypeInfo {
        reference: type_reference(ty),
        name,
        byte_size: size_of(types, ty),
        kind,
        identity: None,
    }
}

/// The canonical spelling of a C base type from its words in any order,
/// such as `unsigned long` for `long unsigned int`, or `None` when the words
/// spell no C type.
pub fn c_type_key(words: &[CWord]) -> Option<String> {
    let count = |word| words.iter().filter(|&&found| found == word).count();
    let (signed, unsigned) = (count(CWord::Signed), count(CWord::Unsigned));
    let (short, long, char) = (count(CWord::Short), count(CWord::Long), count(CWord::Char));
    let (int, float, double) = (count(CWord::Int), count(CWord::Float), count(CWord::Double));
    if words.is_empty() || signed + unsigned > 1 || int > 1 || char > 1 || short > 1 {
        return None;
    }

    let sign = if unsigned == 1 { "unsigned " } else { "" };
    let key = match (short, long, char, float, double) {
        (0, 0, 0, 1, 0) if signed + unsigned + int == 0 => "float".to_owned(),
        (0, 0, 0, 0, 1) if signed + unsigned + int == 0 => "double".to_owned(),
        (0, 1, 0, 0, 1) if signed + unsigned + int == 0 => "long double".to_owned(),
        (0, 0, 1, 0, 0) if int == 0 => match (signed, unsigned) {
            (1, _) => "signed char".to_owned(),
            (_, 1) => "unsigned char".to_owned(),
            _ => "char".to_owned(),
        },
        (1, 0, 0, 0, 0) => format!("{sign}short"),
        (0, 1, 0, 0, 0) => format!("{sign}long"),
        (0, 2, 0, 0, 0) => format!("{sign}long long"),
        (0, 0, 0, 0, 0) => format!("{sign}int"),
        _ => return None,
    };

    Some(key)
}

/// The canonical spelling of a type name written as C base-type words, as
/// producers name base types, or `None` when it is not one.
pub fn c_type_key_of_name(name: &str) -> Option<String> {
    let words: Option<Vec<CWord>> = name.split_whitespace().map(CWord::parse).collect();
    c_type_key(&words?)
}

/// The built-in type a name spells, given the target's address size.
pub fn builtin(name: &str, pointer_size: u8) -> Option<Ty> {
    match name {
        "bool" => return Some(Ty::Bool),
        "f32" => return Some(Ty::Float(FloatFormat::Binary32)),
        "f64" => return Some(Ty::Float(FloatFormat::Binary64)),
        "void" => return Some(Ty::Void),
        "isize" | "usize" => {
            return IntType::new(pointer_size.saturating_mul(8), name == "isize").map(Ty::Int);
        }
        _ => {}
    }

    let signed = match name.as_bytes().first()? {
        b'i' => true,
        b'u' => false,
        _ => return None,
    };

    let digits = &name[1..];
    if digits.starts_with('0') {
        return None;
    }

    digits
        .parse::<u8>()
        .ok()
        .and_then(|width| IntType::new(width, signed))
        .map(Ty::Int)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Ty, type_reference};
    use crate::eval::number::FloatFormat;
    use crate::{ModuleImageId, TypeId, TypeReference};

    fn pointer(ty: Ty) -> Ty {
        Ty::Pointer(Arc::new(ty))
    }

    fn program(image: u32, id: u32) -> Ty {
        Ty::Program(TypeReference {
            image: ModuleImageId::new(image),
            id: TypeId::new(id),
        })
    }

    #[test]
    fn a_pointer_to_a_program_type_has_its_own_identity() {
        let pointers = [
            pointer(program(0, 5)),
            pointer(program(1, 5)),
            pointer(pointer(program(0, 5))),
            pointer(Ty::Float(FloatFormat::Binary32)),
            pointer(pointer(Ty::Float(FloatFormat::Binary32))),
            program(0, 5),
        ];
        for (index, left) in pointers.iter().enumerate() {
            for right in &pointers[index + 1..] {
                assert_ne!(
                    type_reference(left),
                    type_reference(right),
                    "{left:?} and {right:?}"
                );
            }
        }
    }
}
