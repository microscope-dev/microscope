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

pub const PAGE: u32 = 256;

pub struct Item<'a> {
    pub name: &'a str,

    pub path: Option<Expression>,

    pub raw: bool,
    pub type_info: Option<&'a TypeInfo>,
    pub state: &'a VariableState,

    pub declaration: Option<(ModuleId, SourceLocation)>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,

    pub text: String,

    pub type_name: Option<Arc<str>>,
    pub declaration: Option<(ModuleId, SourceLocation)>,

    pub path: Option<Expression>,

    pub raw: bool,

    pub details: Option<Details>,
}

impl Row {
    pub fn evaluate_name(&self) -> Option<&Expression> {
        self.path.as_ref().filter(|_| !self.raw)
    }
}

#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each is an independent fact about the value"
)]
pub struct Details {
    pub expand: Option<Expand>,

    pub code: Option<u64>,

    pub memory: Option<u64>,

    pub memory_bytes: Option<u64>,

    pub stored: bool,

    pub text_bytes: Option<u64>,

    pub editable: bool,
    pub constant: bool,

    pub text_view: bool,

    pub aggregate: bool,

    pub drawings: Vec<Arc<str>>,

    pub detail: Option<Arc<str>>,
}

#[derive(Debug, Clone)]
pub enum Expand {
    Children {
        reference: Arc<ValueChildrenReference>,
        counts: Counts,

        indexed: Option<bool>,
    },

    Pointee(Box<DereferenceReference>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub indexed: Option<u64>,
    pub named: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum Listed {
    Value(Box<Row>),
    Truncated(String),
}

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

#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub start: u64,
    pub count: u64,

    pub filter: Option<Filter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Indexed,
    Named,
}

impl Window {
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

#[derive(Debug)]
pub enum ListError {
    Debugger(microscope::Error),

    Changed(&'static str),
}

impl From<microscope::Error> for ListError {
    fn from(error: microscope::Error) -> Self {
        Self::Debugger(error)
    }
}

#[derive(Debug, Default)]
pub struct Code {
    modules: Vec<(LoadBias, Arc<ModuleImage>)>,
}

impl Code {
    pub const fn new(modules: Vec<(LoadBias, Arc<ModuleImage>)>) -> Self {
        Self { modules }
    }

    pub fn modules(&self) -> &[(LoadBias, Arc<ModuleImage>)] {
        &self.modules
    }

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

pub struct Presenter<'a> {
    pub handle: &'a DebuggerHandle,
    pub code: &'a Code,

    pub hex: bool,
}

impl Presenter<'_> {
    pub fn row(&self, item: Item<'_>, context: StopContext) -> Row {
        let details = match item.state {
            VariableState::Available {
                source,
                value,
                dereference,
                children,
                text,
                presentation,
                ..
            } => {
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

                let code = called(value)
                    .filter(|address| self.code.function_entry(*address).is_some())
                    .map(VirtualAddress::get);

                let memory = match (value, source) {
                    (VariableValue::Address(address), _) => Some(address.address.get()),
                    (_, VariableValueSource::Memory(address)) => Some(address.get()),
                    _ => None,
                };

                let stored_bytes = match (value, source) {
                    (VariableValue::Address(_), _) => None,
                    (_, VariableValueSource::Memory(_)) => {
                        item.type_info.and_then(|info| info.byte_size)
                    }
                    _ => None,
                };
                let memory_bytes = match value {
                    VariableValue::Address(_) => text.as_deref().and_then(|text| text.extent),
                    _ => stored_bytes,
                };

                let text_bytes = stored_bytes.and_then(|size| {
                    text.as_deref()
                        .and_then(|text| text.extent)
                        .filter(|extent| *extent < size)
                });

                let whole = item.path.as_ref().is_some_and(Expression::is_name);

                Some(Details {
                    expand,
                    code,
                    memory,
                    memory_bytes,
                    stored: stored_bytes.is_some(),
                    text_bytes,
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
                    bound.is_some()
                        && snapshot
                            .variables
                            .get(*index)
                            .is_some_and(|variable| storage(&variable.state) == bound)
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

    pub async fn children(
        &self,
        context: StopContext,
        reference: Arc<ValueChildrenReference>,
        path: Option<&Expression>,
        window: Window,
    ) -> Result<Vec<Listed>, ListError> {
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

pub const SCOPES: [(VariableKind, &str); 6] = [
    (VariableKind::Parameter, "Arguments"),
    (VariableKind::Local, "Locals"),
    (VariableKind::Captured, "Closure"),
    (VariableKind::Module, "Module"),
    (VariableKind::Global, "Global"),
    (VariableKind::Unnamed, "Unnamed locals"),
];

pub fn unnamed_reason(snapshot: &VariableSnapshot, kind: VariableKind) -> Option<&str> {
    snapshot
        .variables
        .iter()
        .filter(|variable| in_scope(kind, variable.kind))
        .find_map(|variable| variable.unnamed.as_deref())
}

pub fn in_scope(scope: VariableKind, kind: VariableKind) -> bool {
    kind == scope
        || (scope == VariableKind::Parameter && kind == VariableKind::Result)
        || (scope == VariableKind::Local && kind == VariableKind::Returned)
}

const fn called(value: &VariableValue) -> Option<VirtualAddress> {
    match value {
        VariableValue::Address(address) => Some(address.address),
        VariableValue::Function {
            code: Some(code), ..
        } => Some(*code),
        _ => None,
    }
}

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

pub const fn shown(child: &ValueChild) -> bool {
    !matches!(&child.relationship, ValueChildRelationship::Member(member) if member.artificial)
}

pub fn child_path(parent: Option<&Expression>, child: &ValueChild) -> Option<Expression> {
    let parent = parent?;
    match &child.relationship {
        ValueChildRelationship::ArrayElement { indices, .. } => parent.indexed(indices),
        ValueChildRelationship::SliceElement { index }
        | ValueChildRelationship::Element { index } => parent.indexed(&[i128::from(*index)]),
        ValueChildRelationship::Member(member) => parent.member(member.name.as_deref()?),
        ValueChildRelationship::Raw => Some(parent.clone()),
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

pub fn text(type_info: Option<&TypeInfo>, state: &VariableState, hexadecimal: bool) -> String {
    let presented = match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => {
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
