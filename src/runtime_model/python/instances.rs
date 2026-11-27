//! `CPython` class metadata and attribute resolution across heap and static built-in types.
//!
//! Handles per-interpreter static type dicts (3.12+), MRO traversal, and non-executing descriptor
//! checks so attribute reads never invoke arbitrary Python `__getattr__` or property getters.

use std::sync::Arc;

use super::library::LibraryType;
use super::objects::{Problem, malformed};
use super::values::{Dict, Object, Values};
use crate::eval::error::ErrorKind;
use crate::image::python::{Constant, Field};
use crate::runtime_model::interpreted_identity;
use crate::runtime_model::{InterpretedFailure, InterpretedKey};
use crate::{SourceLanguage, TypeIdentity, VariableUnavailableReason, VirtualAddress};

const MAX_ATTRIBUTES: u64 = 1 << 16;

const MAX_BASES: usize = 256;

#[derive(Debug)]
pub(super) struct Class {
    pub(super) address: VirtualAddress,
    flags: u64,

    pub(super) module: Option<Arc<str>>,
    pub(super) name: Arc<str>,

    pub(super) library: Option<LibraryType>,
}

impl Class {
    pub(super) fn qualified(&self) -> String {
        match self.module.as_deref() {
            Some(module) if module != "builtins" => format!("{module}.{}", self.name),
            _ => self.name.to_string(),
        }
    }
}

struct TypeAttribute {
    value: VirtualAddress,

    sets: bool,
    binds: bool,
    member: bool,
}

fn refused(kind: ErrorKind, message: impl Into<Arc<str>>) -> InterpretedFailure {
    InterpretedFailure::Refused(kind, message.into())
}

pub(super) fn problem(failure: InterpretedFailure) -> Problem {
    match failure {
        InterpretedFailure::Refused(_, message) => message,
        InterpretedFailure::Unavailable(reason) => reason.to_string().into(),
    }
}

impl Values<'_, '_> {
    const fn flagged(&self, flags: u64, bit: Constant) -> bool {
        (flags >> self.reader.layout.constant(bit)) & 1 == 1
    }

    pub(super) fn class(&self, ty: VirtualAddress) -> Result<Arc<Class>, Problem> {
        if let Some(class) = self.classes.borrow().get(&ty) {
            debug_assert_eq!(
                class.address, ty,
                "the class cached for virtual {ty:#x} is another type's",
            );
            return Ok(Arc::clone(class));
        }

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

    pub(super) fn str_text(&self, object: VirtualAddress) -> Option<Arc<str>> {
        let ty = self.reader.field_word(object, Field::ObjectType).ok()?;
        if VirtualAddress::new(ty) != self.types.str {
            return None;
        }

        match self.object(object).ok()? {
            Object::Str { points, length } if points.len() as u64 == length => points
                .into_iter()
                .map(char::from_u32)
                .collect::<Option<String>>()
                .map(Arc::from),
            Object::Str { .. } => self.reader.string(object).ok(),
            _ => None,
        }
    }

    pub(super) fn is_class(&self, ty: VirtualAddress) -> Result<bool, Problem> {
        if ty == self.types.class {
            return Ok(true);
        }

        let meta = self.reader.field_word(ty, Field::TypeFlags)?;
        Ok(self.flagged(meta, Constant::TypeTypeSubclassBit))
    }

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

            let holds = |state: VirtualAddress| -> Result<bool, Problem> {
                let at = state.wrapping_add(reader.offset(Field::StaticTypeStateType)?);
                Ok(reader.word(at)? == ty.get())
            };
            let mut kept = state((
                Field::InterpreterStaticTypes,
                Field::InterpreterStaticTypesEnd,
            ))?;
            if !holds(kept)? && reader.layout.has(Field::InterpreterStaticExtensionTypes) {
                kept = state((
                    Field::InterpreterStaticExtensionTypes,
                    Field::InterpreterStaticExtensionTypesEnd,
                ))?;
            }

            if !holds(kept)? {
                return Err(bad());
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

    const fn defined_in_c(&self, flags: u64) -> bool {
        !self.flagged(flags, Constant::TypeHeapBit)
            || self.flagged(flags, Constant::TypeImmutableBit)
    }

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

    fn mro(&self, class: &Class) -> Result<Vec<VirtualAddress>, Problem> {
        let mro = VirtualAddress::new(self.reader.field_word(class.address, Field::TypeMro)?);
        match self.object(mro)? {
            Object::Tuple { items, length } => self.items(items, 0, length),
            _ => Err(malformed(class.address, "type")),
        }
    }

    pub(super) fn instance_dict(
        &self,
        object: VirtualAddress,
        class: &Class,
    ) -> Result<Option<Dict>, Problem> {
        let (reader, layout) = (&self.reader, self.reader.layout);
        if self.flagged(class.flags, Constant::TypeManagedDictBit) {
            let words = u64::from(layout.constant(Constant::ManagedDictWords));
            let pointer = reader.word(VirtualAddress::new(object.get().wrapping_sub(words * 8)))?;

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

    fn shared_values(&self, class: &Class, values: VirtualAddress) -> Result<Dict, Problem> {
        let keys = VirtualAddress::new(
            self.reader
                .field_word(class.address, Field::HeapTypeCachedKeys)?,
        );
        let (split, size) = self.split(values)?;
        self.table(values, keys, size, Some(split))
    }

    fn slots(
        &self,
        object: VirtualAddress,
        class: &Class,
    ) -> Result<Vec<(Arc<str>, VirtualAddress)>, Problem> {
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

        debug_assert!(
            slots.len() <= taken.len(),
            "the instance at virtual {object:#x} has {} slots of {} names",
            slots.len(),
            taken.len(),
        );
        Ok(slots)
    }

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

    fn attribute_name(&self, key: VirtualAddress) -> Arc<str> {
        self.str_text(key).unwrap_or_else(|| self.summary(key))
    }

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

    fn getattr_hook(&self, class: &Class) -> Result<bool, InterpretedFailure> {
        let unavailable = |problem| self.unavailable(problem);
        let getattro = self
            .reader
            .field_word(class.address, Field::TypeGetattro)
            .map_err(unavailable)?;
        if getattro == self.types.generic_getattr.get() {
            return Ok(false);
        }

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

    fn wraps_generic(&self, value: VirtualAddress) -> Result<bool, Problem> {
        let reader = &self.reader;
        let ty = VirtualAddress::new(reader.field_word(value, Field::ObjectType)?);
        Ok(ty == self.types.wrapper
            && reader.field_word(value, Field::WrapperDescrWrapped)?
                == self.types.generic_getattr.get())
    }

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

    fn find(&self, dict: Dict, name: &str) -> Result<Option<VirtualAddress>, InterpretedFailure> {
        self.lookup(dict, &InterpretedKey::Text(name.as_bytes().to_vec()))
    }

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
