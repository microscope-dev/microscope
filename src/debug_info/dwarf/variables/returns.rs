//! ABI-based return-value capture at function exit (`GoRegisters` and `SystemV` `x86_64`).

use std::sync::Arc;

use crate::debug_info::{Located, ReturnedValue, VariableRuntime};
use crate::inspection::InspectionBudget;
use crate::model::ValueStorage;
use crate::{
    BaseTypeEncoding, ImageAddress, RecordMemberLayout, Result, SourceLanguage, TypeId, TypeKind,
    TypeNode, VariableKind, VariableMalformedKind, VariableValueSource, VirtualAddress,
};

use super::DwarfVariableInfo;
use super::evaluate::EvaluateError;
use super::generic::Generic;
use super::inspect::evaluate_error_state;
use super::types::TypeResolution;
use crate::image::variables::Object;
use crate::unwind::DwarfRegister;

const INTEGER: [DwarfRegister; 9] = DwarfRegister::each([0, 3, 2, 5, 4, 8, 9, 10, 11]);

const FLOATING: [DwarfRegister; 15] =
    DwarfRegister::each([17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31]);

const STACK_POINTER: DwarfRegister = crate::unwind::STACK_POINTER;

const WORD: u64 = 8;

const SYSTEM_V_INTEGER: [DwarfRegister; 2] = DwarfRegister::each([0, 1]);

const SYSTEM_V_FLOATING: [DwarfRegister; 2] = DwarfRegister::each([17, 18]);

const ST0: DwarfRegister = DwarfRegister::new(33);

const EIGHTBYTE: u64 = 8;

pub(super) use crate::image::variables::{ReturnConvention, SystemV};

#[derive(Debug, Clone, Copy)]
enum Source {
    Register(DwarfRegister),

    Stack(u64),

    Indirect(DwarfRegister),
}

#[derive(Debug, Clone, Copy)]
struct Part {
    offset: u64,
    size: u64,
    source: Source,
}

enum Unassigned {
    Stack,

    Unsupported,

    Unspecified,

    Malformed(Arc<str>),
}

#[derive(Default)]
struct Assignment {
    integers: usize,
    floats: usize,
    stack: u64,
}

impl DwarfVariableInfo {
    pub(super) fn returned_values(
        &self,
        function: ImageAddress,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<ReturnedValue>>> {
        let Some(catalog) = self.function_at(function) else {
            return Ok(None);
        };

        match catalog.returns() {
            Some(ReturnConvention::GoRegisters) => {}
            Some(ReturnConvention::SystemV(returned)) => {
                return self.system_v_returned(&returned, runtime, budget);
            }
            None => return Ok(None),
        }

        let own = |kind: VariableKind| {
            catalog.objects().filter(move |object| {
                object.kind() == kind && object.instance().is_none() && object.lexical_depth() == 0
            })
        };

        let results = own(VariableKind::Result).collect::<Vec<_>>();

        // Go's register ABI places stack-spilled results immediately after
        // stack-spilled arguments, aligned to a word boundary.
        let mut arguments = Assignment::default();

        for parameter in own(VariableKind::Parameter) {
            let assigned = type_of(parameter)
                .ok_or_else(|| unknown_type("argument", parameter))
                .and_then(|ty| arguments.assign(self, ty, &mut Vec::new()));
            if let Err(error) = assigned {
                return Self::all_missing(&results, &error).map(Some);
            }
        }

        let mut assignment = Assignment {
            stack: arguments.stack.next_multiple_of(WORD),
            ..Assignment::default()
        };

        let mut returned = Vec::with_capacity(results.len());

        for (index, &result) in results.iter().enumerate() {
            let Some(ty) = type_of(result) else {
                let missing = Self::all_missing(
                    results.get(index..).unwrap_or_default(),
                    &unknown_type("result", result),
                )?;
                returned.extend(missing);
                break;
            };

            let mut parts = Vec::new();
            let captured = assignment
                .assign(self, ty, &mut parts)
                .and_then(|()| self.capture(ty, &parts, runtime, budget));
            let value = match captured {
                Ok(located) => Ok(located),
                Err(error) => Err(evaluate_error_state(
                    error,
                    VariableMalformedKind::InvalidTypeGraph,
                )?),
            };

            let unresolved_shape = match self.generic_type(ty, None, None, runtime, budget)? {
                Generic::Unresolved(_, reason) => Some(reason),
                Generic::Plain | Generic::Resolved(_) => None,
            };

            returned.push(ReturnedValue {
                name: result.name().into(),
                ty: Some(ty),
                value,
                unresolved_shape,
            });
        }

        Ok(Some(returned))
    }

    fn all_missing(results: &[Object<'_>], error: &EvaluateError) -> Result<Vec<ReturnedValue>> {
        let state = evaluate_error_state(error.clone(), VariableMalformedKind::InvalidTypeGraph)?;
        Ok(results
            .iter()
            .map(|result| ReturnedValue {
                name: result.name().into(),
                ty: type_of(*result),
                value: Err(state.clone()),
                unresolved_shape: None,
            })
            .collect())
    }

    fn capture(
        &self,
        ty: TypeId,
        parts: &[Part],
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> std::result::Result<Located, EvaluateError> {
        let size = to_usize(self.size(ty).map_err(placed)?);
        let mut raw = vec![0; size];
        let mut stack_pointer = None;
        let mut in_memory = None;

        for part in parts {
            let length = to_usize(part.size);
            let bytes = match part.source {
                Source::Register(register) => runtime.register(register)?.bytes,
                Source::Stack(offset) => {
                    let base = match stack_pointer {
                        Some(base) => base,
                        None => {
                            *stack_pointer.insert(word(&runtime.register(STACK_POINTER)?.bytes))
                        }
                    };

                    let address = VirtualAddress::new(base.wrapping_add(offset));
                    in_memory = Some(address);
                    budget.consume_memory(length)?;
                    runtime.read_memory(address, length)?
                }
                Source::Indirect(register) => {
                    let address = VirtualAddress::new(word(&runtime.register(register)?.bytes));
                    in_memory = Some(address);
                    budget.consume_memory(length)?;
                    runtime.read_memory(address, length)?
                }
            };

            let start = to_usize(part.offset);
            let (Some(target), Some(source)) = (
                raw.get_mut(start..start.saturating_add(length)),
                bytes.get(..length),
            ) else {
                return Err("a returned value's part lies outside it".into());
            };

            target.copy_from_slice(source);
        }

        let source = match (parts, in_memory) {
            ([_], Some(address)) => VariableValueSource::Memory(address),
            _ => VariableValueSource::Composite,
        };

        Ok(Located {
            ty,
            storage: ValueStorage::Bytes {
                source,
                raw: raw.into(),
                start: 0,
                end: size,
                address: None,
            },
        })
    }

    fn underlying(&self, ty: TypeId) -> std::result::Result<&crate::TypeInfo, Unassigned> {
        self.underlying_type(ty).map(|(_, info)| info)
    }

    fn underlying_type(
        &self,
        ty: TypeId,
    ) -> std::result::Result<(TypeId, &crate::TypeInfo), Unassigned> {
        let mut id = ty;

        for _ in 0..64 {
            let Some(TypeNode::Resolved(info)) = self.types.node(id) else {
                return Err(Unassigned::Malformed("a type is malformed".into()));
            };

            match &info.kind {
                TypeKind::Named {
                    target: Some(target),
                    ..
                }
                | TypeKind::Modified { target, .. } => id = target.id,
                _ => return Ok((id, info)),
            }
        }

        Err(Unassigned::Malformed("a type names itself".into()))
    }

    fn size(&self, ty: TypeId) -> std::result::Result<u64, Unassigned> {
        self.underlying(ty)?
            .byte_size
            .ok_or_else(|| Unassigned::Malformed("a type has no size".into()))
    }

    fn alignment(&self, ty: TypeId) -> std::result::Result<u64, Unassigned> {
        let info = self.underlying(ty)?;
        Ok(match &info.kind {
            TypeKind::Base(base) if base.encoding == BaseTypeEncoding::ComplexFloating => {
                base.byte_size / 2
            }
            TypeKind::Base(base) => base.byte_size,
            TypeKind::Enumeration { representation, .. } => representation.byte_size,
            TypeKind::Pointer { .. } | TypeKind::Function | TypeKind::Slice { .. } => WORD,
            TypeKind::Array { element, .. } => self.alignment(element.id)?,
            TypeKind::Record { members, .. } => {
                let mut alignment = 1;
                for member in members.iter() {
                    alignment = alignment.max(self.alignment(member.type_ref.id)?);
                }

                alignment
            }
            _ => return Err(Unassigned::Unsupported),
        }
        .max(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Integer,
    Floating,

    X87,
}

#[derive(Debug, Clone, Copy)]
struct Leaf {
    offset: u64,
    size: u64,
    class: Class,
}

impl DwarfVariableInfo {
    fn system_v_returned(
        &self,
        returned: &SystemV,
        runtime: &mut dyn VariableRuntime,
        budget: &mut InspectionBudget,
    ) -> Result<Option<Vec<ReturnedValue>>> {
        let ty = match &returned.ty {
            TypeResolution::Resolved(ty) => *ty,
            TypeResolution::Malformed(description) => {
                let state = evaluate_error_state(
                    EvaluateError::Malformed(Arc::clone(description)),
                    VariableMalformedKind::InvalidTypeGraph,
                )?;
                return Ok(Some(vec![ReturnedValue {
                    name: Arc::clone(&returned.name),
                    ty: None,
                    value: Err(state),
                    unresolved_shape: None,
                }]));
            }
        };

        if self.underlying(ty).is_ok_and(|info| {
            matches!(info.kind, TypeKind::Unspecified) && info.name.as_ref() == "void"
        }) || self.size(ty).is_ok_and(|size| size == 0)
        {
            return Ok(None);
        }

        let parts = if returned.rewritten {
            Err(Unassigned::Unspecified)
        } else {
            self.system_v_parts(ty, returned.language)
        };

        let captured = parts
            .map_err(placed)
            .and_then(|parts| self.capture(ty, &parts, runtime, budget));
        let value = match captured {
            Ok(located) => Ok(located),
            Err(error) => Err(evaluate_error_state(
                error,
                VariableMalformedKind::InvalidTypeGraph,
            )?),
        };

        Ok(Some(vec![ReturnedValue {
            name: Arc::clone(&returned.name),
            ty: Some(ty),
            value,
            unresolved_shape: None,
        }]))
    }

    fn system_v_parts(
        &self,
        ty: TypeId,
        language: SourceLanguage,
    ) -> std::result::Result<Vec<Part>, Unassigned> {
        let size = self.size(ty)?;
        let (id, info) = self.underlying_type(ty)?;
        let memory = vec![Part {
            offset: 0,
            size,
            source: Source::Indirect(SYSTEM_V_INTEGER[0]),
        }];
        let aggregate = match &info.kind {
            TypeKind::Base(_)
            | TypeKind::Enumeration { .. }
            | TypeKind::Pointer { .. }
            | TypeKind::Reference { .. }
            | TypeKind::Function => false,
            TypeKind::Record { .. } | TypeKind::Variant { .. }
                if language == SourceLanguage::Rust =>
            {
                return self.rust_pair_parts(ty);
            }
            TypeKind::Record { .. } | TypeKind::Union { .. } => match language {
                SourceLanguage::C | SourceLanguage::Nim => true,
                SourceLanguage::Cpp => match self.type_facts().passed_by_value(id) {
                    Some(true) => true,
                    Some(false) => return Ok(memory),
                    None if size > 2 * EIGHTBYTE => return Ok(memory),
                    None => return Err(Unassigned::Unspecified),
                },
                _ => return Err(Unassigned::Unspecified),
            },
            _ => return Err(Unassigned::Unsupported),
        };

        if size > 2 * EIGHTBYTE {
            return if aggregate {
                Ok(memory)
            } else {
                Err(Unassigned::Unsupported)
            };
        }

        let mut leaves = Vec::new();
        self.leaves(ty, 0, &mut leaves)?;

        if let [
            Leaf {
                class: Class::X87,
                offset: 0,
                ..
            },
        ] = leaves[..]
        {
            return Ok(vec![Part {
                offset: 0,
                size: 10,
                source: Source::Register(ST0),
            }]);
        }

        if leaves.iter().any(|leaf| leaf.offset % leaf.size != 0) {
            return Err(Unassigned::Unsupported);
        }

        let (mut integers, mut floats) = (SYSTEM_V_INTEGER.iter(), SYSTEM_V_FLOATING.iter());
        let mut parts = Vec::new();
        let mut start = 0;

        while start < size {
            let end = (start + EIGHTBYTE).min(size);
            let class = leaves
                .iter()
                .filter(|leaf| leaf.offset < end && leaf.offset + leaf.size > start)
                .fold(None, |merged, leaf| {
                    Some(match (merged, leaf.class) {
                        (Some(Class::X87), _) | (_, Class::X87) => Class::X87,
                        (Some(Class::Integer), _) | (_, Class::Integer) => Class::Integer,
                        _ => Class::Floating,
                    })
                });
            let register = match class {
                Some(Class::Integer) => integers.next(),
                Some(Class::Floating) => floats.next(),
                Some(Class::X87) | None => None,
            }
            .ok_or(Unassigned::Unsupported)?;
            parts.push(Part {
                offset: start,
                size: end - start,
                source: Source::Register(*register),
            });
            start = end;
        }

        Ok(parts)
    }

    fn rust_pair_parts(&self, ty: TypeId) -> std::result::Result<Vec<Part>, Unassigned> {
        let mut leaves = Vec::new();
        self.rust_leaves(ty, 0, &mut leaves)?;
        leaves.sort_by_key(|leaf| (leaf.offset, leaf.size));
        leaves.dedup_by(|leaf, kept| {
            leaf.offset == kept.offset && leaf.size == kept.size && leaf.class == kept.class
        });
        if leaves.is_empty()
            || leaves.len() > 2
            || leaves.windows(2).any(
                |pair| matches!(pair, [first, second] if first.offset + first.size > second.offset),
            )
        {
            return Err(Unassigned::Unspecified);
        }

        let (mut integers, mut floats) = (SYSTEM_V_INTEGER.iter(), SYSTEM_V_FLOATING.iter());
        leaves
            .iter()
            .map(|leaf| {
                let register = match leaf.class {
                    Class::Integer => integers.next(),
                    Class::Floating => floats.next(),
                    Class::X87 => None,
                }
                .ok_or(Unassigned::Unspecified)?;
                Ok(Part {
                    offset: leaf.offset,
                    size: leaf.size,
                    source: Source::Register(*register),
                })
            })
            .collect()
    }

    fn rust_leaves(
        &self,
        ty: TypeId,
        offset: u64,
        leaves: &mut Vec<Leaf>,
    ) -> std::result::Result<(), Unassigned> {
        let members = |members: &[crate::RecordMember], leaves: &mut Vec<Leaf>| {
            members.iter().try_for_each(|member| match member.layout {
                RecordMemberLayout::ByteOffset(at) => {
                    self.rust_leaves(member.type_ref.id, offset + at, leaves)
                }
                _ => Err(Unassigned::Unspecified),
            })
        };

        match &self.underlying(ty)?.kind {
            TypeKind::Record {
                members: fields,
                bases,
                incomplete: false,
                ..
            } if bases.is_empty() => members(fields, leaves),
            TypeKind::Variant {
                common_members,
                bases,
                discriminant,
                variants,
                incomplete: false,
                ..
            } if bases.is_empty() => {
                members(common_members, leaves)?;
                if let crate::VariantDiscriminant::Stored(tag) = discriminant.as_ref() {
                    members(std::slice::from_ref(tag), leaves)?;
                }

                variants
                    .iter()
                    .try_for_each(|variant| members(&variant.members, leaves))
            }

            TypeKind::Base(_)
            | TypeKind::Enumeration { .. }
            | TypeKind::Pointer { .. }
            | TypeKind::Reference { .. }
            | TypeKind::Function => self.leaves(ty, offset, leaves),
            _ => Err(Unassigned::Unspecified),
        }
    }

    fn leaves(
        &self,
        ty: TypeId,
        offset: u64,
        leaves: &mut Vec<Leaf>,
    ) -> std::result::Result<(), Unassigned> {
        let mut leaf = |offset, size, class| {
            leaves.push(Leaf {
                offset,
                size,
                class,
            });
        };

        match &self.underlying(ty)?.kind {
            TypeKind::Base(base) => {
                let size = base.byte_size;
                match base.encoding {
                    BaseTypeEncoding::Floating if size <= EIGHTBYTE => {
                        leaf(offset, size, Class::Floating);
                    }
                    BaseTypeEncoding::Floating if base.base_name.contains("long double") => {
                        leaf(offset, size, Class::X87);
                    }
                    BaseTypeEncoding::ComplexFloating if size <= 2 * EIGHTBYTE => {
                        leaf(offset, size / 2, Class::Floating);
                        leaf(offset + size / 2, size / 2, Class::Floating);
                    }
                    BaseTypeEncoding::Floating | BaseTypeEncoding::ComplexFloating => {
                        return Err(Unassigned::Unsupported);
                    }
                    _ if size == 0 => {}
                    _ if size <= EIGHTBYTE => leaf(offset, size, Class::Integer),
                    _ if size == 2 * EIGHTBYTE => {
                        leaf(offset, EIGHTBYTE, Class::Integer);
                        leaf(offset + EIGHTBYTE, EIGHTBYTE, Class::Integer);
                    }
                    _ => return Err(Unassigned::Unsupported),
                }
            }
            TypeKind::Enumeration { representation, .. } => {
                leaf(offset, representation.byte_size, Class::Integer);
            }
            TypeKind::Pointer { .. } | TypeKind::Reference { .. } | TypeKind::Function => {
                leaf(offset, WORD, Class::Integer);
            }

            TypeKind::Array {
                element,
                dimensions,
                ..
            } => {
                let count = dimensions
                    .iter()
                    .try_fold(1_u64, |count, dimension| count.checked_mul(dimension.count))
                    .filter(|&count| count <= 2 * EIGHTBYTE)
                    .ok_or(Unassigned::Unsupported)?;
                let stride = self.size(element.id)?;

                for index in 0..count {
                    self.leaves(element.id, offset + index * stride, leaves)?;
                }
            }

            TypeKind::Record {
                members,
                bases,
                incomplete: false,
                ..
            } => {
                for base in bases.iter() {
                    let RecordMemberLayout::ByteOffset(at) = base.layout else {
                        return Err(Unassigned::Unsupported);
                    };

                    self.leaves(base.type_ref.id, offset + at, leaves)?;
                }

                for member in members.iter() {
                    self.member_leaves(member, offset, leaves)?;
                }
            }

            TypeKind::Union {
                members,
                incomplete: false,
            } => {
                for member in members.iter() {
                    self.member_leaves(member, offset, leaves)?;
                }
            }
            _ => return Err(Unassigned::Unsupported),
        }

        Ok(())
    }

    fn member_leaves(
        &self,
        member: &crate::RecordMember,
        offset: u64,
        leaves: &mut Vec<Leaf>,
    ) -> std::result::Result<(), Unassigned> {
        match member.layout {
            RecordMemberLayout::ByteOffset(at) => {
                self.leaves(member.type_ref.id, offset + at, leaves)
            }

            RecordMemberLayout::BitRange {
                bit_offset,
                bit_size,
            } => {
                if bit_size > 0 {
                    let bytes = bit_offset / 8..=(bit_offset + bit_size - 1) / 8;
                    leaves.extend(bytes.map(|byte| Leaf {
                        offset: offset + byte,
                        size: 1,
                        class: Class::Integer,
                    }));
                }

                Ok(())
            }
            RecordMemberLayout::Runtime => Err(Unassigned::Unsupported),
        }
    }
}

impl Assignment {
    fn assign(
        &mut self,
        info: &DwarfVariableInfo,
        ty: TypeId,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), EvaluateError> {
        let (integers, floats, assigned) = (self.integers, self.floats, parts.len());

        match self.registers(info, ty, 0, parts) {
            Ok(()) => Ok(()),
            Err(Unassigned::Stack) => {
                (self.integers, self.floats) = (integers, floats);
                parts.truncate(assigned);
                let size = info.size(ty).map_err(placed)?;
                let offset = self
                    .stack
                    .next_multiple_of(info.alignment(ty).map_err(placed)?);
                parts.push(Part {
                    offset: 0,
                    size,
                    source: Source::Stack(offset),
                });
                self.stack = offset + size;
                Ok(())
            }
            Err(unassigned) => Err(placed(unassigned)),
        }
    }

    fn registers(
        &mut self,
        info: &DwarfVariableInfo,
        ty: TypeId,
        offset: u64,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), Unassigned> {
        let resolved = info.underlying(ty)?;
        match &resolved.kind {
            TypeKind::Base(base) => match base.encoding {
                BaseTypeEncoding::Floating => self.float(offset, base.byte_size, parts),
                BaseTypeEncoding::ComplexFloating => {
                    let half = base.byte_size / 2;
                    self.float(offset, half, parts)?;
                    self.float(offset + half, half, parts)
                }
                _ => self.integer(offset, base.byte_size, parts),
            },
            TypeKind::Enumeration { representation, .. } => {
                self.integer(offset, representation.byte_size, parts)
            }
            TypeKind::Pointer { .. } | TypeKind::Function => self.integer(offset, WORD, parts),
            TypeKind::Slice { .. } => {
                let words = resolved.byte_size.ok_or(Unassigned::Stack)? / WORD;
                (0..words).try_for_each(|word| self.integer(offset + word * WORD, WORD, parts))
            }

            TypeKind::Array {
                element,
                dimensions,
                ..
            } => match dimensions
                .iter()
                .try_fold(1_u64, |count, dimension| count.checked_mul(dimension.count))
            {
                Some(0) => Ok(()),
                Some(1) => self.registers(info, element.id, offset, parts),
                _ => Err(Unassigned::Stack),
            },
            TypeKind::Record {
                members,
                bases,
                incomplete: false,
                ..
            } if bases.is_empty() => members.iter().try_for_each(|member| {
                let RecordMemberLayout::ByteOffset(at) = member.layout else {
                    return Err(Unassigned::Malformed(
                        "a struct's field has no byte offset".into(),
                    ));
                };

                self.registers(info, member.type_ref.id, offset + at, parts)
            }),
            _ => Err(Unassigned::Unsupported),
        }
    }

    fn integer(
        &mut self,
        offset: u64,
        size: u64,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), Unassigned> {
        if size > WORD {
            return Err(Unassigned::Unsupported);
        }

        let register = *INTEGER.get(self.integers).ok_or(Unassigned::Stack)?;
        self.integers += 1;
        parts.push(Part {
            offset,
            size,
            source: Source::Register(register),
        });
        Ok(())
    }

    fn float(
        &mut self,
        offset: u64,
        size: u64,
        parts: &mut Vec<Part>,
    ) -> std::result::Result<(), Unassigned> {
        if size > WORD {
            return Err(Unassigned::Unsupported);
        }

        let register = *FLOATING.get(self.floats).ok_or(Unassigned::Stack)?;
        self.floats += 1;
        parts.push(Part {
            offset,
            size,
            source: Source::Register(register),
        });
        Ok(())
    }
}

fn placed(unassigned: Unassigned) -> EvaluateError {
    match unassigned {
        Unassigned::Unsupported => crate::UnsupportedVariableFeature::TypeRepresentation.into(),
        Unassigned::Unspecified => crate::UnsupportedVariableFeature::ReturnPlace.into(),
        Unassigned::Malformed(description) => EvaluateError::Malformed(description),
        Unassigned::Stack => "a type has no stack layout".into(),
    }
}

fn type_of(object: Object<'_>) -> Option<TypeId> {
    object.type_id().filter(|_| object.malformed().is_none())
}

fn unknown_type(what: &str, object: Object<'_>) -> EvaluateError {
    format!("the type of {what} {} is unknown", object.name())
        .as_str()
        .into()
}

fn word(bytes: &[u8]) -> u64 {
    let mut word = [0; 8];
    for (target, source) in word.iter_mut().zip(bytes) {
        *target = *source;
    }

    u64::from_le_bytes(word)
}

fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}
