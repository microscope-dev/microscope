//! Values as rows any front end shows: the debug adapter's variables and the
//! web page's value tree are one presentation.
//!
//! A row's text is the same summary the console's `print` shows. Values
//! without one, such as optimized-out variables, are still rows whose text
//! explains why. Aggregates expand to their children, and pointers expand
//! to what they point to. Front ends decide how a row is named on their
//! wire and how its expansion is referred to later; nothing here keeps
//! state between requests.

pub mod breakpoints;
pub mod complete;
mod completing;
pub mod debug_information;
pub mod frames;
pub mod python_files;
pub mod syscalls;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use microscope::{
    DebuggerHandle, DereferenceReference, DereferenceState, Expression, ImageAddress,
    InspectionExhaustion, InspectionLimits, IntegerValue, LoadBias, ModuleId, ModuleImage,
    PresentedShape, ScalarValue, SourceLocation, StopContext, SymbolKind, TypeInfo, ValueChild,
    ValueChildQuery, ValueChildRelationship, ValueChildren, ValueChildrenReference, VariableKind,
    VariableSnapshot, VariableState, VariableValue, VariableValueSource, VirtualAddress,
};

use crate::cli::value::summary;

/// The most children one debugger request reads.
pub const PAGE: u32 = 256;

/// One value to present.
pub struct Item<'a> {
    pub name: &'a str,

    /// How to evaluate the value again, when it can be.
    pub path: Option<Expression>,

    /// Whether the value is `[raw]`, the value as stored, which its path
    /// would evaluate as its view presents it: its own children are reached
    /// through the path, but it is not named by it.
    pub raw: bool,
    pub type_info: Option<&'a TypeInfo>,
    pub state: &'a VariableState,

    /// Where the value's variable is declared, in a module's sources.
    pub declaration: Option<(ModuleId, SourceLocation)>,
}

/// A presented value.
#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,

    /// The value's summary, or why there is none.
    pub text: String,

    /// The type's name, when the type is known.
    pub type_name: Option<Arc<str>>,
    pub declaration: Option<(ModuleId, SourceLocation)>,

    /// How to evaluate the value again, which its children extend.
    pub path: Option<Expression>,

    /// Whether the row is `[raw]`, which its path does not name.
    pub raw: bool,

    /// What an available value offers; none for a value that has none.
    pub details: Option<Details>,
}

impl Row {
    /// The path that names the row itself, when one does.
    pub fn evaluate_name(&self) -> Option<&Expression> {
        self.path.as_ref().filter(|_| !self.raw)
    }
}

/// What an available value offers beyond its text.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent fact about the value"
)]
pub struct Details {
    pub expand: Option<Expand>,

    /// The function a pointer enters, by address.
    pub code: Option<u64>,

    /// The value's natural memory: what a pointer points to, or where the
    /// value is stored.
    pub memory: Option<u64>,

    /// How many bytes the value occupies at `memory`, when it is stored
    /// there and its size is known: a pointer's pointee has none.
    pub memory_bytes: Option<u64>,

    /// Whether assigning to the row's path can change it: numbers,
    /// enumerations, and pointers in memory, or whole variables in the
    /// innermost frame's registers.
    pub editable: bool,
    pub constant: bool,

    /// Whether a view presents the value as text.
    pub text_view: bool,

    /// Whether the value is a record, union, or variant.
    pub aggregate: bool,

    /// The renderers of the drawings its view offers, each once, which only
    /// the web page draws.
    pub drawings: Vec<Arc<str>>,

    /// The value's text over as many lines as its runtime writes it, where
    /// the row's one line shows less.
    pub detail: Option<Arc<str>>,
}

/// How a row expands.
#[derive(Debug, Clone)]
pub enum Expand {
    Children {
        reference: Arc<ValueChildrenReference>,
        counts: Counts,

        /// Whether the children are all elements (`Some(true)`), all named
        /// (`Some(false)`), or a view's elements followed by its named
        /// fields and `[raw]` (`None`).
        indexed: Option<bool>,
    },

    /// What a pointer points to.
    Pointee(Box<DereferenceReference>),
}

/// How many of a row's children are elements and how many are named.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub indexed: Option<u64>,
    pub named: Option<u64>,
}

/// One entry of a listing: a value, or where the listing stopped short,
/// such as at its memory limit.
#[derive(Debug, Clone)]
pub enum Listed {
    Value(Box<Row>),
    Truncated(String),
}

/// The renderers of the drawings a presentation offers, each once.
#[must_use]
pub fn drawings(presentation: &microscope::Presentation) -> Vec<Arc<str>> {
    let mut names = Vec::<Arc<str>>::new();
    for visualizer in presentation.visualizers.iter() {
        if !names.contains(&visualizer.renderer.name) {
            names.push(Arc::clone(&visualizer.renderer.name));
        }
    }

    names
}

/// The part of a listing a request asks for.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub start: u64,
    pub count: u64,

    /// Which children to list: a view's elements or its named children.
    pub filter: Option<Filter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Indexed,
    Named,
}

impl Window {
    /// Every row.
    pub const ALL: Self = Self {
        start: 0,
        count: u64::MAX,
        filter: None,
    };

    pub fn slice<T>(self, items: impl Iterator<Item = T>) -> impl Iterator<Item = T> {
        items
            .skip(usize::try_from(self.start).unwrap_or(usize::MAX))
            .take(usize::try_from(self.count).unwrap_or(usize::MAX))
    }
}

/// Why a listing failed.
#[derive(Debug)]
pub enum ListError {
    Debugger(microscope::Error),

    /// The program changed so the listing no longer applies.
    Changed(&'static str),
}

impl From<microscope::Error> for ListError {
    fn from(error: microscope::Error) -> Self {
        Self::Debugger(error)
    }
}

/// The loaded modules' code, to tell which addresses enter a function.
#[derive(Debug, Default)]
pub struct Code {
    /// Each module's load bias and image.
    modules: Vec<(LoadBias, Arc<ModuleImage>)>,
}

impl Code {
    pub const fn new(modules: Vec<(LoadBias, Arc<ModuleImage>)>) -> Self {
        Self { modules }
    }

    /// Each module's load bias and image.
    pub fn modules(&self) -> &[(LoadBias, Arc<ModuleImage>)] {
        &self.modules
    }

    /// The image and image address of the function an address enters, when
    /// it is a function's first instruction rather than any other address.
    pub fn function_entry(
        &self,
        address: VirtualAddress,
    ) -> Option<(&Arc<ModuleImage>, ImageAddress)> {
        self.modules.iter().find_map(|(bias, image)| {
            let address = bias.unload(address)?;
            if !image.contains_address(address) {
                return None;
            }

            let symbol = image.symbolize(address)?;
            (symbol.offset == 0
                && matches!(
                    symbol.kind,
                    SymbolKind::Function | SymbolKind::IndirectFunction
                ))
            .then_some((image, address))
        })
    }
}

/// Presents values at one debugger, in one style.
pub struct Presenter<'a> {
    pub handle: &'a DebuggerHandle,
    pub code: &'a Code,

    /// Whether integers show in hexadecimal.
    pub hex: bool,
}

impl Presenter<'_> {
    /// Presents one value.
    pub fn row(&self, item: Item<'_>, context: StopContext) -> Row {
        let details = match item.state {
            VariableState::Available {
                source,
                value,
                dereference,
                children,
                presentation,
                ..
            } => {
                // A value a view presents expands to its elements, its
                // fields, and `[raw]`, the value as stored.
                let presented = presentation
                    .as_deref()
                    .filter(|presentation| presentation.shape != PresentedShape::Raw);
                let children = presented.map_or(children, |presentation| &presentation.children);
                let expand = if let ValueChildren::Available(children) = children {
                    let (counts, indexed) = counts(children, value);
                    Some(Expand::Children {
                        reference: children.clone(),
                        counts,
                        indexed,
                    })
                } else if let DereferenceState::Available(dereference) = dereference {
                    Some(Expand::Pointee(dereference.clone()))
                } else {
                    None
                };

                // A pointer to a function, or a function value, leads to
                // the function's code.
                let code = called(value)
                    .filter(|address| self.code.function_entry(*address).is_some())
                    .map(VirtualAddress::get);

                // A pointer's natural memory is what it points to.
                let memory = match (value, source) {
                    (VariableValue::Address(address), _) => Some(address.address.get()),
                    (_, VariableValueSource::Memory(address)) => Some(address.get()),
                    _ => None,
                };

                let memory_bytes = match (value, source) {
                    (VariableValue::Address(_), _) => None,
                    (_, VariableValueSource::Memory(_)) => {
                        item.type_info.and_then(|info| info.byte_size)
                    }
                    _ => None,
                };

                let whole = item.path.as_ref().is_some_and(Expression::is_name);

                Some(Details {
                    expand,
                    code,
                    memory,
                    memory_bytes,
                    editable: editable(source, value, context, whole, item.path.is_some()),
                    constant: matches!(source, VariableValueSource::Constant),
                    text_view: presented
                        .is_some_and(|presentation| presentation.shape == PresentedShape::Text),
                    aggregate: matches!(
                        value,
                        VariableValue::Record
                            | VariableValue::Union
                            | VariableValue::Variant { .. }
                    ),
                    drawings: presented.map_or_else(Vec::new, drawings),
                    detail: presented.and_then(|presentation| presentation.detail.clone()),
                })
            }
            _ => None,
        };

        Row {
            name: item.name.to_owned(),
            text: text(item.type_info, item.state, self.hex),
            type_name: item.type_info.map(|type_info| type_info.name.clone()),
            declaration: item.declaration,
            path: item.path,
            raw: item.raw,
            details,
        }
    }

    fn listed(&self, item: Item<'_>, context: StopContext) -> Listed {
        Listed::Value(Box::new(self.row(item, context)))
    }

    /// Presents a frame's parameters or locals, from its variables.
    pub async fn scope(
        &self,
        context: StopContext,
        snapshot: &VariableSnapshot,
        kind: VariableKind,
        module: Option<ModuleId>,
        window: Window,
    ) -> Vec<Listed> {
        let unnamed = self.unnamed_variables(context, snapshot).await;
        let mut rows = Vec::new();

        for (index, variable) in window.slice(
            snapshot
                .variables
                .iter()
                .enumerate()
                .filter(|(_, variable)| in_scope(kind, variable.kind)),
        ) {
            // A value a finished function returned is no variable of this
            // frame, and no name reaches it.
            let returned = variable.kind == VariableKind::Returned;
            let path = if unnamed.contains(&index) || returned {
                None
            } else {
                Expression::name(&variable.name)
            };

            let name = if returned {
                format!("returned {}", variable.name)
            } else {
                variable.name.to_string()
            };

            rows.push(self.listed(
                Item {
                    name: &name,
                    path,
                    raw: false,
                    type_info: variable.type_info.as_ref(),
                    state: &variable.state,
                    declaration: module.zip(variable.declaration.clone()),
                },
                context,
            ));
        }

        if kind == VariableKind::Local
            && let Some(exhaustion) = snapshot.completion.exhaustion()
        {
            rows.push(Listed::Truncated(format!(
                "not every variable was read: the {:?} limit is {}",
                exhaustion.resource, exhaustion.limit
            )));
        }

        rows
    }

    /// The frame's variables their names do not reach, because another of
    /// the frame's variables has the same name, such as one an inner block
    /// hides: all but the one the name binds, told apart by their storage,
    /// or all of them when their storage cannot tell.
    async fn unnamed_variables(
        &self,
        context: StopContext,
        snapshot: &VariableSnapshot,
    ) -> BTreeSet<usize> {
        let storage = |state: &VariableState| match state {
            VariableState::Available {
                source: source @ (VariableValueSource::Memory(_) | VariableValueSource::Register(_)),
                ..
            } => Some(source.clone()),
            _ => None,
        };

        let mut by_name = BTreeMap::<&str, Vec<usize>>::new();

        for (index, variable) in snapshot.variables.iter().enumerate() {
            if variable.kind != VariableKind::Returned {
                by_name.entry(&variable.name).or_default().push(index);
            }
        }

        let mut unnamed = BTreeSet::new();

        for (name, indices) in by_name.into_iter().filter(|(_, indices)| indices.len() > 1) {
            let bound = if let Some(expression) = Expression::name(name)
                && let Ok(microscope::Evaluation::Value { value, .. }) =
                    self.handle.at(context).evaluate(&expression).await
            {
                storage(&value.state)
            } else {
                None
            };

            let matching = indices
                .iter()
                .copied()
                .filter(|index| {
                    bound.is_some() && storage(&snapshot.variables[*index].state) == bound
                })
                .collect::<Vec<_>>();

            for index in indices {
                if matching != [index] {
                    unnamed.insert(index);
                }
            }
        }

        unnamed
    }

    /// Presents the static variables declared in a source file of `image`,
    /// loaded as `module`, as the frame's thread sees them.
    pub async fn statics(
        &self,
        context: StopContext,
        image: &ModuleImage,
        module: ModuleId,
        file: microscope::SourceFileId,
        window: Window,
    ) -> Result<Vec<Listed>, ListError> {
        let mut rows = Vec::new();
        let declared = image.globals().filter(|global| {
            global
                .declaration
                .as_ref()
                .is_some_and(|declaration| declaration.file == file)
        });

        for global in window.slice(declared) {
            let variable = self
                .handle
                .at(context)
                .global_with_limits(
                    microscope::GlobalVariableReference {
                        module,
                        image: image.id(),
                        variable: global.id,
                    },
                    InspectionLimits::default(),
                )
                .await?;
            rows.push(
                self.listed(
                    Item {
                        name: &variable.name,
                        path: global_expression(image, &global),
                        raw: false,
                        type_info: variable.type_info.as_ref(),
                        state: &variable.state,
                        declaration: global
                            .declaration
                            .clone()
                            .map(|declaration| (module, declaration)),
                    },
                    context,
                ),
            );
        }

        Ok(rows)
    }

    /// Presents what a pointer named `name` points to: an aggregate's
    /// members directly, or else the one value.
    pub async fn pointee(
        &self,
        context: StopContext,
        reference: Box<DereferenceReference>,
        name: &str,
        path: Option<Expression>,
        window: Window,
    ) -> Result<Vec<Listed>, ListError> {
        let pointee = self.handle.dereference(reference).await?;
        let path = path.and_then(|path| path.dereferenced());
        if let VariableState::Available {
            children,
            presentation,
            ..
        } = &pointee.state
        {
            let children = presentation
                .as_deref()
                .filter(|presentation| presentation.shape != PresentedShape::Raw)
                .map_or(children, |presentation| &presentation.children);
            if let ValueChildren::Available(children) = children {
                return self
                    .children(context, children.clone(), path.as_ref(), window)
                    .await;
            }
        }

        if window.start != 0 {
            return Ok(Vec::new());
        }

        Ok(vec![self.listed(
            Item {
                name: &format!("*{name}"),
                path,
                raw: false,
                type_info: Some(&pointee.type_info),
                state: &pointee.state,
                declaration: None,
            },
            context,
        )])
    }

    /// Presents a window of an evaluated range's elements.
    pub async fn range(
        &self,
        context: StopContext,
        expression: &Expression,
        window: Window,
    ) -> Result<Vec<Listed>, ListError> {
        let evaluation = self.handle.at(context).evaluate(expression).await?;
        let microscope::Evaluation::Range(page) = evaluation else {
            return Err(ListError::Changed(
                "the range no longer evaluates to elements",
            ));
        };

        let base = expression.range_base();
        let mut rows = Vec::new();

        for child in window.slice(page.children.iter()) {
            rows.push(self.listed(
                Item {
                    name: &child_name(child),
                    path: child_path(base.as_ref(), child),
                    raw: false,
                    type_info: Some(&child.type_info),
                    state: &child.state,
                    declaration: None,
                },
                context,
            ));
        }

        if let Some(exhaustion) = page.completion.exhaustion() {
            rows.push(Listed::Truncated(limit_text(exhaustion)));
        }

        Ok(rows)
    }

    /// Presents a window of an aggregate's children, reading them in pages.
    pub async fn children(
        &self,
        context: StopContext,
        reference: Arc<ValueChildrenReference>,
        path: Option<&Expression>,
        window: Window,
    ) -> Result<Vec<Listed>, ListError> {
        // A view's elements come first and its named children after them;
        // anything else's children are all of one kind.
        let (start, end) = match (window.filter, reference.elements()) {
            (Some(Filter::Indexed), Some(elements)) => (window.start, elements),
            (Some(Filter::Named), Some(elements)) => {
                (elements.saturating_add(window.start), reference.total())
            }
            _ => (window.start, reference.total()),
        };

        let end = end.min(start.saturating_add(window.count));
        let mut rows = Vec::new();
        let mut offset = start;

        while offset < end {
            let limit = u32::try_from(end - offset).unwrap_or(PAGE).min(PAGE);
            let page = self
                .handle
                .value_children(reference.clone(), ValueChildQuery { offset, limit })
                .await?;

            for child in page.children.iter().filter(|child| shown(child)) {
                rows.push(self.listed(
                    Item {
                        name: &child_name(child),
                        path: child_path(path, child),
                        raw: matches!(child.relationship, ValueChildRelationship::Raw),
                        type_info: Some(&child.type_info),
                        state: &child.state,
                        declaration: None,
                    },
                    context,
                ));
            }

            if let Some(exhaustion) = page.completion.exhaustion() {
                rows.push(Listed::Truncated(limit_text(exhaustion)));
                break;
            }

            if page.children.is_empty() {
                break;
            }

            offset += page.children.len() as u64;
        }

        Ok(rows)
    }

    /// The names of the members of the value an expression names, or of
    /// what it points to. None when it names no aggregate.
    pub async fn member_names(&self, context: StopContext, base: &str) -> Vec<String> {
        let Ok(expression) = Expression::parse(base) else {
            return Vec::new();
        };

        let Ok(microscope::Evaluation::Value { value, .. }) = self
            .handle
            .at(context)
            .evaluate_with(
                &expression,
                microscope::EvaluationMode::Read,
                InspectionLimits::default(),
            )
            .await
        else {
            return Vec::new();
        };

        let mut state = value.state;

        if let VariableState::Available {
            children: ValueChildren::NotApplicable,
            dereference: DereferenceState::Available(reference),
            ..
        } = &state
            && let Ok(pointee) = self.handle.dereference(reference.clone()).await
        {
            state = pointee.state;
        }

        let VariableState::Available {
            children: ValueChildren::Available(children),
            ..
        } = state
        else {
            return Vec::new();
        };

        let Ok(page) = self
            .handle
            .value_children(
                children,
                ValueChildQuery {
                    offset: 0,
                    limit: PAGE,
                },
            )
            .await
        else {
            return Vec::new();
        };

        page.children
            .iter()
            .filter(|child| shown(child))
            .filter_map(|child| match &child.relationship {
                ValueChildRelationship::Member(member) => member.name.as_deref().map(str::to_owned),
                _ => None,
            })
            .collect()
    }
}

/// How many children a value has, and whether they are all elements
/// (`Some(true)`), all named (`Some(false)`), or a view's elements, which
/// are indexed, followed by its fields and `[raw]`, which are named
/// (`None`). An array's or slice's children are elements, and anything
/// else's named.
const fn counts(
    children: &ValueChildrenReference,
    value: &VariableValue,
) -> (Counts, Option<bool>) {
    let total = children.total();
    match (children.elements(), value) {
        (Some(0), _) => (
            Counts {
                indexed: None,
                named: Some(total),
            },
            Some(false),
        ),
        (Some(elements), _) => (
            Counts {
                indexed: Some(elements),
                named: Some(total.saturating_sub(elements)),
            },
            None,
        ),
        (None, VariableValue::Array { .. } | VariableValue::Slice { .. }) => (
            Counts {
                indexed: Some(total),
                named: None,
            },
            Some(true),
        ),
        (None, _) => (
            Counts {
                indexed: None,
                named: Some(total),
            },
            Some(false),
        ),
    }
}

/// Whether a scope of variables of one kind lists a variable: the
/// arguments scope lists results too, as part of the signature, and the
/// locals scope what a finished function returned.
pub fn in_scope(scope: VariableKind, kind: VariableKind) -> bool {
    kind == scope
        || (scope == VariableKind::Parameter && kind == VariableKind::Result)
        || (scope == VariableKind::Local && kind == VariableKind::Returned)
}

/// The address a pointer or function value would call, if it is one.
const fn called(value: &VariableValue) -> Option<VirtualAddress> {
    match value {
        VariableValue::Address(address) => Some(address.address),
        VariableValue::Function {
            code: Some(code), ..
        } => Some(*code),
        _ => None,
    }
}

/// Whether assigning can change a value: numbers, enumerations, and
/// pointers in memory, or whole variables in the innermost frame's
/// registers, when a path names them.
fn editable(
    source: &VariableValueSource,
    value: &VariableValue,
    context: StopContext,
    whole: bool,
    named: bool,
) -> bool {
    let leaf = matches!(
        value,
        VariableValue::Scalar(_) | VariableValue::Enumeration { .. } | VariableValue::Address(_)
    );
    let storage = match source {
        VariableValueSource::Memory(_) => true,
        VariableValueSource::Register(_) => {
            context.frame == microscope::StackFrameId::INNERMOST && whole
        }
        _ => false,
    };

    leaf && storage && named
}

/// The name a child is shown with.
pub fn child_name(child: &ValueChild) -> String {
    match &child.relationship {
        ValueChildRelationship::ArrayElement { indices, .. } => {
            indices.iter().fold(String::new(), |mut name, index| {
                let _ = write!(name, "[{index}]");
                name
            })
        }

        ValueChildRelationship::SliceElement { index }
        | ValueChildRelationship::Element { index } => {
            format!("[{index}]")
        }

        // A map's entry is named by its key.
        ValueChildRelationship::Entry { key, .. } => summary(Some(&key.type_info), &key.state),
        ValueChildRelationship::Member(member) => {
            member.name.as_deref().unwrap_or("<anonymous>").to_owned()
        }
        ValueChildRelationship::Field { name } => name.to_string(),
        ValueChildRelationship::Raw => "[raw]".to_owned(),
        ValueChildRelationship::Base(_) => format!("<base {}>", child.type_info.name),
        _ => "<child>".to_owned(),
    }
}

/// Whether a child is shown: compiler-generated members are hidden, as the
/// console hides them.
pub const fn shown(child: &ValueChild) -> bool {
    !matches!(&child.relationship, ValueChildRelationship::Member(member) if member.artificial)
}

/// How to evaluate a child again, when its parent can be and the child has
/// a name.
pub fn child_path(parent: Option<&Expression>, child: &ValueChild) -> Option<Expression> {
    let parent = parent?;
    match &child.relationship {
        ValueChildRelationship::ArrayElement { indices, .. } => parent.indexed(indices),
        ValueChildRelationship::SliceElement { index }
        | ValueChildRelationship::Element { index } => parent.indexed(&[i128::from(*index)]),
        ValueChildRelationship::Member(member) => parent.member(member.name.as_deref()?),
        // The value as stored is the parent's value.
        ValueChildRelationship::Raw => Some(parent.clone()),
        // Maps are not indexed by key yet, so an entry's value is named by
        // where it is.
        ValueChildRelationship::Entry { .. } => match &child.state {
            VariableState::Available {
                source: VariableValueSource::Memory(address),
                ..
            } => Expression::at(&child.type_info.name, address.get()),
            _ => None,
        },
        _ => None,
    }
}

/// A value's text: its summary, with integers in hexadecimal when asked.
/// A value a view presents is shown as the view presents it, and one it
/// presents as a number as that number.
pub fn text(type_info: Option<&TypeInfo>, state: &VariableState, hexadecimal: bool) -> String {
    let presented = match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => {
            // A number a view presents is written as that number is.
            if hexadecimal && let Some(number) = &presentation.number {
                return text(number.type_info.as_ref(), &number.state, true);
            }

            true
        }
        _ => false,
    };

    (hexadecimal && !presented)
        .then(|| hex(type_info.and_then(|type_info| type_info.byte_size), state))
        .flatten()
        .unwrap_or_else(|| summary(type_info, state))
}

/// An integer value in hexadecimal, in the width of its type's size.
fn hex(byte_size: Option<u64>, state: &VariableState) -> Option<String> {
    let VariableState::Available { value, .. } = state else {
        return None;
    };

    let bits = byte_size
        .filter(|size| (1..=16).contains(size))
        .map_or(128, |size| size * 8);
    let mask = if bits == 128 {
        u128::MAX
    } else {
        (1_u128 << bits) - 1
    };

    let number = match value {
        VariableValue::Scalar(ScalarValue::Signed(value))
        | VariableValue::Enumeration {
            value: IntegerValue::Signed(value),
            ..
        } => value.cast_unsigned() & mask,
        VariableValue::Scalar(ScalarValue::Unsigned(value))
        | VariableValue::Enumeration {
            value: IntegerValue::Unsigned(value),
            ..
        } => *value & mask,
        _ => return None,
    };

    Some(format!("{number:#x}"))
}

/// A name that reaches a global from any frame, which the frame's own
/// locals cannot shadow: its qualified name, or, when other files declare
/// the same, that name qualified by its file's name or path.
fn global_expression(
    image: &ModuleImage,
    global: &microscope::GlobalVariableInfo,
) -> Option<Expression> {
    let mut selectors = vec![global.qualified_name.to_string()];
    if let Some(file) = global
        .declaration
        .as_ref()
        .and_then(|declaration| image.source_file(declaration.file))
    {
        if let Some(name) = file.path.file_name() {
            selectors.push(format!(
                "{}::{}",
                name.to_string_lossy(),
                global.qualified_name
            ));
        }

        selectors.push(format!(
            "{}::{}",
            file.path.display(),
            global.qualified_name
        ));
    }

    selectors
        .into_iter()
        .find(|selector| {
            image
                .global_named(selector)
                .is_ok_and(|found| found.id == global.id)
        })
        .and_then(|selector| Expression::outermost(&selector))
}

/// Whether an evaluation failed only because the frame does not know the
/// name that starts the text, `name`.
pub fn names_only(failure: &microscope::ExpressionError, name: &str) -> bool {
    failure.kind == microscope::ExpressionErrorKind::UnknownName
        && failure.span.start == 0
        && failure.span.end as usize == name.len()
}

pub fn limit_text(exhaustion: InspectionExhaustion) -> String {
    format!(
        "inspection stopped at its {:?} limit of {}",
        exhaustion.resource, exhaustion.limit
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hexadecimal_integers_use_their_types_width() {
        let state = |value| VariableState::Available {
            source: VariableValueSource::Computed,
            raw: None,
            value,
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: None,
        };

        let signed = state(VariableValue::Scalar(ScalarValue::Signed(-1)));
        assert_eq!(hex(Some(4), &signed).as_deref(), Some("0xffffffff"));
        assert_eq!(hex(Some(1), &signed).as_deref(), Some("0xff"));
        let unsigned = state(VariableValue::Scalar(ScalarValue::Unsigned(255)));
        assert_eq!(hex(None, &unsigned).as_deref(), Some("0xff"));
        let boolean = state(VariableValue::Scalar(ScalarValue::Boolean(true)));
        assert_eq!(hex(Some(1), &boolean), None);
    }

    /// An integer a view presents shows as the view presents it, even in
    /// hexadecimal: the stored number is one step away, as `[raw]`.
    #[test]
    fn hexadecimal_never_hides_what_a_view_presents() {
        let presentation = microscope::Presentation {
            view: Arc::new(microscope::ViewName {
                source: Arc::from("app.views"),
                line: 2,
                header: Arc::from("c status"),
                extend: false,
            }),
            shape: PresentedShape::Empty,
            count: None,
            summary: Arc::from("ok"),
            detail: None,
            children: ValueChildren::NotApplicable,
            problem: None,
            number: None,
            presented: None,
            visualizers: Arc::from([]),
        };

        let state = VariableState::Available {
            source: VariableValueSource::Computed,
            raw: None,
            value: VariableValue::Scalar(ScalarValue::Signed(0)),
            dereference: DereferenceState::NotApplicable,
            children: ValueChildren::NotApplicable,
            text: None,
            presentation: Some(Arc::new(presentation)),
        };

        assert_eq!(text(None, &state, true), text(None, &state, false));
    }
}
