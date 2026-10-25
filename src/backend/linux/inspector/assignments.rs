//! Assignments V8 makes for microscope: `Debugger.setVariableValue` in a
//! frame of a pause V8 holds. No request is sent while a stop is
//! published, so an assignment is an execution. Every thread runs, since
//! V8 answers only while its inspector's thread and the paused thread can,
//! but V8 keeps its pause: the paused thread stays in V8's message loop
//! and runs no JavaScript. Once V8 answers, every thread is stopped and the
//! stop is published with [`StopReason::Assignment`], answered with the
//! variable read again from memory.
//!
//! V8's listing of a scope is a copy it made as it paused, which listing
//! again repeats unchanged, so the value given takes the place of the one
//! listed for the variable; the frame's names hold only while every listed
//! value matches the slot it names, the value given included.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::unistd::Pid;
use serde_json::{Value, json};

use super::super::Controller;
use super::super::cdp::{CdpError, Connection, RequestId, Session};
use super::super::evaluation::Place;
use super::super::native::LinuxTraceOps;
use super::{Isolate, protocol, send};
use crate::eval::interp::Assigned;
use crate::eval::number::{Float, FloatFormat};
use crate::runtime_model::{
    BindingReach, InterpretedFailure, InterpretedFrameRef, InterpretedHandle, InterpretedKind,
    InterpretedOperand, ReportedValue, inspector_protocol,
};
use crate::{RuntimeId, StopReason, TextCompletion};

/// Sets a variable of a paused frame.
const SET_VARIABLE: &str = "Debugger.setVariableValue";

/// How long V8 has to make an assignment and list the frame again.
const ASSIGNMENT_WAIT: Duration = Duration::from_secs(5);

/// An assignment V8 is asked to make in a frame of the pause it holds.
#[derive(Debug)]
pub(in crate::backend::linux) struct Assigning {
    pub(super) number: u64,

    /// The isolate whose pause holds the frame, and the paused thread.
    pub(super) session: Session,
    pub(super) thread: Pid,

    /// The frame's index among the pause's frames.
    frame: usize,

    /// The variable's name and which of the frame's scopes may hold it.
    name: Arc<str>,
    reach: BindingReach,
    refusable: Option<Arc<str>>,
    given: Given,
    state: State,

    /// When V8 has not answered in time, once begun.
    deadline: Option<Instant>,
}

/// What the variable is given.
#[derive(Debug, Clone)]
pub(super) enum Given {
    /// A value V8 makes from its `CallArgument`.
    Argument(Value),

    /// The object a variable of the frame's scopes holds, by its name,
    /// which V8 names by an id only its listing of the scope gives.
    Variable(Arc<str>),
}

#[derive(Debug)]
enum State {
    Unbegun,

    /// The frame's scopes are listed to find the object a variable holds:
    /// each request by the scope it lists, and each scope's argument for
    /// the variable once listed, where it holds it.
    Finding {
        requests: BTreeMap<RequestId, usize>,
        found: BTreeMap<usize, Option<(Value, ReportedValue)>>,
    },

    /// V8 is asked to set the variable in the scope `scope` of the frame's
    /// chain, to `argument`, which it reports as `reported`; the scopes
    /// left to try if it does not hold it there.
    Setting {
        request: RequestId,
        scope: usize,
        remaining: VecDeque<usize>,
        argument: Value,
        reported: ReportedValue,
    },

    /// It is made, or with why, is not.
    Done(Option<Arc<str>>),
}

impl Assigning {
    /// An assignment of `argument` to `name` in the innermost frame of the
    /// main isolate's pause on `thread`, which `number` names.
    #[cfg(test)]
    pub(super) fn scripted(number: u64, thread: Pid, name: &str, argument: Value) -> Self {
        Self {
            number,
            session: Session::main(),
            thread,
            frame: 0,
            name: name.into(),
            reach: BindingReach::Frame,
            refusable: None,
            given: Given::Argument(argument),
            state: State::Unbegun,
            deadline: None,
        }
    }

    /// Whether the assignment waits for the program to run to begin.
    pub(super) const fn unbegun(&self) -> bool {
        matches!(self.state, State::Unbegun)
    }

    /// Whether `id` is a request of this assignment that V8 has not
    /// answered.
    pub(super) fn awaits(&self, id: RequestId) -> bool {
        match &self.state {
            State::Finding { requests, .. } => requests.contains_key(&id),
            State::Setting { request, .. } => *request == id,
            _ => false,
        }
    }

    /// Whether the assignment has ended, made or not.
    pub(super) const fn ended(&self) -> bool {
        matches!(self.state, State::Done(_))
    }

    /// Whether V8 made the assignment.
    pub(super) const fn made(&self) -> bool {
        matches!(self.state, State::Done(None))
    }

    pub(super) const fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Done(_) => None,
            _ => self.deadline,
        }
    }

    fn fail(&mut self, reason: impl Into<Arc<str>>) {
        self.state = State::Done(Some(reason.into()));
    }
}

/// The scopes of `frame`, a frame a pause reported, that may hold a
/// variable `reach` says, innermost first, by their index in its chain.
fn candidates(frame: &Value, reach: BindingReach) -> VecDeque<usize> {
    let chain = frame["scopeChain"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let local = chain
        .iter()
        .position(|scope| scope["type"] == "local")
        .unwrap_or(chain.len());
    chain
        .iter()
        .enumerate()
        .filter(|(index, scope)| {
            let kind = scope["type"].as_str().unwrap_or_default();
            match reach {
                BindingReach::Frame => {
                    *index <= local && matches!(kind, "local" | "block" | "catch")
                }
                BindingReach::Enclosing => {
                    *index > local && matches!(kind, "closure" | "block" | "catch" | "local")
                }
                BindingReach::Script => matches!(kind, "script" | "module"),
            }
        })
        .map(|(index, _)| index)
        .collect()
}

/// The scopes of `frame` whose variables an object a variable holds may
/// be found among: all but the global object and `with` objects.
fn listed_scopes(frame: &Value) -> Vec<(usize, Arc<str>)> {
    frame["scopeChain"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .enumerate()
        .filter(|(_, scope)| !matches!(scope["type"].as_str(), Some("global" | "with")))
        .filter_map(|(index, scope)| {
            Some((index, Arc::from(scope["object"]["objectId"].as_str()?)))
        })
        .collect()
}

/// The `CallArgument` that gives a variable the value V8 described by
/// `object`, a `RemoteObject`.
fn argument_of(object: &Value) -> Value {
    match (
        object["objectId"].as_str(),
        object["unserializableValue"].as_str(),
    ) {
        (Some(id), _) => json!({ "objectId": id }),
        (None, Some(text)) => json!({ "unserializableValue": text }),
        (None, None) if object["type"] == "undefined" => json!({}),
        (None, None) => json!({ "value": object["value"] }),
    }
}

/// What V8 reports a variable given `argument`, a `CallArgument` of a
/// primitive, holds.
fn reported_of(argument: &Value) -> ReportedValue {
    match (&argument["value"], argument["unserializableValue"].as_str()) {
        (_, Some(text)) => text.strip_suffix('n').map_or_else(
            || match text {
                "NaN" => ReportedValue::Number(f64::NAN),
                "Infinity" => ReportedValue::Number(f64::INFINITY),
                "-Infinity" => ReportedValue::Number(f64::NEG_INFINITY),
                "-0" => ReportedValue::Number(-0.0),
                _ => ReportedValue::Unknown,
            },
            |digits| ReportedValue::BigInt(digits.into()),
        ),
        (Value::Null, None) if argument.get("value").is_some() => ReportedValue::Null,
        (Value::Null, None) => ReportedValue::Undefined,
        (Value::Bool(value), None) => ReportedValue::Boolean(*value),
        (Value::Number(value), None) => value
            .as_f64()
            .map_or(ReportedValue::Unknown, ReportedValue::Number),
        (Value::String(text), None) => ReportedValue::String(text.as_str().into()),
        _ => ReportedValue::Unknown,
    }
}

/// The `CallArgument` that gives a variable the value a scope's listing,
/// `properties`, says its variable `name` holds, and what V8 reports it
/// as; `None` if the scope does not hold `name`.
fn listed_as(properties: &Value, name: &str) -> Option<(Value, ReportedValue)> {
    properties["result"]
        .as_array()?
        .iter()
        .find(|property| property["name"].as_str() == Some(name))
        .and_then(|property| property.get("value"))
        .map(|value| (argument_of(value), protocol::reported_value(value)))
}

/// What the variable `name` holds in the frame at `frame` of the pause
/// `isolate` holds, as V8 `found` it in each scope of the frame's chain:
/// the innermost scope that holds the name is where JavaScript finds it.
fn held(
    isolate: &Isolate,
    frame: usize,
    found: BTreeMap<usize, Option<(Value, ReportedValue)>>,
    name: &str,
) -> std::result::Result<(Value, ReportedValue), Arc<str>> {
    let (scope, listed) = found
        .into_iter()
        .find_map(|(scope, listed)| Some((scope, listed?)))
        .ok_or_else(|| {
            format!(
                "V8 lists no variable {name} in the frame's scopes; an object is given as a \
                 variable of the frame holds it"
            )
        })?;
    given_in_pause(isolate, frame, scope, name).unwrap_or(Ok(listed))
}

/// What the variable `name` in the scope at `scope` of the frame at
/// `frame` holds now, if an assignment during the pause `isolate` holds
/// gave it a value, which V8's listing made as it paused does not show;
/// or why it is not known, when one gave a variable of that name in
/// another frame or scope a value, which may share the context holding
/// it.
fn given_in_pause(
    isolate: &Isolate,
    frame: usize,
    scope: usize,
    name: &str,
) -> Option<std::result::Result<(Value, ReportedValue), Arc<str>>> {
    let frames = &isolate.paused.as_ref()?.frames;
    let mut elsewhere = false;
    for (index, paused) in frames.iter().enumerate() {
        for (at, given, argument, reported) in &paused.given {
            if &**given != name {
                continue;
            }
            if (index, *at) == (frame, scope) {
                return Some(Ok((argument.clone(), reported.clone())));
            }
            elsewhere = true;
        }
    }
    elsewhere.then(|| {
        Err(format!(
            "{name} was given a value in another frame or scope during this pause, which \
             may be this variable, and V8's listing still names the value it held as it \
             paused; continue and assign it at the next stop"
        )
        .into())
    })
}

/// The `CallArgument` for a JavaScript number.
fn js_number(value: f64) -> Value {
    if value.is_nan() {
        json!({ "unserializableValue": "NaN" })
    } else if value.is_infinite() {
        json!({ "unserializableValue": if value > 0.0 { "Infinity" } else { "-Infinity" } })
    } else if value == 0.0 && value.is_sign_negative() {
        json!({ "unserializableValue": "-0" })
    } else {
        json!({ "value": value })
    }
}

/// What a variable of `runtime` is given for `value`, or the object of
/// `runtime` whose value it is given, which is read first.
fn given_value(
    value: Assigned<Place>,
    runtime: RuntimeId,
) -> std::result::Result<std::result::Result<Given, InterpretedHandle>, Arc<str>> {
    let argument = match value {
        Assigned::Integer(integer) => {
            let float = Float::from_exact(integer, FloatFormat::Binary64);
            if float.compare_exact(integer) != Some(std::cmp::Ordering::Equal) {
                return Err(format!(
                    "a JavaScript number holds {integer} only approximately; write {integer}n \
                     for a BigInt"
                )
                .into());
            }
            js_number(float.to_f64())
        }
        Assigned::Float(float) => js_number(float),
        Assigned::Bool(value) => json!({ "value": value }),
        Assigned::Text(text) => json!({ "value": text }),
        Assigned::Nothing => json!({ "value": null }),
        Assigned::Undefined => json!({}),
        Assigned::BigInt(integer) => json!({ "unserializableValue": format!("{integer}n") }),
        Assigned::Object(Place::Interpreted {
            runtime: keeper,
            object,
        }) if keeper == runtime => return Ok(Err(object)),
        Assigned::Object(Place::Interpreted { .. }) => {
            return Err("the object belongs to another runtime".into());
        }
        Assigned::Object(Place::Native(_)) => {
            return Err(
                "a value of a program's native type cannot be given to a JavaScript variable"
                    .into(),
            );
        }
    };
    Ok(Ok(Given::Argument(argument)))
}

/// What a variable is given for the value `operand` describes, which
/// the variable `assigned_name` holds when the expression names one.
fn given_operand(
    operand: &InterpretedOperand,
    assigned_name: Option<&str>,
) -> std::result::Result<Given, Arc<str>> {
    let whole = operand.text.as_ref().and_then(|text| {
        matches!(text.completion, TextCompletion::Complete)
            .then(|| String::from_utf8(text.bytes.to_vec()).ok())
            .flatten()
    });
    Ok(match (&operand.kind, whole, assigned_name) {
        (InterpretedKind::Nothing, ..) => Given::Argument(json!({ "value": null })),
        (InterpretedKind::Undefined, ..) => Given::Argument(json!({})),
        (InterpretedKind::Bool(value), ..) => Given::Argument(json!({ "value": value })),
        (InterpretedKind::Float(value), ..) => Given::Argument(js_number(*value)),
        (InterpretedKind::Int(Some(value)), ..) => {
            Given::Argument(json!({ "unserializableValue": format!("{value}n") }))
        }
        (InterpretedKind::Text, Some(text), _) => Given::Argument(json!({ "value": text })),
        (_, _, Some(name)) => Given::Variable(name.into()),
        _ => {
            return Err(
                "an object is given as a variable of the frame holds it, as `x = y`".into(),
            );
        }
    })
}

/// Sends `assignment`'s first request to the isolate `session` reaches,
/// which holds its pause, as the program resumes.
pub(super) fn begin(
    connection: &Connection,
    session: &Session,
    isolate: &Isolate,
    assignment: &mut Assigning,
) {
    assignment.deadline = Some(Instant::now() + ASSIGNMENT_WAIT);
    let Some(frame) = isolate
        .paused
        .as_ref()
        .and_then(|paused| paused.frames.get(assignment.frame))
        .map(|frame| frame.reported.clone())
    else {
        assignment.fail("V8 no longer holds the frame's pause");
        return;
    };
    match assignment.given.clone() {
        Given::Argument(argument) => {
            let reported = reported_of(&argument);
            set(
                connection,
                session,
                &frame,
                assignment,
                (argument, reported),
                None,
            );
        }
        Given::Variable(name) => {
            let mut requests = BTreeMap::new();
            for (index, object) in listed_scopes(&frame) {
                match send(
                    connection,
                    session,
                    inspector_protocol::GET_PROPERTIES,
                    json!({
                        "objectId": &*object,
                        "ownProperties": true,
                        "accessorPropertiesOnly": false,
                        "generatePreview": false,
                    }),
                ) {
                    Ok(request) => {
                        requests.insert(request, index);
                    }
                    Err(error) => {
                        assignment.fail(format!("V8 cannot be asked for {name}: {error}"));
                        return;
                    }
                }
            }
            if requests.is_empty() {
                assignment.fail(format!("V8 lists no scope of the frame that holds {name}"));
                return;
            }
            assignment.state = State::Finding {
                requests,
                found: BTreeMap::new(),
            };
        }
    }
}

/// Asks V8 to give `assignment`'s variable `argument` in the innermost
/// scope of `frame` left to try: the first of those its reach names when
/// `remaining` is `None`.
fn set(
    connection: &Connection,
    session: &Session,
    frame: &Value,
    assignment: &mut Assigning,
    (argument, reported): (Value, ReportedValue),
    remaining: Option<VecDeque<usize>>,
) {
    let mut remaining = remaining.unwrap_or_else(|| candidates(frame, assignment.reach));
    let Some(scope) = remaining.pop_front() else {
        assignment.fail(format!(
            "V8 lists no scope of the frame that holds {}",
            assignment.name
        ));
        return;
    };
    match send(
        connection,
        session,
        SET_VARIABLE,
        json!({
            "scopeNumber": scope,
            "variableName": &*assignment.name,
            "newValue": argument,
            "callFrameId": frame["callFrameId"],
        }),
    ) {
        Ok(request) => {
            assignment.state = State::Setting {
                request,
                scope,
                remaining,
                argument,
                reported,
            };
        }
        Err(error) => assignment.fail(format!("V8 cannot be asked to make it: {error}")),
    }
}

/// The scope V8 was asked to set a variable in, with what it was asked
/// to give it, and the scopes left to try.
struct Tried {
    scope: usize,
    remaining: VecDeque<usize>,
    argument: Value,
    reported: ReportedValue,
}

/// Goes on with `assignment` once V8 answered `result` to setting its
/// variable as `tried` says, in `frame` of the pause `isolate` holds.
#[cfg_attr(
    not(debug_assertions),
    expect(unused_variables, reason = "only recorded")
)]
fn set_replied(
    (connection, session): (&Connection, &Session),
    isolate: &mut Isolate,
    frame: &Value,
    assignment: &mut Assigning,
    result: std::result::Result<Value, CdpError>,
    tried: Tried,
) {
    let Tried {
        scope,
        remaining,
        argument,
        reported,
    } = tried;
    match result {
        Ok(_) => {
            if let Some(paused) = isolate
                .paused
                .as_mut()
                .and_then(|paused| paused.frames.get_mut(assignment.frame))
            {
                paused.assigned(scope, &assignment.name, &argument, &reported);
            }
            assignment.state = State::Done(None);
        }
        // V8 refuses a scope that does not hold the name.
        Err(error) if !remaining.is_empty() => {
            record!("V8 refused {} in one scope: {error}", assignment.name);
            set(
                connection,
                session,
                frame,
                assignment,
                (argument, reported),
                Some(remaining),
            );
        }
        Err(error) => {
            let reason = assignment.refusable.as_ref().map_or_else(
                || format!("V8 did not make it: {error}"),
                |why| format!("V8 did not make it: {error}; {why}"),
            );
            assignment.fail(reason);
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// The assignment V8 is to make, at the stop where it holds thread
    /// `pid` paused, of `value` to the variable in slot `slot` of the
    /// interpreted frame `frame` of `runtime`, which `assigned_name` names
    /// when the expression gives it another variable's value; or why V8
    /// cannot make it exactly. Nothing is sent.
    #[expect(clippy::too_many_arguments, reason = "one assignment's parts")]
    pub(in crate::backend::linux) fn javascript_assignment(
        &self,
        pid: Pid,
        runtime: RuntimeId,
        frame: InterpretedFrameRef,
        slot: u32,
        value: Assigned<Place>,
        assigned_name: Option<&str>,
        number: u64,
    ) -> std::result::Result<Assigning, Arc<str>> {
        let inferior = self.inferior.as_ref().ok_or("the process is gone")?;
        if self.inspector_cleanup_running() {
            return Err("the debugger is leaving Node's inspector".into());
        }
        let inspector = inferior
            .inspectors
            .records
            .get(&runtime)
            .ok_or("the frame's runtime has no inspector")?;
        if !inspector
            .connection
            .as_ref()
            .is_some_and(|link| link.connected)
        {
            return Err("Node's inspector is not connected".into());
        }
        let (session, index) = inspector
            .isolates
            .iter()
            .find_map(|(session, isolate)| {
                let paused = isolate.paused.as_ref()?;
                (paused.thread == Some(pid))
                    .then(|| Some((session.clone(), *paused.bound.get(&frame.address)?)))
            })
            .ok_or(
                "V8 assigns only where it paused the thread, as at a breakpoint, a step's \
                 end, or a debugger statement",
            )?
            .ok_or("V8 did not name this frame among the frames it paused in")?;
        if inferior
            .threads
            .get(&pid)
            .is_some_and(|thread| thread.pending_signal.is_some())
        {
            return Err("the thread has a signal to take first; continue it".into());
        }

        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or("the frame's runtime is no longer loaded")?;
        let mut budget =
            crate::inspection::InspectionBudget::new(crate::InspectionLimits::default());
        let unavailable = |failure: InterpretedFailure| -> Arc<str> {
            match failure {
                InterpretedFailure::Refused(_, reason) => reason,
                InterpretedFailure::Unavailable(reason) => reason.to_string().into(),
            }
        };
        let binding = self
            .with_runtime_stop(inferior, &bound, pid, |stop| {
                bound.model.assigned_binding(stop, frame, slot, &mut budget)
            })
            .map_err(unavailable)?;

        let given = match given_value(value, runtime)? {
            Ok(given) => given,
            Err(object) => {
                let operand = self
                    .with_runtime_stop(inferior, &bound, pid, |stop| {
                        bound.model.interpreted_operand(stop, object, &mut budget)
                    })
                    .map_err(unavailable)?;
                given_operand(&operand, assigned_name)?
            }
        };

        Ok(Assigning {
            number,
            session,
            thread: pid,
            frame: index,
            name: binding.name,
            reach: binding.reach,
            refusable: binding.refusable,
            given,
            state: State::Unbegun,
            deadline: None,
        })
    }

    /// Has the inspector of `runtime` make `assignment` as the program
    /// resumes, or, given none, forgets the one it was to make.
    pub(in crate::backend::linux) fn await_javascript_assignment(
        &mut self,
        runtime: RuntimeId,
        assignment: Option<Assigning>,
    ) {
        if let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        {
            inspector.assigning = assignment;
        }
    }

    /// Goes on with the assignment V8 answered `id` of.
    pub(super) fn assignment_replied(
        &mut self,
        runtime: RuntimeId,
        id: RequestId,
        result: std::result::Result<Value, CdpError>,
    ) -> crate::Result<()> {
        let Some(inspector) = self
            .inferior
            .as_mut()
            .and_then(|inferior| inferior.inspectors.records.get_mut(&runtime))
        else {
            return Ok(());
        };
        let (Some(link), Some(assignment)) =
            (inspector.connection.as_ref(), inspector.assigning.as_mut())
        else {
            return Ok(());
        };
        let session = assignment.session.clone();
        let Some(isolate) = inspector.isolates.get_mut(&session) else {
            assignment.fail("the isolate the frame is in is gone");
            return self.queue_internal_edit(super::super::Edit::InspectorAssigned);
        };
        let Some(frame) = isolate
            .paused
            .as_ref()
            .and_then(|paused| paused.frames.get(assignment.frame))
            .map(|frame| frame.reported.clone())
        else {
            assignment.fail("V8 no longer holds the frame's pause");
            return self.queue_internal_edit(super::super::Edit::InspectorAssigned);
        };

        match std::mem::replace(&mut assignment.state, State::Unbegun) {
            State::Finding {
                mut requests,
                mut found,
            } => {
                let Some(scope) = requests.remove(&id) else {
                    return Ok(());
                };
                let Given::Variable(name) = assignment.given.clone() else {
                    assignment.fail("an assignment of a value lists no scopes");
                    return self.queue_internal_edit(super::super::Edit::InspectorAssigned);
                };
                let listed = match result {
                    Ok(properties) => listed_as(&properties, &name),
                    Err(error) => {
                        assignment.fail(format!("V8 did not list the frame's scopes: {error}"));
                        return self.queue_internal_edit(super::super::Edit::InspectorAssigned);
                    }
                };
                found.insert(scope, listed);
                if !requests.is_empty() {
                    assignment.state = State::Finding { requests, found };
                    return Ok(());
                }
                match held(isolate, assignment.frame, found, &name) {
                    Ok(argument) => set(
                        &link.connection,
                        &session,
                        &frame,
                        assignment,
                        argument,
                        None,
                    ),
                    Err(reason) => assignment.fail(reason),
                }
            }
            State::Setting {
                request,
                scope,
                remaining,
                argument,
                reported,
            } => {
                debug_assert_eq!(request, id, "a reply answers the request sent");
                let link = (&link.connection, &session);
                set_replied(
                    link,
                    isolate,
                    &frame,
                    assignment,
                    result,
                    Tried {
                        scope,
                        remaining,
                        argument,
                        reported,
                    },
                );
            }
            other => assignment.state = other,
        }

        if matches!(assignment.state, State::Done(_)) {
            return self.queue_internal_edit(super::super::Edit::InspectorAssigned);
        }
        Ok(())
    }

    /// Ends each assignment V8 has not answered by `now`.
    pub(in crate::backend::linux) fn assignment_expired(
        &mut self,
        now: Instant,
    ) -> crate::Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        let mut expired = false;
        for assignment in inferior
            .inspectors
            .records
            .values_mut()
            .filter_map(|inspector| inspector.assigning.as_mut())
        {
            if assignment
                .deadline()
                .is_some_and(|deadline| deadline <= now)
            {
                assignment.fail(format!(
                    "V8 did not answer within {} seconds; it may yet make it as the program runs",
                    ASSIGNMENT_WAIT.as_secs()
                ));
                expired = true;
            }
        }
        if expired {
            self.queue_internal_edit(super::super::Edit::InspectorAssigned)?;
        }
        Ok(())
    }

    /// Ends the assignment V8 made, or could not, once every thread is
    /// stopped: its frame's variables are bound again, and the stop is the
    /// assignment's.
    #[cfg_attr(
        not(debug_assertions),
        expect(unused_variables, reason = "only recorded")
    )]
    pub(in crate::backend::linux) fn apply_inspector_assignment(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let Some((runtime, assignment)) =
            inferior
                .inspectors
                .records
                .iter_mut()
                .find_map(|(&runtime, inspector)| {
                    inspector
                        .assigning
                        .take_if(|assignment| matches!(assignment.state, State::Done(_)))
                        .map(|assignment| (runtime, assignment))
                })
        else {
            return;
        };
        let State::Done(failure) = assignment.state else {
            return;
        };
        let pid = assignment.thread;
        if failure.is_none()
            && let Err(error) = self.bind_paused_frames(&(runtime, assignment.session.clone()), pid)
        {
            record!("the assigned frame's variables are not bound: {error}");
        }
        let current = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.active.as_ref())
            .is_some_and(|active| {
                matches!(
                    active.kind,
                    super::super::ActiveKind::RuntimeAssignment { number, .. }
                        if number == assignment.number
                )
            });
        if !current {
            return;
        }
        self.javascript_assignment_ended(failure.is_none(), failure);
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let reason = StopReason::Assignment;
        if let Some(thread) = inferior.threads.get_mut(&pid) {
            thread.reason = Some(reason.clone());
        }
        if let Some(barrier) = inferior.barrier.as_mut()
            && barrier.reason.as_ref().is_none_or(|current| {
                super::super::classify::visible_stop_priority(&reason)
                    > super::super::classify::visible_stop_priority(current)
            })
        {
            barrier.triggering_thread = pid;
            barrier.reason = Some(reason);
        }
    }

    /// Forgets every assignment under way as a stop forms, noting whether
    /// V8 made it, so that its request is answered by the stop.
    pub(in crate::backend::linux) fn end_javascript_assignments(&mut self) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };
        let ended = inferior
            .inspectors
            .records
            .values_mut()
            .filter_map(|inspector| inspector.assigning.take())
            .map(|assignment| assignment.made())
            .reduce(|left, right| left || right);
        if let Some(made) = ended
            && made
        {
            self.javascript_assignment_ended(true, None);
        }
    }
}

impl super::Inspectors {
    /// The earliest time an assignment under way must be answered by.
    pub(in crate::backend::linux) fn assignment_deadline(&self) -> Option<Instant> {
        self.records
            .values()
            .filter_map(|inspector| inspector.assigning.as_ref()?.deadline())
            .min()
    }
}
