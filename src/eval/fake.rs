//! A deterministic world of types, variables, memory, and registers that
//! the evaluator runs against in tests: a stand-in for data access with the
//! simplest layouts, not a second debug-info provider.
#![cfg_attr(
    not(test),
    allow(dead_code, reason = "the view fuzzer uses only part of the world")
)]
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    reason = "a test double reports misuse by panicking"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use super::error::ErrorKind;
use super::number::Exact;
use super::number::Float;
use super::target::{
    Key, Loaded, LoadedValue, Lookup, Machine, Planned, Refusal, Register, Scope, StepKind, Stop,
    TextSpan, TypeLookup, TypeQuery,
};

use super::types::{TypeSource, c_type_key_of_name};
use crate::{
    AddressValue, ArrayDimension, ArrayOrdering, BaseType, BaseTypeEncoding, ByteOrder,
    DereferenceState, EnumerationOrigin, Enumerator, FloatValue, InspectedValue,
    InspectionCompletion, InspectionUsage, IntegerValue, ModuleImageId, NamedTypeRelationship,
    OptimizedOutReason, RecordKind, RecordMember, RecordMemberLayout, ScalarValue, SliceWords,
    TextCompletion, TextSummary, TypeId, TypeInfo, TypeKind, TypeModifier, TypeReference,
    ValueAccessUnavailableReason, ValueChildren, VariableState, VariableUnavailableReason,
    VariableValue, VariableValueSource, VirtualAddress,
};

const IMAGE: ModuleImageId = ModuleImageId::new(1);

#[derive(Debug, Clone)]
enum Storage {
    Memory(u64),
    Register {
        name: &'static str,
        bytes: Vec<u8>,
    },
    OptimizedOut,

    /// A value of a dynamic type, by its index in [`World::dynamic`].
    Dynamic(usize),
}

#[derive(Debug, Clone)]
struct Object {
    name: String,
    ty: TypeReference,
    storage: Storage,
}

/// Where a value of the world is.
#[derive(Debug, Clone)]
pub enum Place {
    Memory {
        address: u64,
        ty: TypeReference,
    },
    Register {
        name: &'static str,
        bytes: Vec<u8>,
        ty: TypeReference,
    },

    /// A value of a dynamic type, by its index in [`World::dynamic`].
    Dynamic(usize),
}

impl Place {
    const fn ty(&self) -> TypeReference {
        match self {
            Self::Memory { ty, .. } | Self::Register { ty, .. } => *ty,
            Self::Dynamic(_) => panic!("a value of a dynamic type has no static type"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Step {
    Deref(TypeReference),
    Member {
        offset: u64,
        ty: TypeReference,
    },

    /// To the member of one variant of a tagged union whose tag is the byte
    /// at its start, inactive unless the tag is `tag`.
    VariantMember {
        tag: u8,
        name: Arc<str>,
        offset: u64,
        ty: TypeReference,
    },
    Array {
        dimensions: Arc<[ArrayDimension]>,
        element_size: u64,
        ty: TypeReference,
    },
    Slice {
        element_size: u64,
        ty: TypeReference,
    },

    /// To a variable, from anywhere: a view's `global(NAME)`.
    Global(usize),

    /// To the value for a key of a map, `{K *keys; V *values; u64 n}`.
    Entry {
        key: TypeReference,
        value: TypeReference,
    },

    /// To what a shared pointer, `{u64 count; V *value}`, points to, as
    /// the view that presents it would.
    Presented(TypeReference),

    /// To an attribute of a value of a dynamic type.
    Attribute(Arc<str>),

    /// To an item of a value of a dynamic type, by its key or index.
    Item,
}

/// A world the evaluator binds and runs in.
#[derive(Debug, Clone, Default)]
pub struct World {
    types: Vec<TypeInfo>,
    objects: Vec<Object>,

    /// Mapped regions by their first address.
    memory: BTreeMap<u64, Vec<u8>>,

    /// Ranges that reading fails the test: what a short circuit skips.
    poisoned: Vec<(u64, u64)>,
    registers: BTreeMap<&'static str, Option<u128>>,

    /// Pointer types that stand for containers.
    containers: Vec<TypeReference>,
    next: u64,

    /// Every memory read, in order.
    pub reads: Vec<(u64, usize)>,

    /// Units of work left, when limited.
    pub work: Option<u64>,

    /// Maps, `{K *keys; V *values; u64 n}`, with their key and value types.
    maps: Vec<(TypeReference, TypeReference, TypeReference)>,

    /// Shared pointers, `{u64 count; V *value}`, with their value types.
    shared: Vec<(TypeReference, TypeReference)>,

    /// The id of the task the stopped thread runs, when the program has
    /// tasks.
    pub task: Option<u64>,

    /// The system call that stopped the thread, when one did.
    pub call: Option<super::target::CallValues>,

    /// Generic functions, by the address their code begins at, with their
    /// type arguments by their parameters' names.
    functions: BTreeMap<u64, Vec<(Arc<str>, TypeReference)>>,

    /// Values of a dynamic type, as Python's are.
    dynamic: Vec<Dynamic>,
}

/// A value of a dynamic type, with Python's built-in types.
#[derive(Debug, Clone)]
pub enum Dynamic {
    Nothing,
    Bool(bool),
    Int(i128),

    /// An int beyond 128 bits.
    BigInt(&'static str),
    Float(f64),
    Str(&'static str),
    Bytes(&'static [u8]),
    List(Vec<usize>),
    Tuple(Vec<usize>),
    Dict(Vec<(usize, usize)>),
    /// An instance of a class, named as its module qualifies it, as
    /// `app.Point` is, deriving from `bases`, named so too.
    Instance {
        type_name: &'static str,
        bases: Vec<&'static str>,
        attributes: Vec<(&'static str, usize)>,
    },
}

impl Dynamic {
    const fn type_name(&self) -> &'static str {
        match self {
            Self::Nothing => "NoneType",
            Self::Bool(_) => "bool",
            Self::Int(_) | Self::BigInt(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "str",
            Self::Bytes(_) => "bytes",
            Self::List(_) => "list",
            Self::Tuple(_) => "tuple",
            Self::Dict(_) => "dict",
            Self::Instance { type_name, .. } => type_name,
        }
    }
}

impl World {
    pub fn new() -> Self {
        Self {
            next: 0x1000,
            ..Self::default()
        }
    }

    fn add(&mut self, name: &str, byte_size: Option<u64>, kind: TypeKind) -> TypeReference {
        let reference = TypeReference {
            image: IMAGE,
            id: TypeId::new(u32::try_from(self.types.len()).expect("few types")),
        };

        self.types.push(TypeInfo {
            reference,
            name: name.into(),
            byte_size,
            kind,
            identity: None,
        });
        reference
    }

    fn info(&self, ty: TypeReference) -> &TypeInfo {
        &self.types[usize::try_from(ty.id.get()).expect("small ids")]
    }

    pub fn base(
        &mut self,
        name: &str,
        encoding: BaseTypeEncoding,
        byte_size: u64,
    ) -> TypeReference {
        self.add(
            name,
            Some(byte_size),
            TypeKind::Base(BaseType {
                name: name.into(),
                base_name: name.into(),
                encoding,
                byte_size,
                bit_size: None,
            }),
        )
    }

    fn info_mut(&mut self, ty: TypeReference) -> &mut TypeInfo {
        &mut self.types[usize::try_from(ty.id.get()).expect("small ids")]
    }

    pub fn record(
        &mut self,
        name: &str,
        byte_size: u64,
        members: &[(&str, TypeReference, u64)],
    ) -> TypeReference {
        let record = self.add(
            name,
            Some(byte_size),
            TypeKind::Record {
                kind: RecordKind::Struct,
                members: Arc::from([]),
                bases: Arc::from([]),
                incomplete: false,
            },
        );
        self.set_members(record, members);
        record
    }

    /// Gives a record its base classes, each at a byte offset.
    pub fn set_bases(&mut self, record: TypeReference, bases: &[(TypeReference, u64)]) {
        let bases: Vec<crate::BaseClass> = bases
            .iter()
            .map(|(ty, offset)| crate::BaseClass {
                type_ref: *ty,
                layout: RecordMemberLayout::ByteOffset(*offset),
                accessibility: crate::Accessibility::Public,
                virtuality: crate::BaseClassVirtuality::None,
            })
            .collect();
        let TypeKind::Record {
            bases: existing, ..
        } = &mut self.info_mut(record).kind
        else {
            panic!("only records have bases");
        };
        *existing = bases.into();
    }

    /// Every path from `from` through its bases to a base of type `target`,
    /// as the offset it is at.
    fn base_offsets(&self, from: TypeReference, target: TypeReference, at: u64) -> Vec<u64> {
        let TypeKind::Record { bases, .. } = &self.info(from).kind else {
            return Vec::new();
        };

        let mut found = Vec::new();
        for base in bases.iter() {
            let RecordMemberLayout::ByteOffset(offset) = base.layout else {
                panic!("the world lays bases out at byte offsets");
            };

            if base.type_ref == target {
                found.push(at + offset);
            } else {
                found.extend(self.base_offsets(base.type_ref, target, at + offset));
            }
        }

        found
    }

    /// Replaces a record's members, as a record that points to its own type
    /// needs.
    pub fn set_members(&mut self, record: TypeReference, members: &[(&str, TypeReference, u64)]) {
        let members: Vec<RecordMember> = members
            .iter()
            .map(|(member, ty, offset)| RecordMember {
                name: Some((*member).into()),
                type_ref: *ty,
                layout: RecordMemberLayout::ByteOffset(*offset),
                accessibility: crate::Accessibility::Public,
                artificial: false,
                embedded: false,
                declaration: None,
            })
            .collect();
        let TypeKind::Record {
            members: existing, ..
        } = &mut self.info_mut(record).kind
        else {
            panic!("only records have members");
        };
        *existing = members.into();
    }

    /// A generic function whose code is at `address`, instantiated with
    /// `generics`, each by its parameter's name.
    pub fn function(&mut self, address: u64, generics: &[(&str, TypeReference)]) {
        self.functions.insert(
            address,
            generics
                .iter()
                .map(|(name, ty)| (Arc::from(*name), *ty))
                .collect(),
        );
    }

    /// A tagged union, as a Rust enum is, whose tag is the byte at its
    /// start: the `i`th variant holds its one member, named for the
    /// variant, when the tag is `i`.
    pub fn variant(
        &mut self,
        name: &str,
        byte_size: u64,
        variants: &[(&str, TypeReference, u64)],
    ) -> TypeReference {
        let tag = self.base("u8", BaseTypeEncoding::Unsigned, 1);
        let member = |name: Option<&str>, ty, offset| RecordMember {
            name: name.map(Into::into),
            type_ref: ty,
            layout: RecordMemberLayout::ByteOffset(offset),
            accessibility: crate::Accessibility::Public,
            artificial: name.is_none(),
            embedded: false,
            declaration: None,
        };

        let variants: Vec<crate::Variant> = variants
            .iter()
            .zip(0_u128..)
            .map(|((variant, ty, offset), index)| crate::Variant {
                name: None,
                selection: crate::VariantSelection::Selectors(Arc::from([
                    crate::VariantSelector::Value(IntegerValue::Unsigned(index)),
                ])),
                members: Arc::from([member(Some(variant), *ty, *offset)]),
            })
            .collect();
        self.add(
            name,
            Some(byte_size),
            TypeKind::Variant {
                storage: crate::VariantStorageKind::Struct,
                common_members: Arc::from([]),
                bases: Arc::from([]),
                discriminant: Box::new(crate::VariantDiscriminant::Stored(member(None, tag, 0))),
                variants: variants.into(),
                incomplete: false,
            },
        )
    }

    pub fn pointer(&mut self, target: Option<TypeReference>) -> TypeReference {
        let name = target.map_or_else(
            || "void*".to_owned(),
            |target| format!("{}*", self.info(target).name),
        );
        self.add(
            &name,
            Some(8),
            TypeKind::Pointer {
                target,
                address_class: 0,
            },
        )
    }

    pub fn reference(&mut self, target: TypeReference) -> TypeReference {
        let name = format!("{}&", self.info(target).name);
        self.add(
            &name,
            Some(8),
            TypeKind::Reference {
                kind: crate::ReferenceKind::Lvalue,
                target,
                address_class: 0,
            },
        )
    }

    pub fn array(&mut self, element: TypeReference, counts: &[u64]) -> TypeReference {
        let size =
            self.info(element).byte_size.unwrap_or_default() * counts.iter().product::<u64>();
        let name = format!(
            "{}{}",
            self.info(element).name,
            counts.iter().fold(String::new(), |mut name, count| {
                use std::fmt::Write as _;
                let _ = write!(name, "[{count}]");
                name
            })
        );
        let dimensions: Vec<ArrayDimension> = counts
            .iter()
            .map(|&count| ArrayDimension {
                lower_bound: 0,
                count,
            })
            .collect();
        self.add(
            &name,
            Some(size),
            TypeKind::Array {
                element,
                dimensions: dimensions.into(),
                ordering: ArrayOrdering::RowMajor,
            },
        )
    }

    pub fn slice(&mut self, element: TypeReference) -> TypeReference {
        let name = format!("[]{}", self.info(element).name);
        self.add(
            &name,
            Some(16),
            TypeKind::Slice {
                element,
                words: SliceWords::POINTER_LENGTH,
                text: false,
            },
        )
    }

    /// A slice whose descriptor records its capacity after its length, as
    /// Go's does.
    pub fn slice_with_capacity(&mut self, element: TypeReference) -> TypeReference {
        let name = format!("[]{} with room", self.info(element).name);
        self.add(
            &name,
            Some(24),
            TypeKind::Slice {
                element,
                words: SliceWords::POINTER_LENGTH_CAPACITY,
                text: false,
            },
        )
    }

    /// A map `{K *keys; V *values; u64 n}`, which a view would present.
    pub fn map_type(
        &mut self,
        name: &str,
        key: TypeReference,
        value: TypeReference,
    ) -> TypeReference {
        let u64 = self.base("u64", BaseTypeEncoding::Unsigned, 8);
        let keys = self.pointer(Some(key));
        let values = self.pointer(Some(value));
        let map = self.record(
            name,
            24,
            &[("keys", keys, 0), ("values", values, 8), ("n", u64, 16)],
        );
        self.maps.push((map, key, value));
        map
    }

    /// A shared pointer `{u64 count; V *value}`, which a view would present
    /// as what it points to.
    pub fn shared_type(&mut self, name: &str, value: TypeReference) -> TypeReference {
        let u64 = self.base("u64", BaseTypeEncoding::Unsigned, 8);
        let pointer = self.pointer(Some(value));
        let shared = self.record(name, 16, &[("count", u64, 0), ("value", pointer, 8)]);
        self.shared.push((shared, value));
        shared
    }

    /// A map's bytes, with its keys and values allocated.
    pub fn map_bytes(&mut self, keys: &[u8], values: &[u8], count: u64) -> Vec<u8> {
        let keys = self.allocate(keys);
        let values = self.allocate(values);
        let mut bytes = keys.to_le_bytes().to_vec();
        bytes.extend(values.to_le_bytes());
        bytes.extend(count.to_le_bytes());
        bytes
    }

    pub fn enumeration(
        &mut self,
        name: &str,
        representation: TypeReference,
        values: &[(&str, i128)],
    ) -> TypeReference {
        let TypeKind::Base(base) = self.info(representation).kind.clone() else {
            panic!("an enumeration's representation is a base type");
        };

        let enumerators: Vec<Enumerator> = values
            .iter()
            .map(|(enumerator, value)| Enumerator {
                name: (*enumerator).into(),
                value: if matches!(
                    base.encoding,
                    BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                ) {
                    IntegerValue::Signed(*value)
                } else {
                    IntegerValue::Unsigned(value.cast_unsigned())
                },
            })
            .collect();
        let size = base.byte_size;
        self.add(
            name,
            Some(size),
            TypeKind::Enumeration {
                representation: base,
                underlying: Some(representation),
                enumerators: enumerators.into(),
                origin: EnumerationOrigin::Language,
                scoped: false,
            },
        )
    }

    /// Gives a type the identity a view's pattern matches.
    pub fn identify(
        &mut self,
        ty: TypeReference,
        language: crate::SourceLanguage,
        path: &[&str],
        base: &str,
        arguments: Vec<crate::TypeArgument>,
    ) {
        self.info_mut(ty).identity = Some(Arc::new(crate::TypeIdentity {
            language,
            path: path.iter().map(|segment| Arc::from(*segment)).collect(),
            inline_namespaces: Arc::default(),
            base: base.into(),
            origin: if arguments.is_empty() {
                crate::ArgumentOrigin::None
            } else {
                crate::ArgumentOrigin::Dwarf
            },
            arguments: arguments.into(),
            pack: None,
            go: None,
        }));
    }

    /// Changes an identified type's identity, as a language's debug
    /// information adds to it.
    pub fn edit_identity(
        &mut self,
        ty: TypeReference,
        edit: impl FnOnce(&mut crate::TypeIdentity),
    ) {
        let identity = self
            .info_mut(ty)
            .identity
            .as_mut()
            .expect("an identified type");
        edit(Arc::make_mut(identity));
    }

    /// Makes a pointer type stand for a container a view presents.
    pub fn container(&mut self, ty: TypeReference) {
        self.containers.push(ty);
    }

    /// Marks where an identified type's template parameter pack begins.
    pub fn pack(&mut self, ty: TypeReference, start: usize) {
        let identity = self
            .info_mut(ty)
            .identity
            .as_mut()
            .expect("an identified type");
        Arc::make_mut(identity).pack = Some(start);
    }

    pub fn typedef(&mut self, name: &str, target: TypeReference) -> TypeReference {
        let size = self.info(target).byte_size;
        self.add(
            name,
            size,
            TypeKind::Named {
                target: Some(target),
                relationship: NamedTypeRelationship::Synonym,
            },
        )
    }

    pub fn constant(&mut self, target: TypeReference) -> TypeReference {
        let name = format!("const {}", self.info(target).name);
        let size = self.info(target).byte_size;
        self.add(
            &name,
            size,
            TypeKind::Modified {
                modifier: TypeModifier::Const,
                target,
            },
        )
    }

    /// Maps `bytes` at a fresh address, returning it.
    pub fn allocate(&mut self, bytes: &[u8]) -> u64 {
        let address = self.next;
        self.memory.insert(address, bytes.to_vec());
        self.next += (bytes.len() as u64).div_ceil(16) * 16 + 16;
        address
    }

    /// Maps `bytes` at `address`, which nothing else may use.
    pub fn map(&mut self, address: u64, bytes: &[u8]) {
        self.memory.insert(address, bytes.to_vec());
    }

    /// A variable in memory, returning its address.
    pub fn variable(&mut self, name: &str, ty: TypeReference, bytes: &[u8]) -> u64 {
        let address = self.allocate(bytes);
        self.objects.push(Object {
            name: name.to_owned(),
            ty,
            storage: Storage::Memory(address),
        });
        address
    }

    pub fn register_variable(
        &mut self,
        name: &str,
        ty: TypeReference,
        register: &'static str,
        bytes: &[u8],
    ) {
        self.objects.push(Object {
            name: name.to_owned(),
            ty,
            storage: Storage::Register {
                name: register,
                bytes: bytes.to_vec(),
            },
        });
    }

    pub fn optimized_out(&mut self, name: &str, ty: TypeReference) {
        self.objects.push(Object {
            name: name.to_owned(),
            ty,
            storage: Storage::OptimizedOut,
        });
    }

    pub fn set_register(&mut self, name: &'static str, value: u128) {
        self.registers.insert(name, Some(value));
    }

    /// Stores an assignment's bytes in the register a scope named.
    pub fn write_register(&mut self, register: &Register, bytes: &[u8]) {
        let value = self
            .registers
            .values_mut()
            .nth(usize::from(register.number))
            .expect("a register the scope named");
        let mut wide = [0_u8; 16];
        wide[..bytes.len()].copy_from_slice(bytes);
        *value = Some(u128::from_le_bytes(wide));
    }

    /// A register unwinding could not recover.
    pub fn lost_register(&mut self, name: &'static str) {
        self.registers.insert(name, None);
    }

    /// Replaces a variable's bytes in memory.
    pub fn set(&mut self, name: &str, bytes: &[u8]) {
        let address = self.address_of(name);
        let region = self.memory.get_mut(&address).expect("a mapped variable");
        region[..bytes.len()].copy_from_slice(bytes);
    }

    /// Stores bytes at a place, as the debugger would.
    pub fn write(&mut self, at: &Place, bytes: &[u8]) {
        match at {
            Place::Memory { address, .. } => {
                let (start, region) = self
                    .memory
                    .range_mut(..=*address)
                    .next_back()
                    .expect("a mapped place");
                let offset = usize::try_from(address - start).expect("small offsets");
                region[offset..offset + bytes.len()].copy_from_slice(bytes);
            }
            Place::Dynamic(_) => panic!("a value of a dynamic type is not written"),
            Place::Register { name, .. } => {
                for object in &mut self.objects {
                    if let Storage::Register {
                        name: register,
                        bytes: stored,
                    } = &mut object.storage
                        && register == name
                    {
                        stored[..bytes.len()].copy_from_slice(bytes);
                    }
                }
            }
        }
    }

    /// Makes reading `[address, address + size)` fail the test.
    pub fn poison(&mut self, address: u64, size: u64) {
        self.poisoned.push((address, address + size));
    }

    /// The type of the variable `name`.
    pub fn type_of(&self, name: &str) -> TypeReference {
        self.objects
            .iter()
            .find(|object| object.name == name)
            .map_or_else(|| panic!("no variable `{name}`"), |object| object.ty)
    }

    pub fn address_of(&self, name: &str) -> u64 {
        match self
            .objects
            .iter()
            .find(|object| object.name == name)
            .map(|object| &object.storage)
        {
            Some(Storage::Memory(address)) => *address,
            _ => panic!("`{name}` is not in memory"),
        }
    }

    fn representation(&self, mut ty: TypeReference) -> &TypeInfo {
        loop {
            match &self.info(ty).kind {
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. } => {
                    ty = *target;
                }
                _ => return self.info(ty),
            }
        }
    }

    fn size(&self, ty: TypeReference) -> usize {
        usize::try_from(self.info(ty).byte_size.unwrap_or_default()).expect("small sizes")
    }

    fn read_memory(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop> {
        let end = address.saturating_add(size as u64);
        if let Some((start, stop)) = self
            .poisoned
            .iter()
            .find(|(start, stop)| address < *stop && *start < end)
        {
            panic!("read {address:#x}..{end:#x} overlaps poisoned memory {start:#x}..{stop:#x}");
        }

        self.reads.push((address, size));
        let region = self
            .memory
            .range(..=address)
            .next_back()
            .filter(|(start, bytes)| end <= **start + bytes.len() as u64);
        let Some((start, bytes)) = region else {
            return Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::MemoryInaccessible {
                    address: VirtualAddress::new(address),
                    requested: size as u64,
                    completed: 0,
                    next_address: VirtualAddress::new(address),
                },
            )));
        };

        let offset = usize::try_from(address - start).expect("small offsets");
        Ok(bytes[offset..offset + size].to_vec())
    }

    fn bytes(&mut self, place: &Place) -> Result<Vec<u8>, Stop> {
        let size = self.size(place.ty());
        match place {
            Place::Memory { address, .. } => self.read_memory(*address, size),
            Place::Register { bytes, .. } => Ok(bytes[..size.min(bytes.len())].to_vec()),
            Place::Dynamic(_) => panic!("a value of a dynamic type has no bytes"),
        }
    }

    fn decode(&self, ty: TypeReference, bytes: &[u8]) -> VariableValue {
        let mut wide = [0_u8; 16];
        wide[..bytes.len().min(16)].copy_from_slice(&bytes[..bytes.len().min(16)]);
        let raw = u128::from_le_bytes(wide);
        let signed = |size: usize| {
            let shift = 128 - size * 8;
            (raw.cast_signed() << shift) >> shift
        };

        match &self.representation(ty).kind {
            TypeKind::Base(base) => VariableValue::Scalar(match base.encoding {
                BaseTypeEncoding::Boolean => ScalarValue::Boolean(raw != 0),
                BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter => {
                    ScalarValue::Signed(signed(bytes.len()))
                }
                BaseTypeEncoding::Unsigned | BaseTypeEncoding::UnsignedCharacter => {
                    ScalarValue::Unsigned(raw)
                }
                BaseTypeEncoding::ComplexFloating => {
                    unreachable!("the fake program has no complex numbers")
                }
                BaseTypeEncoding::Floating => ScalarValue::Floating(match bytes.len() {
                    4 => FloatValue::Binary32(u32::try_from(raw).expect("four bytes")),
                    8 => FloatValue::Binary64(u64::try_from(raw).expect("eight bytes")),
                    _ => FloatValue::X87Extended {
                        significand: u64::try_from(raw & u128::from(u64::MAX))
                            .expect("eight bytes"),
                        sign_exponent: u16::try_from((raw >> 64) & 0xffff).expect("two bytes"),
                    },
                }),
            }),
            TypeKind::Enumeration {
                representation,
                enumerators,
                ..
            } => {
                let value = if matches!(
                    representation.encoding,
                    BaseTypeEncoding::Signed | BaseTypeEncoding::SignedCharacter
                ) {
                    IntegerValue::Signed(signed(bytes.len()))
                } else {
                    IntegerValue::Unsigned(raw)
                };

                let matches: Vec<Enumerator> = enumerators
                    .iter()
                    .filter(|enumerator| enumerator.value == value)
                    .cloned()
                    .collect();
                VariableValue::Enumeration {
                    value,
                    matches: matches.into(),
                }
            }
            TypeKind::Pointer { .. } | TypeKind::Reference { .. } => {
                VariableValue::Address(AddressValue {
                    function: None,
                    address: VirtualAddress::new(u64::try_from(raw).expect("eight bytes")),
                })
            }
            TypeKind::Array { dimensions, .. } => VariableValue::Array {
                dimensions: Arc::clone(dimensions),
            },
            TypeKind::Slice { words, .. } if bytes.len() as u64 >= words.span() * 8 => {
                let word = |at: u8| {
                    let at = usize::from(at) * 8;
                    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"))
                };

                VariableValue::Slice {
                    length: word(words.length),
                    capacity: words.capacity.map(word),
                }
            }
            _ => VariableValue::Record,
        }
    }

    fn available(
        &self,
        ty: TypeReference,
        source: VariableValueSource,
        bytes: Vec<u8>,
    ) -> VariableState {
        VariableState::Available {
            source,
            value: self.decode(ty, &bytes),
            raw: Some(bytes.into()),
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: None,
        }
    }

    fn inspected(&self, ty: TypeReference, state: VariableState) -> InspectedValue {
        InspectedValue {
            type_info: Some(self.info(ty).clone()),
            state,
            completion: InspectionCompletion::Complete,
            usage: InspectionUsage::default(),
        }
    }

    fn is_char(&self, ty: TypeReference) -> bool {
        matches!(
            &self.representation(ty).kind,
            TypeKind::Base(BaseType {
                encoding: BaseTypeEncoding::SignedCharacter | BaseTypeEncoding::UnsignedCharacter,
                ..
            })
        )
    }

    /// The bytes up to a NUL from `address`, at most `limit`.
    fn c_string(&mut self, address: u64, limit: usize) -> TextSummary {
        let mut bytes = Vec::new();
        for offset in 0..limit {
            let at = address.wrapping_add(offset as u64);
            match self.read_memory(at, 1) {
                Ok(byte) if byte[0] == 0 => {
                    return TextSummary {
                        bytes: bytes.into(),
                        completion: TextCompletion::Complete,
                    };
                }
                Ok(byte) => bytes.push(byte[0]),
                Err(_) => {
                    return TextSummary {
                        bytes: bytes.into(),
                        completion: TextCompletion::Unreadable {
                            address: VirtualAddress::new(at),
                        },
                    };
                }
            }
        }

        TextSummary {
            bytes: bytes.into(),
            completion: TextCompletion::Truncated { length: None },
        }
    }
}

impl World {
    /// The step from a shared pointer of type `info` to the value it points to,
    /// as a view presents it.
    fn plan_presented(&self, info: &TypeInfo) -> Result<Planned<Step>, Refusal> {
        let (_, value) = self
            .shared
            .iter()
            .find(|(shared, _)| *shared == info.reference)
            .ok_or_else(|| type_error(format!("`{}` is no pointer", info.name)))?;
        Ok(Planned {
            step: Step::Presented(*value),
            result: Some(*value),
            consumed: 0,
        })
    }

    /// The member `name` of a tagged union's variant.
    fn plan_variant_member(
        info: &TypeInfo,
        variants: &[crate::Variant],
        name: &str,
    ) -> Result<Planned<Step>, Refusal> {
        let (tag, member) = variants
            .iter()
            .zip(0_u8..)
            .find_map(|(variant, tag)| {
                let member = variant.members.first()?;
                (member.name.as_deref() == Some(name)).then_some((tag, member))
            })
            .ok_or_else(|| type_error(format!("`{}` has no member `{name}`", info.name)))?;
        let RecordMemberLayout::ByteOffset(offset) = member.layout else {
            panic!("the world lays members out at byte offsets");
        };

        Ok(Planned {
            step: Step::VariantMember {
                tag,
                name: name.into(),
                offset,
                ty: member.type_ref,
            },
            result: Some(member.type_ref),
            consumed: 0,
        })
    }

    fn plan_entry(&self, info: &TypeInfo) -> Result<Planned<Step>, Refusal> {
        let (_, key, value) = self
            .maps
            .iter()
            .find(|(map, ..)| *map == info.reference)
            .ok_or_else(|| type_error(format!("`{}` is no map", info.name)))?;
        Ok(Planned {
            step: Step::Entry {
                key: *key,
                value: *value,
            },
            result: Some(*value),
            consumed: 1,
        })
    }
}

impl TypeSource for World {
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        (ty.image == IMAGE).then(|| self.info(ty))
    }

    fn pointer_size(&self) -> u8 {
        8
    }

    fn byte_order(&self) -> ByteOrder {
        ByteOrder::Little
    }

    fn c_base_type(&self, ty: crate::CBaseType) -> Option<crate::BaseType> {
        crate::TargetDescription {
            architecture: crate::Architecture::X86_64,
            byte_order: ByteOrder::Little,
            pointer_width: crate::PointerWidth::Bits64,
        }
        .c_base_type(ty)
    }

    /// Types with identities are one type when their identities are, as
    /// copies of one type in several units are.
    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        if left == right {
            return true;
        }

        let (Some(left), Some(right)) = (
            self.info(left).identity.as_deref(),
            self.info(right).identity.as_deref(),
        ) else {
            return false;
        };

        left.language == right.language
            && left.path == right.path
            && left.base == right.base
            && left.arguments.len() == right.arguments.len()
            && left
                .arguments
                .iter()
                .zip(right.arguments.iter())
                .all(|pair| match pair {
                    (crate::TypeArgument::Type(left), crate::TypeArgument::Type(right)) => {
                        self.same_type(*left, *right)
                    }
                    (left, right) => left == right,
                })
    }
}

fn type_error(message: impl Into<String>) -> Refusal {
    Refusal::new(ErrorKind::Type, message)
}

impl Scope for World {
    type Object = usize;
    type Step = Step;

    fn lookup(&self, name: &str, _outermost: bool) -> Result<Lookup<usize>, Refusal> {
        let objects: Vec<usize> = (0..self.objects.len())
            .filter(|&index| self.objects[index].name == name)
            .collect();
        match objects.as_slice() {
            [index] if matches!(self.objects[*index].storage, Storage::Dynamic(_)) => {
                return Ok(Lookup::Dynamic(*index));
            }
            [index] => {
                return Ok(Lookup::Object {
                    object: *index,
                    ty: Ok(self.objects[*index].ty),
                });
            }
            [_, ..] => {
                return Ok(Lookup::Ambiguous(
                    (0..objects.len())
                        .map(|copy| format!("unit{copy}.c::{name}"))
                        .collect(),
                ));
            }
            [] => {}
        }

        let mut found = Vec::new();
        for info in &self.types {
            if let TypeKind::Enumeration { enumerators, .. } = &info.kind {
                for enumerator in enumerators.iter() {
                    let qualified = format!("{}::{}", info.name, enumerator.name);
                    if enumerator.name.as_ref() == name || qualified == name {
                        found.push((qualified, Exact::from(enumerator.value), info.reference));
                    }
                }
            }
        }

        Ok(match found.as_slice() {
            [] => Lookup::NotFound,
            [(_, value, ty)] => Lookup::Enumerator {
                value: *value,
                ty: *ty,
            },
            _ => Lookup::Ambiguous(found.into_iter().map(|(qualified, ..)| qualified).collect()),
        })
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        let found: Vec<TypeReference> = self
            .types
            .iter()
            .filter(|info| {
                !matches!(
                    info.kind,
                    TypeKind::Pointer { .. } | TypeKind::Array { .. } | TypeKind::Reference { .. }
                )
            })
            .filter(|info| {
                info.name.as_ref() == query.name
                    || c_type_key_of_name(&info.name).is_some_and(|key| key == query.name)
            })
            .map(|info| info.reference)
            .collect();
        match found.as_slice() {
            [] => TypeLookup::NotFound,
            [ty] => TypeLookup::Found(*ty),
            _ => TypeLookup::Ambiguous(
                found
                    .iter()
                    .map(|ty| self.info(*ty).name.to_string())
                    .collect(),
            ),
        }
    }

    fn plan(&self, from: TypeReference, step: StepKind<'_>) -> Result<Planned<Step>, Refusal> {
        let info = self.representation(from);
        let planned = |step, result: TypeReference, consumed| Planned {
            step,
            result: Some(result),
            consumed,
        };

        match (step, &info.kind) {
            (
                StepKind::Deref,
                TypeKind::Pointer {
                    target: Some(target),
                    ..
                }
                | TypeKind::Reference { target, .. },
            ) => Ok(planned(Step::Deref(*target), *target, 0)),
            (StepKind::Member(name), TypeKind::Record { members, .. }) => {
                let member = members
                    .iter()
                    .find(|member| member.name.as_deref() == Some(name))
                    .ok_or_else(|| type_error(format!("`{}` has no member `{name}`", info.name)))?;
                let RecordMemberLayout::ByteOffset(offset) = member.layout else {
                    panic!("the world lays members out at byte offsets");
                };

                Ok(planned(
                    Step::Member {
                        offset,
                        ty: member.type_ref,
                    },
                    member.type_ref,
                    0,
                ))
            }
            (StepKind::Member(name), TypeKind::Variant { variants, .. }) => {
                Self::plan_variant_member(info, variants, name)
            }

            (
                StepKind::Index { available },
                TypeKind::Array {
                    element,
                    dimensions,
                    ..
                },
            ) => {
                if available < dimensions.len() {
                    return Err(type_error(format!(
                        "`{}` takes {} indices",
                        info.name,
                        dimensions.len()
                    )));
                }

                Ok(planned(
                    Step::Array {
                        dimensions: Arc::clone(dimensions),
                        element_size: self.info(*element).byte_size.unwrap_or_default(),
                        ty: *element,
                    },
                    *element,
                    dimensions.len(),
                ))
            }
            (StepKind::Index { .. }, TypeKind::Slice { element, .. }) => Ok(planned(
                Step::Slice {
                    element_size: self.info(*element).byte_size.unwrap_or_default(),
                    ty: *element,
                },
                *element,
                1,
            )),
            (StepKind::Base(target), TypeKind::Record { .. }) => {
                match self.base_offsets(from, target, 0).as_slice() {
                    [offset] => Ok(planned(
                        Step::Member {
                            offset: *offset,
                            ty: target,
                        },
                        target,
                        0,
                    )),
                    [] => Err(type_error(format!(
                        "`{}` is not a base class of `{}`",
                        self.info(target).name,
                        info.name
                    ))),
                    _ => Err(Refusal::new(
                        ErrorKind::AmbiguousName,
                        format!(
                            "`{}` has several `{}` base class subobjects",
                            info.name,
                            self.info(target).name
                        ),
                    )),
                }
            }
            (StepKind::Entry, TypeKind::Record { .. }) => self.plan_entry(info),
            (StepKind::Deref, TypeKind::Record { .. }) => self.plan_presented(info),
            (step, _) => Err(type_error(format!(
                "`{}` does not take {step:?}",
                info.name
            ))),
        }
    }

    fn plan_dynamic(&self, step: StepKind<'_>) -> Result<Step, Refusal> {
        match step {
            StepKind::Member(name) => Ok(Step::Attribute(name.into())),
            StepKind::Entry => Ok(Step::Item),
            _ => Err(type_error("a dynamic value takes attributes and items")),
        }
    }

    /// A world of dynamic values has an interpreter, whose objects `py`
    /// casts an address to.
    fn dynamic_type(&self, name: &str) -> bool {
        name == "py" && !self.dynamic.is_empty()
    }

    fn call_values(&self) -> Option<&super::target::CallValues> {
        self.call.as_ref()
    }

    fn register(&self, name: &str) -> Option<Register> {
        let name = match name {
            "pc" => "rip",
            "sp" => "rsp",
            "fp" => "rbp",
            other => other,
        };

        let number = self
            .registers
            .keys()
            .position(|register| *register == name)?;
        Some(Register {
            number: u16::try_from(number).ok()?,
            width: 64,
        })
    }

    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        self.types
            .iter()
            .filter(|info| {
                info.identity
                    .as_deref()
                    .is_some_and(|identity| identity.base.as_ref() == base)
            })
            .map(|info| info.reference)
            .collect()
    }

    fn stands_for_container(&self, ty: TypeReference) -> bool {
        self.containers.contains(&ty)
    }

    fn global_step(&self, name: &str) -> Result<Option<(Step, TypeReference)>, Refusal> {
        let mut objects = (0..self.objects.len()).filter(|&index| self.objects[index].name == name);
        match (objects.next(), objects.next()) {
            (Some(index), None) => Ok(Some((Step::Global(index), self.objects[index].ty))),
            (None, _) => Ok(None),
            (Some(_), Some(_)) => Err(Refusal::new(
                ErrorKind::AmbiguousName,
                format!("several variables are named `{name}`"),
            )),
        }
    }
}

impl Machine for World {
    type Object = usize;
    type Step = Step;
    type Place = Place;

    /// Runs out of work as an inspection's budget does.
    fn charge(&mut self) -> Result<(), Stop> {
        match &mut self.work {
            Some(0) => Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::InspectionLimit(crate::InspectionExhaustion {
                    resource: crate::InspectionLimit::ExpressionWork,
                    limit: 0,
                    used: 0,
                    requested: 1,
                }),
            ))),
            Some(work) => {
                *work -= 1;
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn locate(&mut self, object: &usize) -> Result<Place, Stop> {
        let object = &self.objects[*object];
        match &object.storage {
            Storage::Memory(address) => Ok(Place::Memory {
                address: *address,
                ty: object.ty,
            }),
            Storage::Register { name, bytes } => Ok(Place::Register {
                name,
                bytes: bytes.clone(),
                ty: object.ty,
            }),
            Storage::OptimizedOut => Err(Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::OptimizedOut(OptimizedOutReason::NoLocation),
            ))),
            Storage::Dynamic(index) => Ok(Place::Dynamic(*index)),
        }
    }

    fn check_indices(&self, step: &Step, indices: &[i128]) -> Result<(), Stop> {
        if let Step::Array { dimensions, .. } = step {
            for (index, dimension) in indices.iter().zip(dimensions.iter()) {
                if *index < dimension.lower_bound
                    || *index >= dimension.lower_bound + i128::from(dimension.count)
                {
                    return Err(Stop::Refused(Refusal::new(
                        ErrorKind::Bounds,
                        format!("the index {index} is outside 0..{}", dimension.count),
                    )));
                }
            }
        }

        Ok(())
    }

    fn step(&mut self, from: &Place, step: &Step, indices: &[i128]) -> Result<Place, Stop> {
        if let (Place::Dynamic(index), Step::Attribute(name)) = (from, step) {
            return self.attribute(*index, name);
        }

        let offset_place = |place: &Place, offset: u64, ty| match place {
            Place::Memory { address, .. } => Place::Memory {
                address: address.wrapping_add(offset),
                ty,
            },
            Place::Register { name, bytes, .. } => Place::Register {
                name,
                bytes: bytes[usize::try_from(offset).expect("small")..].to_vec(),
                ty,
            },
            Place::Dynamic(_) => panic!("a value of a dynamic type is stepped by attribute"),
        };

        match step {
            Step::Entry { .. } | Step::Item => Err(Stop::Refused(Refusal::new(
                ErrorKind::Type,
                "a map's entries are found by key",
            ))),
            Step::Attribute(_) => panic!("only a value of a dynamic type has attributes"),
            Step::Global(object) => self.locate(object),
            Step::Presented(target) => {
                let bytes = self.bytes(from)?;
                let address = u64::from_le_bytes(bytes[8..16].try_into().expect("a word"));
                Ok(Place::Memory {
                    address,
                    ty: *target,
                })
            }
            Step::Deref(target) => {
                let bytes = self.bytes(from)?;
                let address = match self.decode(from.ty(), &bytes) {
                    VariableValue::Address(address) => address.address.get(),
                    _ => panic!("the world dereferences pointers only"),
                };

                if address == 0 {
                    return Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::ValueAccess(
                            ValueAccessUnavailableReason::NullPointer,
                        ),
                    )));
                }

                Ok(Place::Memory {
                    address,
                    ty: *target,
                })
            }
            Step::Member { offset, ty } => Ok(offset_place(from, *offset, *ty)),
            Step::VariantMember {
                tag,
                name,
                offset,
                ty,
            } => {
                if self.bytes(from)?.first() != Some(tag) {
                    return Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::ValueAccess(
                            ValueAccessUnavailableReason::InactiveVariant(Some(Arc::clone(name))),
                        ),
                    )));
                }

                Ok(offset_place(from, *offset, *ty))
            }

            Step::Array {
                dimensions,
                element_size,
                ty,
            } => {
                let mut linear = 0_u64;
                for (index, dimension) in indices.iter().zip(dimensions.iter()) {
                    linear = linear * dimension.count
                        + u64::try_from(index - dimension.lower_bound).expect("checked");
                }

                Ok(offset_place(from, linear * element_size, *ty))
            }
            Step::Slice { element_size, ty } => {
                let descriptor = self.bytes(from)?;
                let data = u64::from_le_bytes(descriptor[..8].try_into().expect("eight bytes"));
                let length = u64::from_le_bytes(descriptor[8..16].try_into().expect("eight bytes"));
                let index = indices[0];
                if index < 0 || index >= i128::from(length) {
                    return Err(Stop::missing(VariableState::Unavailable(
                        VariableUnavailableReason::IndexOutOfBounds {
                            index,
                            lower_bound: 0,
                            count: length,
                        },
                    )));
                }

                Ok(Place::Memory {
                    address: data.wrapping_add(
                        u64::try_from(index)
                            .expect("checked")
                            .wrapping_mul(*element_size),
                    ),
                    ty: *ty,
                })
            }
        }
    }

    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Place, Stop> {
        Ok(Place::Memory { address, ty })
    }

    fn address(&self, at: &Place) -> Result<u64, Stop> {
        match at {
            Place::Memory { address, .. } => Ok(*address),
            Place::Register { name, .. } => Err(Stop::Refused(Refusal::new(
                ErrorKind::NotAnLvalue,
                format!("the value is in register {name}, which has no address"),
            ))),
            Place::Dynamic(index) => Ok(dynamic_address(*index)),
        }
    }

    fn load(&mut self, at: &Place) -> Result<VariableValue, Stop> {
        let bytes = self.bytes(at)?;
        Ok(self.decode(at.ty(), &bytes))
    }

    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop> {
        self.read_memory(address, size)
    }

    fn text(&mut self, at: &Place) -> Result<Option<TextSummary>, Stop> {
        match self.representation(at.ty()).kind.clone() {
            TypeKind::Pointer {
                target: Some(target),
                ..
            } if self.is_char(target) => {
                let VariableValue::Address(address) = self.load(at)? else {
                    unreachable!("pointers decode to addresses")
                };

                Ok(Some(
                    self.c_string(address.address.get(), TextSummary::MAX_BYTES),
                ))
            }
            TypeKind::Array { element, .. } if self.is_char(element) => {
                let bytes = self.bytes(at)?;
                let end = bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(bytes.len());
                Ok(Some(TextSummary {
                    bytes: bytes[..end].to_vec().into(),
                    completion: TextCompletion::Complete,
                }))
            }
            _ => Ok(None),
        }
    }

    fn length(&mut self, at: &Place) -> Result<u64, Stop> {
        let descriptor = self.bytes(at)?;
        Ok(u64::from_le_bytes(
            descriptor[8..16].try_into().expect("eight bytes"),
        ))
    }

    fn capacity(&mut self, at: &Place) -> Result<u64, Stop> {
        let descriptor = self.bytes(at)?;
        Ok(u64::from_le_bytes(
            descriptor[16..24].try_into().expect("eight bytes"),
        ))
    }

    fn text_span(&mut self, at: &Place) -> Result<Option<TextSpan>, Stop> {
        let TypeKind::Pointer {
            target: Some(target),
            ..
        } = self.representation(at.ty()).kind.clone()
        else {
            return Ok(None);
        };

        if !self.is_char(target) {
            return Ok(None);
        }

        let VariableValue::Address(address) = self.load(at)? else {
            unreachable!("pointers decode to addresses")
        };

        let address = address.address.get();
        let text = self.c_string(address, TextSummary::MAX_BYTES);
        Ok(Some(TextSpan {
            address,
            length: text.bytes.len() as u64,
        }))
    }

    fn entry(&mut self, from: &Place, step: &Step, key: &Key) -> Result<Option<Place>, Stop> {
        if let Place::Dynamic(index) = from {
            return self.item(*index, key).map(Some);
        }

        let Step::Entry {
            key: key_type,
            value,
        } = step
        else {
            panic!("the world finds entries through entry steps");
        };

        let bytes = self.bytes(from)?;
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("a word"));
        let (keys, values, count) = (word(0), word(8), word(16));
        let (key_size, value_size) = (self.size(*key_type) as u64, self.size(*value) as u64);
        for index in 0..count {
            // A key is presented with its text, as the debugger presents
            // a string.
            let place = Place::Memory {
                address: keys + index * key_size,
                ty: *key_type,
            };

            let mut candidate = self.present(&place)?;
            let read = self.text(&place)?;
            if let VariableState::Available { text, .. } = &mut candidate.state {
                *text = read.map(Arc::new);
            }

            if key.matches(&candidate)? {
                return Ok(Some(Place::Memory {
                    address: values + index * value_size,
                    ty: *value,
                }));
            }
        }

        Ok(None)
    }

    fn function_generics(&mut self, address: u64) -> Result<Vec<(Arc<str>, TypeReference)>, Stop> {
        self.functions.get(&address).cloned().ok_or_else(|| {
            Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                format!("no function the debug information describes has its code at {address:#x}"),
            ))
        })
    }

    fn task(&mut self) -> Result<u64, Stop> {
        self.task.ok_or_else(|| {
            Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                "the program has no tasks",
            ))
        })
    }

    fn register(&mut self, register: &Register) -> Result<u128, Stop> {
        let (name, value) = self
            .registers
            .iter()
            .nth(usize::from(register.number))
            .expect("a register the scope named");
        value.ok_or_else(|| {
            Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::RegisterNotSaved((*name).into()),
            ))
        })
    }

    fn present(&mut self, at: &Place) -> Result<InspectedValue, Stop> {
        let source = match at {
            Place::Memory { address, .. } => {
                VariableValueSource::Memory(VirtualAddress::new(*address))
            }
            Place::Register { .. } => VariableValueSource::Computed,
            Place::Dynamic(index) => return Ok(self.present_dynamic(*index)),
        };

        let bytes = match &self.representation(at.ty()).kind {
            TypeKind::Record { .. } => Vec::new(),
            _ => self.bytes(at)?,
        };

        let state = self.available(at.ty(), source, bytes);
        Ok(self.inspected(at.ty(), state))
    }

    fn present_stored(&mut self, at: &Place) -> Result<InspectedValue, Stop> {
        self.present(at)
    }

    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop> {
        let state = self.available(ty, VariableValueSource::Computed, bytes.to_vec());
        Ok(self.inspected(ty, state))
    }

    fn present_pointer(
        &mut self,
        address: u64,
        _pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop> {
        Ok(self.finish(
            Some(type_info),
            VariableState::Available {
                source: VariableValueSource::Computed,
                raw: Some(address.to_le_bytes().to_vec().into()),
                value: VariableValue::Address(AddressValue {
                    function: None,
                    address: VirtualAddress::new(address),
                }),
                dereference: DereferenceState::NotApplicable,
                children: ValueChildren::NotApplicable,
                text: None,
                presentation: None,
            },
        ))
    }

    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue {
        InspectedValue {
            type_info,
            state,
            completion: InspectionCompletion::Complete,
            usage: InspectionUsage::default(),
        }
    }

    fn dynamic_at(&mut self, ty: &str, address: u64) -> Result<Place, Stop> {
        assert_eq!(
            ty, "py",
            "the binder casts only to the world's dynamic type"
        );
        (0..self.dynamic.len())
            .find(|index| dynamic_address(*index) == address)
            .map(Place::Dynamic)
            .ok_or_else(|| {
                Stop::missing(VariableState::Unavailable(
                    VariableUnavailableReason::Interpreted(
                        format!("no object is at {address:#x}").into(),
                    ),
                ))
            })
    }

    fn load_dynamic(&mut self, at: &Place) -> Result<Loaded, Stop> {
        let Place::Dynamic(index) = at else {
            panic!("only a value of a dynamic type is loaded as one");
        };

        let complete = |bytes: &[u8]| TextSummary {
            bytes: bytes.into(),
            completion: TextCompletion::Complete,
        };
        let value = &self.dynamic[*index];
        let loaded = match value {
            Dynamic::Nothing => LoadedValue::Nothing,
            Dynamic::Bool(value) => LoadedValue::Bool(*value),
            Dynamic::Int(value) => LoadedValue::Int(Exact::from(*value)),
            Dynamic::BigInt(_) => {
                return Err(Stop::Refused(Refusal::new(
                    ErrorKind::Arithmetic,
                    "the int is beyond the 128 bits microscope computes with",
                )));
            }
            Dynamic::Float(value) => LoadedValue::Float(Float::from_f64(*value)),
            Dynamic::Str(text) => LoadedValue::Text(complete(text.as_bytes())),
            Dynamic::Bytes(bytes) => LoadedValue::Bytes(complete(bytes)),
            Dynamic::List(items) | Dynamic::Tuple(items) => LoadedValue::Collection {
                length: items.len() as u64,
            },
            Dynamic::Dict(entries) => LoadedValue::Collection {
                length: entries.len() as u64,
            },
            Dynamic::Instance { .. } => LoadedValue::Object,
        };

        Ok(Loaded {
            type_name: value.type_name().into(),
            value: loaded,
        })
    }

    fn presented_length(&mut self, at: &Place) -> Result<Option<u64>, Stop> {
        let Place::Dynamic(index) = at else {
            return Ok(None);
        };

        let value = &self.dynamic[*index];
        let length = match value {
            Dynamic::Str(text) => text.chars().count(),
            Dynamic::Bytes(bytes) => bytes.len(),
            Dynamic::List(items) | Dynamic::Tuple(items) => items.len(),
            Dynamic::Dict(entries) => entries.len(),
            _ => {
                return Err(Stop::Refused(type_error(format!(
                    "a `{}` has no length microscope reads",
                    value.type_name()
                ))));
            }
        };

        Ok(Some(length as u64))
    }
}

/// Where the world says a value of a dynamic type is.
const fn dynamic_address(index: usize) -> u64 {
    0x7f00_0000 + index as u64 * 0x40
}

impl World {
    /// A value of a dynamic type, by the index it has.
    pub fn dynamic(&mut self, value: Dynamic) -> usize {
        self.dynamic.push(value);
        self.dynamic.len() - 1
    }

    /// The value the variable `name` of a dynamic type holds, if it is one.
    pub fn dynamic_index(&self, name: &str) -> Option<usize> {
        self.objects
            .iter()
            .find(|object| object.name == name)
            .and_then(|object| match object.storage {
                Storage::Dynamic(index) => Some(index),
                _ => None,
            })
    }

    /// The classes of the value at `index`, each named as its module
    /// qualifies it: its own, then each it derives from, in Python's order,
    /// ending at `builtins.object`. A built-in type is `builtins`'.
    pub fn lineage(&self, index: usize) -> Vec<String> {
        let value = &self.dynamic[index];
        let bases = match value {
            Dynamic::Instance { bases, .. } => {
                bases.iter().map(|base| (*base).to_owned()).collect()
            }
            _ => Vec::new(),
        };
        let own = match value {
            Dynamic::Instance { type_name, .. } => (*type_name).to_owned(),
            value => format!("builtins.{}", value.type_name()),
        };

        std::iter::once(own)
            .chain(bases)
            .chain(std::iter::once("builtins.object".to_owned()))
            .collect()
    }

    /// A variable of a dynamic type, holding the value at `index`.
    pub fn dynamic_variable(&mut self, name: &str, index: usize) {
        self.objects.push(Object {
            name: name.to_owned(),
            ty: super::types::UNIDENTIFIED,
            storage: Storage::Dynamic(index),
        });
    }

    /// Gives the variable `name` of a dynamic type another value.
    pub fn replace_dynamic(&mut self, name: &str, value: Dynamic) {
        let object = self
            .objects
            .iter()
            .find(|object| object.name == name)
            .expect("a variable of a dynamic type");
        let Storage::Dynamic(index) = object.storage else {
            panic!("`{name}` has a static type");
        };

        self.dynamic[index] = value;
    }

    fn attribute(&self, index: usize, name: &str) -> Result<Place, Stop> {
        let value = &self.dynamic[index];
        let Dynamic::Instance { attributes, .. } = value else {
            return Err(Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                format!(
                    "microscope reads no attributes of a `{}`",
                    value.type_name()
                ),
            )));
        };

        attributes
            .iter()
            .find(|(attribute, _)| *attribute == name)
            .map(|(_, index)| Place::Dynamic(*index))
            .ok_or_else(|| {
                Stop::Refused(Refusal::new(
                    ErrorKind::MissingKey,
                    format!("the `{}` has no attribute `{name}`", value.type_name()),
                ))
            })
    }

    /// The item of a sequence at an index, from its end when negative, or
    /// of a dict for a key, as Python's indexing finds it.
    fn item(&self, index: usize, key: &Key) -> Result<Place, Stop> {
        let value = &self.dynamic[index];
        match value {
            Dynamic::List(items) | Dynamic::Tuple(items) => {
                let position = match key {
                    Key::Integer(position) => position.to_i128(),
                    Key::Bool(value) => Some(i128::from(*value)),
                    _ => {
                        return Err(Stop::Refused(type_error(format!(
                            "a `{}` is indexed by integers",
                            value.type_name()
                        ))));
                    }
                };

                let length = items.len() as i128;
                position
                    .map(|position| {
                        if position < 0 {
                            position + length
                        } else {
                            position
                        }
                    })
                    .filter(|position| (0..length).contains(position))
                    .and_then(|position| usize::try_from(position).ok())
                    .map(|position| Place::Dynamic(items[position]))
                    .ok_or_else(|| {
                        Stop::Refused(Refusal::new(
                            ErrorKind::Bounds,
                            format!(
                                "the `{}` has {length} items, and {key} indexes none",
                                value.type_name()
                            ),
                        ))
                    })
            }
            Dynamic::Dict(entries) => entries
                .iter()
                .find(|(entry, _)| self.equals(*entry, key))
                .map(|(_, value)| Place::Dynamic(*value))
                .ok_or_else(|| {
                    Stop::Refused(Refusal::new(
                        ErrorKind::MissingKey,
                        format!("the `dict` holds no key {key}"),
                    ))
                }),
            Dynamic::Str(_) => Err(Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                "an item of a `str` is a new `str`, which microscope does not make",
            ))),
            _ => Err(Stop::Refused(type_error(format!(
                "a `{}` has no items",
                value.type_name()
            )))),
        }
    }

    /// Whether a dict's key equals the key written, as Python's `==` says.
    fn equals(&self, entry: usize, key: &Key) -> bool {
        let number = match &self.dynamic[entry] {
            Dynamic::Bool(value) => Some(f64::from(u8::from(*value))),
            #[expect(clippy::cast_precision_loss, reason = "the world's ints are small")]
            Dynamic::Int(value) => Some(*value as f64),
            Dynamic::Float(value) => Some(*value),
            Dynamic::Str(text) => {
                return matches!(key, Key::Text(bytes) if bytes == text.as_bytes());
            }
            Dynamic::Nothing => return *key == Key::Nothing,
            _ => return false,
        };

        #[expect(
            clippy::float_cmp,
            clippy::cast_precision_loss,
            reason = "Python compares numbers exactly, and the world's ints are small"
        )]
        match (number, key) {
            (Some(number), Key::Integer(value)) => {
                value.to_i128().is_some_and(|value| number == value as f64)
            }
            (Some(number), Key::Bool(value)) => number == f64::from(u8::from(*value)),
            (Some(number), Key::Float(value)) => {
                Float::from_f64(number).compare(*value) == Some(std::cmp::Ordering::Equal)
            }
            _ => false,
        }
    }

    /// A value of a dynamic type as inspection presents it: by its text in
    /// Python's notation.
    fn present_dynamic(&self, index: usize) -> InspectedValue {
        let value = &self.dynamic[index];
        let scalar = match value {
            Dynamic::Bool(value) => Some(ScalarValue::Boolean(*value)),
            Dynamic::Int(value) => Some(ScalarValue::Signed(*value)),
            Dynamic::Float(value) => {
                Some(ScalarValue::Floating(FloatValue::Binary64(value.to_bits())))
            }
            _ => None,
        };

        let presentation = crate::Presentation {
            view: Arc::new(crate::ViewName {
                source: "world".into(),
                line: 0,
                header: "dynamic values".into(),
                extend: false,
            }),
            shape: crate::PresentedShape::Value,
            count: None,
            summary: self.repr(index).into(),
            children: ValueChildren::NotApplicable,
            problem: None,
            number: None,
            presented: None,
            visualizers: Arc::from([]),
        };

        let address = VirtualAddress::new(dynamic_address(index));
        self.finish(
            Some(TypeInfo {
                reference: super::types::UNIDENTIFIED,
                name: value.type_name().into(),
                byte_size: None,
                kind: TypeKind::Opaque {
                    description: "a dynamic value".into(),
                },
                identity: None,
            }),
            VariableState::Available {
                source: VariableValueSource::Memory(address),
                raw: None,
                value: scalar.map_or(
                    VariableValue::Address(AddressValue {
                        function: None,
                        address,
                    }),
                    VariableValue::Scalar,
                ),
                dereference: DereferenceState::NotApplicable,
                children: ValueChildren::NotApplicable,
                text: None,
                presentation: Some(Arc::new(presentation)),
            },
        )
    }

    /// A value's text in Python's notation.
    fn repr(&self, index: usize) -> String {
        let items = |items: &[usize]| {
            items
                .iter()
                .map(|item| self.repr(*item))
                .collect::<Vec<_>>()
                .join(", ")
        };

        match &self.dynamic[index] {
            Dynamic::Nothing => "None".to_owned(),
            Dynamic::Bool(true) => "True".to_owned(),
            Dynamic::Bool(false) => "False".to_owned(),
            Dynamic::Int(value) => value.to_string(),
            Dynamic::BigInt(digits) => (*digits).to_owned(),
            Dynamic::Float(value) => format!("{value:?}"),
            Dynamic::Str(text) => format!("'{text}'"),
            Dynamic::Bytes(bytes) => format!("b{:?}", String::from_utf8_lossy(bytes)),
            Dynamic::List(values) => format!("[{}]", items(values)),
            Dynamic::Tuple(values) if values.len() == 1 => format!("({},)", items(values)),
            Dynamic::Tuple(values) => format!("({})", items(values)),
            Dynamic::Dict(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, value)| format!("{}: {}", self.repr(*key), self.repr(*value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Dynamic::Instance { type_name, .. } => format!("<{type_name} object>"),
        }
    }
}

/// A world by the name an example block gives.
pub fn world(name: &str) -> World {
    match name {
        "scalars" => scalars(),
        "memory" => memory(),
        "syscall" => syscall(),
        "python" => python(),
        other => panic!("no world named {other}"),
    }
}

/// A Python frame's variables, whose types only their values know.
pub fn python() -> World {
    let mut world = World::new();
    let variable = |world: &mut World, name: &str, value: Dynamic| {
        let index = world.dynamic(value);
        world.dynamic_variable(name, index);
        index
    };

    let none = variable(&mut world, "None", Dynamic::Nothing);
    world.dynamic_variable("nothing", none);
    variable(&mut world, "True", Dynamic::Bool(true));
    variable(&mut world, "False", Dynamic::Bool(false));
    variable(&mut world, "count", Dynamic::Int(7));
    variable(&mut world, "ratio", Dynamic::Float(0.5));
    variable(&mut world, "name", Dynamic::Str("ann"));
    variable(&mut world, "data", Dynamic::Bytes(b"ab"));
    variable(
        &mut world,
        "big",
        Dynamic::BigInt("1361129467683753853853498429727072845824"),
    );
    let ten = world.dynamic(Dynamic::Int(10));
    let twenty = world.dynamic(Dynamic::Int(20));
    let thirty = world.dynamic(Dynamic::Int(30));
    variable(
        &mut world,
        "numbers",
        Dynamic::List(vec![ten, twenty, thirty]),
    );
    variable(&mut world, "empty", Dynamic::List(Vec::new()));
    let one = world.dynamic(Dynamic::Int(1));
    let two = world.dynamic(Dynamic::Str("two"));
    variable(&mut world, "pair", Dynamic::Tuple(vec![one, two]));
    let ann = world.dynamic(Dynamic::Str("ann"));
    let bob = world.dynamic(Dynamic::Str("bob"));
    let thirty_one = world.dynamic(Dynamic::Int(30));
    let forty_one = world.dynamic(Dynamic::Int(41));
    variable(
        &mut world,
        "ages",
        Dynamic::Dict(vec![(ann, thirty_one), (bob, forty_one)]),
    );
    let squares: Vec<(usize, usize)> = [(1, 1), (2, 4), (3, 9)]
        .into_iter()
        .map(|(key, value)| {
            (
                world.dynamic(Dynamic::Int(key)),
                world.dynamic(Dynamic::Int(value)),
            )
        })
        .collect();
    variable(&mut world, "squares", Dynamic::Dict(squares));
    let one_text = world.dynamic(Dynamic::Str("one"));
    let none_text = world.dynamic(Dynamic::Str("none"));
    variable(
        &mut world,
        "labels",
        Dynamic::Dict(vec![(one, one_text), (none, none_text)]),
    );
    let x = world.dynamic(Dynamic::Int(3));
    let y = world.dynamic(Dynamic::Int(4));
    variable(
        &mut world,
        "point",
        Dynamic::Instance {
            type_name: "Point",
            bases: Vec::new(),
            attributes: vec![("x", x), ("y", y)],
        },
    );
    world
}

/// A thread stopped as `openat(AT_FDCWD, "/etc/hosts", O_RDONLY)` returned
/// `-ENOENT`.
pub fn syscall() -> World {
    World {
        call: Some(super::target::CallValues {
            args: [(-100_i64).cast_unsigned(), 0x7ffd_0000, 0, 0, 0, 0],
            returned: Some(-2),
            path: Some(b"/etc/hosts".to_vec()),
        }),
        ..World::new()
    }
}

/// Scalars of every C width, named as GCC names them.
pub fn scalars() -> World {
    use BaseTypeEncoding as E;
    let mut world = World::new();
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    let schar = world.base("signed char", E::SignedCharacter, 1);
    world.base("short int", E::Signed, 2);
    let int = world.base("int", E::Signed, 4);
    let uint = world.base("unsigned int", E::Unsigned, 4);
    let long = world.base("long int", E::Signed, 8);
    world.base("long unsigned int", E::Unsigned, 8);
    let ushort = world.base("short unsigned int", E::Unsigned, 2);
    let ulonglong = world.base("long long unsigned int", E::Unsigned, 8);
    let float = world.base("float", E::Floating, 4);
    let double = world.base("double", E::Floating, 8);
    let long_double = world.base("long double", E::Floating, 16);
    let boolean = world.base("_Bool", E::Boolean, 1);
    world.variable("uc", uchar, &[250]);
    world.variable("sc", schar, &(-7_i8).to_le_bytes());
    world.variable("u32v", uint, &4_000_000_000_u32.to_le_bytes());
    world.variable("i32v", int, &(-123_456_i32).to_le_bytes());
    world.variable("i64v", long, &(-100_i64).to_le_bytes());
    world.variable("u16v", ushort, &u16::MAX.to_le_bytes());
    world.variable("u64v", ulonglong, &u64::MAX.to_le_bytes());
    world.variable("f", float, &1.5_f32.to_le_bytes());
    world.variable("d", double, &10.0_f64.to_le_bytes());

    // 1.25 in x87 extended precision, padded to sixteen bytes.
    let mut extended = 0xa000_0000_0000_0000_u64.to_le_bytes().to_vec();
    extended.extend_from_slice(&0x3fff_u16.to_le_bytes());
    extended.resize(16, 0);
    world.variable("ld", long_double, &extended);
    world.variable("flag", boolean, &[1]);
    world
}

/// Records, one of them zero-sized, pointers, arrays, a slice, text,
/// enumerations, a typedef, a constant, a reference, values in a register or
/// optimized out, and names that need qualifying.
pub fn memory() -> World {
    use BaseTypeEncoding as E;
    let mut world = World::new();
    let int = world.base("int", E::Signed, 4);
    let long = world.base("long int", E::Signed, 8);
    let char = world.base("char", E::SignedCharacter, 1);
    let uint = world.base("unsigned int", E::Unsigned, 4);
    let record = world.record("S", 16, &[("a", int, 0), ("b", long, 8)]);
    let record_pointer = world.pointer(Some(record));
    let int_array = world.array(int, &[4]);
    let int_pointer = world.pointer(Some(int));
    let matrix = world.array(int, &[2, 3]);
    let char_pointer = world.pointer(Some(char));
    let char_array = world.array(char, &[8]);
    let color = world.enumeration("Color", uint, &[("RED", 0), ("GREEN", 1), ("BLUE", 2)]);
    let sign = world.enumeration("Sign", int, &[("NEGATIVE", -1), ("POSITIVE", 1)]);
    let uchar = world.base("unsigned char", E::UnsignedCharacter, 1);
    world.enumeration("Small", uchar, &[("ONE", 1), ("TWO", 2), ("HIGH", 0x80)]);
    let void_pointer = world.pointer(None);
    let slice = world.slice(int);
    let empty = world.record("Empty", 0, &[]);
    let empty_pointer = world.pointer(Some(empty));

    let mut s = 5_i32.to_le_bytes().to_vec();
    s.extend_from_slice(&[0; 4]);
    s.extend_from_slice(&7_i64.to_le_bytes());
    let s_address = world.variable("s", record, &s);
    world.variable("ptr", record_pointer, &s_address.to_le_bytes());
    world.variable("null_ptr", record_pointer, &0_u64.to_le_bytes());
    let arr: Vec<u8> = [11_i32, 22, 33, 44]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let arr_address = world.variable("arr", int_array, &arr);
    world.variable("ip", int_pointer, &(arr_address + 4).to_le_bytes());
    let m: Vec<u8> = (1..=6_i32).flat_map(i32::to_le_bytes).collect();
    world.variable("m", matrix, &m);
    let hello = world.allocate(b"hello\0");
    world.variable("name", char_pointer, &hello.to_le_bytes());
    world.variable("buf", char_array, b"abc\0\0\0\0\0");
    world.variable("color", color, &2_u32.to_le_bytes());
    world.variable("sign", sign, &(-1_i32).to_le_bytes());
    world.variable("vp", void_pointer, &s_address.to_le_bytes());
    world.variable("ep", empty_pointer, &s_address.to_le_bytes());
    let items: Vec<u8> = [10_i32, 20, 30]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let items_address = world.allocate(&items);
    let mut descriptor = items_address.to_le_bytes().to_vec();
    descriptor.extend_from_slice(&3_u64.to_le_bytes());
    world.variable("items", slice, &descriptor);
    let counter = world.typedef("counter_t", long);
    world.variable("count", counter, &12_i64.to_le_bytes());
    let constant = world.constant(int);
    world.variable("limit", constant, &100_i32.to_le_bytes());
    let int_reference = world.reference(int);
    world.variable("first", int_reference, &arr_address.to_le_bytes());
    world.variable("$future", record, &s);
    world.register_variable("r", int, "rbx", &9_i32.to_le_bytes());
    world.optimized_out("gone", int);
    world.set_register("rip", 0x40_1000);
    world.set_register("rsp", 0x7ffe_0000);
    world.lost_register("rbp");

    // Text that runs into unmapped memory before it ends.
    let cut = world.allocate(b"hel");
    world.variable("partial", char_pointer, &cut.to_le_bytes());

    // Two variables of one name, and an enumerator two enumerations share.
    world.variable("twice", int, &1_i32.to_le_bytes());
    world.variable("twice", int, &2_i32.to_le_bytes());
    let light = world.enumeration("Light", uint, &[("RED", 10), ("AMBER", 11)]);
    world.variable("light", light, &11_u32.to_le_bytes());

    // Names C reserves, which Go, Rust, and Zig programs may use.
    world.variable("long", int, &3_i32.to_le_bytes());
    let words = world.record("Words", 8, &[("int", int, 0), ("class", int, 4)]);
    let mut words_bytes = 4_i32.to_le_bytes().to_vec();
    words_bytes.extend(6_i32.to_le_bytes());
    world.variable("words", words, &words_bytes);

    // C++ classes: a Tile is a Named and a Shape, and a Twice holds two
    // Shapes, its own and its Tile's.
    let shape = world.record("Shape", 4, &[("id", int, 0)]);
    let named = world.record("Named", 8, &[("tag", long, 0)]);
    let tile = world.record("Tile", 16, &[("row", int, 12)]);
    world.set_bases(tile, &[(named, 0), (shape, 8)]);
    let twice = world.record("Twice", 24, &[]);
    world.set_bases(twice, &[(shape, 0), (tile, 8)]);
    let mut bytes = 2_i64.to_le_bytes().to_vec();
    bytes.extend(7_i32.to_le_bytes());
    bytes.extend(9_i32.to_le_bytes());
    world.variable("tile", tile, &bytes);
    let mut twice_bytes = 1_i32.to_le_bytes().to_vec();
    twice_bytes.resize(8, 0);
    twice_bytes.extend(bytes);
    world.variable("twice_shaped", twice, &twice_bytes);

    // A Rust `Option<i32>` holding 5, and one holding nothing.
    let some = world.record("Some", 8, &[("__0", int, 4)]);
    let none = world.record("None", 0, &[]);
    let option = world.variant("Option<i32>", 8, &[("None", none, 0), ("Some", some, 0)]);
    let mut held = vec![1, 0, 0, 0];
    held.extend(5_i32.to_le_bytes());
    world.variable("maybe", option, &held);
    world.variable("nothing", option, &[0; 8]);

    containers(&mut world, int, char_pointer);
    world.task = Some(7);
    world
}

/// A slice with room for two more, and maps by integer and by text.
fn containers(world: &mut World, int: TypeReference, char_pointer: TypeReference) {
    let spare = world.slice_with_capacity(int);
    let room: Vec<u8> = [1_i32, 2, 3, 0, 0]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let room = world.allocate(&room);
    let mut descriptor = room.to_le_bytes().to_vec();
    descriptor.extend_from_slice(&3_u64.to_le_bytes());
    descriptor.extend_from_slice(&5_u64.to_le_bytes());
    world.variable("spare", spare, &descriptor);
    let squares = world.map_type("Squares", int, int);
    let keys: Vec<u8> = [1_i32, 2, 3]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let values: Vec<u8> = [1_i32, 4, 9]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let bytes = world.map_bytes(&keys, &values, 3);
    world.variable("squares", squares, &bytes);
    let ages = world.map_type("Ages", char_pointer, int);
    let ann = world.allocate(b"ann\0");
    let bob = world.allocate(b"bob\0");
    let keys: Vec<u8> = [ann, bob]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let values: Vec<u8> = [30_i32, 41]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let bytes = world.map_bytes(&keys, &values, 2);
    world.variable("ages", ages, &bytes);
    let shared = world.shared_type("Shared", int);
    let value = world.allocate(&22_i32.to_le_bytes());
    let mut bytes = 2_u64.to_le_bytes().to_vec();
    bytes.extend(value.to_le_bytes());
    world.variable("shared", shared, &bytes);
}
