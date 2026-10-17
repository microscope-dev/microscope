//! Expressions at a stop: a frame's names for binding, and its state for
//! running, over the debug-info providers' structural primitives.

use std::cell::{Cell, OnceCell};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use nix::unistd::Pid;

use crate::debug_info::{Located, ObjectKey, PlannedStep, Step};
use crate::eval::Evaluation;
use crate::eval::bind::{Mode, bind};
use crate::eval::error::ErrorKind;
use crate::eval::interp::{Failure, Outcome, run};
use crate::eval::number::Exact;
use crate::eval::syntax::ast::Tag;
use crate::eval::syntax::{Expression, Span};
use crate::eval::target::{
    Key, Loaded, Lookup, Machine, Planned, Refusal, Register, Scope, StepKind, Stop, TextSpan,
    TypeLookup, TypeQuery,
};

use crate::eval::types::{TypeSource, type_info};
use crate::inspection::InspectionBudget;
use crate::model::{DereferenceTarget, ValueStorage};
use crate::protocol::{DebuggerEvent, StopId};
use crate::{
    AddressValue, ByteOrder, CodeInstanceId, DereferenceReference, DereferenceState,
    DereferenceUnavailableReason, Error, ImageAddress, InspectedValue, ModuleId, RecordKind,
    RegisterSnapshot, Result, StackFrameId, TextCompletion, TextSummary, TypeId, TypeInfo,
    TypeKind, TypeReference, ValueChildren, VariableState, VariableUnavailableReason,
    VariableValue, VariableValueSource, VirtualAddress,
};

use super::frames::{FrameRegisters, FrameScope, ResolvedFrame, StackRoot};
use super::inspection::{
    LinuxVariableRuntime, global_context_address, validate_inspection_limits, variable_context,
};
use super::interpreted_values::interpreted_key;

use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_register_snapshot;
use super::{Controller, Inferior, RuntimeModule, validate_image_current};

/// A data object a frame names.
#[derive(Debug, Clone, Copy)]
pub(super) struct StopObject {
    module: ModuleId,
    key: ObjectRef,

    /// Whether it is a local or parameter of the frame.
    local: bool,
}

/// Which data object a frame names.
#[derive(Debug, Clone, Copy)]
enum ObjectRef {
    /// One the debug information describes.
    Data(ObjectKey),

    /// A variable a suspended async frame keeps in its future, or the
    /// future itself: a value of type `ty` at `address`.
    Saved { ty: TypeId, address: VirtualAddress },

    /// What a name means in an interpreted frame, which `runtime`'s
    /// interpreter keeps at `frame`.
    Interpreted {
        runtime: crate::RuntimeId,
        frame: VirtualAddress,
        name: crate::runtime_model::InterpretedName,
    },
}

impl StopObject {
    /// A global of a module.
    pub(super) const fn global(module: ModuleId, key: ObjectKey) -> Self {
        Self {
            module,
            key: ObjectRef::Data(key),
            local: false,
        }
    }

    /// What a name means in the interpreted frame `runtime`, carried by
    /// `module`, keeps at `frame`.
    pub(super) const fn interpreted(
        module: ModuleId,
        runtime: crate::RuntimeId,
        frame: VirtualAddress,
        name: crate::runtime_model::InterpretedName,
    ) -> Self {
        Self {
            module,
            key: ObjectRef::Interpreted {
                runtime,
                frame,
                name,
            },
            local: true,
        }
    }
}

/// A structural step planned from types alone.
#[derive(Clone)]
pub(super) enum StopStep {
    /// A step one module's image planned.
    Provider { module: ModuleId, step: PlannedStep },

    /// To an element of a value a view presents as a sequence.
    Element(Arc<ViewBound>),

    /// To the value for a key of a value a view presents as a map.
    Entry(Arc<ViewBound>),

    /// To the value a view presents a value as, for `*x`.
    Presented(Arc<ViewBound>),

    /// To a global, from anywhere: a view's `global(NAME)`.
    Global(StopObject),

    /// From an interpreter's object to its attribute of this name.
    Attribute(Arc<str>),

    /// From an interpreter's object to the item a key or an index finds.
    Item,
}

/// A view bound against one type, whose steps are a stop's.
pub(super) type ViewBound = crate::view::bind::BoundView<StopStep>;

impl fmt::Debug for StopStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider { module, .. } => write!(formatter, "StopStep({module:?})"),
            Self::Element(bound) => write!(formatter, "StopStep(element of {})", bound.view.header),
            Self::Entry(bound) => write!(formatter, "StopStep(entry of {})", bound.view.header),
            Self::Presented(bound) => {
                write!(formatter, "StopStep(presented by {})", bound.view.header)
            }
            Self::Global(object) => write!(formatter, "StopStep(global {object:?})"),
            Self::Attribute(name) => write!(formatter, "StopStep(attribute {name})"),
            Self::Item => write!(formatter, "StopStep(item)"),
        }
    }
}

/// The step from an interpreter's object to an attribute or an item, which
/// the machine takes by the object's type as it runs.
pub(super) fn plan_interpreted(step: StepKind<'_>) -> std::result::Result<StopStep, Refusal> {
    match step {
        StepKind::Member(name) => Ok(StopStep::Attribute(name.into())),
        StepKind::Entry => Ok(StopStep::Item),
        _ => Err(Refusal::new(
            ErrorKind::Type,
            "an interpreter's object has attributes and items",
        )),
    }
}

/// Where a value of one module's types is.
#[derive(Debug, Clone)]
pub(super) struct StopPlace {
    pub(super) module: ModuleId,
    pub(super) located: Located,
}

/// Where a value an expression reaches is.
#[derive(Debug, Clone)]
pub(super) enum Place {
    /// A value of one module's types.
    Native(StopPlace),

    /// An interpreter's object, which `runtime` keeps at `object`.
    Interpreted {
        runtime: crate::RuntimeId,
        object: VirtualAddress,
    },
}

impl From<StopPlace> for Place {
    fn from(place: StopPlace) -> Self {
        Self::Native(place)
    }
}

impl Place {
    /// The value of a module's types the binder made this place, which no
    /// expression can make an interpreter's object.
    pub(super) fn native(&self) -> std::result::Result<&StopPlace, Stop> {
        match self {
            Self::Native(place) => Ok(place),
            Self::Interpreted { .. } => Err(Stop::Failed(Error::internal(
                "the binder gives an interpreter's object no native operation",
            ))),
        }
    }
}

/// One frame of a stop, as expressions see it.
pub(super) struct Frame<'a, P: InspectionOps> {
    pub(super) controller: &'a Controller<P>,
    inferior: &'a Inferior,
    root: &'a StackRoot,
    pub(super) stop_id: StopId,
    resolved: &'a ResolvedFrame,

    /// The frame's module, address, and inline instance, when it has debug
    /// information.
    code: Option<(&'a RuntimeModule, ImageAddress, Option<CodeInstanceId>)>,
    registers: OnceCell<Option<RegisterSnapshot>>,

    /// Units of work every machine running at the frame has done; the
    /// views presenting values one inside another each have a machine.
    charges: Cell<u32>,

    /// The system call the frame's thread stopped at, which `$arg0` to
    /// `$arg5`, `$ret`, and `$path` read in any of its frames.
    pub(super) call: Option<crate::eval::target::CallValues>,
}

/// What a catchpoint's stop tells the evaluator of its call.
pub(super) fn call_values(reason: &crate::StopReason) -> Option<crate::eval::target::CallValues> {
    let crate::StopReason::Syscall(stop) = reason else {
        return None;
    };

    Some(crate::eval::target::CallValues {
        args: stop.args,
        returned: stop.returned,
        path: stop.path.as_ref().map(|path| path.as_bytes().to_vec()),
    })
}

impl<P: InspectionOps> Controller<P> {
    /// The loaded module whose image defines `ty`.
    pub(super) fn module_of(&self, ty: TypeReference) -> Option<&RuntimeModule> {
        self.modules
            .values()
            .find(|module| module.loaded.image == ty.image)
    }

    pub(super) fn pointer_size(&self) -> u8 {
        self.module_image.target().pointer_width.bytes()
    }
}

impl<'a, P: InspectionOps> Frame<'a, P> {
    pub(super) const fn inferior(&self) -> &'a Inferior {
        self.inferior
    }

    pub(super) const fn root(&self) -> &'a StackRoot {
        self.root
    }

    pub(super) const fn resolved(&self) -> &'a ResolvedFrame {
        self.resolved
    }

    pub(super) fn module(&self, id: ModuleId) -> Option<&'a RuntimeModule> {
        self.controller.modules.get(&id)
    }

    /// Reads the frame's registers and memory for values `module` describes.
    pub(super) fn runtime(&self, module: &'a RuntimeModule) -> LinuxVariableRuntime<'a, P> {
        self.controller
            .frame_runtime(self.inferior, self.root, self.resolved, module)
    }

    /// What the value stored at `value` dynamically holds, when the
    /// runtime the module `module` carries records it in the record named
    /// `representation`.
    pub(super) fn runtime_dynamic(
        &self,
        module: ModuleId,
        representation: &str,
        value: crate::runtime_model::StoredValue<'_>,
    ) -> Option<std::result::Result<crate::runtime_model::DynamicValue, Arc<str>>> {
        let runtime = self
            .controller
            .runtimes(self.inferior)
            .into_iter()
            .find(|runtime| runtime.module.id == module)?;
        self.controller
            .with_runtime_stop(self.inferior, &runtime, self.root.reader(), |stop| {
                runtime.model.dynamic_value(stop, representation, value)
            })
    }

    /// Modules in the order names are looked up: the frame's first.
    fn modules(&self) -> impl Iterator<Item = &'a RuntimeModule> + 'a {
        let first = self.code.map(|(module, ..)| module);
        let id = first.map(|module| module.loaded.id);
        first.into_iter().chain(
            self.controller
                .modules
                .values()
                .filter(move |module| Some(module.loaded.id) != id),
        )
    }

    fn registers(&self) -> Option<&RegisterSnapshot> {
        self.registers
            .get_or_init(|| {
                let discarded = crate::unwind::RegisterFile::new([]);
                let (native, caller) = match &self.resolved.registers {
                    FrameRegisters::Caller(registers) => {
                        let native = self
                            .root
                            .thread()
                            .map(|pid| self.controller.ptrace.registers(pid));
                        (native.transpose().ok()?, Some(registers))
                    }

                    // A tail call discarded every register of the frame.
                    FrameRegisters::Discarded => {
                        let native = self
                            .root
                            .thread()
                            .map(|pid| self.controller.ptrace.registers(pid));
                        (native.transpose().ok()?, Some(&discarded))
                    }
                    FrameRegisters::Thread(native) => (Some(*native), None),
                };

                Some(x86_64_register_snapshot(
                    self.controller.revision,
                    self.root.context,
                    self.controller.module_image.target(),
                    native.as_ref(),
                    caller,
                ))
            })
            .as_ref()
    }
}

impl<P: InspectionOps> Frame<'_, P> {
    /// The variable `name` names in a suspended async frame, which its
    /// future keeps, or the future for `$future`; `None` in any other
    /// frame, or for a name it keeps no variable of.
    fn lookup_saved(&self, name: &str) -> std::result::Result<Option<Lookup<StopObject>>, Refusal> {
        // The future a suspended frame holds, and the type of its own.
        let (object, ty) = match (self.resolved.scope, self.resolved.frame.as_ref()) {
            (FrameScope::Suspended { object, ty, .. }, _) => (object, ty),
            (_, Some(frame)) => match frame.kind {
                crate::FrameKind::Awaited { object, ty } => (object, ty),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };

        let Some(module) = self.controller.module_of(ty) else {
            return Ok(None);
        };

        let saved = |ty: TypeReference, address| Lookup::Object {
            object: StopObject {
                module: module.loaded.id,
                key: ObjectRef::Saved { ty: ty.id, address },
                local: true,
            },
            ty: Ok(ty),
        };

        if name == "$future" {
            return Ok(Some(saved(ty, object)));
        }

        let variables = self
            .controller
            .saved_variables(self.resolved)
            .map_err(|error| refusal(&error))?;
        let mut named = variables.iter().filter(|variable| **variable.name == *name);
        match (named.next(), named.next()) {
            (Some(variable), None) => Ok(Some(saved(variable.ty, variable.address))),
            (Some(_), Some(_)) => Ok(Some(Lookup::Ambiguous(vec![name.to_owned()]))),
            (None, _) => Ok(None),
        }
    }

    /// The global `name` names in exactly one loaded module.
    fn lookup_global(
        &self,
        name: &str,
    ) -> std::result::Result<Option<Lookup<StopObject>>, Refusal> {
        let mut globals = Vec::new();
        let mut candidates = BTreeSet::new();
        for module in self.modules() {
            match module.image.global_named(name) {
                Ok(global) => globals.push((module, global.id)),
                Err(Error::VariableNotFound(_)) => {}
                Err(Error::AmbiguousGlobalVariable {
                    candidates: found, ..
                }) => {
                    for candidate in found {
                        let file = candidate
                            .declaration_path
                            .as_ref()
                            .and_then(|path| path.file_name())
                            .map(|file| file.to_string_lossy().into_owned());
                        candidates.insert(file.map_or_else(
                            || candidate.qualified_name.to_string(),
                            |file| format!("{file}::{}", candidate.qualified_name),
                        ));
                    }
                }
                Err(error) => return Err(refusal(&error)),
            }
        }

        match (globals.as_slice(), candidates.is_empty()) {
            ([(module, id)], true) => {
                let key = module
                    .variables
                    .global_object(*id)
                    .map_err(|error| refusal(&error))?;
                Ok(Some(object(module, key, false)))
            }
            ([], true) => Ok(None),
            _ => {
                for (module, id) in &globals {
                    if let Some(global) = module.image.global(*id) {
                        let file = module
                            .image
                            .path()
                            .file_name()
                            .map(|file| file.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        candidates.insert(format!("{file}::{}", global.qualified_name));
                    }
                }

                Ok(Some(Lookup::Ambiguous(candidates.into_iter().collect())))
            }
        }
    }

    /// The enumerator `name` names, by its own name or qualified by its
    /// enumeration's, in the frame's module before others.
    fn lookup_enumerator(&self, name: &str) -> Option<Lookup<StopObject>> {
        for module in self.modules() {
            let mut found = module
                .image
                .enumerators_named(name)
                .into_iter()
                .map(|(qualified, value, ty)| (qualified, Exact::from(value), ty))
                .collect::<Vec<_>>();
            found.sort_by(|left, right| left.0.cmp(&right.0));
            found.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
            match found.as_slice() {
                [] => {}
                [(_, value, ty)] => {
                    return Some(Lookup::Enumerator {
                        value: *value,
                        ty: *ty,
                    });
                }
                _ => {
                    return Some(Lookup::Ambiguous(
                        found.into_iter().map(|(qualified, ..)| qualified).collect(),
                    ));
                }
            }
        }

        None
    }
}

/// A data object of `module`, as a name lookup finds it.
fn object(module: &RuntimeModule, key: ObjectKey, local: bool) -> Lookup<StopObject> {
    Lookup::Object {
        object: StopObject {
            module: module.loaded.id,
            key: ObjectRef::Data(key),
            local,
        },
        ty: module.variables.object_type(key).map(|id| TypeReference {
            image: module.loaded.image,
            id,
        }),
    }
}

impl<P: InspectionOps> TypeSource for Frame<'_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.controller.module_of(ty)?.image.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.controller.pointer_size()
    }

    fn byte_order(&self) -> ByteOrder {
        self.controller.module_image.target().byte_order
    }

    fn c_base_type(&self, ty: crate::CBaseType) -> Option<crate::BaseType> {
        self.controller.module_image.target().c_base_type(ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left == right
            || self
                .controller
                .module_of(left)
                .is_some_and(|module| module.image.same_type(left, right))
    }
}

/// A shallow description of a type's definition, under which two types of
/// one name are one type.
fn definition(image: &crate::ModuleImage, info: &TypeInfo) -> String {
    let name = |reference: TypeReference| {
        image
            .type_info(reference)
            .map_or_else(|| "?".to_owned(), |info| info.name.to_string())
    };

    let members = |members: &[crate::RecordMember]| {
        members
            .iter()
            .map(|member| {
                format!(
                    "{}:{:?}:{}",
                    member.name.as_deref().unwrap_or_default(),
                    member.layout,
                    name(member.type_ref)
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    };

    let shape = match &info.kind {
        TypeKind::Record {
            members: fields, ..
        }
        | TypeKind::Union {
            members: fields, ..
        } => members(fields),
        TypeKind::Variant {
            common_members,
            discriminant,
            variants,
            ..
        } => {
            let tag = match discriminant.as_ref() {
                crate::VariantDiscriminant::Stored(member) => members(std::slice::from_ref(member)),
                crate::VariantDiscriminant::TagType(tag) => name(*tag),
                crate::VariantDiscriminant::Absent => String::new(),
            };

            let variants = variants
                .iter()
                .map(|variant| format!("{:?}={}", variant.selection, members(&variant.members)))
                .collect::<Vec<_>>()
                .join(";");
            format!("{}|{tag}|{variants}", members(common_members))
        }

        TypeKind::Named {
            target: Some(target),
            ..
        }
        | TypeKind::Modified { target, .. } => name(*target),
        TypeKind::Enumeration { enumerators, .. } => enumerators
            .iter()
            .map(|enumerator| format!("{}={:?}", enumerator.name, enumerator.value))
            .collect::<Vec<_>>()
            .join(","),
        TypeKind::Base(base) => format!("{:?}{:?}", base.encoding, base.bit_size),
        other => format!("{other:?}"),
    };

    format!("{}/{:?}/{shape}", info.name, info.byte_size)
}

const fn tag_matches(tag: Option<Tag>, kind: &TypeKind) -> bool {
    match tag {
        None => !matches!(
            kind,
            TypeKind::Pointer { .. } | TypeKind::Reference { .. } | TypeKind::Array { .. }
        ),
        Some(Tag::Struct | Tag::Class) => {
            matches!(
                kind,
                TypeKind::Record {
                    kind: RecordKind::Struct | RecordKind::Class,
                    ..
                }
            )
        }
        Some(Tag::Union) => matches!(kind, TypeKind::Union { .. }),
        Some(Tag::Enum) => matches!(kind, TypeKind::Enumeration { .. }),
    }
}

/// A provider's refusal of a step, as an expression error.
fn refusal(error: &Error) -> Refusal {
    let kind = match error {
        Error::AmbiguousBase { .. } | Error::AmbiguousMember { .. } => ErrorKind::AmbiguousName,
        Error::MemberNotFound { .. }
        | Error::BaseNotFound { .. }
        | Error::MemberAccessOnNonRecord { .. }
        | Error::IndexAccessOnNonIndexable { .. }
        | Error::IncompleteArrayIndex { .. }
        | Error::InvalidValueExpression(_) => ErrorKind::Type,
        Error::ValueIndexOutOfBounds { .. } => ErrorKind::Bounds,
        _ => ErrorKind::Unsupported,
    };

    Refusal::new(kind, error.to_string())
}

/// The loaded module that defines `ty`.
fn type_module<P: InspectionOps>(
    controller: &Controller<P>,
    ty: TypeReference,
) -> std::result::Result<&RuntimeModule, Refusal> {
    controller
        .module_of(ty)
        .ok_or_else(|| Refusal::new(ErrorKind::Unsupported, "the type's module is not loaded"))
}

/// Plans one step from a value of `from` in the image of its module.
pub(super) fn plan_in<P: InspectionOps>(
    controller: &Controller<P>,
    from: TypeReference,
    step: StepKind<'_>,
) -> std::result::Result<Planned<StopStep>, Refusal> {
    let module = type_module(controller, from)?;
    let image = &module.image;
    let base_name;
    let is_target;
    let step = match step {
        StepKind::Deref => Step::Deref,
        StepKind::Member(name) => Step::Member(name),
        StepKind::Index { available } => Step::Index { available },
        StepKind::Entry => {
            return Err(Refusal::new(
                ErrorKind::Type,
                "only a value a view presents as a map is indexed by key",
            ));
        }
        StepKind::Base(target) => {
            if target.image != from.image {
                return Err(Refusal::new(
                    ErrorKind::Type,
                    "a base class is in the same module as the class",
                ));
            }

            base_name = image
                .type_info(target)
                .map_or_else(|| Arc::from("?"), |info| Arc::clone(&info.name));
            is_target = move |id| {
                image.same_type(
                    TypeReference {
                        image: target.image,
                        id,
                    },
                    target,
                )
            };

            Step::Base(crate::debug_info::BaseTarget {
                name: &base_name,
                is_target: &is_target,
            })
        }
    };

    let planned = module
        .variables
        .plan_step(from.id, step)
        .map_err(|error| refusal(&error))?;
    Ok(Planned {
        result: planned.result().map(|id| TypeReference {
            image: module.loaded.image,
            id,
        }),
        consumed: planned.consumed(),
        step: StopStep::Provider {
            module: module.loaded.id,
            step: planned,
        },
    })
}

/// The types a name means in one module, through its image's identity
/// index: a name may omit outer path segments and trailing arguments. One
/// type defined alike in several units is one type, and so is a synonym
/// that only renames another candidate, as Go's typedefs of its named
/// types do.
pub(super) fn lookup_type_in(module: &RuntimeModule, query: &TypeQuery) -> TypeLookup {
    let candidates = module
        .image
        .types_named(&query.name)
        .into_iter()
        .filter_map(|reference| module.image.type_info(reference))
        .filter(|info| tag_matches(query.tag, &info.kind))
        .collect::<Vec<_>>();
    let mut found: Vec<(String, TypeReference)> = candidates
        .iter()
        .filter(|info| {
            !matches!(
                info.kind,
                TypeKind::Named { target: Some(target), .. }
                    if candidates.iter().any(|other| {
                        other.reference == target && other.name == info.name
                    })
            )
        })
        .map(|info| (definition(&module.image, info), info.reference))
        .collect();
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found.dedup_by(|left, right| left.0 == right.0);
    match found.as_slice() {
        [] => TypeLookup::NotFound,
        [(_, ty)] => TypeLookup::Found(*ty),
        _ => TypeLookup::Ambiguous(
            found
                .into_iter()
                .map(|(definition, _)| definition)
                .collect(),
        ),
    }
}

impl<P: InspectionOps> Scope for Frame<'_, P> {
    type Object = StopObject;
    type Step = StopStep;

    fn lookup(
        &self,
        name: &str,
        outermost: bool,
    ) -> std::result::Result<Lookup<StopObject>, Refusal> {
        if !outermost && let Some(found) = self.lookup_saved(name)? {
            return Ok(found);
        }

        if !outermost && let Some((module, address, selected)) = self.code {
            match module.variables.visible_object(address, selected, name) {
                Ok(key) => return Ok(object(module, key, true)),
                Err(Error::VariableNotFound(_)) => {}
                Err(Error::AmbiguousVariable(_)) => {
                    return Ok(Lookup::Ambiguous(vec![name.to_owned()]));
                }
                Err(error) => return Err(refusal(&error)),
            }
        }

        // An interpreted frame's names are its interpreter's: its variables,
        // then the globals and built-ins its code reads.
        if !outermost && let Some(found) = self.lookup_interpreted(name)? {
            return Ok(found);
        }

        if let Some(global) = self.lookup_global(name)? {
            return Ok(global);
        }

        if let Some(found) = self.lookup_enumerator(name) {
            return Ok(found);
        }

        Ok(Lookup::NotFound)
    }

    /// The types a name means, in the frame's module first.
    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        for module in self.modules() {
            match lookup_type_in(module, query) {
                TypeLookup::NotFound => {}
                found => return found,
            }
        }

        TypeLookup::NotFound
    }

    fn has_view(&self, ty: TypeReference) -> bool {
        self.controller.views.enabled
            && self
                .controller
                .view_choice(ty)
                .bound
                .as_ref()
                .is_some_and(|bound| bound.shape.has_elements() || bound.shape.has_text())
    }

    fn stands_for_container(&self, ty: TypeReference) -> bool {
        crate::view::stands_for_container(self, ty)
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> std::result::Result<Planned<StopStep>, Refusal> {
        let planned = plan_in(self.controller, from, step);

        // A value with no indexing of its own is indexed through the view
        // that presents it as a sequence.
        match (&planned, step) {
            (Err(_), StepKind::Index { .. }) => {
                self.controller.view_index(from).map_or(planned, Ok)
            }
            (Err(_), StepKind::Entry) => self.controller.view_entry(from).map_or(planned, Ok),
            // A value that is no pointer dereferences to the value the
            // view that presents it presents it as.
            (Err(_) | Ok(Planned { result: None, .. }), StepKind::Deref) => {
                self.controller.view_deref(from).map_or(planned, Ok)
            }
            _ => planned,
        }
    }

    fn call_values(&self) -> Option<&crate::eval::target::CallValues> {
        self.call.as_ref()
    }

    fn dynamic_type(&self, name: &str) -> bool {
        self.interpreter_cast(name).is_some()
    }

    fn plan_dynamic(&self, step: StepKind<'_>) -> std::result::Result<StopStep, Refusal> {
        plan_interpreted(step)
    }

    fn register(&self, name: &str) -> Option<Register> {
        let name = match name {
            "pc" => "rip",
            "sp" => "rsp",
            "fp" => "rbp",
            other => other,
        };

        let registers = self.registers()?;
        let number = registers
            .registers
            .iter()
            .position(|value| value.register.name.as_ref() == name)?;
        let width = u8::try_from(registers.registers[number].register.bits.min(128)).ok()?;
        Some(Register {
            number: u16::try_from(number).ok()?,
            width,
        })
    }
}

/// A frame's state at one stop, which a bound program runs against.
pub(super) struct StopMachine<'a, 'b, P: InspectionOps> {
    pub(super) frame: &'b Frame<'a, P>,
    pub(super) budget: &'b mut InspectionBudget,

    /// How many views are presenting the values this machine presents, one
    /// inside another.
    pub(super) depth: u8,

    /// Whether a run-control request waiting ends the work, which then
    /// runs again after it; false for work run control itself does.
    pub(super) interruptible: bool,

    /// Whether values present as the types they dynamically are; false for
    /// a base-class subobject, which is part of an object, not one.
    pub(super) dynamic: bool,
}

/// How many units of work a frame's machines do between checks for waiting
/// run control.
pub(super) const INTERRUPT_INTERVAL: u32 = 64;

impl<'a, 'b, P: InspectionOps> StopMachine<'a, 'b, P> {
    pub(super) const fn new(
        frame: &'b Frame<'a, P>,
        budget: &'b mut InspectionBudget,
        interruptible: bool,
    ) -> Self {
        Self {
            frame,
            budget,
            depth: 0,
            interruptible,
            dynamic: true,
        }
    }

    /// A machine on this one's budget, presenting values `depth` views deep.
    pub(super) const fn nested(&mut self, depth: u8) -> StopMachine<'a, '_, P> {
        let mut machine = StopMachine::new(self.frame, self.budget, self.interruptible);
        machine.depth = depth;
        machine
    }

    pub(super) fn module(&self, id: ModuleId) -> std::result::Result<&'a RuntimeModule, Stop> {
        self.frame
            .module(id)
            .ok_or_else(|| Stop::Failed(Error::ModuleNotLoaded(id)))
    }

    /// The instruction context values of `module` are evaluated at.
    fn address(&self, module: &RuntimeModule) -> Option<ImageAddress> {
        global_context_address(self.frame.resolved, module)
    }

    pub(super) fn context(&self, module: &RuntimeModule) -> crate::debug_info::VariableContext {
        variable_context(
            self.frame.stop_id,
            self.frame.root.context,
            self.frame.resolved.id,
            module,
            self.address(module),
        )
    }

    /// A value as it is stored, without a view.
    pub(super) fn materialize(
        &mut self,
        module: &'a RuntimeModule,
        located: &Located,
    ) -> std::result::Result<InspectedValue, Stop> {
        let context = self.context(module);
        let mut runtime = self.frame.runtime(module);
        module
            .variables
            .materialize(located, context, &mut runtime, self.budget)
            .map_err(Stop::Failed)
    }

    fn accessed(
        accessed: crate::debug_info::Accessed,
        module: ModuleId,
    ) -> std::result::Result<StopPlace, Stop> {
        accessed
            .map(|located| StopPlace { module, located })
            .map_err(Stop::missing)
    }

    /// Any module's runtime, for memory the expression addresses directly.
    fn memory_module(&self) -> std::result::Result<&'a RuntimeModule, Stop> {
        self.frame.modules().next().ok_or_else(|| {
            Stop::Refused(Refusal::new(ErrorKind::Unsupported, "no module is loaded"))
        })
    }
}

impl<P: InspectionOps> TypeSource for StopMachine<'_, '_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.frame.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.frame.pointer_size()
    }

    fn byte_order(&self) -> ByteOrder {
        self.frame.byte_order()
    }

    fn c_base_type(&self, ty: crate::CBaseType) -> Option<crate::BaseType> {
        self.frame.c_base_type(ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        self.frame.same_type(left, right)
    }
}

impl<P: InspectionOps> Machine for StopMachine<'_, '_, P> {
    type Object = StopObject;
    type Step = StopStep;
    type Place = Place;

    fn charge(&mut self) -> std::result::Result<(), Stop> {
        self.budget
            .consume_expression_work(1)
            .map_err(|exhaustion| Stop::missing(VariableState::Unavailable(exhaustion.into())))?;
        let charges = self.frame.charges.get().wrapping_add(1);
        self.frame.charges.set(charges);
        if self.interruptible
            && charges.is_multiple_of(INTERRUPT_INTERVAL)
            && self.frame.controller.run_control_waiting()
        {
            return Err(Stop::Failed(Error::Interrupted));
        }

        Ok(())
    }

    fn locate(&mut self, object: &StopObject) -> std::result::Result<Place, Stop> {
        let module = self.module(object.module)?;
        let address = self.address(module);
        let key = match object.key {
            ObjectRef::Data(key) => key,
            ObjectRef::Saved { ty, address } => {
                return Ok(Place::Native(StopPlace {
                    module: object.module,
                    located: Located {
                        ty,
                        storage: ValueStorage::Memory(address),
                    },
                }));
            }
            ObjectRef::Interpreted {
                runtime,
                frame,
                name,
            } => return self.interpreted_name(runtime, frame, name),
        };

        let mut runtime = self.frame.runtime(module);
        let accessed = module
            .variables
            .locate(key, address, &mut runtime, self.budget)
            .map_err(Stop::Failed)?;
        Self::accessed(accessed, object.module).map(Place::Native)
    }

    fn check_indices(&self, step: &StopStep, indices: &[i128]) -> std::result::Result<(), Stop> {
        match step {
            StopStep::Provider { step, .. } => step
                .check_indices(indices)
                .map_err(|error| Stop::Refused(refusal(&error))),
            StopStep::Element(_)
            | StopStep::Entry(_)
            | StopStep::Presented(_)
            | StopStep::Global(_)
            | StopStep::Attribute(_)
            | StopStep::Item => Ok(()),
        }
    }

    fn step(
        &mut self,
        from: &Place,
        step: &StopStep,
        indices: &[i128],
    ) -> std::result::Result<Place, Stop> {
        if let StopStep::Attribute(name) = step {
            return self
                .interpreted_step(from, crate::runtime_model::InterpretedStep::Attribute(name));
        }

        let from = from.native()?;
        let (module_id, step) = match step {
            StopStep::Provider { module, step } => (*module, step),
            StopStep::Element(bound) => {
                let [index] = indices else {
                    return Err(Stop::Refused(Refusal::new(
                        ErrorKind::Type,
                        "a view's elements take one index",
                    )));
                };

                return self.view_element(bound, from, *index);
            }
            StopStep::Global(object) => return self.locate(object),
            StopStep::Presented(bound) => return self.view_presented(bound, from),
            StopStep::Entry(_) | StopStep::Item => {
                return Err(Stop::Refused(Refusal::new(
                    ErrorKind::Type,
                    "a map's entries are found by key",
                )));
            }
            StopStep::Attribute(_) => {
                return Err(Stop::Failed(Error::internal(
                    "attributes are stepped above",
                )));
            }
        };

        let module = self.module(module_id)?;
        let address = self.address(module);
        let mut runtime = self.frame.runtime(module);
        let accessed = module
            .variables
            .apply(
                &from.located,
                step,
                indices,
                address,
                &mut runtime,
                self.budget,
            )
            .map_err(|error| match error {
                Error::ValueIndexOutOfBounds { .. } => Stop::Refused(refusal(&error)),
                error => Stop::Failed(error),
            })?;
        Self::accessed(accessed, module_id).map(Place::Native)
    }

    fn place_at(&mut self, address: u64, ty: TypeReference) -> std::result::Result<Place, Stop> {
        let module = type_module(self.frame.controller, ty).map_err(Stop::Refused)?;
        Ok(Place::Native(StopPlace {
            module: module.loaded.id,
            located: Located {
                ty: ty.id,
                storage: ValueStorage::Memory(VirtualAddress::new(address)),
            },
        }))
    }

    fn address(&self, at: &Place) -> std::result::Result<u64, Stop> {
        let at = match at {
            Place::Native(at) => at,
            Place::Interpreted { object, .. } => return Ok(object.get()),
        };

        let not_in_memory = |where_: &str| {
            Stop::Refused(Refusal::new(
                ErrorKind::NotAnLvalue,
                format!("the value is {where_}, which has no address"),
            ))
        };

        match &at.located.storage {
            ValueStorage::Memory(address)
            | ValueStorage::Bytes {
                address: Some(address),
                ..
            } => Ok(address.get()),
            ValueStorage::Bytes {
                source: VariableValueSource::Register(register),
                ..
            } => Err(not_in_memory(&format!("in register {}", register.name))),
            ValueStorage::Bytes {
                source: VariableValueSource::Constant,
                ..
            } => Err(not_in_memory("a constant")),
            // Including a returned value gathered from several registers.
            ValueStorage::Bytes {
                source: VariableValueSource::Composite,
                ..
            }
            | ValueStorage::Composite(_) => Err(not_in_memory("split across several places")),
            ValueStorage::Bytes { .. } => Err(not_in_memory("computed")),
            ValueStorage::ImplicitPointer { .. } => {
                Err(not_in_memory("optimized into its referent"))
            }
        }
    }

    fn load(&mut self, at: &Place) -> std::result::Result<VariableValue, Stop> {
        let at = at.native()?;
        let module = self.module(at.module)?;
        let context = self.context(module);
        let mut runtime = self.frame.runtime(module);
        module
            .variables
            .load(&at.located, context, &mut runtime, self.budget)
            .map_err(Stop::Failed)?
            .map_err(Stop::missing)
    }

    fn read(&mut self, address: u64, size: usize) -> std::result::Result<Vec<u8>, Stop> {
        use crate::debug_info::VariableRuntime as _;
        self.budget
            .consume_memory(size)
            .map_err(|exhaustion| Stop::missing(VariableState::Unavailable(exhaustion.into())))?;
        let module = self.memory_module()?;
        let mut runtime = self.frame.runtime(module);
        runtime
            .read_memory(VirtualAddress::new(address), size)
            .map(|bytes| bytes.to_vec())
            .map_err(|error| match error {
                crate::debug_info::VariableRuntimeError::Unavailable(reason) => {
                    Stop::missing(VariableState::Unavailable(reason))
                }

                crate::debug_info::VariableRuntimeError::Malformed(description)
                | crate::debug_info::VariableRuntimeError::Fatal(description) => {
                    Stop::Failed(Error::VariableRuntime(description))
                }
            })
    }

    fn text(&mut self, at: &Place) -> std::result::Result<Option<TextSummary>, Stop> {
        let value = self.present(at)?;
        match value.state {
            VariableState::Available { text, .. } => Ok(text.map(|text| (*text).clone())),
            state => Err(Stop::missing(state)),
        }
    }

    fn length(&mut self, at: &Place) -> std::result::Result<u64, Stop> {
        match self.load(at)? {
            VariableValue::Slice { length, .. } => Ok(length),
            VariableValue::Array { dimensions } => {
                Ok(dimensions.first().map_or(0, |dimension| dimension.count))
            }
            _ => Err(Stop::Refused(Refusal::new(
                ErrorKind::Type,
                "the value is not a slice",
            ))),
        }
    }

    fn presented_length(&mut self, at: &Place) -> std::result::Result<Option<u64>, Stop> {
        match at {
            Place::Native(at) => self.view_length(at),
            Place::Interpreted { runtime, object } => {
                self.interpreted_length(*runtime, *object).map(Some)
            }
        }
    }

    fn capacity(&mut self, at: &Place) -> std::result::Result<u64, Stop> {
        match self.load(at)? {
            VariableValue::Slice {
                capacity: Some(capacity),
                ..
            } => Ok(capacity),
            _ => Err(Stop::Refused(Refusal::new(
                ErrorKind::Type,
                "the value is not a slice with a capacity",
            ))),
        }
    }

    fn presented_capacity(&mut self, at: &Place) -> std::result::Result<Option<u64>, Stop> {
        self.view_capacity(at.native()?)
    }

    fn presented_element(
        &mut self,
        at: &Place,
        index: i128,
    ) -> std::result::Result<Option<Place>, Stop> {
        let at = at.native()?;
        let Some(bound) = self.view_of(at)? else {
            return Ok(None);
        };

        self.view_element(&bound, at, index).map(Some)
    }

    fn text_span(&mut self, at: &Place) -> std::result::Result<Option<TextSpan>, Stop> {
        let place = at;
        let at = at.native()?;

        // A view that presents the value as text says where it is, and
        // otherwise the debug information does.
        let found = if let Some(found) = self.view_text_span(at)? {
            Some(found)
        } else {
            let module = self.module(at.module)?;
            let context = self.context(module);
            let mut runtime = self.frame.runtime(module);
            module
                .variables
                .text_span(&at.located, context, &mut runtime, self.budget)
                .map_err(Stop::Failed)?
                .map_err(Stop::missing)?
                .map(|location| (location.address.get(), location.length))
        };

        let Some((address, length)) = found else {
            return Ok(None);
        };

        // Text that ends at a NUL is as long as reading it finds.
        let length = match length {
            Some(length) => length,
            None => match self.text(place)? {
                Some(TextSummary {
                    bytes,
                    completion: TextCompletion::Complete,
                }) => bytes.len() as u64,
                Some(TextSummary { completion, .. }) => {
                    return Err(Stop::missing(VariableState::Unavailable(
                        match completion {
                            TextCompletion::Unreadable { address } => {
                                VariableUnavailableReason::MemoryInaccessible {
                                    address,
                                    requested: 1,
                                    completed: 0,
                                    next_address: address,
                                }
                            }
                            TextCompletion::Limited { exhaustion, .. } => {
                                VariableUnavailableReason::InspectionLimit(exhaustion)
                            }
                            _ => VariableUnavailableReason::EvaluationLimit,
                        },
                    )));
                }
                None => return Ok(None),
            },
        };

        Ok(Some(TextSpan { address, length }))
    }

    fn entry(
        &mut self,
        from: &Place,
        step: &StopStep,
        key: &Key,
    ) -> std::result::Result<Option<Place>, Stop> {
        if matches!(step, StopStep::Item) {
            let key = interpreted_key(key)?;
            return self
                .interpreted_step(from, crate::runtime_model::InterpretedStep::Item(&key))
                .map(Some);
        }

        let StopStep::Entry(bound) = step else {
            return Err(Stop::Refused(Refusal::new(
                ErrorKind::Type,
                "only a value a view presents as a map is indexed by key",
            )));
        };

        self.view_entry_place(bound, from.native()?, key)
    }

    fn dynamic_at(&mut self, ty: &str, address: u64) -> std::result::Result<Place, Stop> {
        let runtime = self.frame.interpreter_cast(ty).ok_or_else(|| {
            Stop::Failed(Error::internal(
                "the binder casts only to an interpreter's dynamic type",
            ))
        })?;
        Ok(Place::Interpreted {
            runtime,
            object: VirtualAddress::new(address),
        })
    }

    fn load_dynamic(&mut self, at: &Place) -> std::result::Result<Loaded, Stop> {
        let Place::Interpreted { runtime, object } = at else {
            return Err(Stop::Failed(Error::internal(
                "only an interpreter's object has a dynamic type",
            )));
        };

        self.interpreted_operand(*runtime, *object)
    }

    fn function_generics(
        &mut self,
        address: u64,
    ) -> std::result::Result<Vec<(Arc<str>, TypeReference)>, Stop> {
        let described = self.frame.modules().find_map(|module| {
            let image_address = module
                .loaded
                .image_address(crate::VirtualAddress::new(address))
                .ok()
                .filter(|image_address| module.image.contains_address(*image_address))?;
            let instance = module.image.locate(image_address).physical_instance?;
            let function = module
                .image
                .function(module.image.code_instance(instance)?.function())?;
            let generics = function
                .generics()
                .map(|(name, argument)| (Arc::from(name), argument))
                .collect::<crate::FunctionGenerics>();
            Some((module.image.id(), generics))
        });
        let Some((image, generics)) = described else {
            return Err(Stop::Refused(Refusal::new(
                ErrorKind::Unsupported,
                format!("no function the debug information describes has its code at {address:#x}"),
            )));
        };

        Ok(generics
            .iter()
            .map(|(name, id)| (Arc::clone(name), TypeReference { image, id: *id }))
            .collect())
    }

    fn task(&mut self) -> std::result::Result<u64, Stop> {
        let frame = self.frame;
        let refused = |reason: &str| Stop::Refused(Refusal::new(ErrorKind::Unsupported, reason));
        if let crate::ExecutionContext::Task(task) = frame.root.context {
            return Ok(task.number);
        }

        // A condition at a hit, before the other threads stop, reads the
        // task from the thread's own state alone, which they cannot change.
        if frame.inferior.public_stop.is_none() {
            return match frame
                .controller
                .current_task(frame.inferior, frame.root.reader())
            {
                Some(Ok(Some(task))) => Ok(task.number),
                Some(Ok(None)) => Err(Stop::missing(VariableState::Unavailable(
                    VariableUnavailableReason::NoTask,
                ))),
                Some(Err(reason)) => Err(refused(&reason)),
                None => Err(refused("the program has no tasks")),
            };
        }

        match frame
            .controller
            .thread_activity(frame.inferior, frame.root.reader())
        {
            Some(crate::ThreadActivity::Task { task, .. }) => Ok(task.number),
            Some(crate::ThreadActivity::Idle | crate::ThreadActivity::Outside) => {
                Err(Stop::missing(VariableState::Unavailable(
                    VariableUnavailableReason::NoTask,
                )))
            }
            Some(crate::ThreadActivity::Unknown(reason)) => Err(refused(&reason)),
            None => Err(refused("the program has no tasks")),
        }
    }

    fn register(&mut self, register: &Register) -> std::result::Result<u128, Stop> {
        let snapshot = self.frame.registers().ok_or_else(|| {
            Stop::missing(VariableState::Unavailable(
                VariableUnavailableReason::RegisterUnavailable("registers".into()),
            ))
        })?;
        let value = &snapshot.registers[usize::from(register.number)];
        let Some(bytes) = &value.bytes else {
            let reason = if matches!(self.frame.resolved.registers, FrameRegisters::Discarded) {
                VariableUnavailableReason::CallFrameUnavailable(
                    crate::CallFrameUnavailableReason::TailCall,
                )
            } else {
                VariableUnavailableReason::RegisterNotSaved(Arc::clone(&value.register.name))
            };

            return Err(Stop::missing(VariableState::Unavailable(reason)));
        };

        let mut wide = [0_u8; 16];
        let length = bytes.len().min(16);
        wide[..length].copy_from_slice(&bytes[..length]);
        Ok(match self.frame.byte_order() {
            ByteOrder::Little => u128::from_le_bytes(wide),
            ByteOrder::Big => u128::from_be_bytes(wide) >> (8 * (16 - length)),
        })
    }

    fn present(&mut self, at: &Place) -> std::result::Result<InspectedValue, Stop> {
        let at = match at {
            Place::Native(at) => at,
            Place::Interpreted { runtime, object } => {
                return self.interpreted_present(*runtime, *object);
            }
        };

        let module = self.module(at.module)?;
        let value = self.materialize(module, &at.located)?;
        self.presented(value)
    }

    fn present_stored(&mut self, at: &Place) -> std::result::Result<InspectedValue, Stop> {
        match at {
            Place::Native(_) => self.present(at),
            Place::Interpreted { runtime, object } => {
                self.interpreted_present_stored(*runtime, *object)
            }
        }
    }

    fn present_bytes(
        &mut self,
        ty: TypeReference,
        bytes: &[u8],
    ) -> std::result::Result<InspectedValue, Stop> {
        let module = type_module(self.frame.controller, ty).map_err(Stop::Refused)?;
        let located = Located {
            ty: ty.id,
            storage: ValueStorage::Bytes {
                source: VariableValueSource::Computed,
                raw: Arc::from(bytes),
                start: 0,
                end: bytes.len(),
                address: None,
            },
        };

        let value = self.materialize(module, &located)?;
        self.presented(value)
    }

    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> std::result::Result<InspectedValue, Stop> {
        let size = usize::from(self.pointer_size());
        let mut raw = address.to_le_bytes()[..size].to_vec();
        if self.byte_order() == ByteOrder::Big {
            raw.reverse();
        }

        let dereference = match (pointee, address) {
            (None, _) => DereferenceState::Unavailable {
                pointee: None,
                reason: DereferenceUnavailableReason::UnspecifiedPointee,
            },
            (Some(pointee), 0) => DereferenceState::Unavailable {
                pointee: self.type_info(pointee).cloned().map(Box::new),
                reason: DereferenceUnavailableReason::Null,
            },
            (Some(pointee), address) => match self.frame.controller.module_of(pointee) {
                Some(module) => DereferenceState::Available(Box::new(DereferenceReference {
                    stop_id: self.frame.stop_id,
                    context: self.frame.root.context,
                    frame: self.frame.resolved.id,
                    module: module.loaded.id,
                    image: module.loaded.image,
                    context_address: self.address(module),
                    target_type: pointee.id,
                    target: DereferenceTarget::Address(VirtualAddress::new(address)),
                })),
                None => DereferenceState::Unavailable {
                    pointee: None,
                    reason: DereferenceUnavailableReason::UnspecifiedPointee,
                },
            },
        };

        Ok(self.finish(
            Some(type_info),
            VariableState::Available {
                source: VariableValueSource::Computed,
                raw: Some(raw.into()),
                value: VariableValue::Address(AddressValue {
                    address: VirtualAddress::new(address),
                    function: None,
                }),
                dereference,
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
            completion: self.budget.completion(),
            usage: self.budget.usage(),
        }
    }
}

/// What evaluating an expression asks of the controller.
enum Evaluated {
    Done(Box<Evaluation>),

    /// Store `bytes` in `target`, then read it again.
    Write {
        target: StopPlace,
        bytes: Vec<u8>,
        whole: bool,
        span: Span,
    },

    /// Store `bytes` in a register, then read it again.
    WriteRegister {
        register: Register,
        bytes: Vec<u8>,
        span: Span,
    },
}

/// An interpreter failure as the debugger's error.
fn failure(failure: Failure) -> Error {
    match failure {
        Failure::Expression(error) => Error::Expression(error),
        Failure::Debugger(error) => error,
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Evaluates an expression that may assign, in one frame of a validated
    /// stop, and makes its assignment. Assigning the program counter moves
    /// the thread, whose stop is then published again: the event is
    /// returned, to follow the evaluation's reply.
    pub(super) fn evaluate_assigning(
        &mut self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        expression: &Expression,
        limits: crate::InspectionLimits,
    ) -> Result<(Evaluation, Option<DebuggerEvent>)> {
        let (target, bytes, whole, span) =
            match self.run_expression(stop_id, root, frame, expression, Mode::Assign, limits)? {
                Evaluated::Done(evaluation) => return Ok((*evaluation, None)),
                Evaluated::Write {
                    target,
                    bytes,
                    whole,
                    span,
                } => (target, bytes, whole, span),
                Evaluated::WriteRegister {
                    register,
                    bytes,
                    span,
                } => {
                    return self.assign_register(
                        stop_id, root, frame, expression, limits, &register, &bytes, span,
                    );
                }
            };
        let refused = |reason: String| {
            Error::Expression(crate::ExpressionError::new(
                ErrorKind::Assignment,
                span,
                reason,
            ))
        };

        let thread_registers = self.shares_thread_registers(root, frame)?;
        match &target.located.storage {
            ValueStorage::Memory(address)
            | ValueStorage::Bytes {
                address: Some(address),
                ..
            } => {
                let written = self.write_memory_as(root.reader(), *address, &bytes)?;
                if written != bytes.len() as u64 {
                    return Err(Error::MemoryNotWritable(*address));
                }
            }

            ValueStorage::Bytes {
                source: VariableValueSource::Register(register),
                ..
            } => {
                // A register belongs to the innermost activation; a caller's
                // copy lives in memory its callees saved, and part of a
                // register cannot be told from the whole.
                let Some(pid) = root.thread() else {
                    return Err(refused(
                        "it is held in a register the task's runtime saved".into(),
                    ));
                };

                if !thread_registers {
                    return Err(refused(
                        "it is held in a register of a caller's frame".into(),
                    ));
                }

                if !whole {
                    return Err(refused("it is part of a value held in a register".into()));
                }

                self.write_register(pid, register.id, &bytes).map_err(|_| {
                    refused(format!("register {} cannot be changed", register.name))
                })?;
            }
            ValueStorage::Composite(_) => {
                return Err(refused("it is split across several places".into()));
            }
            _ => {
                return Err(refused(
                    "the debug information computes it; it has no storage".into(),
                ));
            }
        }

        self.read_assigned(stop_id, root, frame, expression, limits)
            .map(|evaluation| (evaluation, None))
    }

    /// Stores an assignment's bytes in a register of a frame that shares
    /// the thread's own, and moves the thread when the register is its
    /// program counter.
    #[expect(clippy::too_many_arguments, reason = "one assignment's parts")]
    fn assign_register(
        &mut self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        expression: &Expression,
        limits: crate::InspectionLimits,
        register: &Register,
        bytes: &[u8],
        span: Span,
    ) -> Result<(Evaluation, Option<DebuggerEvent>)> {
        let refused = |reason: String| {
            Error::Expression(crate::ExpressionError::new(
                ErrorKind::Assignment,
                span,
                reason,
            ))
        };

        let Some(pid) = root.thread() else {
            return Err(refused(
                "a parked task's registers are what its runtime saved; only a thread's can be \
                 changed"
                    .into(),
            ));
        };

        if !self.shares_thread_registers(root, frame)? {
            return Err(refused(
                "a caller's registers are what unwinding recovered; only the innermost frame's \
                 belong to the thread"
                    .into(),
            ));
        }

        let snapshot = self.registers(stop_id, root, frame)?;
        let descriptor = snapshot
            .registers
            .get(usize::from(register.number))
            .map(|value| value.register.clone())
            .ok_or_else(|| refused("the register is unknown".into()))?;
        self.write_register(pid, descriptor.id, bytes)
            .map_err(|_| refused(format!("register {} cannot be changed", descriptor.name)))?;
        let evaluation = self.read_assigned(stop_id, root, frame, expression, limits)?;
        if descriptor.role != Some(crate::RegisterRole::ProgramCounter) {
            return Ok((evaluation, None));
        }

        let (_, moved) = self.publish_moved_thread(pid)?;
        Ok((evaluation, Some(moved)))
    }

    /// Whether a frame's registers are the thread's own, as the innermost
    /// activation's frames, inlined ones too, share them.
    fn shares_thread_registers(&self, root: &StackRoot, frame: StackFrameId) -> Result<bool> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let resolved = self.resolve_frame(inferior, root, frame)?;
        Ok(matches!(resolved.registers, FrameRegisters::Thread(_)))
    }

    /// An assignment's value: its target read again, so what the target's
    /// own type makes of the stored bytes shows. It is read in the
    /// assignment's own mode, which run control waiting cannot interrupt:
    /// the write is made, and serving the request again would make it
    /// twice.
    fn read_assigned(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        expression: &Expression,
        limits: crate::InspectionLimits,
    ) -> Result<Evaluation> {
        let target = Expression::parse(
            expression
                .assignment_target()
                .unwrap_or_else(|| expression.text()),
        )
        .map_err(Error::Expression)?;
        self.evaluate(stop_id, root, frame, &target, Mode::Assign, limits)
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Evaluates an expression in one frame of a validated stop.
    pub(super) fn evaluate(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        expression: &Expression,
        mode: Mode,
        limits: crate::InspectionLimits,
    ) -> Result<Evaluation> {
        match self.run_expression(stop_id, root, frame, expression, mode, limits)? {
            Evaluated::Done(evaluation) => Ok(*evaluation),
            Evaluated::Write { span, .. } | Evaluated::WriteRegister { span, .. } => {
                Err(Error::Expression(crate::ExpressionError::new(
                    ErrorKind::Mode,
                    span,
                    "this process's state cannot be changed",
                )))
            }
        }
    }

    fn run_expression(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        expression: &Expression,
        mode: Mode,
        limits: crate::InspectionLimits,
    ) -> Result<Evaluated> {
        validate_inspection_limits(limits)?;
        let inferior = self.stopped_root(stop_id, root)?;
        let resolved = self.resolve_frame(inferior, root, frame)?;
        let scope = self.frame_for(inferior, stop_id, root, &resolved);
        let program = bind(expression, &scope, mode).map_err(Error::Expression)?;
        let mut budget = InspectionBudget::new(limits);

        // Reading may wait for run control; an assignment, which changes
        // the stop, may not.
        let mut machine = StopMachine::new(&scope, &mut budget, mode == Mode::Read);
        let outcome = run(&program, &mut machine);
        record!("evaluate `{}`: {outcome:?}", expression.text());
        Ok(match outcome.map_err(failure)? {
            Outcome::Value { value, cause } => {
                Evaluated::Done(Box::new(Evaluation::Value { value, cause }))
            }
            Outcome::Range { base, start, end } => Evaluated::Done(Box::new(Evaluation::Range(
                self.range_page(stop_id, &base, start, end, &mut budget)?,
            ))),
            Outcome::Assign {
                target,
                bytes,
                whole,
                span,
            } => Evaluated::Write {
                // The binder assigns only to values of a module's types.
                target: match target {
                    Place::Native(target) => target,
                    Place::Interpreted { .. } => {
                        return Err(Error::internal("an interpreter's object is assigned"));
                    }
                },
                bytes,
                whole,
                span,
            },
            Outcome::AssignRegister {
                register,
                bytes,
                span,
            } => Evaluated::WriteRegister {
                register,
                bytes,
                span,
            },
        })
    }

    /// Evaluates an expression where a breakpoint or watchpoint hit stopped
    /// one thread while others may run, as a condition or log message does:
    /// in the innermost frame the hit's stop `reason` presents, with
    /// capabilities that belong to no stop. A condition's value is its
    /// truth.
    pub(super) fn evaluate_at_hit(
        &self,
        pid: Pid,
        expression: &Expression,
        condition: bool,
        reason: &crate::StopReason,
    ) -> Result<Evaluation> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;

        // The thread is in the ptrace-stop that reported the hit, which its
        // recorded state does not reflect until the hit is resolved.
        validate_image_current(inferior)?;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let presentation = self.presentation_for_thread(pid, Some(reason))?;
        let root = StackRoot::of_thread(pid);
        let resolved = self.resolve_presented_frame(
            inferior,
            &root,
            StackFrameId::INNERMOST,
            Some(&presentation),
        )?;
        let mut scope = self.frame_for(inferior, StopId::new(0), &root, &resolved);

        // The thread's recorded reason is not yet this hit's.
        scope.call = call_values(reason);
        let program = if condition {
            crate::eval::bind::bind_condition(expression, &scope)
        } else {
            bind(expression, &scope, Mode::Read)
        }
        .map_err(Error::Expression)?;

        // Run control is evaluating, so nothing interrupts it.
        let mut machine = StopMachine::new(&scope, &mut budget, false);
        let outcome = run(&program, &mut machine);
        record!("evaluate `{}` at a hit: {outcome:?}", expression.text());
        match outcome.map_err(failure)? {
            Outcome::Value { value, cause } => Ok(Evaluation::Value { value, cause }),
            _ => Err(Error::Expression(crate::ExpressionError::new(
                ErrorKind::Type,
                Span::new(0, expression.text().len()),
                "a breakpoint's expression must have a value",
            ))),
        }
    }

    /// Resolves an expression at a stop to the memory it occupies and the
    /// lifetime of that storage: the lifetime of the object it is part of,
    /// or none when it was reached through a pointer.
    pub(super) fn resolve_watch_target(
        &self,
        stop_id: StopId,
        pid: Pid,
        frame: StackFrameId,
        expression: &Expression,
    ) -> Result<crate::WatchTarget> {
        let inferior = self.stopped_inferior(stop_id, pid)?;
        let root = StackRoot::of_thread(pid);
        let resolved = self.resolve_frame(inferior, &root, frame)?;
        let scope = self.frame_for(inferior, stop_id, &root, &resolved);
        let program = bind(expression, &scope, Mode::Read).map_err(Error::Expression)?;
        let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
        let mut machine = StopMachine::new(&scope, &mut budget, false);
        if matches!(program.result(), crate::eval::types::Ty::Dynamic) {
            return Err(Error::WatchTargetUnsupported(
                "an interpreter's object moves and changes as its interpreter likes; watch \
                 native memory"
                    .into(),
            ));
        }

        let Outcome::Value { value, .. } = run(&program, &mut machine).map_err(failure)? else {
            return Err(Error::WatchTargetUnsupported(
                "a range cannot be watched; watch one element".into(),
            ));
        };

        let (address, byte_size) = super::watchpoints::watchable_storage(&value)?;
        let (watch_scope, evidence) = match program.root_object() {
            Some(object) => {
                let module = self
                    .modules
                    .get(&object.module)
                    .ok_or(Error::ModuleNotLoaded(object.module))?;
                let ObjectRef::Data(key) = object.key else {
                    return Err(Error::WatchTargetUnsupported(
                        "a suspended task's variables cannot be watched".into(),
                    ));
                };

                let storage = module.variables.object_storage(key);
                let local = object
                    .local
                    .then(|| scope.code.map(|(_, address, _)| address))
                    .flatten();
                self.root_watch_scope(inferior, pid, frame, object.module, storage, local)?
            }
            None => (crate::WatchScope::Location, None),
        };

        Ok(crate::WatchTarget {
            stop_id,
            expression: expression.clone(),
            address,
            byte_size,
            type_info: value.type_info,
            scope: watch_scope,
            frame: evidence,
        })
    }

    /// The type an expression has in one frame, reading no memory, but
    /// for a value of a dynamic type, which has the type its value has.
    pub(super) fn expression_type(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: StackFrameId,
        expression: &Expression,
    ) -> Result<TypeInfo> {
        let inferior = self.stopped_root(stop_id, root)?;
        let resolved = self.resolve_frame(inferior, root, frame)?;
        let scope = self.frame_for(inferior, stop_id, root, &resolved);
        let program = bind(expression, &scope, Mode::Read).map_err(Error::Expression)?;
        if matches!(program.result(), crate::eval::types::Ty::Dynamic) {
            let mut budget = InspectionBudget::new(crate::InspectionLimits::default());
            let mut machine = StopMachine::new(&scope, &mut budget, true);
            if let Outcome::Value {
                value:
                    InspectedValue {
                        type_info: Some(type_info),
                        ..
                    },
                ..
            } = run(&program, &mut machine).map_err(failure)?
            {
                return Ok(type_info);
            }
        }

        Ok(type_info(&scope, program.result()))
    }

    pub(super) fn frame_for<'a>(
        &'a self,
        inferior: &'a Inferior,
        stop_id: StopId,
        root: &'a StackRoot,
        resolved: &'a ResolvedFrame,
    ) -> Frame<'a, P> {
        Frame {
            controller: self,
            inferior,
            root,
            stop_id,
            resolved,
            code: self.frame_scope(resolved),
            registers: OnceCell::new(),
            charges: Cell::new(0),
            call: root
                .thread()
                .and_then(|pid| inferior.threads.get(&pid))
                .and_then(|thread| thread.reason.as_ref())
                .and_then(call_values),
        }
    }
}
