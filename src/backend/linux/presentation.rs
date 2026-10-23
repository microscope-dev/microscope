//! Views at a stop: which view presents each type, the presentation every
//! inspection path attaches to the values it returns, and the children of
//! presented values.
//!
//! Views run on the controller thread, inside the inspection that asked for
//! the value and charged to its budget. A view binds once per type and view
//! set; each presentation then reads only what it shows.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::debug_info::Located;
use crate::eval::error::ErrorKind;
use crate::eval::target::{
    Lookup, Machine as _, Planned, Refusal, Register, Scope, StepKind, Stop, TypeLookup, TypeQuery,
};

use crate::eval::types::{Ty, TypeSource};
use crate::inspection::InspectionBudget;
use crate::model::{ValueStorage, ViewChildren};
use crate::protocol::StopId;
use crate::runtime_model::{DynamicValue, HeldPlace, StoredValue};
use crate::view::bind::BoundShape;
use crate::view::kernel::Recordings;
use crate::view::run::{Child, Failure};
use crate::view::scan::Checkpoints;
use crate::view::{Choice, ViewSet};
use crate::{
    Error, InspectedValue, Presentation, PresentedCount, PresentedShape, Result, TypeInfo,
    TypeKind, TypeReference, ValueChild, ValueChildPage, ValueChildRelationship, ValueChildren,
    ValueChildrenReference, VariableState, ViewProblem, VirtualAddress,
};

use super::evaluation::{
    Place, StopMachine, StopObject, StopPlace, StopStep, ViewBound, lookup_type_in, plan_in,
};

use super::frames::StackRoot;
use super::native::InspectionOps;
use super::{Controller, RuntimeModule};

/// How deeply views present values inside the values they present.
pub(super) const MAX_DEPTH: u8 = 4;

/// The most values whose scans the controller keeps checkpoints for at one
/// stop; the oldest is forgotten first.
const MAX_SCANS: usize = 64;

/// How many children a recording of kernel runs presents after a value.
const RECORDED_CHILDREN: u32 = 256;

/// The most children of a variant a sum's presentation reads.
const MAX_SUM_CHILDREN: u32 = 64;

/// The views a controller presents values with, the view each type has,
/// and where the scans of values presented at this stop have been.
pub(super) struct Views {
    /// The files loaded for the session, tried before a module's own views
    /// and the built-in ones.
    pub(super) set: Arc<ViewSet>,
    pub(super) enabled: bool,

    /// The view each type has under this set, bound when first needed.
    choices: RefCell<BTreeMap<TypeReference, Arc<Choice<StopStep>>>>,

    /// The view each interpreter's class has at this stop.
    classes: RefCell<ClassChoices>,
    scans: RefCell<Scans>,

    /// Where kernel runs are recorded while a request records them; scans
    /// then neither use nor keep checkpoints.
    recording: RefCell<Option<Recordings>>,
}

/// The view each interpreter's class has at one stop, by its runtime and
/// its address. A class is only known by its address while the stop lasts:
/// once the program runs, it may free the class and make another there.
#[derive(Default)]
struct ClassChoices {
    stop: Option<StopId>,
    choices: BTreeMap<
        (crate::RuntimeId, crate::runtime_model::InterpretedHandle),
        Arc<Choice<StopStep>>,
    >,
}

/// Which value a scan presents: its type, its view, and the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScanKey {
    ty: TypeReference,
    view: (Arc<str>, u32),
    scanned: Scanned,
}

/// The value a scan presents.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scanned {
    /// A native value, where it is stored.
    Stored(ValueStorage),

    /// An interpreter's object, by the runtime that names it and its
    /// handle, which may name no memory.
    Interpreted(crate::RuntimeId, crate::runtime_model::InterpretedHandle),
}

/// The checkpoints of scans at one stop, oldest first. They hold while the
/// program's state does, so a new stop or a write forgets them.
#[derive(Default)]
struct Scans {
    stop: Option<StopId>,
    entries: Vec<(ScanKey, Checkpoints)>,
}

impl Default for Views {
    fn default() -> Self {
        Self {
            set: ViewSet::empty(),
            enabled: true,
            choices: RefCell::default(),
            classes: RefCell::default(),
            scans: RefCell::default(),
            recording: RefCell::default(),
        }
    }
}

impl Views {
    /// Presents with another set from now on.
    pub(super) fn replace(&mut self, set: Arc<ViewSet>) {
        self.set = set;
        self.choices.get_mut().clear();
        self.forget_scans();
    }

    /// Forgets where scans have been, and the views of classes, which the
    /// program keeps, after the program's state changed.
    pub(super) fn forget_scans(&self) {
        self.scans.borrow_mut().entries.clear();
        *self.classes.borrow_mut() = ClassChoices::default();
    }

    /// The view the class `class` of `runtime` has at `stop`, once chosen.
    pub(super) fn class_choice(
        &self,
        stop: StopId,
        runtime: crate::RuntimeId,
        class: crate::runtime_model::InterpretedHandle,
    ) -> Option<Arc<Choice<StopStep>>> {
        let mut classes = self.classes.borrow_mut();
        if classes.stop != Some(stop) {
            *classes = ClassChoices {
                stop: Some(stop),
                choices: BTreeMap::new(),
            };
        }

        classes.choices.get(&(runtime, class)).cloned()
    }

    /// Keeps the view the class `class` of `runtime` has at `stop`.
    pub(super) fn keep_class_choice(
        &self,
        stop: StopId,
        runtime: crate::RuntimeId,
        class: crate::runtime_model::InterpretedHandle,
        choice: Arc<Choice<StopStep>>,
    ) {
        let mut classes = self.classes.borrow_mut();
        if classes.stop == Some(stop) {
            classes.choices.insert((runtime, class), choice);
        }
    }

    /// Runs one scan of the value `key` names at `stop` from the checkpoints
    /// earlier scans of it left, and keeps the ones this scan leaves.
    pub(super) fn scan<T>(
        &self,
        stop: StopId,
        key: ScanKey,
        run: impl FnOnce(&mut Checkpoints) -> T,
    ) -> T {
        let recording = self.recording.borrow().clone();
        if let Some(recordings) = recording {
            return run(&mut Checkpoints::recording(recordings));
        }

        let mut checkpoints = {
            let mut scans = self.scans.borrow_mut();
            if scans.stop != Some(stop) {
                scans.stop = Some(stop);
                scans.entries.clear();
            }

            scans
                .entries
                .iter()
                .find(|(existing, _)| *existing == key)
                .map(|(_, checkpoints)| checkpoints.clone())
                .unwrap_or_default()
        };

        let result = run(&mut checkpoints);

        let mut scans = self.scans.borrow_mut();
        if scans.stop == Some(stop) {
            scans.entries.retain(|(existing, _)| *existing != key);
            if checkpoints != Checkpoints::default() {
                if scans.entries.len() == MAX_SCANS {
                    scans.entries.remove(0);
                }

                scans.entries.push((key, checkpoints));
            }
        }

        result
    }
}

/// The key of the scan presenting `place` with `bound`.
fn scan_key(bound: &ViewBound, image: crate::ModuleImageId, place: &StopPlace) -> ScanKey {
    ScanKey {
        ty: TypeReference {
            image,
            id: place.located.ty,
        },
        view: (Arc::clone(&bound.view.source), bound.view.line),
        scanned: Scanned::Stored(place.located.storage.clone()),
    }
}

impl StopPlace {
    /// Where the value a children reference belongs to is.
    pub(super) fn of(reference: &ValueChildrenReference) -> Self {
        Self {
            module: reference.module,
            located: Located {
                ty: reference.target_type,
                storage: reference.storage.clone(),
            },
        }
    }
}

/// The scope a view binds in: one module's types, and no names. A view
/// sees nothing a frame names, so it means the same at every stop.
pub(super) struct ModuleScope<'a, P: InspectionOps> {
    pub(super) controller: &'a Controller<P>,
    pub(super) module: &'a RuntimeModule,
}

impl<P: InspectionOps> TypeSource for ModuleScope<'_, P> {
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.controller.module_of(ty)?.image.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.controller.pointer_size()
    }

    fn byte_order(&self) -> crate::ByteOrder {
        self.controller.main.image.target().byte_order
    }

    fn c_base_type(&self, ty: crate::CBaseType) -> Option<crate::BaseType> {
        self.controller.main.image.target().c_base_type(ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        left.image == self.module.loaded.image && self.module.image.same_type(left, right)
    }
}

impl<P: InspectionOps> Scope for ModuleScope<'_, P> {
    type Object = StopObject;
    type Step = StopStep;

    fn lookup(
        &self,
        _name: &str,
        _outermost: bool,
    ) -> std::result::Result<Lookup<StopObject>, Refusal> {
        Ok(Lookup::NotFound)
    }

    fn lookup_type(&self, query: &TypeQuery) -> TypeLookup {
        lookup_type_in(self.module, query)
    }

    fn plan(
        &self,
        from: TypeReference,
        step: StepKind<'_>,
    ) -> std::result::Result<Planned<StopStep>, Refusal> {
        let planned = plan_in(self.controller, from, step);

        // A member with no indexing of its own, such as a `std::vector`, is
        // indexed through the view that presents it, as in any expression.
        match (&planned, step) {
            (Err(_), StepKind::Index { .. }) => {
                self.controller.view_index(from).map_or(planned, Ok)
            }
            (Err(_), StepKind::Entry) => self.controller.view_entry(from).map_or(planned, Ok),
            _ => planned,
        }
    }

    fn register(&self, _name: &str) -> Option<Register> {
        None
    }

    /// A view of an interpreter's objects reaches their attributes and
    /// items, as an expression in an interpreted frame does.
    fn plan_dynamic(&self, step: StepKind<'_>) -> std::result::Result<StopStep, Refusal> {
        super::evaluation::plan_interpreted(step)
    }

    fn types_with_base(&self, base: &str) -> Vec<TypeReference> {
        self.module.image.types_with_base(base)
    }

    fn stands_for_container(&self, ty: TypeReference) -> bool {
        crate::view::stands_for_container(self, ty)
    }

    fn global_step(
        &self,
        name: &str,
    ) -> std::result::Result<Option<(StopStep, TypeReference)>, Refusal> {
        let global = match self.module.image.global_named(name) {
            Ok(global) => global,
            Err(Error::VariableNotFound(_)) => return Ok(None),
            Err(error) => return Err(Refusal::new(ErrorKind::AmbiguousName, error.to_string())),
        };

        let key = self
            .module
            .variables
            .global_object(global.id)
            .map_err(|error| Refusal::new(ErrorKind::Unsupported, error.to_string()))?;
        let ty = self.module.variables.object_type(key).map_err(|reason| {
            Refusal::new(
                ErrorKind::Unsupported,
                format!("`{name}` has a malformed type: {reason}"),
            )
        })?;

        Ok(Some((
            StopStep::Global(StopObject::global(self.module.loaded.id, key)),
            TypeReference {
                image: self.module.loaded.image,
                id: ty,
            },
        )))
    }
}

/// The program type of a sequence's elements, when every branch that
/// presents one agrees.
fn element_type<P: InspectionOps>(
    controller: &Controller<P>,
    shape: &BoundShape<StopStep>,
    depth: u8,
) -> Option<TypeReference> {
    match shape {
        BoundShape::Sequence { element, .. } => match element.result() {
            Ty::Program(reference) if element.is_place() => Some(*reference),
            _ => None,
        },
        // A value presented as another has that one's elements, as far as
        // views present values inside the values they present.
        BoundShape::Value(program) if depth < MAX_DEPTH => match program.result() {
            Ty::Program(inner) if program.is_place() => {
                let bound = controller.view_choice(*inner).bound.clone()?;
                element_type(controller, &bound.shape, depth + 1)
            }
            _ => None,
        },
        BoundShape::If {
            then, otherwise, ..
        } => match (
            element_type(controller, then, depth),
            element_type(controller, otherwise, depth),
        ) {
            (Some(left), Some(right)) if left == right => Some(left),
            (Some(found), None) if !otherwise.has_elements() => Some(found),
            (None, Some(found)) if !then.has_elements() => Some(found),
            _ => None,
        },
        _ => None,
    }
}

/// The program type of the value a view presents a value as, when every
/// branch that presents one agrees and it is a place; a branch that
/// presents the value as empty, as a null pointer is, presents none.
fn presented_type(shape: &BoundShape<StopStep>) -> Option<TypeReference> {
    match shape {
        BoundShape::Value(program) => match program.result() {
            Ty::Program(reference) if program.is_place() => Some(*reference),
            _ => None,
        },
        BoundShape::If {
            then, otherwise, ..
        } => match (presented_type(then), presented_type(otherwise)) {
            (Some(left), Some(right)) if left == right => Some(left),
            (Some(found), None) if matches!(**otherwise, BoundShape::Empty(_)) => Some(found),
            (None, Some(found)) if matches!(**then, BoundShape::Empty(_)) => Some(found),
            _ => None,
        },
        _ => None,
    }
}

/// The program type of a map's values, when every branch that presents
/// one agrees and its values are places.
fn entry_type<P: InspectionOps>(
    controller: &Controller<P>,
    shape: &BoundShape<StopStep>,
    depth: u8,
) -> Option<TypeReference> {
    match shape {
        BoundShape::Map { value, .. } => match value.result() {
            Ty::Program(reference) if value.is_place() => Some(*reference),
            _ => None,
        },
        // A value presented as another has that one's entries, as it has
        // that one's elements.
        BoundShape::Value(program) if depth < MAX_DEPTH => match program.result() {
            Ty::Program(inner) if program.is_place() => {
                let bound = controller.view_choice(*inner).bound.clone()?;
                entry_type(controller, &bound.shape, depth + 1)
            }
            _ => None,
        },
        BoundShape::If {
            then, otherwise, ..
        } => match (
            entry_type(controller, then, depth),
            entry_type(controller, otherwise, depth),
        ) {
            (Some(left), Some(right)) if left == right => Some(left),
            (Some(found), None) if !otherwise.has_elements() => Some(found),
            (None, Some(found)) if !then.has_elements() => Some(found),
            _ => None,
        },
        _ => None,
    }
}

/// Each view a choice tried, with why it did not bind.
fn candidates(choice: &Choice<StopStep>) -> Vec<crate::ViewCandidate> {
    choice
        .candidates
        .iter()
        .map(|candidate| crate::ViewCandidate {
            view: Arc::clone(&candidate.name),
            rejection: candidate
                .rejection
                .as_ref()
                .map(|rejection| rejection.to_string().into()),
        })
        .collect()
}

/// The drawings the view a choice binds, and its `extend`s, offer, each
/// with whether it binds.
fn drawing_bindings(choice: &Choice<StopStep>) -> Vec<crate::VisualizerBinding> {
    let Some(bound) = &choice.bound else {
        return Vec::new();
    };

    crate::view::run::visualizers(bound)
        .into_iter()
        .map(|(owner, visualizer)| {
            let drawing = visualizer.bound.as_ref().ok();
            crate::VisualizerBinding {
                view: crate::view::name_of(&owner.view),
                line: visualizer.line,
                name: Arc::clone(&visualizer.name),
                origin: drawing.map(|drawing| Arc::clone(&drawing.renderer.origin)),
                inputs: drawing.map_or_else(
                    || Arc::from([]),
                    |drawing| {
                        drawing
                            .inputs
                            .iter()
                            .map(|input| Arc::clone(&input.name))
                            .collect()
                    },
                ),
                rejection: visualizer
                    .bound
                    .as_ref()
                    .err()
                    .map(|rejection| Arc::from(rejection.reason.as_str())),
            }
        })
        .collect()
}

impl<P: InspectionOps> Controller<P> {
    /// The view that presents values of `ty`, with why each candidate
    /// before it did not bind.
    pub(super) fn view_choice(&self, ty: TypeReference) -> Arc<Choice<StopStep>> {
        if let Some(choice) = self.views.choices.borrow().get(&ty) {
            return Arc::clone(choice);
        }

        // While its view binds, a type has none: a view that indexes a value
        // of its own type through itself binds no view.
        self.views
            .choices
            .borrow_mut()
            .insert(ty, Arc::new(Choice::default()));

        let choice = Arc::new(self.module_of(ty).map_or_else(Choice::default, |module| {
            crate::view::choose_among(
                &[&self.views.set, module.image.views(), &ViewSet::built_in()],
                ty,
                &ModuleScope {
                    controller: self,
                    module,
                },
            )
        }));

        self.views
            .choices
            .borrow_mut()
            .insert(ty, Arc::clone(&choice));

        choice
    }

    /// The views whose patterns name the types `name` means in each module,
    /// and why each did not bind.
    pub(super) fn explain_type(&self, name: &str) -> Vec<crate::TypeViews> {
        let mut found = Vec::new();
        for module in self.modules.values() {
            let mut seen = Vec::<TypeReference>::new();
            for reference in module.image.types_named(name) {
                if seen
                    .iter()
                    .any(|other| module.image.same_type(*other, reference))
                {
                    continue;
                }

                seen.push(reference);
                found.extend(self.type_views(module, reference));
            }
        }

        found
    }

    /// How every module's types are presented: each type a view's pattern
    /// names, once however many units define it, and the session's and
    /// modules' own views that present no type.
    pub(super) fn check_views(&self) -> crate::ViewCheck {
        let built_in = ViewSet::built_in();
        let mut types = Vec::new();
        let mut used = std::collections::BTreeSet::new();
        for module in self.modules.values() {
            let sets = [&*self.views.set, &**module.image.views(), &*built_in];
            let mut seen = std::collections::BTreeSet::new();
            for node in module.image.types() {
                let crate::model::TypeNode::Resolved(info) = node else {
                    continue;
                };

                let Some(identity) = info.identity.as_deref() else {
                    continue;
                };

                if !sets.iter().any(|set| set.names(identity))
                    || module
                        .image
                        .type_class(info.reference)
                        .is_some_and(|class| !seen.insert(class))
                {
                    continue;
                }

                let Some(views) = self.type_views(module, info.reference) else {
                    continue;
                };

                if views.candidates.is_empty() {
                    continue;
                }

                used.extend(
                    views
                        .candidates
                        .iter()
                        .filter(|candidate| candidate.rejection.is_none())
                        .map(|candidate| (Arc::clone(&candidate.view.source), candidate.view.line)),
                );
                types.push(views);
            }
        }

        types.sort_by(|left, right| {
            (&left.type_info.name, &left.module).cmp(&(&right.type_info.name, &right.module))
        });
        let unused = self
            .views
            .set
            .views()
            .iter()
            .chain(
                self.modules
                    .values()
                    .flat_map(|module| module.image.views().views().iter()),
            )
            .filter(|view| !used.contains(&(Arc::clone(&view.source), view.line)))
            .map(|view| crate::view::name_of(view))
            .collect();

        let kernels = self
            .views
            .set
            .kernels()
            .chain(
                self.modules
                    .values()
                    .flat_map(|module| module.image.views().kernels()),
            )
            .collect();

        let renderers = self
            .views
            .set
            .renderers()
            .chain(
                self.modules
                    .values()
                    .flat_map(|module| module.image.views().renderers()),
            )
            .cloned()
            .collect();

        crate::ViewCheck {
            types: types.into(),
            unused,
            kernels,
            renderers,
        }
    }

    /// The views whose patterns name one type, as its choice tried them.
    fn type_views(
        &self,
        module: &RuntimeModule,
        reference: TypeReference,
    ) -> Option<crate::TypeViews> {
        let choice = self.view_choice(reference);
        Some(crate::TypeViews {
            type_info: module.image.type_info(reference)?.clone(),
            module: Arc::from(module.image.path()),
            candidates: candidates(&choice).into(),
            visualizers: drawing_bindings(&choice).into(),
        })
    }

    /// Why an expression's value is presented as it is.
    pub(super) fn explain_view(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: crate::StackFrameId,
        expression: &crate::Expression,
    ) -> Result<crate::ViewExplanation> {
        let value = self.view_subject(stop_id, root, frame, expression, "explain")?;
        let choice = value
            .type_info
            .as_ref()
            .map(|info| self.view_choice(info.reference));
        let (candidates, visualizers) = choice.map_or_else(Default::default, |choice| {
            (candidates(&choice), drawing_bindings(&choice))
        });
        let presentation = match &value.state {
            VariableState::Available { presentation, .. } => presentation.clone(),
            _ => None,
        };

        Ok(crate::ViewExplanation {
            type_info: value.type_info,
            enabled: self.views.enabled,
            candidates: candidates.into(),
            visualizers: visualizers.into(),
            presentation,
        })
    }

    /// Presents an expression's value and its first page of children, and
    /// returns a recording of each kernel run that took, as text.
    pub(super) fn record_kernels(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: crate::StackFrameId,
        expression: &crate::Expression,
    ) -> Result<Vec<String>> {
        let recordings = Recordings::default();
        *self.views.recording.borrow_mut() = Some(recordings.clone());

        let presented = (|| -> Result<()> {
            let value = self.view_subject(stop_id, root, frame, expression, "record")?;
            if let VariableState::Available {
                presentation: Some(presentation),
                ..
            } = &value.state
                && let ValueChildren::Available(reference) = &presentation.children
            {
                self.value_children(
                    reference,
                    &crate::ValueChildQuery {
                        offset: 0,
                        limit: RECORDED_CHILDREN,
                    },
                    crate::InspectionLimits::default(),
                )?;
            }

            Ok(())
        })();

        *self.views.recording.borrow_mut() = None;
        presented?;

        Ok(recordings.take().iter().map(ToString::to_string).collect())
    }

    /// The one value an expression has, for a request about its view.
    fn view_subject(
        &self,
        stop_id: StopId,
        root: &StackRoot,
        frame: crate::StackFrameId,
        expression: &crate::Expression,
        verb: &str,
    ) -> Result<InspectedValue> {
        match self.evaluate(
            stop_id,
            root,
            frame,
            expression,
            crate::EvaluationMode::Read,
            crate::InspectionLimits::default(),
        )? {
            crate::Evaluation::Value { value, .. } => Ok(value),
            _ => Err(Error::InvalidValueExpression(format!(
                "a range of elements has no view; {verb} one element"
            ))),
        }
    }

    /// The step from a value of `from` to an element its view presents,
    /// for `v[i]` on a value with no indexing of its own.
    pub(super) fn view_index(&self, from: TypeReference) -> Option<Planned<StopStep>> {
        if !self.views.enabled {
            return None;
        }

        let bound = self.view_choice(from).bound.clone()?;
        let element = element_type(self, &bound.shape, 0)?;

        Some(Planned {
            step: StopStep::Element(bound),
            result: Some(element),
            consumed: 1,
        })
    }

    /// The step from a value of `from` to the value its view presents it
    /// as, for `*x`.
    pub(super) fn view_deref(&self, from: TypeReference) -> Option<Planned<StopStep>> {
        if !self.views.enabled {
            return None;
        }

        let bound = self.view_choice(from).bound.clone()?;
        let presented = presented_type(&bound.shape)?;

        Some(Planned {
            step: StopStep::Presented(bound),
            result: Some(presented),
            consumed: 0,
        })
    }

    /// The step from a value of `from` to the value its view presents as a
    /// map holds for a key, for `m[key]`.
    pub(super) fn view_entry(&self, from: TypeReference) -> Option<Planned<StopStep>> {
        if !self.views.enabled {
            return None;
        }

        let bound = self.view_choice(from).bound.clone()?;
        let value = entry_type(self, &bound.shape, 0)?;

        Some(Planned {
            step: StopStep::Entry(bound),
            result: Some(value),
            consumed: 1,
        })
    }

    /// The children a view presents: its elements, its fields, and the
    /// value as stored.
    pub(super) fn view_children(
        &self,
        reference: &ValueChildrenReference,
        view: &ViewChildren,
        offset: u64,
        limit: u32,
        budget: &mut InspectionBudget,
    ) -> Result<ValueChildPage> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let root = self.stack_root(reference.stop_id, reference.context)?;
        let frame = self.resolve_frame(inferior, &root, reference.frame)?;
        let scope = self.frame_for(inferior, reference.stop_id, &root, &frame);
        let (bound, this) = viewed(reference, view)?;
        let end = offset.saturating_add(u64::from(limit)).min(reference.total);
        let mut children = Vec::new();
        let mut exhausted = None;

        // A view that presents the value as another lends it that value's
        // children.
        let mut next = offset;
        if let (Some(inner), Some(picked)) = (&view.inner, &view.picked) {
            // Elements picked from another value's children come a page of
            // one each.
            while exhausted.is_none() && next < end.min(view.elements) {
                let position = usize::try_from(next)
                    .map_err(|_| Error::internal("a picked element index fits usize"))?;
                let page = self.value_children_with_budget(
                    inner,
                    &crate::ValueChildQuery {
                        offset: picked[position],
                        limit: 1,
                    },
                    budget,
                )?;
                children.extend(page.children.iter().cloned());
                exhausted = page.completion.exhaustion();
                next += 1;
            }
        } else if let Some(inner) = &view.inner
            && next < view.elements
        {
            let count = end.min(view.elements) - next;
            let page = self.value_children_with_budget(
                inner,
                &crate::ValueChildQuery {
                    offset: next,
                    limit: u32::try_from(count).unwrap_or(u32::MAX),
                },
                budget,
            )?;
            children.extend(page.children.iter().cloned());
            next = view.elements;
            exhausted = page.completion.exhaustion();
        }

        if exhausted.is_none() && next < end {
            let mut machine = StopMachine::new(&scope, budget, true);
            match &bound {
                // What the debugger presents without a view has only `[raw]`
                // after the elements it lends.
                None => children.push(value_child(Child::Raw, &this, &mut machine)?),
                Some(bound) => {
                    // With borrowed elements, the view's own children start
                    // at its fields.
                    let (start, elements) = if view.inner.is_some() {
                        (next - view.elements, 0)
                    } else {
                        (next, view.elements)
                    };

                    let key = this.scan_key(bound, reference.image);
                    let presented = self.views.scan(reference.stop_id, key, |checkpoints| {
                        crate::view::run::children(
                            bound,
                            &mut machine,
                            this.place(),
                            elements,
                            start,
                            end - next,
                            checkpoints,
                        )
                    });
                    match presented {
                        Ok(presented) => {
                            for child in presented {
                                children.push(value_child(child, &this, &mut machine)?);
                            }
                        }
                        Err(Failure::Debugger(error)) => return Err(error),
                        Err(Failure::Problem(ViewProblem::Unavailable(
                            crate::VariableUnavailableReason::InspectionLimit(exhaustion),
                        ))) => exhausted = Some(exhaustion),
                        Err(Failure::Problem(problem)) => {
                            return Err(Error::ViewFailed(problem.to_string().into()));
                        }
                    }
                }
            }
        }

        Ok(ValueChildPage {
            stop_id: reference.stop_id,
            offset,
            total: reference.total,
            children: children.into(),
            completion: exhausted.map_or_else(
                || budget.completion(),
                crate::InspectionCompletion::Truncated,
            ),
            usage: budget.usage(),
        })
    }
}

/// The value whose children a view presents.
#[derive(Debug, Clone)]
enum Viewed {
    Native(StopPlace),

    /// An interpreter's object, which `runtime` names by `object`.
    Interpreted {
        runtime: crate::RuntimeId,
        object: crate::runtime_model::InterpretedHandle,
    },
}

impl Viewed {
    fn place(&self) -> Place {
        match self {
            Self::Native(place) => place.clone().into(),
            Self::Interpreted { runtime, object } => Place::Interpreted {
                runtime: *runtime,
                object: *object,
            },
        }
    }

    /// The key of the scan that presents the value through `bound`.
    fn scan_key(&self, bound: &ViewBound, image: crate::ModuleImageId) -> ScanKey {
        match self {
            Self::Native(place) => scan_key(bound, image, place),
            Self::Interpreted { runtime, object } => interpreted_scan_key(bound, *runtime, *object),
        }
    }

    /// The type of a child whose own type is unknown.
    fn placeholder(&self) -> TypeInfo {
        match self {
            Self::Native(place) => placeholder(place),
            Self::Interpreted { .. } => TypeInfo {
                reference: crate::eval::types::UNIDENTIFIED,
                name: "<unknown type>".into(),
                byte_size: None,
                kind: TypeKind::Unspecified,
                identity: None,
            },
        }
    }
}

/// The scan of `runtime`'s object `object` through `bound`.
pub(super) fn interpreted_scan_key(
    bound: &ViewBound,
    runtime: crate::RuntimeId,
    object: crate::runtime_model::InterpretedHandle,
) -> ScanKey {
    ScanKey {
        ty: crate::eval::types::UNIDENTIFIED,
        view: (Arc::clone(&bound.view.source), bound.view.line),
        scanned: Scanned::Interpreted(runtime, object),
    }
}

/// The view a children capability presents through, if any, and the value
/// whose children they are.
fn viewed(
    reference: &ValueChildrenReference,
    view: &ViewChildren,
) -> Result<(Option<Arc<ViewBound>>, Viewed)> {
    let native = || Viewed::Native(StopPlace::of(reference));
    let Some(bound) = &view.bound else {
        return Ok((None, native()));
    };

    if let Some(interpreted) = bound.downcast_ref::<super::interpreted_views::InterpretedView>() {
        return Ok((
            Some(Arc::clone(&interpreted.bound)),
            Viewed::Interpreted {
                runtime: interpreted.runtime,
                object: interpreted.object,
            },
        ));
    }

    let bound = Arc::clone(bound).downcast::<ViewBound>().map_err(|_| {
        Error::InvalidValueExpression("the view belongs to another debugger".into())
    })?;

    Ok((Some(bound), native()))
}

/// The view a capability presents through, and where the value it
/// presents is: a native value, or an interpreter's object.
pub(super) fn view_place(reference: &ValueChildrenReference) -> Result<(Arc<ViewBound>, Place)> {
    let no_view = || Error::InvalidValueExpression("the value has no view".into());
    let view = reference.view.as_ref().ok_or_else(no_view)?;
    let (bound, viewed) = viewed(reference, view)?;
    Ok((bound.ok_or_else(no_view)?, viewed.place()))
}

/// A child a view presents, as the public model has it.
fn value_child<P: InspectionOps>(
    child: Child,
    this: &Viewed,
    machine: &mut StopMachine<'_, '_, P>,
) -> Result<ValueChild> {
    let (relationship, value) = match child {
        Child::Element(index, value) => (ValueChildRelationship::Element { index }, value),
        Child::Entry(index, key, value) => (
            ValueChildRelationship::Entry {
                index,
                key: Arc::new(crate::MapKey {
                    type_info: key.type_info.clone().unwrap_or_else(|| this.placeholder()),
                    state: key.state,
                }),
            },
            value,
        ),
        Child::Field(name, value) => (ValueChildRelationship::Field { name }, value),
        Child::Raw => {
            let value = match this {
                Viewed::Native(this) => {
                    let module = machine.module(this.module).map_err(stopped)?;
                    machine
                        .materialize(module, &this.located)
                        .map_err(stopped)?
                }
                Viewed::Interpreted { .. } => {
                    machine.present_stored(&this.place()).map_err(stopped)?
                }
            };
            (ValueChildRelationship::Raw, value)
        }
    };

    Ok(ValueChild {
        relationship,
        type_info: value.type_info.unwrap_or_else(|| this.placeholder()),
        state: value.state,
    })
}

/// What a value dynamically is.
enum Dynamic {
    /// Nothing, as a nil interface holds.
    Nil,

    /// A value of type `ty` at `place`.
    Value { ty: TypeReference, place: StopPlace },
}

impl Dynamic {
    /// The value of type `ty` at `address`, in `module`.
    const fn at(module: &RuntimeModule, ty: TypeReference, address: u64) -> Self {
        Self::Value {
            ty,
            place: StopPlace {
                module: module.loaded.id,
                located: Located {
                    ty: ty.id,
                    storage: ValueStorage::Memory(VirtualAddress::new(address)),
                },
            },
        }
    }
}

/// A type and the types its typedefs and qualifiers stand for, outermost
/// first.
fn typedef_chain(types: &dyn TypeSource, ty: TypeReference) -> Vec<&TypeInfo> {
    let mut chain = Vec::new();
    let mut current = Some(ty);
    while let Some(ty) = current.filter(|_| chain.len() < 64) {
        let Some(info) = types.type_info(ty) else {
            break;
        };

        current = match &info.kind {
            TypeKind::Modified { target, .. }
            | TypeKind::Named {
                target: Some(target),
                ..
            } => Some(*target),
            _ => None,
        };

        chain.push(info);
    }

    chain
}

/// Whether a C++ class is polymorphic: it, or a base at its start, holds a
/// vtable pointer, which GCC names `_vptr.X` and Clang `_vptr$X`.
fn polymorphic(types: &dyn TypeSource, ty: TypeReference, depth: usize) -> bool {
    let Ok((_, info)) = crate::eval::types::representation(types, ty) else {
        return false;
    };

    let TypeKind::Record { members, bases, .. } = &info.kind else {
        return false;
    };

    depth < 16
        && (members.iter().any(|member| {
            member.artificial
                && member
                    .name
                    .as_deref()
                    .is_some_and(|name| name.starts_with("_vptr"))
        }) || bases.iter().any(|base| {
            matches!(base.layout, crate::RecordMemberLayout::ByteOffset(0))
                && polymorphic(types, base.type_ref, depth + 1)
        }))
}

/// The one type a name means in an image, as its several units' copies of
/// one type are one.
fn one_type(image: &crate::ModuleImage, name: &str) -> Option<TypeReference> {
    let found = image
        .types_named(name)
        .into_iter()
        .filter(|reference| {
            image
                .type_info(*reference)
                .is_some_and(|info| info.identity.is_some())
        })
        .collect::<Vec<_>>();
    let first = *found.first()?;

    found
        .iter()
        .all(|other| image.same_type(first, *other))
        .then_some(first)
}

/// How a sum type's value reads: the value standing for it, if any, and
/// its summary.
struct Sum {
    payload: Option<ValueChild>,
    summary: String,
}

impl Sum {
    /// The sum whose active variant is `name`, with `members`. A variant
    /// whose one member is named as it holds that member: Rust's is a record
    /// of the variant's fields, `__0` and on for a tuple variant, and a Zig
    /// tagged union's any value: `Some(42)`, `Ok(7)`, `Point {x: 1, y: 2}`,
    /// `circle(3)`, `None`. A Zig optional or error union is its payload,
    /// `null`, or its error, and an Odin union the value it holds, or
    /// `nil`.
    fn of<P: InspectionOps>(
        language: Option<crate::SourceLanguage>,
        name: &str,
        members: &[ValueChild],
        machine: &mut StopMachine<'_, '_, P>,
    ) -> std::result::Result<Self, Stop> {
        let zig = language == Some(crate::SourceLanguage::Zig);

        // Zig's optionals and error unions stand for what they hold: the
        // LLVM backend names their variants `null`, `some`, `success`, and
        // `error`, and the self-hosted one `null`, `?`, `value`, and
        // `error`, whose errors it names without the `error.` the LLVM
        // backend gives them.
        match (zig, name, members) {
            (true, "null", [] | [_]) if members.iter().all(holds_nothing) => {
                return Ok(Self {
                    payload: None,
                    summary: "null".to_owned(),
                });
            }
            (true, "some" | "success" | "?" | "value", [member]) => {
                return Ok(Self {
                    summary: crate::view::summary::value(Some(&member.type_info), &member.state),
                    payload: Some(member.clone()),
                });
            }
            (true, "error", [member]) => {
                let summary = crate::view::summary::value(Some(&member.type_info), &member.state);
                let named = matches!(
                    &member.state,
                    VariableState::Available {
                        value: crate::VariableValue::Enumeration { matches, .. },
                        ..
                    } if !matches.is_empty()
                );
                return Ok(Self {
                    summary: if named && !summary.starts_with("error.") {
                        format!("error.{summary}")
                    } else {
                        summary
                    },
                    payload: Some(member.clone()),
                });
            }
            _ => {}
        }

        // An Odin union's variant is named for its one member's type.
        if let (Some(crate::SourceLanguage::Odin), [member]) = (language, members) {
            return Ok(Self {
                summary: crate::view::summary::value(Some(&member.type_info), &member.state),
                payload: Some(member.clone()),
            });
        }

        // One member named as the variant is its payload: a Rust variant's
        // record of fields, or a Zig tagged union's value.
        let fields = match members {
            // A payload of no size or no type, such as Zig's `void`, holds
            // nothing.
            [member] if member_name(member) == Some(name) && holds_nothing(member) => Vec::new(),
            [member] if member_name(member) == Some(name) => match &member.state {
                // Rust's tuple variant is a record of `__0` and on, which
                // is presented as a tuple of its own.
                VariableState::Available {
                    value: crate::VariableValue::Record,
                    children: ValueChildren::Available(reference),
                    presentation,
                    ..
                } if presentation
                    .as_deref()
                    .is_none_or(Presentation::is_rust_tuple) =>
                {
                    let reference = Arc::clone(reference);
                    machine.children_of(&reference)?
                }
                _ => {
                    return Ok(Self {
                        summary: format!(
                            "{name}({})",
                            crate::view::summary::value(Some(&member.type_info), &member.state)
                        ),
                        payload: Some(member.clone()),
                    });
                }
            },
            _ => members.to_vec(),
        };

        let tuple = fields
            .iter()
            .all(|field| member_name(field).is_some_and(|field| field.starts_with("__")));
        let rendered = fields
            .iter()
            .map(|field| crate::view::summary::value(Some(&field.type_info), &field.state))
            .collect::<Vec<_>>();
        let summary = match (fields.as_slice(), tuple) {
            ([], _) => name.to_owned(),
            (_, true) => format!("{name}({})", rendered.join(", ")),
            _ => format!(
                "{name} {{{}}}",
                fields
                    .iter()
                    .zip(&rendered)
                    .map(|(field, value)| format!(
                        "{}: {value}",
                        member_name(field).unwrap_or("<anonymous>")
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };

        // A variant of one field stands for that field; one of several, or
        // a Rust variant's record, for its members.
        let payload = match (fields.as_slice(), members) {
            ([field], _) => Some(field.clone()),
            ([], _) => None,
            (_, [member]) => Some(member.clone()),
            _ => None,
        };

        Ok(Self { payload, summary })
    }
}

/// Whether a variant's member holds nothing: of no size, or of a type
/// that is no type, as Zig's `void` and the type of `null` are.
fn holds_nothing(member: &ValueChild) -> bool {
    member.type_info.byte_size == Some(0) || matches!(member.type_info.kind, TypeKind::Unspecified)
}

/// A child's member name.
fn member_name(child: &ValueChild) -> Option<&str> {
    match &child.relationship {
        ValueChildRelationship::Member(member) => member.name.as_deref(),
        _ => None,
    }
}

/// A coroutine's summary: its state, then the variables it keeps.
fn coroutine_summary(
    image: &crate::ModuleImage,
    state: &crate::CoroutineState,
    shown: &[String],
) -> String {
    let mut summary = match state.kind {
        crate::CoroutineStateKind::Unresumed => "unresumed".to_owned(),
        crate::CoroutineStateKind::Returned => "returned".to_owned(),
        crate::CoroutineStateKind::Panicked => "panicked".to_owned(),
        crate::CoroutineStateKind::Suspended { .. } => {
            let at = state.location.as_ref().map(|location| {
                let file = image
                    .source_file(location.file)
                    .and_then(|file| file.path.file_name())
                    .map_or_else(|| "?".into(), |name| name.to_string_lossy());
                format!("{file}:{}", location.line)
            });
            format!("suspended at {}", at.as_deref().unwrap_or("an await"))
        }
    };

    if !shown.is_empty() {
        summary.push_str(" {");
        for (index, part) in shown.iter().enumerate() {
            if summary.chars().count() > crate::view::summary::MAX_CHARACTERS {
                summary.push_str(", …");
                break;
            }

            if index > 0 {
                summary.push_str(", ");
            }

            summary.push_str(part);
        }

        summary.push('}');
    }

    summary
}

/// Children one value lends another, and how many.
type Lent = (Arc<ValueChildrenReference>, u64);

/// The children a value lends the value it stands for, and how many: a
/// presented value's elements and fields, but not its `[raw]`, which is
/// its own; or the members of one no view presents.
fn lent(state: &VariableState) -> Option<Lent> {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } if presentation.shape != PresentedShape::Raw => match &presentation.children {
            ValueChildren::Available(reference) => {
                Some((Arc::clone(reference), reference.total().saturating_sub(1)))
            }
            _ => None,
        },
        VariableState::Available {
            children: ValueChildren::Available(reference),
            ..
        } => Some((Arc::clone(reference), reference.total())),
        _ => None,
    }
}

/// A type for a child whose own type is unknown.
fn placeholder(this: &StopPlace) -> TypeInfo {
    TypeInfo {
        reference: TypeReference {
            image: crate::eval::types::LANGUAGE_IMAGE,
            id: this.located.ty,
        },
        name: "<unknown type>".into(),
        byte_size: None,
        kind: TypeKind::Unspecified,
        identity: None,
    }
}

/// A machine's stop as the debugger's error.
pub(super) fn stopped(stop: Stop) -> Error {
    match stop {
        Stop::Failed(error) => error,
        Stop::Missing(state) => Error::ViewFailed(format!("{state:?}").into()),
        Stop::Refused(refusal) => Error::ViewFailed(refusal.message.into()),
    }
}

/// How a value a view presented reads: its shape and count, and the value
/// that lends it elements, with how many. A value presented as a sequence
/// or map stands for this one as that sequence or map: its elements, then
/// this value's own fields and `[raw]`.
fn collection(
    presented: &crate::view::run::Presented,
) -> (
    PresentedShape,
    Option<PresentedCount>,
    Option<Arc<ValueChildrenReference>>,
    u64,
) {
    let inner = presented
        .inner
        .as_ref()
        .and_then(|inner| lent(&inner.state));
    let collection = presented
        .inner
        .as_ref()
        .and_then(|inner| match &inner.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } if matches!(
                presentation.shape,
                PresentedShape::Sequence | PresentedShape::Map
            ) =>
            {
                Some((presentation.shape, presentation.count))
            }
            _ => None,
        });
    let (shape, count) = collection.unwrap_or((presented.shape, presented.count));
    let elements = match (&inner, collection) {
        (Some(_), Some((_, count))) => count.map_or(0, PresentedCount::known),
        (Some((_, lent)), None) => *lent,
        (None, _) => presented.count.map_or(0, PresentedCount::known),
    };

    (
        shape,
        count,
        inner.map(|(reference, _)| reference),
        elements,
    )
}

/// The children of `raw` presented through a view: `elements` that `inner`
/// lends or `bound` presents, then `fields`, then `[raw]`.
fn presented_children(
    raw: &ValueChildrenReference,
    bound: Option<Arc<ViewBound>>,
    inner: Option<Arc<ValueChildrenReference>>,
    elements: u64,
    fields: u64,
) -> ValueChildren {
    let mut reference = raw.clone();
    reference.total = elements.saturating_add(fields).saturating_add(1);
    reference.active_variant = None;
    reference.view = Some(ViewChildren {
        bound: bound.map(|bound| bound as Arc<dyn std::any::Any + Send + Sync>),
        elements,
        fields,
        inner,
        picked: None,
    });
    ValueChildren::Available(Arc::new(reference))
}

/// The presentation of a value of the raw children `raw` that `bound`,
/// named `name`, presented as `presented`.
pub(super) fn view_presentation(
    name: Arc<crate::ViewName>,
    bound: &Arc<ViewBound>,
    raw: &ValueChildrenReference,
    presented: crate::view::run::Presented,
) -> Presentation {
    let (shape, count, inner, elements) = collection(&presented);
    let children = presented_children(
        raw,
        Some(Arc::clone(bound)),
        inner,
        elements,
        presented.named,
    );
    Presentation {
        view: name,
        shape,
        count,
        summary: presented.summary.into(),
        detail: None,
        visualizers: drawings(bound, &children),
        children,
        problem: presented.partial,
        number: presented.inner.as_ref().and_then(number),
        presented: presented.inner.map(Arc::new),
    }
}

/// The drawings a view and its `extend`s offer of a value whose children,
/// through the view, are `children`.
pub(super) fn drawings(bound: &ViewBound, children: &ValueChildren) -> Arc<[crate::Visualizer]> {
    let ValueChildren::Available(children) = children else {
        return Arc::from([]);
    };

    crate::view::run::visualizers(bound)
        .iter()
        .enumerate()
        .filter_map(|(index, (_, visualizer))| {
            let drawing = visualizer.bound.as_ref().ok()?;
            Some(crate::Visualizer {
                renderer: Arc::clone(&drawing.renderer),
                inputs: Arc::new(crate::VisualizerReference {
                    children: Arc::clone(children),
                    index,
                }),
            })
        })
        .collect()
}

/// A presentation the debugger makes from debug information alone, under
/// `header`, standing for the value whose children are `lent`.
fn built_in_presentation(
    header: &str,
    shape: PresentedShape,
    summary: String,
    raw: &ValueChildrenReference,
    lent: Option<(Arc<ValueChildrenReference>, u64)>,
) -> Presentation {
    let (inner, elements) = lent.map_or((None, 0), |(inner, elements)| (Some(inner), elements));
    Presentation {
        view: Arc::new(crate::ViewName {
            source: "microscope".into(),
            line: 0,
            header: header.into(),
            extend: false,
        }),
        shape,
        count: None,
        summary: summary.into(),
        detail: None,
        children: presented_children(raw, None, inner, elements, 0),
        problem: None,
        number: None,
        presented: None,
        visualizers: Arc::from([]),
    }
}

/// The integer `value` is, or a view presents it as.
fn number(value: &InspectedValue) -> Option<Arc<InspectedValue>> {
    match &value.state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation.number.clone(),
        VariableState::Available {
            value:
                crate::VariableValue::Scalar(
                    crate::ScalarValue::Signed(_) | crate::ScalarValue::Unsigned(_),
                )
                | crate::VariableValue::Enumeration { .. },
            ..
        } => Some(Arc::new(value.clone())),
        _ => None,
    }
}

/// `value` with a presentation already shared.
fn present_as_shared(mut value: InspectedValue, presentation: Arc<Presentation>) -> InspectedValue {
    if let VariableState::Available {
        presentation: slot, ..
    } = &mut value.state
    {
        *slot = Some(presentation);
    }

    value
}

fn present_as(mut value: InspectedValue, presentation: Presentation) -> InspectedValue {
    if let VariableState::Available {
        presentation: slot, ..
    } = &mut value.state
    {
        *slot = Some(Arc::new(presentation));
    }

    value
}

/// The presentation of a value a view failed to present, which shows it as
/// stored, with the reason.
fn failed(view: Arc<crate::ViewName>, problem: ViewProblem) -> Presentation {
    Presentation {
        view,
        shape: PresentedShape::Raw,
        count: None,
        summary: problem.to_string().into(),
        detail: None,
        children: ValueChildren::NotApplicable,
        problem: Some(problem),
        number: None,
        presented: None,
        visualizers: Arc::from([]),
    }
}

/// A view's failure to run for an expression, as the machine's stop.
fn view_stop(failure: Failure, bound: &ViewBound, kind: ErrorKind) -> Stop {
    match failure {
        Failure::Debugger(error) => Stop::Failed(error),
        Failure::Problem(ViewProblem::Unavailable(reason)) => {
            Stop::missing(VariableState::Unavailable(reason))
        }
        Failure::Problem(problem) => Stop::Refused(Refusal::new(
            kind,
            format!("the view {}: {problem}", bound.view.header),
        )),
    }
}

impl<'a, P: InspectionOps> StopMachine<'a, '_, P> {
    /// `value` with its presentation, when a view presents its type. The
    /// view runs with a share of the budget, so a presentation that runs
    /// out never takes the rest of the inspection with it.
    pub(super) fn presented(
        &mut self,
        mut value: InspectedValue,
    ) -> std::result::Result<InspectedValue, Stop> {
        let controller = self.frame.controller;
        if !controller.views.enabled || self.depth >= MAX_DEPTH {
            return Ok(value);
        }

        let bound = value
            .type_info
            .as_ref()
            .and_then(|info| controller.view_choice(info.reference).bound.clone());
        if bound.is_none()
            && let VariableState::Available {
                value: crate::VariableValue::Address(_),
                dereference: crate::DereferenceState::Available(reference),
                text: None,
                ..
            } = &value.state
            && let crate::DereferenceReference {
                module,
                image,
                target_type,
                target: crate::model::DereferenceTarget::Address(address),
                ..
            } = &**reference
        {
            // A pointer or reference to text shows the text, as a pointer
            // to characters does, or why its view could not read it. A
            // null pointer, like a null pointer to characters, points at
            // nothing to read.
            if address.get() == 0 {
                return Ok(value);
            }

            let target = TypeReference {
                image: *image,
                id: *target_type,
            };

            let place = StopPlace {
                module: *module,
                located: Located {
                    ty: *target_type,
                    storage: ValueStorage::Memory(*address),
                },
            };

            let (text, presentation) = self.pointee_text(target, place)?;

            // A pointer to what is not text may still be shown as what
            // it points to, as Go's debuggers show one.
            if text.is_some() || presentation.is_some() {
                if let VariableState::Available {
                    text: state_text,
                    presentation: state_presentation,
                    ..
                } = &mut value.state
                {
                    *state_text = text.map(Arc::new);
                    *state_presentation = presentation.map(Arc::new);
                }

                return Ok(value);
            }
        }

        let (
            Some(type_info),
            VariableState::Available {
                presentation: None, ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(value);
        };

        let Some(bound) = bound else {
            return self.built_in(value);
        };

        // An aggregate's children say where it is; another value, such as
        // a Go map's pointer, is where its state was read from.
        let Some(raw) = self.presented_place(type_info, &value.state) else {
            return Ok(value);
        };

        let this = StopPlace::of(&raw);
        let key = scan_key(&bound, type_info.reference.image, &this);
        let result = controller
            .views
            .scan(self.frame.stop_id, key, |checkpoints| {
                self.in_share(|machine| {
                    crate::view::run::present(&bound, machine, this.into(), checkpoints)
                })
            });
        let name = crate::view::name_of(&bound.view);
        let presented = match result {
            Ok(presented) => presented,
            Err(Failure::Problem(problem)) => return Ok(present_as(value, failed(name, problem))),
            Err(Failure::Debugger(error)) => return Err(Stop::Failed(error)),
        };

        let text = presented.text.clone();
        let mut value = present_as(value, view_presentation(name, &bound, &raw, presented));
        if let Some(text) = text
            && let VariableState::Available { text: slot, .. } = &mut value.state
        {
            *slot = Some(Arc::new(text));
        }

        Ok(value)
    }

    /// What the debugger presents of a value no view presents, from its
    /// debug information alone: a value as the type it dynamically is, or a
    /// sum type as its active variant.
    fn built_in(&mut self, value: InspectedValue) -> std::result::Result<InspectedValue, Stop> {
        if self.dynamic
            && let Some(dynamic) = self.dynamic(&value)?
        {
            return self.with_dynamic(value, dynamic);
        }

        if self.dynamic
            && let Some(presentation) = self.interpreter_pointer(&value)?
        {
            return Ok(present_as_shared(value, presentation));
        }

        if let Some(type_info) = &value.type_info
            && self.is_go_pointer(type_info)
            && let Some(raw) = self.place_of(type_info, &value.state)
        {
            let nil = matches!(
                &value.state,
                VariableState::Available {
                    value: crate::VariableValue::Address(address),
                    ..
                } if address.address.get() == 0
            );
            let pointee = if nil {
                Some(("nil".to_owned(), None))
            } else {
                self.pointee_summary(&value.state)?
            };

            if let Some((summary, lent)) = pointee {
                let shape = if nil {
                    PresentedShape::Empty
                } else {
                    PresentedShape::Value
                };

                return Ok(present_as(
                    value,
                    built_in_presentation("Go pointers", shape, summary, &raw, lent),
                ));
            }
        }

        if let Some(tuple) = self.rust_tuple(&value)? {
            return Ok(tuple);
        }

        if let Some(future) = self.coroutine(&value)? {
            return Ok(future);
        }

        let (
            Some(type_info),
            VariableState::Available {
                value:
                    crate::VariableValue::Variant {
                        active: Some(variant),
                        ..
                    },
                children: ValueChildren::Available(raw),
                presentation: None,
                ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(value);
        };

        let (variant, raw) = (Arc::clone(variant), Arc::clone(raw));
        let language = type_info
            .identity
            .as_ref()
            .map(|identity| identity.language);
        let children = self.children_of(&raw)?;
        let members = &children[children.len().saturating_sub(variant.members.len())..];
        let name = variant
            .name
            .clone()
            .or_else(|| match variant.members.as_ref() {
                [member] => member.name.clone(),
                _ => None,
            })
            .unwrap_or_else(|| Arc::from("<unnamed variant>"));
        let sum = Sum::of(language, &name, members, self)?;
        let shape = if sum.payload.is_some() {
            PresentedShape::Value
        } else {
            PresentedShape::Empty
        };

        let lent = sum
            .payload
            .as_ref()
            .and_then(|payload| lent(&payload.state));

        Ok(present_as(
            value,
            built_in_presentation("sum types", shape, sum.summary, &raw, lent),
        ))
    }

    /// The presentation of the interpreter's object a native pointer points
    /// to, where its pointee's type, or a type it names, is the one an
    /// interpreter of the process gives its objects, as CPython's
    /// `PyObject`; `None` for any other value, and a null pointer.
    fn interpreter_pointer(
        &mut self,
        value: &InspectedValue,
    ) -> std::result::Result<Option<Arc<Presentation>>, Stop> {
        let (
            Some(type_info),
            VariableState::Available {
                value: crate::VariableValue::Address(address),
                ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(None);
        };

        let target = match typedef_chain(self, type_info.reference)
            .last()
            .map(|info| &info.kind)
        {
            Some(TypeKind::Pointer {
                target: Some(target),
                ..
            }) => *target,
            _ => return Ok(None),
        };

        if address.address.get() == 0 {
            return Ok(None);
        }

        let names: Vec<Arc<str>> = typedef_chain(self, target)
            .iter()
            .map(|info| Arc::clone(&info.name))
            .collect();

        self.interpreted_object(&names, address.address)
    }

    /// What a value dynamically is, when its debug information and the
    /// program's own tables say so exactly: the object a C++ vtable
    /// pointer belongs to, the value a Rust trait object or Go interface
    /// holds, or nothing at all, as a nil interface holds.
    fn dynamic(&mut self, value: &InspectedValue) -> std::result::Result<Option<Dynamic>, Stop> {
        let Some(type_info) = &value.type_info else {
            return Ok(None);
        };

        let Some(this) = self.place_of(type_info, &value.state) else {
            return Ok(None);
        };

        let place = StopPlace::of(&this);
        let language = typedef_chain(self, type_info.reference)
            .iter()
            .find_map(|info| info.identity.as_ref().map(|identity| identity.language));

        match language {
            Some(crate::SourceLanguage::Cpp) => self.cpp_dynamic(type_info, &place),
            Some(crate::SourceLanguage::Rust) => self.rust_dynamic(type_info, &place),
            Some(crate::SourceLanguage::Go) => Ok(self.go_dynamic(type_info, &place)),
            _ => Ok(None),
        }
    }

    /// A pointer-sized word of memory.
    fn word(&mut self, address: u64) -> std::result::Result<u64, Stop> {
        let bytes = self.read(address, 8)?;
        Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
            Stop::Refused(Refusal::new(
                crate::ExpressionErrorKind::Unsupported,
                "a short read",
            ))
        })?))
    }

    /// The module a virtual address lies in, with the address in its image.
    fn module_at(&self, address: u64) -> Option<(&'a RuntimeModule, crate::ImageAddress)> {
        let controller: &'a Controller<P> = self.frame.controller;
        controller.modules.values().find_map(|module| {
            let image = module
                .loaded
                .image_address(crate::VirtualAddress::new(address))
                .ok()?;
            module
                .image
                .contains_address(image)
                .then_some((module, image))
        })
    }

    /// A C++ object of a polymorphic class is the object its vtable pointer
    /// belongs to (the Itanium ABI): the pointer lies in the vtable group a
    /// `vtable for X` symbol names, the word before the pointer's target
    /// says how far the whole object begins before this one, and that
    /// object's own vtable pointer must be the group's primary one.
    fn cpp_dynamic(
        &mut self,
        type_info: &TypeInfo,
        place: &StopPlace,
    ) -> std::result::Result<Option<Dynamic>, Stop> {
        if !polymorphic(self, type_info.reference, 0) {
            return Ok(None);
        }

        let ValueStorage::Memory(address) = place.located.storage else {
            return Ok(None);
        };

        let address = address.get();
        let vptr = self.word(address)?;
        let Some((module, image)) = self.module_at(vptr) else {
            return Ok(None);
        };

        let Some((class, group)) = module.image.vtable_class(image) else {
            return Ok(None);
        };

        let offset_to_top = self.word(vptr.wrapping_sub(16))?.cast_signed();
        let Some(whole) = address.checked_add_signed(offset_to_top) else {
            return Ok(None);
        };

        let primary = module
            .loaded
            .load_bias
            .wrapping_load(group)
            .get()
            .wrapping_add(16);
        if self.word(whole)? != primary {
            return Ok(None);
        }

        let Some(ty) = one_type(&module.image, &class) else {
            return Ok(None);
        };

        if whole == address && module.image.same_type(ty, type_info.reference) {
            return Ok(None);
        }

        Ok(Some(Dynamic::at(module, ty, whole)))
    }

    /// A Rust trait object, `{pointer, vtable}` with a `dyn` pointee, holds
    /// the value of the type its vtable, `<C as Trait>::{vtable}`, is for.
    fn rust_dynamic(
        &mut self,
        type_info: &TypeInfo,
        place: &StopPlace,
    ) -> std::result::Result<Option<Dynamic>, Stop> {
        let TypeKind::Record { members, .. } = &type_info.kind else {
            return Ok(None);
        };

        let member = |name: &str| {
            members
                .iter()
                .find(|member| member.name.as_deref() == Some(name))
        };

        let (Some(pointer), Some(vtable)) = (member("pointer"), member("vtable")) else {
            return Ok(None);
        };

        let points_to_dyn = self
            .type_info(pointer.type_ref)
            .and_then(|info| match info.kind {
                TypeKind::Pointer {
                    target: Some(target),
                    ..
                } => self.type_info(target),
                _ => None,
            })
            .is_some_and(|target| target.name.starts_with("dyn "));
        let (
            true,
            ValueStorage::Memory(address),
            crate::RecordMemberLayout::ByteOffset(pointer_offset),
            crate::RecordMemberLayout::ByteOffset(vtable_offset),
        ) = (
            points_to_dyn,
            &place.located.storage,
            pointer.layout,
            vtable.layout,
        )
        else {
            return Ok(None);
        };

        let address = address.get();
        let data = self.word(address + pointer_offset)?;
        let table = self.word(address + vtable_offset)?;
        let Some((module, image)) = self.module_at(table) else {
            return Ok(None);
        };

        let Some(ty) = module.image.trait_object_type(image) else {
            return Ok(None);
        };

        Ok(Some(Dynamic::at(module, ty, data)))
    }

    /// A Go interface holds what the runtime the module carries says it
    /// does: the value of the type its runtime type descriptor describes,
    /// which a type's `DW_AT_go_runtime_type` names, or nothing. Where the
    /// runtime cannot say, the interface shows as the record it is.
    fn go_dynamic(&self, type_info: &TypeInfo, place: &StopPlace) -> Option<Dynamic> {
        let (_, record) = crate::eval::types::representation(self, type_info.reference).ok()?;
        let TypeKind::Record { .. } = &record.kind else {
            return None;
        };

        let stored = match &place.located.storage {
            ValueStorage::Memory(address) => StoredValue::Memory(*address),
            ValueStorage::Bytes {
                raw, start, end, ..
            } => StoredValue::Bytes(raw.get(*start..*end)?),
            _ => return None,
        };

        let Some(Ok(held)) = self
            .frame
            .runtime_dynamic(place.module, &record.name, stored)
        else {
            return None;
        };

        let DynamicValue::Held {
            descriptor,
            offset,
            place: held,
        } = held
        else {
            return Some(Dynamic::Nil);
        };

        let (module, _) = self.module_at(descriptor.get())?;
        let ty = module.image.go_runtime_type(offset)?;

        match (held, &place.located.storage) {
            (HeldPlace::Memory(address), _) => Some(Dynamic::at(module, ty, address.get())),
            // A value stored directly in an interface held in bytes is
            // those of its data word.
            (
                HeldPlace::Within(within),
                ValueStorage::Bytes {
                    source, raw, start, ..
                },
            ) => {
                let size = self.type_info(ty)?.byte_size?;
                let begin = start.checked_add(usize::try_from(within).ok()?)?;
                let finish = begin.checked_add(usize::try_from(size).ok()?)?;
                (finish <= raw.len()).then(|| Dynamic::Value {
                    ty,
                    place: StopPlace {
                        module: module.loaded.id,
                        located: Located {
                            ty: ty.id,
                            storage: ValueStorage::Bytes {
                                source: source.clone(),
                                raw: Arc::clone(raw),
                                start: begin,
                                end: finish,
                                address: None,
                            },
                        },
                    },
                })
            }
            (HeldPlace::Within(_), _) => None,
        }
    }

    /// `value` with the presentation of what it dynamically is.
    fn with_dynamic(
        &mut self,
        value: InspectedValue,
        dynamic: Dynamic,
    ) -> std::result::Result<InspectedValue, Stop> {
        let Some(raw) = value
            .type_info
            .as_ref()
            .and_then(|info| self.place_of(info, &value.state))
        else {
            return Ok(value);
        };

        let (shape, summary, lent) = match dynamic {
            Dynamic::Nil => (PresentedShape::Empty, "nil".to_owned(), None),
            Dynamic::Value { ty, place } => {
                let module = self.module(place.module)?;
                let stored = self.materialize(module, &place.located)?;
                let presented = self.nested(self.depth + 1).presented(stored)?;
                let name = self
                    .type_info(ty)
                    .map_or_else(|| "<unknown type>".to_owned(), |info| info.name.to_string());
                let go = self
                    .type_info(ty)
                    .and_then(|info| info.identity.as_deref())
                    .is_some_and(|identity| identity.language == crate::SourceLanguage::Go);
                let pointee = if go {
                    self.pointee_summary(&presented.state)?
                } else {
                    None
                };

                let (shown, lent) = match pointee {
                    Some((pointee, lent_by_pointee)) => {
                        (pointee, lent_by_pointee.or_else(|| lent(&presented.state)))
                    }
                    None => (
                        match self.record_summary(&presented.state, 0)? {
                            Some(fields) => fields,
                            None => crate::view::summary::value(
                                presented.type_info.as_ref(),
                                &presented.state,
                            ),
                        },
                        lent(&presented.state),
                    ),
                };

                (PresentedShape::Dynamic, format!("{name} {shown}"), lent)
            }
        };

        Ok(present_as(
            value,
            built_in_presentation("dynamic types", shape, summary, &raw, lent),
        ))
    }

    /// A record's members on one line, `{x: 1, y: 2}`, with its base
    /// classes' members first, as many as fit a summary; `None` for what is
    /// no record or a view presents.
    fn record_summary(
        &mut self,
        state: &VariableState,
        depth: usize,
    ) -> std::result::Result<Option<String>, Stop> {
        let VariableState::Available {
            value: crate::VariableValue::Record,
            children: ValueChildren::Available(reference),
            presentation: None,
            text: None,
            ..
        } = state
        else {
            return Ok(None);
        };

        let reference = Arc::clone(reference);
        let mut parts = Vec::new();
        self.record_members(&reference, depth, &mut parts)?;

        let mut line = String::from("{");
        for (index, part) in parts.iter().enumerate() {
            if line.chars().count() > crate::view::summary::MAX_CHARACTERS {
                line.push_str(", …");
                break;
            }

            if index > 0 {
                line.push_str(", ");
            }

            line.push_str(part);
        }

        line.push('}');

        Ok(Some(line))
    }

    /// A pointer to a record as Go's debuggers show one an interface holds,
    /// `*{name: value, …}`, or `nil`; `None` for any other value. A pointer
    /// to a value a view presents is `*` and the view's summary, and lends
    /// the view's children.
    fn pointee_summary(
        &mut self,
        state: &VariableState,
    ) -> std::result::Result<Option<(String, Option<Lent>)>, Stop> {
        let VariableState::Available {
            value: crate::VariableValue::Address(address),
            dereference: crate::DereferenceState::Available(reference),
            ..
        } = state
        else {
            return Ok(None);
        };

        let crate::DereferenceReference {
            module,
            target_type,
            target: crate::model::DereferenceTarget::Address(target),
            ..
        } = &**reference
        else {
            return Ok(None);
        };

        if address.address.get() == 0 {
            return Ok(Some(("nil".to_owned(), None)));
        }

        let place = Located {
            ty: *target_type,
            storage: crate::model::ValueStorage::Memory(*target),
        };

        let module = self.module(*module)?;
        let pointee = self.materialize(module, &place)?;
        let presented = self.nested(self.depth + 1).presented(pointee.clone())?;
        if let VariableState::Available {
            presentation: Some(presentation),
            ..
        } = &presented.state
            && presentation.shape != PresentedShape::Raw
        {
            return Ok(Some((
                format!("*{}", presentation.summary),
                lent(&presented.state),
            )));
        }

        Ok(self
            .record_summary(&pointee.state, 0)?
            .map(|fields| (format!("*{fields}"), lent(&pointee.state))))
    }

    /// A coroutine, such as an async function's future, as the state it
    /// holds rather than the number encoding it: `suspended at main.rs:43
    /// {id: 3, label: "leaf 3"}` with what it keeps across that await,
    /// `unresumed` with what it captured, `returned`, or `panicked`. The
    /// compiler's own members, such as the awaited future and drop flags,
    /// are left out of the summary, and so are an async function's
    /// captures once it has started, which its body moved into variables
    /// of its own. The variables it shows are its children, and the rest
    /// are under `[raw]`.
    fn coroutine(
        &mut self,
        value: &InspectedValue,
    ) -> std::result::Result<Option<InspectedValue>, Stop> {
        let (
            Some(type_info),
            VariableState::Available {
                value:
                    crate::VariableValue::Variant {
                        discriminant: Some(discriminant),
                        active: Some(variant),
                    },
                children: ValueChildren::Available(raw),
                presentation: None,
                ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(None);
        };

        let image = Arc::clone(&self.module(raw.module)?.image);
        let Some(Ok(coroutine)) = image.coroutine(type_info.reference.id) else {
            return Ok(None);
        };

        let number = match discriminant {
            crate::IntegerValue::Unsigned(number) => u64::try_from(*number).ok(),
            crate::IntegerValue::Signed(number) => u64::try_from(*number).ok(),
        };

        let Some(state) = number.and_then(|number| coroutine.state(number)) else {
            return Ok(None);
        };

        let (variant, raw) = (Arc::clone(variant), Arc::clone(raw));
        let children = self.children_of(&raw)?;
        let [record] = &children[children.len().saturating_sub(variant.members.len())..] else {
            return Ok(None);
        };

        let fields = match &record.state {
            VariableState::Available {
                children: ValueChildren::Available(reference),
                ..
            } => {
                let reference = Arc::clone(reference);
                self.children_of(&reference)?
            }
            _ => Vec::new(),
        };

        // Every state but `Unresumed` ends with the captures.
        let held = if state.kind == crate::CoroutineStateKind::Unresumed
            || coroutine.kind != crate::CoroutineKind::AsyncFunction
        {
            fields.len()
        } else {
            fields.len().saturating_sub(coroutine.captures.len())
        };

        let kept = fields[..held]
            .iter()
            .enumerate()
            .filter(|(_, field)| member_name(field).is_some_and(|name| !name.starts_with("__")))
            .collect::<Vec<_>>();
        let shown = kept
            .iter()
            .map(|(_, field)| {
                format!(
                    "{}: {}",
                    member_name(field).unwrap_or("<anonymous>"),
                    crate::view::summary::value(Some(&field.type_info), &field.state)
                )
            })
            .collect::<Vec<_>>();
        let summary = coroutine_summary(&image, state, &shown);

        // Its children are the variables it keeps; the awaited future,
        // drop flags, and moved captures are under `[raw]`.
        let picked = kept
            .iter()
            .map(|(index, _)| *index as u64)
            .collect::<Arc<[u64]>>();
        let mut presentation = built_in_presentation(
            "Rust coroutines",
            PresentedShape::Value,
            summary,
            &raw,
            lent(&record.state).map(|(inner, _)| (inner, picked.len() as u64)),
        );

        if let ValueChildren::Available(reference) = &mut presentation.children {
            let reference = Arc::make_mut(reference);
            if let Some(view) = &mut reference.view {
                view.picked = Some(picked);
            }
        }

        Ok(Some(present_as(value.clone(), presentation)))
    }

    /// A Rust tuple, `(1, "two")`, or tuple struct, `Meters(7)`, as Rust
    /// writes one: a record whose members are `__0`, `__1`, and on.
    fn rust_tuple(
        &mut self,
        value: &InspectedValue,
    ) -> std::result::Result<Option<InspectedValue>, Stop> {
        let (
            Some(type_info),
            VariableState::Available {
                value: crate::VariableValue::Record,
                children: ValueChildren::Available(reference),
                presentation: None,
                ..
            },
        ) = (&value.type_info, &value.state)
        else {
            return Ok(None);
        };

        let Some(identity) = type_info.identity.as_ref() else {
            return Ok(None);
        };

        if identity.language != crate::SourceLanguage::Rust || reference.total() == 0 {
            return Ok(None);
        }

        let reference = Arc::clone(reference);
        let children = self.children_of(&reference)?;
        let positional = !children.is_empty()
            && children.iter().enumerate().all(|(index, child)| {
                matches!(&child.relationship, ValueChildRelationship::Member(member)
                    if !member.artificial
                        && member.name.as_deref() == Some(format!("__{index}").as_str()))
            });

        if !positional {
            return Ok(None);
        }

        let fields = children
            .iter()
            .map(|child| crate::view::summary::value(Some(&child.type_info), &child.state))
            .collect::<Vec<_>>()
            .join(", ");

        // rustc names a tuple by its elements' types, `(i32, &str)`.
        let name = if type_info.name.starts_with('(') {
            ""
        } else {
            identity.base.as_ref()
        };

        let summary = format!("{name}({fields})");

        Ok(Some(present_as(
            value.clone(),
            built_in_presentation(
                crate::model::RUST_TUPLES,
                PresentedShape::Value,
                summary,
                &reference,
                lent(&value.state),
            ),
        )))
    }

    /// Whether a type is a Go pointer, which Go's debuggers show as what it
    /// points to.
    fn is_go_pointer(&self, type_info: &TypeInfo) -> bool {
        let chain = typedef_chain(self, type_info.reference);
        chain.iter().any(|info| {
            info.identity
                .as_ref()
                .is_some_and(|identity| identity.language == crate::SourceLanguage::Go)
        }) && chain.last().is_some_and(|info| {
            matches!(
                info.kind,
                TypeKind::Pointer {
                    target: Some(_),
                    ..
                }
            )
        })
    }

    /// The `name: value` parts of a record's members, its bases' first.
    fn record_members(
        &mut self,
        reference: &ValueChildrenReference,
        depth: usize,
        parts: &mut Vec<String>,
    ) -> std::result::Result<(), Stop> {
        if depth > 4 || parts.len() >= crate::view::summary::MAX_ELEMENTS {
            return Ok(());
        }

        for child in self.children_of(reference)? {
            match &child.relationship {
                ValueChildRelationship::Base(_) => {
                    if let VariableState::Available {
                        children: ValueChildren::Available(base),
                        ..
                    } = &child.state
                    {
                        let base = Arc::clone(base);
                        self.record_members(&base, depth + 1, parts)?;
                    }
                }
                ValueChildRelationship::Member(member) if !member.artificial => {
                    parts.push(format!(
                        "{}: {}",
                        member.name.as_deref().unwrap_or("<anonymous>"),
                        crate::view::summary::value(Some(&child.type_info), &child.state)
                    ));
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// A value's children as stored, each presented one level deeper.
    fn children_of(
        &mut self,
        reference: &ValueChildrenReference,
    ) -> std::result::Result<Vec<ValueChild>, Stop> {
        if reference.total == 0 {
            return Ok(Vec::new());
        }

        let module = self.module(reference.module)?;
        let mut runtime = self.frame.runtime(module);
        let limit = u32::try_from(reference.total.min(u64::from(MAX_SUM_CHILDREN)))
            .unwrap_or(MAX_SUM_CHILDREN);
        let page = module
            .variables
            .value_children(reference, 0, limit, &mut runtime, self.budget)
            .map_err(Stop::Failed)?;

        let mut machine = self.nested(self.depth + 1);
        let mut children = page.children.to_vec();

        for child in &mut children {
            let state = std::mem::replace(
                &mut child.state,
                VariableState::Unavailable(crate::VariableUnavailableReason::EvaluationLimit),
            );
            machine.dynamic = !matches!(child.relationship, ValueChildRelationship::Base(_));
            let presented =
                machine.presented(machine.finish(Some(child.type_info.clone()), state))?;
            child.state = presented.state;
        }

        Ok(children)
    }

    /// Runs a view one level deeper. A view presenting a value at the top
    /// runs on a share of the budget, so running out never takes the rest
    /// of the inspection with it; the values inside it share that share.
    pub(super) fn in_share<T>(&mut self, run: impl FnOnce(&mut StopMachine<'_, '_, P>) -> T) -> T {
        if self.depth > 0 {
            return run(&mut self.nested(self.depth + 1));
        }

        let mut share = self.budget.share();
        let result = {
            let mut machine = StopMachine::new(self.frame, &mut share, self.interruptible);
            machine.depth = self.depth + 1;
            run(&mut machine)
        };

        self.budget.absorb(share.usage());
        result
    }

    /// Where the value a view presents is: a slice's descriptor, though its
    /// children are its elements, or anything else's place.
    fn presented_place(
        &self,
        type_info: &TypeInfo,
        state: &VariableState,
    ) -> Option<Arc<ValueChildrenReference>> {
        match state {
            VariableState::Available {
                value: crate::VariableValue::Slice { .. },
                ..
            } => self.storage_of(type_info, state),
            state => self.place_of(type_info, state),
        }
    }

    /// Where a value of `type_info` is, as a capability over its storage.
    fn place_of(
        &self,
        type_info: &TypeInfo,
        state: &VariableState,
    ) -> Option<Arc<ValueChildrenReference>> {
        let VariableState::Available { children, .. } = state else {
            return None;
        };

        if let ValueChildren::Available(reference) = children {
            return Some(Arc::clone(reference));
        }

        self.storage_of(type_info, state)
    }

    /// Where a value of `type_info` was read from, as a capability over that
    /// storage, whatever its children say.
    fn storage_of(
        &self,
        type_info: &TypeInfo,
        state: &VariableState,
    ) -> Option<Arc<ValueChildrenReference>> {
        let VariableState::Available { source, raw, .. } = state else {
            return None;
        };

        let storage = match (source, raw) {
            (crate::VariableValueSource::Memory(address), _) => ValueStorage::Memory(*address),
            (source, Some(raw)) => ValueStorage::Bytes {
                source: source.clone(),
                raw: Arc::clone(raw),
                start: 0,
                end: raw.len(),
                address: None,
            },
            _ => return None,
        };

        let module = self.frame.controller.module_of(type_info.reference)?;
        let context = self.context(module);

        Some(Arc::new(ValueChildrenReference {
            stop_id: context.stop_id,
            context: context.context,
            frame: context.frame,
            module: context.module,
            image: context.image,
            context_address: context.address,
            target_type: type_info.reference.id,
            storage,
            total: 0,
            active_variant: None,
            view: None,
            placement: None,
        }))
    }

    /// The text a view presents the value at `place` as, when it presents
    /// values of `ty` as text, or the presentation that says why it could
    /// not.
    fn pointee_text(
        &mut self,
        ty: TypeReference,
        place: StopPlace,
    ) -> std::result::Result<(Option<crate::TextSummary>, Option<Presentation>), Stop> {
        let Some(bound) = self.frame.controller.view_choice(ty).bound.clone() else {
            return Ok((None, None));
        };

        if !bound.shape.has_text() {
            return Ok((None, None));
        }

        // A text view scans nothing.
        let result = self.in_share(|machine| {
            crate::view::run::present(&bound, machine, place.into(), &mut Checkpoints::default())
        });

        match result {
            Ok(presented) => Ok((presented.text, None)),
            Err(Failure::Problem(problem)) => Ok((
                None,
                Some(failed(crate::view::name_of(&bound.view), problem)),
            )),
            Err(Failure::Debugger(error)) => Err(Stop::Failed(error)),
        }
    }

    /// The place of element `index` of a value a view presents, for
    /// `v[i]`.
    pub(super) fn view_element(
        &mut self,
        bound: &Arc<ViewBound>,
        from: &StopPlace,
        index: i128,
    ) -> std::result::Result<Place, Stop> {
        let controller = self.frame.controller;
        let stop = self.frame.stop_id;
        let key = scan_key(bound, self.module(from.module)?.loaded.image, from);
        let mut machine = self.nested(self.depth);

        controller
            .views
            .scan(stop, key, |checkpoints| {
                crate::view::run::element_place(
                    bound,
                    &mut machine,
                    from.clone().into(),
                    index,
                    checkpoints,
                )
            })
            .map_err(|failure| view_stop(failure, bound, ErrorKind::Unsupported))
    }

    /// The place of the value the view presents the value at `from` as,
    /// for `*x`.
    pub(super) fn view_presented(
        &mut self,
        bound: &Arc<ViewBound>,
        from: &StopPlace,
    ) -> std::result::Result<Place, Stop> {
        let mut machine = self.nested(self.depth);

        crate::view::run::presented_place(bound, &mut machine, from.clone().into())
            .map_err(|failure| view_stop(failure, bound, ErrorKind::Type))
    }

    /// The places of the elements of the value at `from`, when its view
    /// presents it as a tuple.
    pub(super) fn view_tuple(
        &mut self,
        bound: &Arc<ViewBound>,
        from: &StopPlace,
    ) -> std::result::Result<Option<Vec<Place>>, Stop> {
        let mut machine = self.nested(self.depth);

        crate::view::run::tuple_places(bound, &mut machine, from.clone().into())
            .map_err(|failure| view_stop(failure, bound, ErrorKind::Type))
    }

    /// The place of the value the map at `from` holds for `key`, as a view
    /// presents the map, for `m[key]`.
    pub(super) fn view_entry_place(
        &mut self,
        bound: &Arc<ViewBound>,
        from: &StopPlace,
        key: &crate::eval::target::Key,
    ) -> std::result::Result<Option<Place>, Stop> {
        let controller = self.frame.controller;
        let stop = self.frame.stop_id;
        let scan = scan_key(bound, self.module(from.module)?.loaded.image, from);
        let mut machine = self.nested(self.depth);

        controller
            .views
            .scan(stop, scan, |checkpoints| {
                crate::view::run::entry_place(
                    bound,
                    &mut machine,
                    from.clone().into(),
                    key,
                    checkpoints,
                )
            })
            .map_err(|failure| view_stop(failure, bound, ErrorKind::Type))
    }

    /// The view that presents the value at `at`, when views are on.
    pub(super) fn view_of(
        &self,
        at: &StopPlace,
    ) -> std::result::Result<Option<Arc<ViewBound>>, Stop> {
        let controller = self.frame.controller;
        if !controller.views.enabled {
            return Ok(None);
        }

        let ty = TypeReference {
            image: self.module(at.module)?.loaded.image,
            id: at.located.ty,
        };

        Ok(controller.view_choice(ty).bound.clone())
    }

    /// How many elements the value at `at` has room for, as the
    /// `capacity` field of its view says, for `cap(v)`.
    pub(super) fn view_capacity(
        &mut self,
        at: &StopPlace,
    ) -> std::result::Result<Option<u64>, Stop> {
        let Some(bound) = self.view_of(at)? else {
            return Ok(None);
        };

        let mut machine = self.nested(self.depth);

        crate::view::run::capacity(&bound, &mut machine, at.clone().into())
            .map_err(|failure| view_stop(failure, &bound, ErrorKind::Type))
    }

    /// Where the text a view presents the value at `at` as is, for slicing
    /// it: `None` when no view presents it as text.
    pub(super) fn view_text_span(
        &mut self,
        at: &StopPlace,
    ) -> std::result::Result<Option<(u64, Option<u64>)>, Stop> {
        let Some(bound) = self.view_of(at)? else {
            return Ok(None);
        };

        if !bound.shape.has_text() {
            return Ok(None);
        }

        let mut machine = self.nested(self.depth);

        crate::view::run::text_span(&bound, &mut machine, at.clone().into())
            .map_err(|failure| view_stop(failure, &bound, ErrorKind::Type))
    }

    /// How many elements the value at `at` holds, or the length of its
    /// text, as the view of its type presents it, for `len(v)`.
    pub(super) fn view_length(&mut self, at: &StopPlace) -> std::result::Result<Option<u64>, Stop> {
        let controller = self.frame.controller;
        if !controller.views.enabled {
            return Ok(None);
        }

        let ty = TypeReference {
            image: self.module(at.module)?.loaded.image,
            id: at.located.ty,
        };

        let Some(bound) = controller.view_choice(ty).bound.clone() else {
            return Ok(None);
        };

        let stop = self.frame.stop_id;
        let key = scan_key(&bound, ty.image, at);
        let mut machine = self.nested(self.depth);

        controller
            .views
            .scan(stop, key, |checkpoints| {
                crate::view::run::length(&bound, &mut machine, at.clone().into(), checkpoints)
            })
            .map(Some)
            .map_err(|failure| view_stop(failure, &bound, ErrorKind::Type))
    }

    /// Presents a value the provider read in place. A value whose
    /// presentation needed what the budget or the program could not provide
    /// is missing for that reason, as any value is, rather than failing the
    /// inspection it is part of.
    pub(super) fn present_state(
        &mut self,
        type_info: Option<TypeInfo>,
        state: &mut VariableState,
    ) -> Result<()> {
        let read = std::mem::replace(
            state,
            VariableState::Unavailable(crate::VariableUnavailableReason::EvaluationLimit),
        );

        *state = match self.presented(self.finish(type_info, read)) {
            Ok(value) => value.state,
            Err(Stop::Missing(missing)) => *missing,
            Err(stop) => return Err(stopped(stop)),
        };

        Ok(())
    }
}
