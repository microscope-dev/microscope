//! Python's object model past the built-in types' values: classes and the
//! static types, the attributes an instance keeps, and the attribute lookup
//! `object.__getattribute__` makes, made without running any code.
//!
//! An instance keeps the attributes its dict holds in a dict, or in values
//! whose keys its class's instances share: inline after the object from
//! 3.13, or behind a pointer tagged by its low bit before the object in
//! 3.12. It keeps the rest in slots, which its classes' `__slots__` name. A
//! static built-in type, such as `object`, keeps no dict of its own: its
//! interpreter keeps the dict.
//!
//! A lookup follows Python's order: a data descriptor of the type, then the
//! instance's own attributes, then any other attribute of the type. Where
//! that order would run code, as a property's getter, a method's binding,
//! or a class's own `__getattr__`, the lookup is refused, never guessed.

use std::sync::Arc;

use super::library::LibraryType;
use super::objects::{Problem, malformed};
use super::values::{Dict, Object, Values};
use crate::eval::error::ErrorKind;
use crate::image::python::{Constant, Field};
use crate::runtime_model::interpreted_identity;
use crate::runtime_model::{InterpretedFailure, InterpretedKey};
use crate::{SourceLanguage, TypeIdentity, VariableUnavailableReason, VirtualAddress};

/// The most attributes of one instance read.
const MAX_ATTRIBUTES: u64 = 1 << 16;

/// The most classes a class derives through to the static type its
/// instances are laid out as.
const MAX_BASES: usize = 256;

/// A type object, as the object model reads it.
#[derive(Debug)]
pub(super) struct Class {
    pub(super) address: VirtualAddress,
    flags: u64,

    /// The module that defines it, and its qualified name there.
    pub(super) module: Option<Arc<str>>,
    pub(super) name: Arc<str>,

    /// The standard library's type that only C defines it is, if it is
    /// one ([`super::library`]).
    pub(super) library: Option<LibraryType>,
}

impl Class {
    /// The name Python's `repr` gives it: qualified by its module, unless
    /// that is `builtins`.
    pub(super) fn qualified(&self) -> String {
        match self.module.as_deref() {
            Some(module) if module != "builtins" => format!("{module}.{}", self.name),
            _ => self.name.to_string(),
        }
    }
}

/// Where an attribute was found on a type, and what its type makes it.
struct TypeAttribute {
    value: VirtualAddress,

    /// Whether its type sets as a descriptor, which makes it take
    /// precedence over the instance's own attributes, and binds as one.
    sets: bool,
    binds: bool,
    member: bool,
}

fn refused(kind: ErrorKind, message: impl Into<Arc<str>>) -> InterpretedFailure {
    InterpretedFailure::Refused(kind, message.into())
}

/// Why a lookup the object model made failed, as a problem reading it.
fn problem(failure: InterpretedFailure) -> Problem {
    match failure {
        InterpretedFailure::Refused(_, message) => message,
        InterpretedFailure::Unavailable(reason) => reason.to_string().into(),
    }
}

impl Values<'_, '_> {
    /// Whether `flags` have the bit `bit` names.
    const fn flagged(&self, flags: u64, bit: Constant) -> bool {
        (flags >> self.reader.layout.constant(bit)) & 1 == 1
    }

    /// The type object at `ty`, read once in an inspection.
    pub(super) fn class(&self, ty: VirtualAddress) -> Result<Arc<Class>, Problem> {
        if let Some(class) = self.classes.borrow().get(&ty) {
            return Ok(Arc::clone(class));
        }

        // A type is an object whose type is `type` or a metaclass, which a
        // pointer that is no object's type, as native code may hold, is
        // not.
        let reader = &self.reader;
        let meta = VirtualAddress::new(reader.field_word(ty, Field::ObjectType)?);
        if !self.is_class(meta)? {
            return Err(malformed(ty, "type"));
        }

        let flags = reader.field_word(ty, Field::TypeFlags)?;
        let (module, name) = if self.flagged(flags, Constant::TypeHeapBit) {
            let qualname = VirtualAddress::new(reader.field_word(ty, Field::HeapTypeQualname)?);
            let name = self
                .str_text(qualname)
                .ok_or_else(|| malformed(ty, "class"))?;
            let module = self
                .own_dict(ty, flags)?
                .map(|dict| self.lookup(dict, &InterpretedKey::Text(b"__module__".to_vec())))
                .transpose()
                .map_err(problem)?
                .flatten()
                .and_then(|module| self.str_text(module));
            (module, name)
        } else {
            // A static type's name is qualified by its module, where its
            // module is not `builtins`.
            let name =
                reader.c_string(VirtualAddress::new(reader.field_word(ty, Field::TypeName)?))?;
            match name.rsplit_once('.') {
                Some((module, base)) => (Some(module.into()), base.into()),
                None => (Some("builtins".into()), name),
            }
        };

        let library =
            self.library_type(ty, self.defined_in_c(flags), (module.as_deref(), &name))?;
        let class = Arc::new(Class {
            address: ty,
            flags,
            module,
            name,
            library,
        });
        self.classes.borrow_mut().insert(ty, Arc::clone(&class));
        Ok(class)
    }

    /// The text of the `str` at `object`, where it is a whole `str` of
    /// characters, as a name is.
    pub(super) fn str_text(&self, object: VirtualAddress) -> Option<Arc<str>> {
        match self.object(object).ok()? {
            Object::Str { points, length } if points.len() as u64 == length => points
                .into_iter()
                .map(char::from_u32)
                .collect::<Option<String>>()
                .map(Arc::from),
            _ => None,
        }
    }

    /// Whether the type object at `ty` is a class: `type`'s instances,
    /// and its subclasses'.
    pub(super) fn is_class(&self, ty: VirtualAddress) -> Result<bool, Problem> {
        if ty == self.types.class {
            return Ok(true);
        }

        let meta = self.reader.field_word(ty, Field::TypeFlags)?;
        Ok(self.flagged(meta, Constant::TypeTypeSubclassBit))
    }

    /// The dict of `class`: its own, or for a static built-in type, the
    /// one its interpreter keeps.
    fn type_dict(&self, class: &Class) -> Result<Option<Dict>, Problem> {
        self.own_dict(class.address, class.flags)
    }

    fn own_dict(&self, ty: VirtualAddress, flags: u64) -> Result<Option<Dict>, Problem> {
        let reader = &self.reader;
        let dict = if self.flagged(flags, Constant::TypeStaticBuiltinBit) {
            let bad = || malformed(ty, "static type");
            let index = reader
                .field_word(ty, Field::TypeSubclasses)?
                .checked_sub(1)
                .ok_or_else(bad)?;
            let interpreter = VirtualAddress::new(
                reader.word(
                    self.runtime
                        .wrapping_add(reader.offset(Field::RuntimeInterpretersHead)?),
                )?,
            );
            let size = reader.offset(Field::StaticTypeStateSize)?;
            let state = |(first, end): (Field, Field)| -> Result<VirtualAddress, Problem> {
                let (first, end) = (reader.offset(first)?, reader.offset(end)?);
                index
                    .checked_mul(size)
                    .and_then(|at| at.checked_add(first))
                    .filter(|at| at.checked_add(size).is_some_and(|last| last <= end))
                    .map(|at| interpreter.wrapping_add(at))
                    .ok_or_else(bad)
            };

            // A static type of an extension the interpreter manages, as
            // `_datetime`'s from 3.13, has its state at its index among
            // theirs, where the built-in types' state there is another's.
            let mut kept = state((
                Field::InterpreterStaticTypes,
                Field::InterpreterStaticTypesEnd,
            ))?;
            if reader.word(kept.wrapping_add(reader.offset(Field::StaticTypeStateType)?))?
                != ty.get()
                && reader.layout.has(Field::InterpreterStaticExtensionTypes)
            {
                kept = state((
                    Field::InterpreterStaticExtensionTypes,
                    Field::InterpreterStaticExtensionTypesEnd,
                ))?;
            }

            reader.word(kept.wrapping_add(reader.offset(Field::StaticTypeStateDict)?))?
        } else {
            reader.field_word(ty, Field::TypeDict)?
        };

        if dict == 0 {
            return Ok(None);
        }

        match self.object(VirtualAddress::new(dict))? {
            Object::Dict(dict) => Ok(Some(dict)),
            _ => Err(malformed(ty, "type")),
        }
    }

    /// Whether a type of `flags` is defined in C, as a static type is, and
    /// a heap type whose attributes cannot be set, which no class is.
    const fn defined_in_c(&self, flags: u64) -> bool {
        !self.flagged(flags, Constant::TypeHeapBit)
            || self.flagged(flags, Constant::TypeImmutableBit)
    }

    /// The type defined in C the instances of `class` are laid out as: the
    /// first its bases lead to, as `int` is for a class deriving from it,
    /// or `object` for most classes; `None` for a type defined in C, which
    /// is its own.
    pub(super) fn static_base(&self, class: &Class) -> Result<Option<VirtualAddress>, Problem> {
        let reader = &self.reader;
        if self.defined_in_c(class.flags) {
            return Ok(None);
        }

        let mut ty = class.address;
        for _ in 0..MAX_BASES {
            ty = VirtualAddress::new(reader.field_word(ty, Field::TypeBase)?);
            if ty == VirtualAddress::new(0) {
                return Err(malformed(class.address, "class"));
            }

            let flags = reader.field_word(ty, Field::TypeFlags)?;
            if self.defined_in_c(flags) {
                return Ok(Some(ty));
            }
        }

        Err(format!(
            "the class at {:#x} derives through more than {MAX_BASES} classes",
            class.address
        )
        .into())
    }

    /// Whether the type at `ty` is `base` or derives from it, through its
    /// bases.
    pub(super) fn derives(
        &self,
        mut ty: VirtualAddress,
        base: VirtualAddress,
    ) -> Result<bool, Problem> {
        for _ in 0..MAX_BASES {
            if ty == base {
                return Ok(true);
            }

            if ty == VirtualAddress::new(0) {
                return Ok(false);
            }

            ty = VirtualAddress::new(self.reader.field_word(ty, Field::TypeBase)?);
        }

        Err(format!("the type at {ty:#x} derives through more than {MAX_BASES} types").into())
    }

    /// What an object of `class`, holding the built-in value `base` if it
    /// holds one, is taken as for the special method `name`, which Python
    /// calls on it: its base where the method found is the one the base's
    /// type has, its own or one it inherits, as an `OrderedDict` has
    /// `dict`'s `__getitem__`. The object is refused where another type defines `name`,
    /// whose code would run, and is a type error where none does, as in
    /// Python, which `doing` describes, as "has no items".
    pub(super) fn special(
        &self,
        class: &Class,
        base: Option<Box<Object>>,
        name: &str,
        doing: &str,
    ) -> Result<Object, InterpretedFailure> {
        let found = self.type_attribute(class, name)?.map(|(owner, _)| owner);
        let inherited = match (
            found,
            base.as_deref().and_then(|base| self.built_in_type(base)),
        ) {
            (Some(owner), Some(base_type)) => {
                self.derives(base_type, owner).map_err(|problem| {
                    InterpretedFailure::Unavailable(VariableUnavailableReason::Interpreted(problem))
                })?
            }
            _ => false,
        };
        match (found, base) {
            (Some(_), Some(base)) if inherited => Ok(*base),
            (Some(_), _) => Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "a `{}` {doing} by its own `{name}`, which microscope does not run",
                    class.qualified()
                ),
            )),
            (None, _) => Err(refused(
                ErrorKind::Type,
                format!("a `{}` defines no `{name}`", class.qualified()),
            )),
        }
    }

    /// The identities of the class at `class` and of each class it derives
    /// from, in its method resolution order.
    pub(super) fn lineage(&self, class: VirtualAddress) -> Result<Vec<TypeIdentity>, Problem> {
        let class = self.class(class)?;
        self.mro(&class)?
            .into_iter()
            .map(|ty| {
                let class = self.class(ty)?;
                Ok(interpreted_identity(
                    SourceLanguage::Python,
                    class.module.as_deref(),
                    &class.name,
                ))
            })
            .collect()
    }

    /// The types `class` finds its attributes in, in order: its method
    /// resolution order.
    fn mro(&self, class: &Class) -> Result<Vec<VirtualAddress>, Problem> {
        let mro = VirtualAddress::new(self.reader.field_word(class.address, Field::TypeMro)?);
        match self.object(mro)? {
            Object::Tuple { items, length } => self.items(items, 0, length),
            _ => Err(malformed(class.address, "type")),
        }
    }

    /// Where the instance at `object`, of `class`, keeps the attributes
    /// its dict holds, or `None` where it keeps none.
    pub(super) fn instance_dict(
        &self,
        object: VirtualAddress,
        class: &Class,
    ) -> Result<Option<Dict>, Problem> {
        let (reader, layout) = (&self.reader, self.reader.layout);
        if self.flagged(class.flags, Constant::TypeManagedDictBit) {
            let words = u64::from(layout.constant(Constant::ManagedDictWords));
            let pointer = reader.word(VirtualAddress::new(object.get().wrapping_sub(words * 8)))?;

            // 3.12 keeps the address before the values, which are aligned,
            // so that its low bit tags them.
            if layout.constant(Constant::ManagedDictTagged) == 1 && pointer & 1 == 1 {
                return self
                    .shared_values(class, VirtualAddress::new(pointer.wrapping_add(1)))
                    .map(Some);
            }

            if pointer != 0 {
                return self.dict_at(VirtualAddress::new(pointer)).map(Some);
            }

            if layout.has(Field::DictValuesValid)
                && self.flagged(class.flags, Constant::TypeInlineValuesBit)
            {
                let values =
                    object.wrapping_add(reader.field_word(class.address, Field::TypeBasicsize)?);
                let valid = reader.bytes(
                    values.wrapping_add(reader.offset(Field::DictValuesValid)?),
                    1,
                )?;
                if valid.first() == Some(&0) {
                    // Values given up for a dict leave the dict's pointer.
                    return Err(malformed(object, "object"));
                }

                return self.shared_values(class, values).map(Some);
            }

            return Ok(None);
        }

        let offset = reader
            .field_word(class.address, Field::TypeDictoffset)?
            .cast_signed();
        if offset < 0 {
            return Err(format!(
                "a `{}` keeps its dict past its items, which is not read yet",
                class.qualified()
            )
            .into());
        }

        if offset == 0 {
            return Ok(None);
        }

        let pointer = reader.word(object.wrapping_add(offset.cast_unsigned()))?;
        if pointer == 0 {
            return Ok(None);
        }

        self.dict_at(VirtualAddress::new(pointer)).map(Some)
    }

    fn dict_at(&self, address: VirtualAddress) -> Result<Dict, Problem> {
        match self.object(address)? {
            Object::Dict(dict) => Ok(dict),
            _ => Err(malformed(address, "dict")),
        }
    }

    /// The table of the values at `values`, whose keys `class` keeps for
    /// its instances.
    fn shared_values(&self, class: &Class, values: VirtualAddress) -> Result<Dict, Problem> {
        let keys = VirtualAddress::new(
            self.reader
                .field_word(class.address, Field::HeapTypeCachedKeys)?,
        );
        let (split, size) = self.split(values)?;
        self.table(values, keys, size, Some(split))
    }

    /// The slots of the instance at `object`, of `class`, that hold
    /// values: each one's name and value, from its most derived class's on,
    /// each class's in the sorted order CPython keeps them in. A slot whose
    /// name a more derived class's slot takes is left out, whatever either
    /// holds, as that name finds the other.
    fn slots(
        &self,
        object: VirtualAddress,
        class: &Class,
    ) -> Result<Vec<(Arc<str>, VirtualAddress)>, Problem> {
        // Only classes have slots, and a static type derives from no class.
        let mut slots: Vec<(Arc<str>, VirtualAddress)> = Vec::new();
        let mut taken: Vec<Arc<str>> = Vec::new();
        if !self.flagged(class.flags, Constant::TypeHeapBit) {
            return Ok(slots);
        }

        for ty in self.mro(class)? {
            let class = self.class(ty)?;
            if !self.flagged(class.flags, Constant::TypeHeapBit) {
                continue;
            }

            let names = self.reader.field_word(ty, Field::HeapTypeSlots)?;
            if names == 0 {
                continue;
            }

            let Object::Tuple { items, length } = self.object(VirtualAddress::new(names))? else {
                return Err(malformed(ty, "class"));
            };

            let Some(dict) = self.type_dict(&class)? else {
                continue;
            };

            for name in self.items(items, 0, length)? {
                let text = self.str_text(name).ok_or_else(|| malformed(ty, "class"))?;
                if taken.contains(&text) {
                    continue;
                }

                taken.push(Arc::clone(&text));

                let found = self
                    .lookup(dict, &InterpretedKey::Text(text.as_bytes().to_vec()))
                    .map_err(problem)?;
                let Some(descriptor) = found else {
                    continue;
                };

                if let Some(value) = self.member(object, descriptor)?
                    && value != VirtualAddress::new(0)
                {
                    slots.push((text, value));
                }
            }
        }

        Ok(slots)
    }

    /// What the instance at `object` keeps in the member the descriptor at
    /// `descriptor` defines, a null for none; `None` where the descriptor is
    /// no member descriptor of an object, which only its code reads.
    fn member(
        &self,
        object: VirtualAddress,
        descriptor: VirtualAddress,
    ) -> Result<Option<VirtualAddress>, Problem> {
        let reader = &self.reader;
        if reader.field_word(descriptor, Field::ObjectType)? != self.types.member.get() {
            return Ok(None);
        }

        let definition =
            VirtualAddress::new(reader.field_word(descriptor, Field::MemberDescrMember)?);
        let kind = reader.field_bytes(definition, Field::MemberDefType, 4)?;
        let kind = u32::from_le_bytes(
            kind.try_into()
                .map_err(|_| malformed(descriptor, "member"))?,
        );
        if kind != u32::from(reader.layout.constant(Constant::MemberObjectEx)) {
            return Ok(None);
        }

        let offset = reader.field_word(definition, Field::MemberDefOffset)?;
        Ok(Some(VirtualAddress::new(
            reader.word(object.wrapping_add(offset))?,
        )))
    }

    /// How many attributes the instance at `object`, of `class`, has: those
    /// its dict holds, then those its slots hold.
    pub(super) fn attribute_count(
        &self,
        object: VirtualAddress,
        class: &Class,
    ) -> Result<u64, Problem> {
        let held = self
            .instance_dict(object, class)?
            .map_or(0, |dict| dict.length);
        let slots = self.slots(object, class)?.len() as u64;
        let count = held + slots;
        if count > MAX_ATTRIBUTES {
            return Err(format!("the object at {object:#x} has {count} attributes").into());
        }

        Ok(count)
    }

    /// The attributes `range` of the instance at `object`, of `class`, in
    /// the order [`Self::attribute_count`] counts them, each one's name and
    /// value; but a dataclass's fields first, in the order it defines them,
    /// which its `__init__` and `repr` follow, as its slots do not.
    pub(super) fn attributes(
        &self,
        object: VirtualAddress,
        class: &Class,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<(Arc<str>, VirtualAddress)>, Problem> {
        let Some(fields) = self.dataclass_fields(class)? else {
            return self.stored_attributes(object, class, range);
        };

        let mut attributes = self.stored_attributes(object, class, 0..MAX_ATTRIBUTES)?;
        attributes.sort_by_key(|(name, _)| {
            fields
                .iter()
                .position(|field| field == name)
                .unwrap_or(fields.len())
        });
        let start = usize::try_from(range.start).unwrap_or(usize::MAX);
        let end = usize::try_from(range.end).unwrap_or(usize::MAX);
        Ok(attributes
            .into_iter()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect())
    }

    /// The names of the fields of the dataclass `class` is, in the order it
    /// defines them, or `None` where it is none: the keys of the dict of
    /// `__dataclass_fields__` it finds, which `dataclasses` keeps in that
    /// order.
    fn dataclass_fields(&self, class: &Class) -> Result<Option<Vec<Arc<str>>>, Problem> {
        let found = self
            .type_attribute(class, "__dataclass_fields__")
            .map_err(problem)?;
        let Some((_, fields)) = found else {
            return Ok(None);
        };

        let Object::Dict(dict) = self.object(fields)? else {
            return Ok(None);
        };

        let mut names = Vec::new();
        self.entries(dict, 0..MAX_ATTRIBUTES, |entry| {
            names.extend(self.str_text(entry.key));
            Ok(())
        })?;
        Ok(Some(names))
    }

    /// [`Self::attributes`] in the order the instance keeps them.
    fn stored_attributes(
        &self,
        object: VirtualAddress,
        class: &Class,
        range: std::ops::Range<u64>,
    ) -> Result<Vec<(Arc<str>, VirtualAddress)>, Problem> {
        let mut attributes = Vec::new();
        let held = match self.instance_dict(object, class)? {
            Some(dict) => {
                self.entries(dict, range.clone(), |entry| {
                    attributes.push((self.attribute_name(entry.key), entry.value));
                    Ok(())
                })?;
                dict.length
            }
            None => 0,
        };

        let slots = self.slots(object, class)?;
        let start = range.start.saturating_sub(held);
        let end = range.end.saturating_sub(held);
        attributes.extend(
            (0_u64..)
                .zip(slots)
                .filter(|(index, _)| (start..end).contains(index))
                .map(|(_, slot)| slot),
        );
        Ok(attributes)
    }

    /// An attribute's name: the text of its key, or, for a key that is no
    /// name, its `repr`.
    fn attribute_name(&self, key: VirtualAddress) -> Arc<str> {
        self.str_text(key).unwrap_or_else(|| self.summary(key))
    }

    /// The attribute `name` of the object at `object`, as Python finds it
    /// without running code.
    pub(super) fn attribute(
        &self,
        object: VirtualAddress,
        name: &str,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let unavailable = |problem| self.unavailable(problem);
        let read = self.object(object).map_err(unavailable)?;
        match read {
            Object::Module { dict } => {
                let class = self.class(self.types.module).map_err(unavailable)?;
                let dict = self.dict_at(dict).map_err(unavailable)?;
                let getattr = self.find(dict, "__getattr__")?.is_some();
                self.generic(object, &class, Some(dict), name, getattr)
            }
            Object::Class { this, meta } => self.class_attribute(&this, &meta, name),
            _ => {
                let ty = VirtualAddress::new(
                    self.reader
                        .field_word(object, Field::ObjectType)
                        .map_err(unavailable)?,
                );
                let class = self.class(ty).map_err(unavailable)?;
                let getattr = self.getattr_hook(&class)?;
                let dict = self.instance_dict(object, &class).map_err(unavailable)?;
                self.generic(object, &class, dict, name, getattr)
            }
        }
    }

    /// Whether `class` finds the attributes its instances lack with a
    /// `__getattr__` of its own; refused where it reads attributes with
    /// any code other than `object.__getattribute__`.
    fn getattr_hook(&self, class: &Class) -> Result<bool, InterpretedFailure> {
        let unavailable = |problem| self.unavailable(problem);
        let getattro = self
            .reader
            .field_word(class.address, Field::TypeGetattro)
            .map_err(unavailable)?;
        if getattro == self.types.generic_getattr.get() {
            return Ok(false);
        }

        // Any other function is a class's hook, which runs its
        // `__getattribute__` and then its `__getattr__`, or a type's own.
        let generic = match self.type_attribute(class, "__getattribute__")? {
            Some((_, value)) => self.wraps_generic(value).map_err(unavailable)?,
            None => false,
        };
        if !generic {
            return Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "a `{}` reads its attributes with its own `__getattribute__`, which \
                     microscope does not run",
                    class.qualified()
                ),
            ));
        }

        if self.type_attribute(class, "__getattr__")?.is_none() {
            return Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "a `{}` reads its attributes with its type's own code, which microscope \
                     does not run",
                    class.qualified()
                ),
            ));
        }

        Ok(true)
    }

    /// Whether the object at `value` is the wrapper of
    /// `PyObject_GenericGetAttr` that a type's dict holds as its
    /// `__getattribute__`, as `object`'s and `int`'s are.
    fn wraps_generic(&self, value: VirtualAddress) -> Result<bool, Problem> {
        let reader = &self.reader;
        let ty = VirtualAddress::new(reader.field_word(value, Field::ObjectType)?);
        Ok(ty == self.types.wrapper
            && reader.field_word(value, Field::WrapperDescrWrapped)?
                == self.types.generic_getattr.get())
    }

    /// `object.__getattribute__`'s lookup of `name` on the instance at
    /// `object`, of `class`, whose own attributes `dict` holds, and which
    /// finds those it lacks with code where `getattr`.
    fn generic(
        &self,
        object: VirtualAddress,
        class: &Class,
        dict: Option<Dict>,
        name: &str,
        getattr: bool,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        let found = self
            .type_attribute(class, name)?
            .map(|(_, value)| self.describe(value))
            .transpose()?;
        if let Some(attribute) = &found
            && attribute.sets
        {
            return self.data_descriptor(object, class, name, attribute);
        }

        if let Some(dict) = dict
            && let Some(value) = self.find(dict, name)?
        {
            return Ok(value);
        }

        if let Some(attribute) = found {
            if attribute.binds {
                return Err(refused(
                    ErrorKind::Unsupported,
                    format!(
                        "`{name}` of a `{}` is a `{}`, which binds with code microscope does \
                         not run, as a method does",
                        class.qualified(),
                        self.type_of(attribute.value)?
                    ),
                ));
            }

            return Ok(attribute.value);
        }

        if getattr {
            return Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "a `{}` has no attribute `{name}` of its own, and finds it with its \
                     `__getattr__`, which microscope does not run",
                    class.qualified()
                ),
            ));
        }

        Err(refused(
            ErrorKind::MissingKey,
            format!("the `{}` has no attribute `{name}`", class.qualified()),
        ))
    }

    /// What a data descriptor of `class` gives for `name` of the instance
    /// at `object`: what its slot holds, for a slot's.
    fn data_descriptor(
        &self,
        object: VirtualAddress,
        class: &Class,
        name: &str,
        attribute: &TypeAttribute,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        if attribute.member {
            let value = self
                .member(object, attribute.value)
                .map_err(|problem| self.unavailable(problem))?;
            return match value {
                Some(value) if value != VirtualAddress::new(0) => Ok(value),
                Some(_) => Err(refused(
                    ErrorKind::MissingKey,
                    format!(
                        "the slot `{name}` of the `{}` holds no value",
                        class.qualified()
                    ),
                )),
                None => Err(refused(
                    ErrorKind::Unsupported,
                    format!(
                        "`{name}` of a `{}` is a member its type's code reads",
                        class.qualified()
                    ),
                )),
            };
        }

        Err(refused(
            ErrorKind::Unsupported,
            format!(
                "`{name}` of a `{}` is a `{}`, whose code microscope does not run",
                class.qualified(),
                self.type_of(attribute.value)?
            ),
        ))
    }

    /// The attribute `name` of the class `this`, whose type is `meta`, as
    /// `type.__getattribute__` finds it without running code.
    fn class_attribute(
        &self,
        this: &Class,
        meta: &Class,
        name: &str,
    ) -> Result<VirtualAddress, InterpretedFailure> {
        if meta.address != self.types.class {
            return Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "`{}` is a class of the metaclass `{}`, whose attributes microscope does not \
                     read",
                    this.qualified(),
                    meta.qualified()
                ),
            ));
        }

        let from_meta = self
            .type_attribute(meta, name)?
            .map(|(_, value)| self.describe(value))
            .transpose()?;
        if from_meta.as_ref().is_some_and(|attribute| attribute.sets) {
            return Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "`{name}` of a class is computed by `type`'s code, which microscope does not run"
                ),
            ));
        }

        let own = self
            .type_attribute(this, name)?
            .map(|(_, value)| self.describe(value))
            .transpose()?;
        let Some(attribute) = own.or(from_meta) else {
            return Err(refused(
                ErrorKind::MissingKey,
                format!("the class `{}` has no attribute `{name}`", this.qualified()),
            ));
        };

        if attribute.binds {
            return Err(refused(
                ErrorKind::Unsupported,
                format!(
                    "`{name}` of `{}` is a `{}`, which binds with code microscope does not run",
                    this.qualified(),
                    self.type_of(attribute.value)?
                ),
            ));
        }

        Ok(attribute.value)
    }

    /// The first type in `class`'s method resolution order whose dict holds
    /// `name`, and what it holds.
    pub(super) fn type_attribute(
        &self,
        class: &Class,
        name: &str,
    ) -> Result<Option<(VirtualAddress, VirtualAddress)>, InterpretedFailure> {
        let unavailable = |problem| self.unavailable(problem);
        for ty in self.mro(class).map_err(unavailable)? {
            let class = self.class(ty).map_err(unavailable)?;
            let Some(dict) = self.type_dict(&class).map_err(unavailable)? else {
                continue;
            };

            if let Some(value) = self.find(dict, name)? {
                return Ok(Some((ty, value)));
            }
        }

        Ok(None)
    }

    /// What `dict` holds for the name `name`.
    fn find(&self, dict: Dict, name: &str) -> Result<Option<VirtualAddress>, InterpretedFailure> {
        self.lookup(dict, &InterpretedKey::Text(name.as_bytes().to_vec()))
    }

    /// What the type of the type attribute at `value` makes it.
    fn describe(&self, value: VirtualAddress) -> Result<TypeAttribute, InterpretedFailure> {
        let reader = &self.reader;
        let read = || -> Result<TypeAttribute, Problem> {
            let ty = VirtualAddress::new(reader.field_word(value, Field::ObjectType)?);
            Ok(TypeAttribute {
                value,
                sets: reader.field_word(ty, Field::TypeDescrSet)? != 0,
                binds: reader.field_word(ty, Field::TypeDescrGet)? != 0,
                member: ty == self.types.member,
            })
        };
        read().map_err(|problem| self.unavailable(problem))
    }

    /// The qualified name of the type of the object at `object`.
    fn type_of(&self, object: VirtualAddress) -> Result<String, InterpretedFailure> {
        let unavailable = |problem| self.unavailable(problem);
        let ty = self
            .reader
            .field_word(object, Field::ObjectType)
            .map_err(unavailable)?;
        Ok(self
            .class(VirtualAddress::new(ty))
            .map_err(unavailable)?
            .qualified())
    }
}
