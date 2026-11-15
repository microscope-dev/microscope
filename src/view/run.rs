use std::sync::Arc;

use crate::eval::interp::{self, Outcome, Value};
use crate::eval::target::{Key, Machine, Refusal, Register, Stop, TextSpan};
use crate::eval::types::{Category, TypeSource, category};
use crate::{
    InspectedValue, PresentedShape, TextCompletion, TextSummary, TypeInfo, TypeReference,
    ValueAccessUnavailableReason, VariableState, VariableUnavailableReason, VariableValue,
    ViewProblem,
};

use super::bind::{
    BoundDynamic, BoundField, BoundFormat, BoundInputValue, BoundScan, BoundShape, BoundView,
    BoundVisualizer, TextSource, ViewObject, ViewProgram,
};

use super::pattern::{Captured, Captures};
use super::scan::{Checkpoints, Scanner, Var};
use super::summary;
use super::syntax::Pattern;

const PAGE_SIZE: u64 = 4096;

#[derive(Debug)]
pub enum Failure {
    Problem(ViewProblem),

    Debugger(crate::Error),
}

impl From<Stop> for Failure {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Missing(state) => Self::Problem(match *state {
                VariableState::Unavailable(reason) => ViewProblem::Unavailable(reason),
                VariableState::Malformed(reason) => ViewProblem::Refused(reason.description),
                VariableState::Invalid { reason, .. } => {
                    ViewProblem::Refused(reason.to_string().into())
                }
                VariableState::Available { .. } => {
                    ViewProblem::Internal("an available value stopped the view".into())
                }
            }),
            Stop::Refused(refusal) => Self::Problem(ViewProblem::Refused(refusal.message.into())),
            Stop::Failed(error) => Self::Debugger(error),
        }
    }
}

impl From<interp::Failure> for Failure {
    fn from(failure: interp::Failure) -> Self {
        match failure {
            interp::Failure::Expression(error) => {
                Self::Problem(ViewProblem::Refused(error.message.into()))
            }
            interp::Failure::Debugger(error) => Self::Debugger(error),
        }
    }
}

pub(super) fn internal(message: &str) -> Failure {
    Failure::Problem(ViewProblem::Internal(message.into()))
}

fn refused(message: impl Into<Arc<str>>) -> Failure {
    Failure::Problem(ViewProblem::Refused(message.into()))
}

fn unsupported(message: &str) -> Stop {
    Stop::Refused(Refusal::new(
        crate::ExpressionErrorKind::Unsupported,
        message,
    ))
}

#[derive(Debug)]
pub struct Presented {
    pub shape: PresentedShape,

    pub count: Option<crate::PresentedCount>,
    pub text: Option<TextSummary>,
    pub summary: String,

    pub inner: Option<InspectedValue>,

    pub partial: Option<ViewProblem>,

    pub named: u64,
}

impl Presented {
    const fn new(shape: PresentedShape, summary: String) -> Self {
        Self {
            shape,
            count: None,
            text: None,
            summary,
            inner: None,
            partial: None,
            named: 0,
        }
    }

    fn value(inner: InspectedValue) -> Self {
        Self {
            summary: summary::value(inner.type_info.as_ref(), &inner.state),
            inner: Some(inner),
            ..Self::new(PresentedShape::Value, String::new())
        }
    }
}

#[derive(Debug)]
pub enum Child {
    Element(u64, InspectedValue),

    Entry(u64, Box<InspectedValue>, InspectedValue),
    Field(Arc<str>, InspectedValue),

    Raw,
}

pub struct ViewMachine<'m, M: Machine> {
    base: &'m mut M,
    this: M::Place,
    lets: &'m [ViewProgram<M::Step>],
    values: Vec<Option<Value<M::Place>>>,

    variables: Vec<Value<M::Place>>,
}

impl<'m, M: Machine> ViewMachine<'m, M> {
    fn new(base: &'m mut M, bound: &'m BoundView<M::Step>, this: M::Place) -> Self {
        Self {
            base,
            this,
            lets: &bound.lets,
            values: vec![None; bound.lets.len()],
            variables: Vec::new(),
        }
    }

    pub fn set_variables(&mut self, variables: &[Value<M::Place>]) {
        self.variables.clear();
        self.variables.extend_from_slice(variables);
    }

    fn let_value(&mut self, index: usize) -> Result<Value<M::Place>, Stop> {
        if let Some(Some(value)) = self.values.get(index) {
            return Ok(value.clone());
        }

        let lets = self.lets;
        let program = lets
            .get(index)
            .ok_or_else(|| unsupported("no such `let`"))?;
        let value = interp::value(program, self)?;
        if let Some(slot) = self.values.get_mut(index) {
            *slot = Some(value.clone());
        }

        Ok(value)
    }
}

impl<M: Machine> TypeSource for ViewMachine<'_, M> {
    fn type_info(&self, ty: TypeReference) -> Option<&TypeInfo> {
        self.base.type_info(ty)
    }

    fn pointer_size(&self) -> u8 {
        self.base.pointer_size()
    }

    fn byte_order(&self) -> crate::ByteOrder {
        self.base.byte_order()
    }

    fn c_base_type(&self, ty: crate::CBaseType) -> Option<crate::BaseType> {
        self.base.c_base_type(ty)
    }

    fn same_type(&self, left: TypeReference, right: TypeReference) -> bool {
        self.base.same_type(left, right)
    }
}

impl<M: Machine> Machine for ViewMachine<'_, M> {
    type Object = ViewObject<M::Step>;
    type Step = M::Step;
    type Place = M::Place;

    fn charge(&mut self) -> Result<(), Stop> {
        self.base.charge()
    }

    fn locate(&mut self, object: &Self::Object) -> Result<Self::Place, Stop> {
        match object {
            ViewObject::This => Ok(self.this.clone()),

            ViewObject::Member(step) | ViewObject::Global(step) => {
                let this = self.this.clone();
                self.base.step(&this, step, &[])
            }
            ViewObject::Let(index) => match self.let_value(*index)? {
                Value::Place(place) => Ok(place),
                _ => Err(unsupported("the `let` is not a place")),
            },
            ViewObject::Variable(depth) => match self.variables.get(*depth) {
                Some(Value::Place(place)) => Ok(place.clone()),
                _ => Err(unsupported("a generator's variable is not a place")),
            },
        }
    }

    fn check_indices(&self, step: &Self::Step, indices: &[i128]) -> Result<(), Stop> {
        self.base.check_indices(step, indices)
    }

    fn step(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        indices: &[i128],
    ) -> Result<Self::Place, Stop> {
        self.base.step(from, step, indices)
    }

    fn place_at(&mut self, address: u64, ty: TypeReference) -> Result<Self::Place, Stop> {
        self.base.place_at(address, ty)
    }

    fn address(&self, at: &Self::Place) -> Result<u64, Stop> {
        self.base.address(at)
    }

    fn load(&mut self, at: &Self::Place) -> Result<VariableValue, Stop> {
        self.base.load(at)
    }

    fn read(&mut self, address: u64, size: usize) -> Result<Vec<u8>, Stop> {
        self.base.read(address, size)
    }

    fn text(&mut self, at: &Self::Place) -> Result<Option<TextSummary>, Stop> {
        self.base.text(at)
    }

    fn length(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        self.base.length(at)
    }

    fn presented_length(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        self.base.presented_length(at)
    }

    fn capacity(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        self.base.capacity(at)
    }

    fn presented_capacity(&mut self, at: &Self::Place) -> Result<Option<u64>, Stop> {
        self.base.presented_capacity(at)
    }

    fn presented_element(
        &mut self,
        at: &Self::Place,
        index: i128,
    ) -> Result<Option<Self::Place>, Stop> {
        self.base.presented_element(at, index)
    }

    fn presented_entry(
        &mut self,
        at: &Self::Place,
        key: &Key,
    ) -> Result<Option<Self::Place>, Stop> {
        self.base.presented_entry(at, key)
    }

    fn tuple_elements(&mut self, at: &Self::Place) -> Result<Option<Vec<Self::Place>>, Stop> {
        self.base.tuple_elements(at)
    }

    fn text_span(&mut self, at: &Self::Place) -> Result<Option<TextSpan>, Stop> {
        self.base.text_span(at)
    }

    fn entry(
        &mut self,
        from: &Self::Place,
        step: &Self::Step,
        key: &Key,
    ) -> Result<Option<Self::Place>, Stop> {
        self.base.entry(from, step, key)
    }

    fn function_generics(&mut self, address: u64) -> Result<Vec<(Arc<str>, TypeReference)>, Stop> {
        self.base.function_generics(address)
    }

    fn task(&mut self) -> Result<u64, Stop> {
        Err(unsupported("views read no tasks"))
    }

    fn register(&mut self, _register: &Register) -> Result<u128, Stop> {
        Err(unsupported("views read no registers"))
    }

    fn present(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop> {
        self.base.present(at)
    }

    fn present_stored(&mut self, at: &Self::Place) -> Result<InspectedValue, Stop> {
        self.base.present_stored(at)
    }

    fn dynamic_at(&mut self, ty: &str, address: u64) -> Result<Self::Place, Stop> {
        self.base.dynamic_at(ty, address)
    }

    fn dynamic_key(&mut self, at: &Self::Place) -> Result<u64, Stop> {
        self.base.dynamic_key(at)
    }

    fn load_dynamic(&mut self, at: &Self::Place) -> Result<crate::eval::target::Loaded, Stop> {
        self.base.load_dynamic(at)
    }

    fn present_bytes(&mut self, ty: TypeReference, bytes: &[u8]) -> Result<InspectedValue, Stop> {
        self.base.present_bytes(ty, bytes)
    }

    fn present_pointer(
        &mut self,
        address: u64,
        pointee: Option<TypeReference>,
        type_info: TypeInfo,
    ) -> Result<InspectedValue, Stop> {
        self.base.present_pointer(address, pointee, type_info)
    }

    fn finish(&self, type_info: Option<TypeInfo>, state: VariableState) -> InspectedValue {
        self.base.finish(type_info, state)
    }

    fn bound(&mut self, object: &Self::Object) -> Result<Value<Self::Place>, Stop> {
        match object {
            ViewObject::Let(index) => self.let_value(*index),
            ViewObject::Variable(depth) => self
                .variables
                .get(*depth)
                .cloned()
                .ok_or_else(|| unsupported("the generator's variable is out of scope")),
            ViewObject::This | ViewObject::Member(_) | ViewObject::Global(_) => {
                Err(unsupported("a place is not a bound value"))
            }
        }
    }
}

pub(super) fn truth<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<bool, Failure> {
    match interp::value(program, machine)? {
        Value::Bool(value) => Ok(value),
        _ => Err(internal("a condition is not a truth value")),
    }
}

pub(super) fn count<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
    what: &str,
) -> Result<u64, Failure> {
    let Value::Int(integer) = interp::scalar(interp::value(program, machine)?) else {
        return Err(internal("a count is not an integer"));
    };

    let value = integer.value();

    value
        .to_u128()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| refused(format!("the {what} is {value}")))
}

fn side_text<P>(value: &Value<P>) -> String {
    match value {
        Value::Int(integer) => integer.value().to_string(),
        Value::Float(float) => float.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Pointer(address) | Value::Raw(address) => format!("{address:#x}"),
        Value::Place(_) | Value::Text(_) | Value::Loaded(_) => "…".to_owned(),
    }
}

fn checks<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<(), Failure> {
    for check in &bound.checks {
        if truth(&check.program, machine)? {
            continue;
        }

        let detail = check.sides.as_ref().and_then(|sides| {
            let mut parts = Vec::new();
            for (text, program) in sides {
                let value = interp::value(program, machine).ok()?;
                parts.push(format!("`{text}` is {}", side_text(&value)));
            }

            Some(Arc::from(parts.join(", ")))
        });
        return Err(Failure::Problem(ViewProblem::CheckFailed {
            check: check.text.as_str().into(),
            detail,
        }));
    }

    Ok(())
}

fn resolve<'b, M: Machine>(
    bound: &'b BoundView<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<&'b BoundShape<M::Step>, Failure> {
    checks(bound, machine)?;
    let mut shape = &bound.shape;

    while let BoundShape::If {
        condition,
        then,
        otherwise,
    } = shape
    {
        shape = if truth(condition, machine)? {
            then
        } else {
            otherwise
        };
    }

    if let BoundShape::Unmatched { text, program } = shape {
        let value = interp::value(program, machine)?;
        return Err(refused(format!(
            "`{text}` is {}, which no arm of the match names",
            side_text(&value)
        )));
    }

    Ok(shape)
}

struct Named<'b, St> {
    owner: &'b BoundView<St>,
    field: &'b BoundField<St>,
}

fn named<'b, St>(bound: &'b BoundView<St>, shape: &'b BoundShape<St>) -> Vec<Named<'b, St>> {
    let hidden = |name: &Arc<str>| {
        std::iter::once(bound)
            .chain(bound.extensions.iter().map(AsRef::as_ref))
            .any(|view| view.hidden.iter().any(|(hidden, _)| hidden == name))
    };

    let members = match shape {
        BoundShape::Record { members, .. } => members.as_slice(),
        _ => &[],
    };

    members
        .iter()
        .chain(&bound.fields)
        .map(|field| Named {
            owner: bound,
            field,
        })
        .chain(bound.extensions.iter().flat_map(|extension| {
            extension.fields.iter().map(move |field| Named {
                owner: extension,
                field,
            })
        }))
        .filter(|named| !hidden(&named.field.name))
        .collect()
}

fn format_of<St>(bound: &BoundView<St>, name: &str) -> Option<BoundFormat> {
    bound
        .extensions
        .iter()
        .rev()
        .map(AsRef::as_ref)
        .chain(std::iter::once(bound))
        .find_map(|view| {
            view.formats
                .iter()
                .rev()
                .find(|(formatted, ..)| formatted.as_ref() == name)
                .map(|(_, format, _)| *format)
        })
}

fn named_value<M: Machine>(
    bound: &BoundView<M::Step>,
    named: &Named<'_, M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<InspectedValue, Failure> {
    let value = if std::ptr::eq(named.owner, bound) {
        machine.set_variables(&[]);
        element(&named.field.program, machine)?
    } else {
        let this = machine.this.clone();
        let mut extension = ViewMachine::new(&mut *machine.base, named.owner, this);
        element(&named.field.program, &mut extension)?
    };

    formatted(bound, &named.field.name, value, machine)
}

fn formatted<M: Machine>(
    bound: &BoundView<M::Step>,
    name: &str,
    mut value: InspectedValue,
    machine: &mut ViewMachine<'_, M>,
) -> Result<InspectedValue, Failure> {
    let Some(format) = format_of(bound, name) else {
        return Ok(value);
    };

    let Some(summary) = super::format::write(format, &value, machine)? else {
        return Ok(value);
    };

    if let VariableState::Available {
        presentation,
        children,
        ..
    } = &mut value.state
    {
        *presentation = Some(Arc::new(crate::Presentation {
            view: super::name_of(&bound.view),
            shape: PresentedShape::Formatted,
            count: None,
            summary: summary.into(),
            detail: None,
            children: children.clone(),
            problem: None,
            number: None,
            presented: None,
            visualizers: Arc::from([]),
        }));
    }

    Ok(value)
}

fn declared_length<M: Machine>(
    scan: &BoundScan<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<Option<u64>, Failure> {
    let declared = scan
        .count
        .as_ref()
        .map(|program| count(program, machine, "count"))
        .transpose()?;
    if !scan.random_access() {
        return Ok(declared);
    }

    let [
        super::bind::BoundClause {
            generator: super::bind::BoundGenerator::Range(length),
            ..
        },
    ] = scan.clauses.as_slice()
    else {
        return Err(internal("random access is one range"));
    };

    machine.set_variables(&[]);
    let generated = count(length, machine, "range's length")?;
    match declared {
        Some(declared) if declared != generated => {
            Err(Failure::Problem(ViewProblem::CountMismatch {
                declared,
                generated,
            }))
        }
        _ => Ok(Some(generated)),
    }
}

fn run_value<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<InspectedValue, Failure> {
    match interp::run(program, machine)? {
        Outcome::Value { value, .. } => Ok(value),
        _ => Err(refused("a view's value is one value")),
    }
}

fn dynamic_place<M: Machine>(
    pointer: &ViewProgram<M::Step>,
    ty: &BoundDynamic<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<M::Place, Failure> {
    let Value::Pointer(address) = interp::value(pointer, machine)? else {
        return Err(internal("a dynamic value's pointer is not a pointer"));
    };

    let ty = match ty {
        BoundDynamic::Fixed(ty) => *ty,
        BoundDynamic::Function {
            code,
            name,
            argument,
            pattern,
            known,
            candidates,
        } => {
            if *argument {
                function_argument(code, name, machine)?
            } else {
                function_type(code, name, pattern, known, candidates, machine)?
            }
        }
        BoundDynamic::Argument { types, index } => {
            let index = count(index, machine, "type argument's position")?;
            usize::try_from(index)
                .ok()
                .and_then(|index| types.get(index).copied().flatten())
                .ok_or_else(|| refused(format!("the type has no type argument {index}")))?
        }
    };

    if address == 0 {
        return Err(Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::ValueAccess(ValueAccessUnavailableReason::NullPointer),
        )));
    }

    Ok(machine.place_at(address, ty)?)
}

fn code_address<M: Machine>(
    code: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    match interp::value(code, machine)? {
        Value::Pointer(address) => Ok(address),
        Value::Int(_) => count(code, machine, "code's address"),
        _ => Err(internal("a function's code is not an address")),
    }
}

fn function_argument<M: Machine>(
    code: &ViewProgram<M::Step>,
    name: &str,
    machine: &mut ViewMachine<'_, M>,
) -> Result<TypeReference, Failure> {
    let address = code_address(code, machine)?;
    machine
        .function_generics(address)?
        .into_iter()
        .find_map(|(parameter, ty)| (*parameter == *name).then_some(ty))
        .ok_or_else(|| {
            refused(format!(
                "the function at {address:#x} has no type parameter `{name}`"
            ))
        })
}

fn function_type<M: Machine>(
    code: &ViewProgram<M::Step>,
    name: &str,
    pattern: &Pattern,
    known: &Captures,
    candidates: &[TypeReference],
    machine: &mut ViewMachine<'_, M>,
) -> Result<TypeReference, Failure> {
    let address = code_address(code, machine)?;
    let mut captures = known.clone();

    for (parameter, ty) in machine.function_generics(address)? {
        captures.retain(|(captured, _)| **captured != *parameter);
        captures.push((parameter.to_string(), Captured::Type(ty)));
    }

    let mut found = Vec::<TypeReference>::new();

    for candidate in candidates {
        let Some(identity) = machine
            .type_info(*candidate)
            .and_then(|info| info.identity.as_deref())
        else {
            continue;
        };

        if super::pattern::matches_with(pattern, identity, machine, captures.clone()).is_some()
            && !found
                .iter()
                .any(|other| machine.same_type(*other, *candidate))
        {
            found.push(*candidate);
        }
    }

    match found.as_slice() {
        [ty] => Ok(*ty),
        [] => Err(refused(format!(
            "no type is `{name}` of the function at {address:#x}"
        ))),
        _ => Err(refused(format!(
            "`{name}` of the function at {address:#x} names several types"
        ))),
    }
}

fn element<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<InspectedValue, Failure> {
    match run_value(program, machine) {
        Err(Failure::Problem(ViewProblem::Unavailable(reason))) => {
            Ok(machine.finish(None, VariableState::Unavailable(reason)))
        }
        Err(Failure::Problem(problem)) => Ok(machine.finish(
            None,
            VariableState::Malformed(crate::VariableMalformedReason {
                kind: crate::VariableMalformedKind::InvalidExpression,
                description: problem.to_string().into(),
            }),
        )),
        value => value,
    }
}

enum Walk<'b, St, P> {
    Direct {
        scan: &'b BoundScan<St>,
        length: u64,
        next: u64,
    },
    Scan(Scanner<'b, St, P>),
}

impl<'b, St, P: Clone> Walk<'b, St, P> {
    fn at<M: Machine<Step = St, Place = P>>(
        scan: &'b BoundScan<St>,
        declared: Option<u64>,
        index: u64,
        machine: &mut ViewMachine<'_, M>,
        checkpoints: &mut Checkpoints,
    ) -> Result<Option<Self>, Failure> {
        if scan.random_access() {
            let length = declared.unwrap_or(0);
            return Ok((index <= length).then_some(Self::Direct {
                scan,
                length,
                next: index,
            }));
        }

        let mut scanner = Scanner::at(scan, declared, checkpoints, index);
        Ok(scanner
            .skip_to(index, machine, checkpoints)?
            .then_some(Self::Scan(scanner)))
    }

    fn next<M: Machine<Step = St, Place = P>>(
        &mut self,
        machine: &mut ViewMachine<'_, M>,
        checkpoints: &mut Checkpoints,
    ) -> Result<Option<u64>, Failure> {
        match self {
            Self::Direct { scan, length, next } => {
                if *next >= *length {
                    return Ok(None);
                }

                let index = *next;
                *next += 1;
                let mut values = vec![Var::Integer(i128::from(index)).value()];
                machine.set_variables(&values);

                let [clause] = scan.clauses.as_slice() else {
                    return Err(internal("random access is one range"));
                };

                for item in &clause.items {
                    if let super::bind::BoundItem::Let(program) = item {
                        values.push(interp::value(program, machine)?);
                        machine.set_variables(&values);
                    }
                }

                Ok(Some(index))
            }
            Self::Scan(scanner) => scanner.next(machine, checkpoints),
        }
    }
}

fn item<M: Machine>(
    shape: &BoundShape<M::Step>,
    machine: &mut ViewMachine<'_, M>,
    index: u64,
) -> Result<Child, Failure> {
    match shape {
        BoundShape::Sequence {
            element: program, ..
        } => Ok(Child::Element(index, element(program, machine)?)),
        BoundShape::Map { key, value, .. } => Ok(Child::Entry(
            index,
            Box::new(element(key, machine)?),
            element(value, machine)?,
        )),
        _ => Err(internal("only sequences and maps have elements")),
    }
}

pub fn present<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    checkpoints: &mut Checkpoints,
) -> Result<Presented, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;
    let mut presented = match shape {
        BoundShape::Text { source, length } => {
            let text = read_text(source, length.as_ref(), &mut machine)?;
            Presented {
                summary: summary::quoted(&text),
                text: Some(text),
                ..Presented::new(PresentedShape::Text, String::new())
            }
        }
        BoundShape::Value(program) => {
            let inner = run_value(program, &mut machine)?;
            Presented::value(formatted(bound, "self", inner, &mut machine)?)
        }
        BoundShape::Empty(pieces) => {
            Presented::new(PresentedShape::Empty, rendered(pieces, &mut machine)?)
        }
        BoundShape::Sequence { scan, .. } | BoundShape::Map { scan, .. } => {
            preview(shape, scan, &mut machine, checkpoints)?
        }
        BoundShape::Record { members, .. } => {
            let shown = named(bound, shape)
                .into_iter()
                .filter(|named| {
                    members
                        .iter()
                        .any(|member| std::ptr::eq(member, named.field))
                })
                .collect::<Vec<_>>();
            let mut parts = Vec::new();
            for named in shown.iter().take(summary::MAX_ELEMENTS) {
                let value = named_value(bound, named, &mut machine)?;
                parts.push((
                    Arc::clone(&named.field.name),
                    summary::value(value.type_info.as_ref(), &value.state),
                ));
            }

            Presented::new(
                PresentedShape::Record,
                summary::record(&parts, parts.len() == shown.len()),
            )
        }
        BoundShape::Dynamic { pointer, ty } => {
            let place = dynamic_place(pointer, ty, &mut machine)?;
            Presented::value(machine.present(&place)?)
        }
        BoundShape::Stored => {
            let this = machine.this.clone();
            Presented::value(machine.base.present_stored(&this)?)
        }
        BoundShape::If { .. } | BoundShape::Unmatched { .. } => {
            return Err(internal("`if` and `match` are resolved"));
        }
    };

    presented.named = named(bound, shape).len() as u64;

    if !matches!(
        shape,
        BoundShape::Value(_) | BoundShape::Text { .. } | BoundShape::Stored
    ) && let Some(text) = self_text(bound, &mut machine)?
    {
        presented.shape = PresentedShape::Text;
        presented.summary = summary::quoted(&text);
        presented.text = Some(text);
    }

    if let Some(pieces) = &bound.summary {
        presented.summary = rendered(pieces, &mut machine)?;
    }

    Ok(presented)
}

fn rendered<M: Machine>(
    pieces: &[super::bind::BoundPiece<M::Step>],
    machine: &mut ViewMachine<'_, M>,
) -> Result<String, Failure> {
    let mut text = String::new();
    for piece in pieces {
        match piece {
            super::bind::BoundPiece::Literal(literal) => text.push_str(literal),
            super::bind::BoundPiece::Hole { program, format } => {
                let value = run_value(program, machine)?;
                let written = match format {
                    Some(format) => super::format::write(*format, &value, machine)?,
                    None => None,
                };

                text.push_str(
                    &written
                        .unwrap_or_else(|| summary::value(value.type_info.as_ref(), &value.state)),
                );
            }
        }
    }

    Ok(text)
}

fn self_text<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<Option<TextSummary>, Failure> {
    let owner = bound
        .extensions
        .iter()
        .rev()
        .map(AsRef::as_ref)
        .chain(std::iter::once(bound))
        .find(|view| {
            view.formats
                .iter()
                .any(|(name, ..)| name.as_ref() == "self")
        });
    let Some((owner, source)) =
        owner.and_then(|owner| owner.self_text.as_ref().map(|source| (owner, source)))
    else {
        return Ok(None);
    };

    let text = if std::ptr::eq(owner, bound) {
        machine.set_variables(&[]);
        read_text(source, None, machine)
    } else {
        let this = machine.this.clone();
        let mut extension = ViewMachine::new(&mut *machine.base, owner, this);
        read_text(source, None, &mut extension)
    };

    match text {
        Ok(text) => Ok(super::format::utf8_text(&text)),
        Err(Failure::Problem(_)) => Ok(None),
        Err(failure) => Err(failure),
    }
}

fn preview<M: Machine>(
    shape: &BoundShape<M::Step>,
    scan: &BoundScan<M::Step>,
    machine: &mut ViewMachine<'_, M>,
    checkpoints: &mut Checkpoints,
) -> Result<Presented, Failure> {
    let declared = declared_length(scan, machine)?;
    let is_map = matches!(shape, BoundShape::Map { .. });
    let mut items = Vec::new();
    let mut characters = 0;
    let mut partial = None;
    let mut counted = 0;
    let mut ended = false;
    let mut walk = Walk::at(scan, declared, 0, machine, checkpoints)?;

    while let Some(current) = walk.as_mut() {
        let previewing = partial.is_none()
            && items.len() < summary::MAX_ELEMENTS
            && characters < summary::MAX_CHARACTERS;

        if !previewing && declared.is_some() {
            break;
        }

        let index = match current.next(machine, checkpoints) {
            Ok(Some(index)) => index,
            Ok(None) => {
                ended = true;
                break;
            }

            Err(Failure::Problem(ViewProblem::Unavailable(reason))) if !previewing => {
                partial = Some(ViewProblem::Unavailable(reason));
                break;
            }

            Err(Failure::Problem(ViewProblem::Unavailable(
                reason @ VariableUnavailableReason::InspectionLimit(_),
            ))) => {
                items.push(summary::value(
                    None,
                    &VariableState::Unavailable(reason.clone()),
                ));
                partial = Some(ViewProblem::Unavailable(reason));
                break;
            }
            Err(failure) => return Err(failure),
        };

        counted = index + 1;
        if !previewing {
            continue;
        }

        let child = item(shape, machine, index)?;
        let (rendered, states) = match &child {
            Child::Element(_, value) => (
                summary::value(value.type_info.as_ref(), &value.state),
                vec![&value.state],
            ),
            Child::Entry(_, key, value) => (
                format!(
                    "{}: {}",
                    summary::value(key.type_info.as_ref(), &key.state),
                    summary::value(value.type_info.as_ref(), &value.state)
                ),
                vec![&key.state, &value.state],
            ),
            _ => return Err(internal("items are elements or entries")),
        };

        characters += rendered.chars().count() + 2;
        items.push(rendered);

        for state in states {
            match state {
                VariableState::Unavailable(reason) => {
                    partial = Some(ViewProblem::Unavailable(reason.clone()));
                }
                VariableState::Malformed(reason) => {
                    partial = Some(ViewProblem::Refused(reason.description.clone()));
                }
                _ => {}
            }

            if partial.is_some() {
                break;
            }
        }
    }

    let count = match declared {
        Some(declared) => crate::PresentedCount::Exact(declared),
        None if ended || walk.is_none() => crate::PresentedCount::Exact(counted),
        None => crate::PresentedCount::AtLeast(counted),
    };

    let complete =
        items.len() as u64 == count.known() && matches!(count, crate::PresentedCount::Exact(_));
    let presented = if is_map {
        Presented::new(PresentedShape::Map, summary::map(count, &items, complete))
    } else {
        Presented::new(
            PresentedShape::Sequence,
            summary::sequence(count, &items, complete),
        )
    };

    Ok(Presented {
        count: Some(count),
        partial,
        ..presented
    })
}

pub fn children<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    elements: u64,
    offset: u64,
    limit: u64,
    checkpoints: &mut Checkpoints,
) -> Result<Vec<Child>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;

    let named = named(bound, shape);
    let end = offset.saturating_add(limit).min(
        elements
            .saturating_add(named.len() as u64)
            .saturating_add(1),
    );
    let mut children = Vec::new();

    if offset < elements
        && let BoundShape::Sequence { scan, .. } | BoundShape::Map { scan, .. } = shape
    {
        let declared = declared_length(scan, &mut machine)?;
        let mut walk = Walk::at(scan, declared, offset, &mut machine, checkpoints)?.ok_or(
            Failure::Problem(ViewProblem::CountMismatch {
                declared: elements,
                generated: offset,
            }),
        )?;

        for _ in offset..end.min(elements) {
            let index = match walk.next(&mut machine, checkpoints) {
                Ok(Some(index)) => index,
                Ok(None) => {
                    return Err(Failure::Problem(ViewProblem::CountMismatch {
                        declared: elements,
                        generated: children.len() as u64 + offset,
                    }));
                }

                Err(failure) if out_of_work(&failure) && !children.is_empty() => {
                    return Ok(children);
                }
                Err(failure) => return Err(failure),
            };

            let child = item(shape, &mut machine, index)?;

            if out_of_budget(&child) {
                return Ok(children);
            }

            children.push(child);
        }
    }

    for index in offset.max(elements)..end {
        let position = usize::try_from(index - elements).unwrap_or(usize::MAX);

        let child = match named.get(position) {
            Some(child) => Child::Field(
                Arc::clone(&child.field.name),
                named_value(bound, child, &mut machine)?,
            ),
            None => Child::Raw,
        };

        if out_of_budget(&child) {
            break;
        }

        children.push(child);
    }

    Ok(children)
}

pub fn visualizers<St>(bound: &BoundView<St>) -> Vec<(&BoundView<St>, &BoundVisualizer<St>)> {
    std::iter::once(bound)
        .chain(bound.extensions.iter().map(AsRef::as_ref))
        .flat_map(|view| {
            view.visualizers
                .iter()
                .map(move |visualizer| (view, visualizer))
        })
        .collect()
}

#[derive(Debug)]
pub enum Input {
    Value(Box<InspectedValue>),

    Bytes {
        address: u64,
        length: u64,
    },

    /// A string the `visualize` writes.
    Text(Arc<str>),
}

pub type NamedInput = (Arc<str>, Input, Option<Arc<str>>);

pub fn inputs<M: Machine>(
    bound: &BoundView<M::Step>,
    index: usize,
    machine: &mut M,
    this: M::Place,
) -> Result<Vec<NamedInput>, Failure> {
    let listed = visualizers(bound);
    let Some(&(owner, visualizer)) = listed.get(index) else {
        return Err(internal("no such visualizer"));
    };

    let drawing = visualizer
        .bound
        .as_ref()
        .map_err(|rejection| refused(rejection.to_string()))?;
    let mut machine = ViewMachine::new(machine, bound, this.clone());
    resolve(bound, &mut machine)?;
    let mut extension;
    let machine = if std::ptr::eq(owner, bound) {
        &mut machine
    } else {
        extension = ViewMachine::new(&mut *machine.base, owner, this);
        &mut extension
    };

    let mut inputs = Vec::new();

    for input in &drawing.inputs {
        machine.set_variables(&[]);
        let value = match &input.value {
            BoundInputValue::Value(program) => Input::Value(Box::new(element(program, machine)?)),
            BoundInputValue::Bytes { pointer, length } => Input::Bytes {
                address: code_address(pointer, machine)?,
                length: count(length, machine, "length")?,
            },
            BoundInputValue::Text(text) => Input::Text(Arc::clone(text)),
        };

        inputs.push((Arc::clone(&input.name), value, input.path.clone()));
    }

    Ok(inputs)
}

pub fn contiguous<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<Option<(u64, u64, TypeReference, u64)>, Failure>
where
    M::Step: Clone,
{
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;
    let BoundShape::Sequence { scan, element } = shape else {
        return Ok(None);
    };

    if !scan.random_access()
        || scan
            .clauses
            .first()
            .is_none_or(|clause| !clause.items.is_empty())
    {
        return Ok(None);
    }

    let Some((pointer, scale, element_type)) =
        element.indexed_pointer(|object| matches!(object, ViewObject::Variable(0)))
    else {
        return Ok(None);
    };

    if pointer.mentions(&|object| matches!(object, ViewObject::Variable(_))) {
        return Ok(None);
    }

    let Some(count) = declared_length(scan, &mut machine)? else {
        return Ok(None);
    };

    machine.set_variables(&[]);
    let Value::Pointer(address) = interp::value(&pointer, &mut machine)? else {
        return Ok(None);
    };

    Ok(Some((address, count, element_type, scale)))
}

const fn out_of_work(failure: &Failure) -> bool {
    matches!(
        failure,
        Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::InspectionLimit(_)
        ))
    )
}

fn out_of_budget(child: &Child) -> bool {
    let limited = |value: &InspectedValue| {
        matches!(
            value.state,
            VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(_))
        )
    };

    match child {
        Child::Element(_, value) | Child::Field(_, value) => limited(value),
        Child::Entry(_, key, value) => limited(key) || limited(value),
        Child::Raw => false,
    }
}

const NO_LENGTH: &str = "the value it presents has no length";

pub fn length<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    checkpoints: &mut Checkpoints,
) -> Result<u64, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    match resolve(bound, &mut machine)? {
        BoundShape::Sequence { scan, .. } | BoundShape::Map { scan, .. } => {
            if let Some(length) = declared_length(scan, &mut machine)? {
                return Ok(length);
            }

            let mut scanner = Scanner::<_, M::Place>::at(scan, None, checkpoints, u64::MAX);

            while scanner.next(&mut machine, checkpoints)?.is_some() {}
            Ok(scanner.position())
        }
        BoundShape::Text { source, length } => {
            let text = read_text(source, length.as_ref(), &mut machine)?;
            match text.completion {
                TextCompletion::Complete => Ok(text.bytes.len() as u64),
                TextCompletion::Truncated {
                    length: Some(length),
                }
                | TextCompletion::Limited {
                    length: Some(length),
                    ..
                } => Ok(length),
                _ => Err(refused("the text's length could not be read")),
            }
        }

        BoundShape::Empty(_) => Ok(0),

        BoundShape::Value(program) => {
            let Value::Place(place) = interp::value(program, &mut machine)? else {
                return Err(refused(NO_LENGTH));
            };

            machine
                .presented_length(&place)?
                .ok_or_else(|| refused(NO_LENGTH))
        }
        BoundShape::Dynamic { pointer, ty } => {
            let place = dynamic_place(pointer, ty, &mut machine)?;
            machine
                .presented_length(&place)?
                .ok_or_else(|| refused(NO_LENGTH))
        }
        BoundShape::Record { .. } => Err(refused("a record has no length")),
        BoundShape::Stored => Err(refused(NO_LENGTH)),
        BoundShape::If { .. } | BoundShape::Unmatched { .. } => {
            Err(internal("`if` and `match` are resolved"))
        }
    }
}

pub fn presented_place<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<M::Place, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    match resolve(bound, &mut machine)? {
        BoundShape::Value(program) => match interp::value(program, &mut machine)? {
            Value::Place(place) => Ok(place),
            _ => Err(refused("the view computes the value it presents")),
        },
        BoundShape::Empty(pieces) => Err(refused(format!(
            "the value is `{}`, which holds no value",
            rendered(pieces, &mut machine)?
        ))),
        _ => Err(refused("the view presents no value it stands for")),
    }
}

pub fn element_place<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    index: i128,
    checkpoints: &mut Checkpoints,
) -> Result<M::Place, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;

    if let BoundShape::Value(program) = shape {
        let Value::Place(place) = interp::value(program, &mut machine)? else {
            return Err(refused("the view computes the value it presents"));
        };

        return machine
            .presented_element(&place, index)?
            .ok_or_else(|| refused("the value the view presents has no elements"));
    }

    let BoundShape::Sequence { scan, element } = shape else {
        return Err(refused(if matches!(shape, BoundShape::Map { .. }) {
            "a map's entries are not indexed by position"
        } else {
            "the view presents no elements"
        }));
    };

    let declared = declared_length(scan, &mut machine)?;

    let out_of_bounds = |count: u64| {
        Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::IndexOutOfBounds {
                index,
                lower_bound: 0,
                count,
            },
        ))
    };

    let Ok(wanted) = u64::try_from(index) else {
        return Err(out_of_bounds(declared.unwrap_or(0)));
    };

    if declared.is_some_and(|declared| wanted >= declared) {
        return Err(out_of_bounds(declared.unwrap_or(0)));
    }

    let Some(mut walk) = Walk::at(scan, declared, wanted, &mut machine, checkpoints)? else {
        return Err(out_of_bounds(wanted));
    };

    if walk.next(&mut machine, checkpoints)?.is_none() {
        return Err(out_of_bounds(wanted));
    }

    match interp::value(element, &mut machine)? {
        Value::Place(place) => Ok(place),
        _ => Err(refused(
            "the view computes its elements, which have no place",
        )),
    }
}

pub fn tuple_places<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<Option<Vec<M::Place>>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let BoundShape::Record {
        members,
        tuple: true,
    } = resolve(bound, &mut machine)?
    else {
        return Ok(None);
    };

    let mut places = Vec::with_capacity(members.len());
    for member in members {
        machine.set_variables(&[]);
        match interp::value(&member.program, &mut machine)? {
            Value::Place(place) => places.push(place),
            _ => {
                return Err(refused(format!(
                    "the view computes the tuple's element `{}`, which has no place",
                    member.name
                )));
            }
        }
    }

    Ok(Some(places))
}

fn read_text<M: Machine>(
    source: &TextSource<M::Step>,
    length: Option<&ViewProgram<M::Step>>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<TextSummary, Failure> {
    let (address, length) = text_location(source, length, machine)?;
    if address == 0 && length != Some(0) {
        return Err(Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::ValueAccess(ValueAccessUnavailableReason::NullPointer),
        )));
    }

    let width = source.width();
    if width == 1 {
        return read_bytes(machine, address, length);
    }

    read_units(machine, address, length, width)
}

fn read_units<M: Machine>(
    machine: &mut M,
    address: u64,
    length: Option<u64>,
    width: usize,
) -> Result<TextSummary, Failure> {
    let limit = TextSummary::MAX_BYTES as u64;
    let wide = width as u64;
    let wanted = length.map_or(limit, |length| length.min(limit)) * wide;
    let in_bytes = |length: Option<u64>| length.and_then(|length| length.checked_mul(wide));
    let mut bytes = Vec::new();
    let stopped = loop {
        let read = bytes.len() as u64;
        if read == wanted {
            break None;
        }

        let Some(next) = address.checked_add(read) else {
            break None;
        };

        let size = (PAGE_SIZE - next % PAGE_SIZE).min(wanted - read);
        let chunk = match machine.read(next, usize::try_from(size).unwrap_or(usize::MAX)) {
            Ok(chunk) => chunk,
            Err(Stop::Missing(state)) => {
                break Some(match *state {
                    VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(
                        exhaustion,
                    )) => TextCompletion::Limited {
                        length: in_bytes(length),
                        exhaustion,
                    },
                    _ => TextCompletion::Unreadable {
                        address: crate::VirtualAddress::new(next),
                    },
                });
            }
            Err(stop) => return Err(stop.into()),
        };

        let first = bytes.len() - bytes.len() % width;
        bytes.extend_from_slice(&chunk);
        if length.is_none()
            && let Some(position) = bytes
                .chunks_exact(width)
                .skip(first / width)
                .position(|unit| unit.iter().all(|byte| *byte == 0))
        {
            bytes.truncate(first + position * width);
            break Some(TextCompletion::Complete);
        }
    };

    let completion = match (stopped, length) {
        (Some(completion), _) => completion,
        (None, Some(length)) if bytes.len() as u64 == length * wide => TextCompletion::Complete,
        (None, length) => TextCompletion::Truncated {
            length: in_bytes(length),
        },
    };

    Ok(TextSummary::from_units(
        &bytes,
        width,
        machine.byte_order(),
        completion,
    ))
}

fn text_location<M: Machine>(
    source: &TextSource<M::Step>,
    length: Option<&ViewProgram<M::Step>>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<(u64, Option<u64>), Failure> {
    let declared = length
        .map(|program| count(program, machine, "text's length"))
        .transpose()?;

    Ok(match source {
        TextSource::Pointer { program, .. } => match interp::value(program, machine)? {
            Value::Pointer(address) => (address, declared),
            _ => return Err(internal("a text's pointer is not a pointer")),
        },
        TextSource::Elements { program, first, .. } => {
            let Value::Place(place) = interp::value(program, machine)? else {
                return Err(internal("a text's elements are not a place"));
            };

            let available = match category(machine, program.result()) {
                Category::Array { dimensions, .. } => {
                    dimensions.first().map_or(0, |dimension| dimension.count)
                }
                _ => machine.length(&place)?,
            };

            let length = declared.unwrap_or(available);
            if length > available {
                return Err(refused(format!(
                    "the text's length {length} exceeds its {available} elements"
                )));
            }

            if length == 0 {
                return Ok((0, Some(0)));
            }

            let first = machine.step(&place, first, &[0])?;
            (machine.address(&first)?, Some(length))
        }
    })
}

pub fn text_span<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<Option<(u64, Option<u64>)>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    match resolve(bound, &mut machine)? {
        BoundShape::Text { source, .. } if source.width() != 1 => Ok(None),
        BoundShape::Text { source, length } => {
            let (address, length) = text_location(source, length.as_ref(), &mut machine)?;
            if address == 0 && length != Some(0) {
                return Err(Failure::Problem(ViewProblem::Unavailable(
                    VariableUnavailableReason::ValueAccess(
                        ValueAccessUnavailableReason::NullPointer,
                    ),
                )));
            }

            Ok(Some((address, length)))
        }

        BoundShape::Value(program) => {
            let Value::Place(place) = interp::value(program, &mut machine)? else {
                return Ok(None);
            };

            Ok(machine
                .text_span(&place)?
                .map(|span| (span.address, Some(span.length))))
        }
        _ => Ok(None),
    }
}

pub fn capacity<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<Option<u64>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this.clone());
    let shape = resolve(bound, &mut machine)?;
    if let Some(field) = bound.fields.iter().find(|field| &*field.name == "capacity") {
        machine.set_variables(&[]);
        return capacity_of(&field.program, &mut machine).map(Some);
    }

    for extension in &bound.extensions {
        if let Some(field) = extension
            .fields
            .iter()
            .find(|field| &*field.name == "capacity")
        {
            let mut extended = ViewMachine::new(&mut *machine.base, extension, this);
            return capacity_of(&field.program, &mut extended).map(Some);
        }
    }

    let place = match shape {
        BoundShape::Value(program) => match interp::value(program, &mut machine)? {
            Value::Place(place) => place,
            _ => return Ok(None),
        },
        BoundShape::Dynamic { pointer, ty } => dynamic_place(pointer, ty, &mut machine)?,
        _ => return Ok(None),
    };

    Ok(machine.presented_capacity(&place)?)
}

fn capacity_of<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    let Value::Int(integer) = interp::scalar(interp::value(program, machine)?) else {
        return Err(refused("its `capacity` field is not an integer"));
    };

    let value = integer.value();

    value
        .to_u128()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| refused(format!("its capacity is {value}")))
}

pub fn entry_place<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    key: &Key,
    checkpoints: &mut Checkpoints,
) -> Result<Option<M::Place>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;

    if let BoundShape::Value(program) = shape {
        let Value::Place(place) = interp::value(program, &mut machine)? else {
            return Err(refused("the view computes the value it presents"));
        };

        return Ok(machine.presented_entry(&place, key)?);
    }

    let BoundShape::Map {
        scan,
        key: key_program,
        value,
    } = shape
    else {
        return Err(refused(if matches!(shape, BoundShape::Sequence { .. }) {
            "a sequence's elements are indexed by position"
        } else {
            "the view presents no map"
        }));
    };

    let declared = declared_length(scan, &mut machine)?;
    let Some(mut walk) = Walk::at(scan, declared, 0, &mut machine, checkpoints)? else {
        return Ok(None);
    };

    let mut unreadable = None;

    while walk.next(&mut machine, checkpoints)?.is_some() {
        let matched = if let Key::Tuple(items) = key {
            tuple_key_matches(items, key_program, &mut machine)
        } else {
            let candidate = element(key_program, &mut machine)?;
            key.matches(&candidate)
        };

        match matched {
            Ok(true) => {
                return match interp::value(value, &mut machine)? {
                    Value::Place(place) => Ok(Some(place)),
                    _ => Err(refused("the view computes its values, which have no place")),
                };
            }
            Ok(false) => {}
            Err(Stop::Missing(state)) => {
                unreadable.get_or_insert(Stop::Missing(state));
            }
            Err(stop) => return Err(stop.into()),
        }
    }

    unreadable.map_or_else(|| Ok(None), |stop| Err(stop.into()))
}

fn tuple_key_matches<M: Machine>(
    key: &[Key],
    key_program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<bool, Stop> {
    let place = match interp::value(key_program, machine) {
        Ok(Value::Place(place)) => place,
        Ok(_) => {
            return Err(unsupported(
                "the view computes its keys, whose elements have no places",
            ));
        }
        Err(Stop::Refused(refusal)) => {
            return Err(Stop::missing(VariableState::Malformed(
                crate::VariableMalformedReason {
                    kind: crate::VariableMalformedKind::InvalidExpression,
                    description: refusal.message.into(),
                },
            )));
        }
        Err(stop) => return Err(stop),
    };

    crate::eval::target::tuple_matches(key, &place, machine, &mut |machine, at| machine.present(at))
}

fn read_bytes<M: Machine>(
    machine: &mut M,
    address: u64,
    length: Option<u64>,
) -> Result<TextSummary, Failure> {
    let limit = TextSummary::MAX_BYTES as u64;
    let wanted = length.map_or(limit, |length| length.min(limit));
    let mut bytes = Vec::new();
    let stopped = loop {
        let read = bytes.len() as u64;
        if read == wanted {
            break None;
        }

        let Some(next) = address.checked_add(read) else {
            break None;
        };

        let size = (PAGE_SIZE - next % PAGE_SIZE).min(wanted - read);
        let chunk = match machine.read(next, usize::try_from(size).unwrap_or(usize::MAX)) {
            Ok(chunk) => chunk,
            Err(Stop::Missing(state)) => {
                break Some(match *state {
                    VariableState::Unavailable(VariableUnavailableReason::InspectionLimit(
                        exhaustion,
                    )) => TextCompletion::Limited { length, exhaustion },
                    _ => TextCompletion::Unreadable {
                        address: crate::VirtualAddress::new(next),
                    },
                });
            }
            Err(stop) => return Err(stop.into()),
        };

        if length.is_none()
            && let Some(position) = chunk.iter().position(|byte| *byte == 0)
        {
            bytes.extend(chunk.iter().take(position));
            return Ok(TextSummary {
                extent: None,
                bytes: bytes.into(),
                completion: TextCompletion::Complete,
            });
        }

        bytes.extend_from_slice(&chunk);
    };

    let completion = match (stopped, length) {
        (Some(completion), _) => completion,
        (None, Some(length)) if bytes.len() as u64 == length => TextCompletion::Complete,
        (None, length) => TextCompletion::Truncated { length },
    };

    Ok(TextSummary {
        extent: None,
        bytes: bytes.into(),
        completion,
    })
}
