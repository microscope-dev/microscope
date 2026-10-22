//! Running a bound view at one stop. A view reaches the program only
//! through the evaluator's [`Machine`], which charges the inspection's
//! budget for every read and every evaluated part, so a presentation's cost
//! is bounded and the same inputs always stop at the same point.

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

/// The size of the pages text is read in, which a read never crosses, so
/// an unmapped page ends the text rather than failing what came before.
const PAGE_SIZE: u64 = 4096;

/// Why a view could not present a value.
#[derive(Debug)]
pub enum Failure {
    /// The value shows as it is stored, with this problem beside it.
    Problem(ViewProblem),

    /// The debugger failed, which fails the whole inspection.
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

/// What running a view presented.
#[derive(Debug)]
pub struct Presented {
    pub shape: PresentedShape,

    /// How many elements or entries a sequence or map holds, also when it
    /// presents as text.
    pub count: Option<crate::PresentedCount>,
    pub text: Option<TextSummary>,
    pub summary: String,

    /// For `value`, the value that stands for this one.
    pub inner: Option<InspectedValue>,

    /// Why the summary stopped short of every element.
    pub partial: Option<ViewProblem>,

    /// How many named children it has: a record's members and the
    /// view's fields, less those it hides, which come after its elements.
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

    /// A value presented as `inner`.
    fn value(inner: InspectedValue) -> Self {
        Self {
            summary: summary::value(inner.type_info.as_ref(), &inner.state),
            inner: Some(inner),
            ..Self::new(PresentedShape::Value, String::new())
        }
    }
}

/// One child of a presented value.
#[derive(Debug)]
pub enum Child {
    Element(u64, InspectedValue),

    /// A map's entry: its index, key, and value.
    Entry(u64, Box<InspectedValue>, InspectedValue),
    Field(Arc<str>, InspectedValue),

    /// The value as it is stored, which the caller materializes.
    Raw,
}

/// A machine that answers a view's names: `self` and its members, its
/// `let`s, each computed once, and its generators' variables.
pub struct ViewMachine<'m, M: Machine> {
    base: &'m mut M,
    this: M::Place,
    lets: &'m [ViewProgram<M::Step>],
    values: Vec<Option<Value<M::Place>>>,

    /// The generators' variables and clauses' `let`s, by position.
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

    /// The generators' variables and clauses' `let`s, by position.
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
        self.values[index] = Some(value.clone());
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
            // A global's step reaches it from anywhere.
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

/// A count: a non-negative integer.
pub(super) fn count<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
    what: &str,
) -> Result<u64, Failure> {
    let Value::Int(integer) = interp::value(program, machine)? else {
        return Err(internal("a count is not an integer"));
    };

    let value = integer.value();

    value
        .to_u128()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| refused(format!("the {what} is {value}")))
}

/// A value of a check's side, for saying why it failed.
fn side_text<P>(value: &Value<P>) -> String {
    match value {
        Value::Int(integer) => integer.value().to_string(),
        Value::Float(float) => float.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Pointer(address) | Value::Raw(address) => format!("{address:#x}"),
        Value::Place(_) | Value::Text(_) | Value::Loaded(_) => "…".to_owned(),
    }
}

/// Runs the view's checks, failing at the first that does not hold.
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

/// The shape a value has, once its checks hold, choosing `if` branches.
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

    // A `match` that names no value it has is that value's problem.
    if let BoundShape::Unmatched { text, program } = shape {
        let value = interp::value(program, machine)?;
        return Err(refused(format!(
            "`{text}` is {}, which no arm of the match names",
            side_text(&value)
        )));
    }

    Ok(shape)
}

/// A child a view names: a member of its record or a field, of the view
/// or of an `extend` of it.
struct Named<'b, St> {
    owner: &'b BoundView<St>,
    field: &'b BoundField<St>,
}

/// The named children of a view's value, in order: its record's members,
/// its fields, and its extensions' fields, less those any of them hides.
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

/// How a view, or the last `extend` of it that says, formats `name`.
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

/// A named child's value, computed in its own view's scope, as its format
/// writes it.
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

/// `value` as the format a view gives `name` writes it, when it gives one
/// that suits the value.
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
            children: children.clone(),
            problem: None,
            number: None,
            presented: None,
            visualizers: Arc::from([]),
        }));
    }

    Ok(value)
}

/// How many elements a sequence or map holds, as far as its declared count
/// and, for random access, its range say. A random-access range that
/// disagrees with the declared count is refused; a scan's count is checked
/// as it is generated.
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

    let super::bind::BoundGenerator::Range(length) = &scan.clauses[0].generator else {
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

/// Presents one value, the result of a program, as inspection presents it.
fn run_value<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<InspectedValue, Failure> {
    match interp::run(program, machine)? {
        Outcome::Value { value, .. } => Ok(value),
        _ => Err(refused("a view's value is one value")),
    }
}

/// Where the value a `dynamic` shape presents is: what its pointer points
/// to, as the type it names or the type argument its index chooses.
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

/// The address of the code a program's value addresses.
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

/// The type argument of the function whose code `code` addresses that its
/// parameter `name` names.
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

/// The one type among `candidates` that `pattern` names, the captures
/// `known` does not name being the type arguments of the function whose
/// code `code` addresses, which come before the view's own.
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

/// An element, key, or value, with the generators' variables in the
/// machine. One the view cannot compute, such as one past the address
/// space, is that element's problem, not the sequence's.
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

/// Where to find a sequence's or map's elements: directly by index, or by
/// scanning.
enum Walk<'b, St, P> {
    Direct {
        scan: &'b BoundScan<St>,
        length: u64,
        next: u64,
    },
    Scan(Scanner<'b, St, P>),
}

impl<'b, St, P: Clone> Walk<'b, St, P> {
    /// A walk whose next element is `index`, or `None` when there are
    /// fewer elements.
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

    /// The next element's index, with its variables in the machine.
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

                // A random-access clause has only `let`s after its range.
                for item in &scan.clauses[0].items {
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

/// One element, or one entry's key and value, at the walk's position.
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

/// Presents `this`, a value of the view's type, as the view says.
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
            // The summary shows the record's members the view does not
            // hide, as their formats write them.
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

    // Text the view writes `self` as presents the value, which keeps the
    // children its shape gives it.
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

/// A summary's text, with each hole's value's summary in its place.
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

/// The text of `self`, when the view, or the last `extend` of it that
/// formats `self`, writes it as UTF-8 and its bytes are text.
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

/// A sequence's or map's count and summary, previewing its first elements.
/// Without a declared count, the scan goes on to count every element, as
/// far as the budget allows.
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

        // With a declared count, nothing past the preview is needed.
        if !previewing && declared.is_some() {
            break;
        }

        let index = match current.next(machine, checkpoints) {
            Ok(Some(index)) => index,
            Ok(None) => {
                ended = true;
                break;
            }

            // Running out of budget, or memory, while only counting leaves
            // the count a lower bound.
            Err(Failure::Problem(ViewProblem::Unavailable(reason))) if !previewing => {
                partial = Some(ViewProblem::Unavailable(reason));
                break;
            }

            // Running out of budget while looking for the next element
            // ends the preview there.
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

        // An element the budget or memory could not provide ends the
        // preview, rather than spending more on the rest.
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

/// The children `offset..offset + limit` of `this` as the view presents
/// it: its elements or entries, then its fields, then the stored value.
/// `elements` is how many elements or entries the presentation counted.
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

    // A record's members come before the view's fields.
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

                // A budget that ends while the scan looks for the next
                // element ends the page with the elements it found.
                Err(failure) if out_of_work(&failure) && !children.is_empty() => {
                    return Ok(children);
                }
                Err(failure) => return Err(failure),
            };

            let child = item(shape, &mut machine, index)?;

            // A page ends at the first child its budget cannot afford,
            // which the page's completion then reports.
            if out_of_budget(&child) {
                return Ok(children);
            }

            children.push(child);
        }
    }

    for index in offset.max(elements)..end {
        let position = usize::try_from(index - elements).unwrap_or(usize::MAX);

        // A member or field the program cannot provide is that child's
        // problem.
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

/// The `visualize` statements of a view and of the `extend`s that add to
/// it, in order, each with the view it belongs to.
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

/// An input a drawing hands its renderer, as run at a stop.
#[derive(Debug)]
pub enum Input {
    Value(Box<InspectedValue>),

    /// `bytes(PTR, LEN)`: where the bytes are and how many, which the
    /// caller reads at once.
    Bytes {
        address: u64,
        length: u64,
    },

    /// A string the `visualize` writes.
    Text(Arc<str>),
}

/// An input by name, with the part of the drawn value it is, when it is
/// one: `""` for the value itself, and none for `bytes(…)` or what the view
/// computes.
pub type NamedInput = (Arc<str>, Input, Option<Arc<str>>);

/// Runs the inputs of the `index`th of [`visualizers`], once the view's
/// checks hold. An input the program cannot provide is that input's
/// problem, as a field's is.
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

/// Where a sequence's elements lie when they lie next to each other in
/// memory: when the view's elements are `p[i]` for each `i` in `range(n)`,
/// with `p` a pointer that `i` does not change. The address of the
/// first, how many there are, the type of each, and its size.
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

    if !scan.random_access() || !scan.clauses[0].items.is_empty() {
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

/// Whether running stopped because the inspection's budget ran out.
const fn out_of_work(failure: &Failure) -> bool {
    matches!(
        failure,
        Failure::Problem(ViewProblem::Unavailable(
            VariableUnavailableReason::InspectionLimit(_)
        ))
    )
}

/// Whether a child is unavailable because the budget ran out.
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

/// How many elements or entries `this` holds as the view presents it, or
/// the length of its text, for `len(v)`.
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

        // A value that holds nothing, such as Go's nil map, has nothing.
        BoundShape::Empty(_) => Ok(0),
        // A value presented as another has that one's length.
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

/// The place of the value `this` is presented as, for `*x`.
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

/// The place of element `index` of `this`, for `v[i]`.
pub fn element_place<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    index: i128,
    checkpoints: &mut Checkpoints,
) -> Result<M::Place, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;

    // A value presented as another has that one's elements.
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

/// The places of the elements of `this`, when its view presents it as a
/// tuple, for a key of several values; `None` when it presents no tuple.
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

/// The text a `text` shape presents, at most [`TextSummary::MAX_BYTES`].
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

/// The text of `width`-byte characters at `address`, `length` of them or
/// up to a NUL, decoded from UTF-16 or UTF-32. A length it gives is in
/// bytes.
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

        // A unit may straddle the chunks, so the search begins at the unit
        // the chunk's first byte belongs to.
        let first = bytes.len() - bytes.len() % width;
        bytes.extend_from_slice(&chunk);
        if length.is_none()
            && let Some(position) = bytes[first..]
                .chunks_exact(width)
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

/// Where a `text` shape's characters are: the first one's address, and
/// how many there are when the view or its elements say.
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

/// Where the text `this` is presented as is, for slicing it: the first
/// character's address, and how many there are when the view says.
pub fn text_span<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
) -> Result<Option<(u64, Option<u64>)>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    match resolve(bound, &mut machine)? {
        // Slices of text count bytes, which wider characters are not.
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

        // A value presented as another is that one's text.
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

/// How many elements `this` has room for, as the view's `capacity` field,
/// or an extension's, says, for `cap(v)`; a view that presents the value as
/// another gives that one's.
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

/// A `capacity` field's value, which must be a count.
fn capacity_of<M: Machine>(
    program: &ViewProgram<M::Step>,
    machine: &mut ViewMachine<'_, M>,
) -> Result<u64, Failure> {
    let Value::Int(integer) = interp::value(program, machine)? else {
        return Err(refused("its `capacity` field is not an integer"));
    };

    let value = integer.value();

    value
        .to_u128()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| refused(format!("its capacity is {value}")))
}

/// The place of the value the map `this` holds for `key`, for `m[key]`,
/// or `None` when it holds no such key. Its entries are searched in order;
/// a key that could not be read leaves the answer unavailable unless a
/// later one matches.
pub fn entry_place<M: Machine>(
    bound: &BoundView<M::Step>,
    machine: &mut M,
    this: M::Place,
    key: &Key,
    checkpoints: &mut Checkpoints,
) -> Result<Option<M::Place>, Failure> {
    let mut machine = ViewMachine::new(machine, bound, this);
    let shape = resolve(bound, &mut machine)?;

    // A value presented as another has that one's entries.
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

/// Whether the key a map's view gives for its current entry, by the
/// program `key_program`, is the tuple `key`, element by element. A key
/// the view cannot compute leaves the entry unreadable, as an element's
/// problem does.
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

/// Reads text at `address`, `length` bytes or up to a NUL, a page at a
/// time, at most [`TextSummary::MAX_BYTES`].
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
            bytes.extend_from_slice(&chunk[..position]);
            return Ok(TextSummary {
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
        bytes: bytes.into(),
        completion,
    })
}
