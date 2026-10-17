//! Python frames' locals at native stops, each the value CPython reported
//! for it, in the text CPython's own `repr` gives it.

use std::collections::BTreeMap;
use std::sync::Arc;

use microscope::{
    CoreDumpOptions, ExecutionContext, ExpressionErrorKind, FloatValue, FrameKind, InferiorState,
    ScalarValue, StackFrameId, StopContext, StopId, ThreadId, ValueChildQuery,
    ValueChildRelationship, ValueChildren, ValueChildrenReference, Variable, VariableKind,
    VariableState, VariableUnavailableReason, VariableValue,
};

use crate::expressions::{Outcome, outcome};
use crate::session::{CORES, MATRIX, PythonSession};
use crate::support::Scenario;
use crate::support::truth::Report;

/// The fixture's functions, each parked on a thread of its own, and the
/// parameters of each, in order.
pub const PARKED: [(&str, &[&str]); 4] = [
    ("scalars", &["gate"]),
    ("containers", &["gate"]),
    ("unbound_and_shared", &["gate"]),
    (
        "calls_with_arguments",
        &["gate", "positional", "keyword", "rest", "options"],
    ),
];

/// The locals not yet assigned at the checkpoint, which CPython leaves out
/// of a frame's locals, and the function whose they are.
const UNBOUND: [&str; 1] = ["later"];
const UNBOUND_IN: &str = "unbound_and_shared";

/// Each frame's locals as CPython reported them, by its thread's native id
/// and its qualified name: each local's name and canonical text.
pub fn reported_locals(truth: &[Vec<String>]) -> BTreeMap<(u64, String), Vec<(String, String)>> {
    let mut frames = BTreeMap::new();
    let mut names = BTreeMap::new();
    for fields in truth {
        let thread = || -> u64 { fields[1].parse().expect("a native thread id") };
        match fields[0].as_str() {
            "frame" => {
                names.insert((thread(), fields[2].clone()), fields[3].clone());
                frames.insert((thread(), fields[3].clone()), Vec::new());
            }
            "local" => {
                let name = &names[&(thread(), fields[2].clone())];
                frames
                    .get_mut(&(thread(), name.clone()))
                    .expect("a local follows its frame")
                    .push((fields[3].clone(), fields[4].clone()));
            }
            _ => {}
        }
    }

    frames
}

/// A stop's frames and their variables.
pub struct Stop<'a> {
    pub scenario: &'a Scenario,
    pub id: StopId,
    pub context: String,
}

impl Stop<'_> {
    /// The variables of the frame of `thread` that runs `function`.
    pub async fn variables(&self, thread: u64, function: &str) -> Vec<Variable> {
        let at = self.frame(thread, function).await;
        let snapshot = self
            .scenario
            .operation("frame variables", self.scenario.handle().at(at).variables())
            .await;
        snapshot.variables.to_vec()
    }

    /// The frame of `thread` that runs `function`.
    pub async fn frame(&self, thread: u64, function: &str) -> StopContext {
        let at = |frame| StopContext {
            stop: self.id,
            execution: ExecutionContext::Thread(ThreadId::new(thread)),
            frame,
        };
        let trace = self
            .scenario
            .operation(
                "thread backtrace",
                self.scenario
                    .handle()
                    .at(at(StackFrameId::INNERMOST))
                    .backtrace(),
            )
            .await;
        let frame = trace
            .frames
            .iter()
            .find(|frame| {
                matches!(frame.kind, FrameKind::Interpreted { .. })
                    && frame
                        .recorded
                        .as_ref()
                        .is_some_and(|recorded| &*recorded.function == function)
            })
            .unwrap_or_else(|| panic!("{}: no frame of {function}: {trace:#?}", self.context));
        at(frame.id)
    }

    /// Every child of a value, in order.
    pub async fn children(
        &self,
        reference: &Arc<ValueChildrenReference>,
        offset: u64,
        limit: u32,
    ) -> Vec<microscope::ValueChild> {
        let page = self
            .scenario
            .operation(
                "value children",
                self.scenario
                    .handle()
                    .value_children(Arc::clone(reference), ValueChildQuery { offset, limit }),
            )
            .await;
        page.children.to_vec()
    }
}

/// The one-line text a value is presented as.
pub fn summary(state: &VariableState) -> &str {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => &presentation.summary,
        _ => panic!("no presented value: {state:?}"),
    }
}

/// The capability of a value's children.
pub fn children_of(state: &VariableState) -> &Arc<ValueChildrenReference> {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => match &presentation.children {
            ValueChildren::Available(children) => children,
            children => panic!("no children: {children:?}"),
        },
        _ => panic!("no presented value: {state:?}"),
    }
}

/// The one-line text a value is presented as, if it is presented.
pub fn presented(state: &VariableState) -> Option<&str> {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => Some(&presentation.summary),
        _ => None,
    }
}

/// Checks the locals of the frame of `function`, whose parameters are
/// `parameters`, against those CPython reported for it: each bound local
/// by its name, its kind, and its canonical text, in order; the unbound
/// ones said to be.
pub fn check_locals(
    function: &str,
    parameters: &[&str],
    variables: &[Variable],
    reported: &[(String, String)],
) -> Result<(), String> {
    // CPython leaves unbound locals out; the debugger says they are.
    let mut bound = Vec::new();
    for variable in variables {
        if !UNBOUND.contains(&&*variable.name) {
            bound.push(variable);
        } else if variable.state != VariableState::Unavailable(VariableUnavailableReason::Unbound) {
            return Err(format!(
                "{} is not unbound: {:?}",
                variable.name, variable.state
            ));
        }
    }

    let unbound = variables.len() - bound.len();
    let expected = if function == UNBOUND_IN {
        UNBOUND.len()
    } else {
        0
    };
    if unbound != expected {
        return Err(format!("{unbound} locals are unbound, not {expected}"));
    }

    let names: Vec<&str> = bound.iter().map(|variable| &*variable.name).collect();
    let reported_names: Vec<&str> = reported.iter().map(|(name, _)| name.as_str()).collect();
    if names != reported_names {
        return Err(format!(
            "locals {names:?} where CPython reported {reported_names:?}"
        ));
    }

    for (variable, (name, text)) in bound.iter().zip(reported) {
        let kind = if parameters.contains(&name.as_str()) {
            VariableKind::Parameter
        } else {
            VariableKind::Local
        };
        if variable.kind != kind {
            return Err(format!("{name} is a {:?}, not a {kind:?}", variable.kind));
        }

        if presented(&variable.state) != Some(text) {
            return Err(format!(
                "{name} is {:?} where CPython reported {text}",
                variable.state
            ));
        }
    }

    Ok(())
}

/// Each presented local's text, by its function and name, where it is
/// the same on every interpreter: not an object shown by its address.
type Agreed = BTreeMap<(String, String), String>;

/// Checks the locals of every parked function's frame against CPython's,
/// and gives the texts every interpreter must agree on.
pub async fn assert_locals(stop: &Stop<'_>, truth: &[Vec<String>]) -> Agreed {
    let reported = reported_locals(truth);
    let mut agreed = Agreed::new();
    for (function, parameters) in PARKED {
        let (thread, expected) = reported
            .iter()
            .find_map(|((thread, name), locals)| (name == function).then_some((*thread, locals)))
            .unwrap_or_else(|| panic!("{}: CPython reported no {function}", stop.context));
        let variables = stop.variables(thread, function).await;
        check_locals(function, parameters, &variables, expected)
            .unwrap_or_else(|lie| panic!("{} {function}: {variables:#?}\n{lie}", stop.context));
        for variable in &variables {
            if let Some(text) = presented(&variable.state).filter(|text| !text.starts_with('<')) {
                agreed.insert(
                    (function.to_owned(), variable.name.to_string()),
                    text.to_owned(),
                );
            }
        }
    }

    agreed
}

/// Checks that every interpreter presented the locals each presents as
/// the first did.
fn assert_agreement(runs: &[(String, Agreed)]) {
    let Some((first, expected)) = runs.first() else {
        return;
    };

    for (name, agreed) in &runs[1..] {
        assert_eq!(agreed, expected, "{name} disagrees with {first}");
    }
}

/// The thread CPython reported runs `function`.
fn thread_of(truth: &[Vec<String>], function: &str) -> u64 {
    reported_locals(truth)
        .keys()
        .find_map(|(thread, name)| (name == function).then_some(*thread))
        .expect("the function's thread")
}

/// The values of a few locals, as numbers.
async fn assert_numbers(stop: &Stop<'_>, truth: &[Vec<String>]) {
    let scalars = stop.variables(thread_of(truth, "scalars"), "scalars").await;
    let value = |name: &str| {
        let variable = scalars
            .iter()
            .find(|variable| &*variable.name == name)
            .expect(name);
        match &variable.state {
            VariableState::Available { value, .. } => value.clone(),
            state => panic!("{name}: {state:?}"),
        }
    };
    assert_eq!(
        value("small"),
        VariableValue::Scalar(ScalarValue::Signed(7))
    );
    assert_eq!(
        value("big"),
        VariableValue::Scalar(ScalarValue::Signed((1 << 100) + 12_345))
    );
    assert_eq!(
        value("yes"),
        VariableValue::Scalar(ScalarValue::Boolean(true))
    );
    assert_eq!(
        value("half"),
        VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(
            0.5_f64.to_bits()
        )))
    );
}

/// The children of the containers, page by page.
async fn assert_children(stop: &Stop<'_>, truth: &[Vec<String>]) {
    let containers = stop
        .variables(thread_of(truth, "containers"), "containers")
        .await;
    let state = |name: &str| {
        &containers
            .iter()
            .find(|variable| &*variable.name == name)
            .expect(name)
            .state
    };
    let texts = |children: &[microscope::ValueChild]| -> Vec<(Option<String>, String)> {
        children
            .iter()
            .map(|child| {
                let key = match &child.relationship {
                    ValueChildRelationship::Entry { key, .. } => {
                        Some(summary(&key.state).to_owned())
                    }
                    _ => None,
                };
                (key, summary(&child.state).to_owned())
            })
            .collect()
    };

    let mixed = stop.children(children_of(state("mixed")), 0, 16).await;
    assert_eq!(
        texts(&mixed),
        [
            (Some("1".into()), "'one'".into()),
            (Some("'two'".into()), "2".into()),
            (Some("(3,)".into()), "[3]".into()),
            (Some("None".into()), "b'n'".into()),
        ],
        "{}",
        stop.context
    );

    // A page from the middle of a dict whose deleted entries keep their
    // places in its table.
    let thinned = stop.children(children_of(state("thinned")), 3, 2).await;
    assert_eq!(
        texts(&thinned),
        [
            (Some("'k7'".into()), "7".into()),
            (Some("'k9'".into()), "9".into()),
        ],
        "{}",
        stop.context
    );

    let nested = stop.children(children_of(state("nested")), 0, 16).await;
    assert_eq!(
        texts(&nested),
        [
            (None, "[1, [2]]".into()),
            (None, "(3,)".into()),
            (None, "{'k': [4]}".into()),
        ],
        "{}",
        stop.context
    );
    let inner = stop.children(children_of(&nested[0].state), 0, 16).await;
    assert_eq!(
        texts(&inner),
        [(None, "1".into()), (None, "[2]".into())],
        "{}",
        stop.context
    );
}

/// Expressions over a frame's locals give the values the frame holds.
async fn assert_evaluated(stop: &Stop<'_>, truth: &[Vec<String>]) {
    let frame = stop
        .frame(thread_of(truth, "containers"), "containers")
        .await;
    let value = |text: &str| Outcome::Value(text.to_owned());
    for (text, expected) in [
        ("numbers[-1]", value("3")),
        ("numbers[0] + numbers[2]", value("4")),
        ("nested[2][\"k\"][0]", value("4")),
        ("by_name[\"b\"]", value("'two'")),
        ("by_name[\"c\"] == None", value("True")),
        ("mixed[None]", value("b'n'")),
        ("len(thinned)", value("10")),
        ("thinned[\"k5\"]", value("5")),
        (
            "thinned[\"k4\"]",
            Outcome::Error(ExpressionErrorKind::MissingKey),
        ),
        ("numbers[3]", Outcome::Error(ExpressionErrorKind::Bounds)),
    ] {
        let outcome = outcome(stop, frame, text).await;
        assert_eq!(outcome, expected, "{}: {text}", stop.context);
    }
}

/// Every parked frame's locals are the ones CPython reports, presented as
/// its `repr` presents them, on every interpreter in the matrix.
#[tokio::test]
async fn each_frames_locals_are_the_ones_cpython_reports() {
    let mut runs = Vec::new();
    for interpreter in MATRIX {
        let mut session = PythonSession::launch(interpreter, "locals.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let stop = Stop {
            id: session.stop().await,
            scenario: &session.scenario,
            context: session.context(),
        };
        runs.push((interpreter.to_owned(), assert_locals(&stop, &truth).await));
        assert_numbers(&stop, &truth).await;
        assert_children(&stop, &truth).await;
        session.shutdown().await;
    }

    // Every interpreter presents the same values alike, free-threaded or
    // not, as its own build or a distribution's.
    assert_agreement(&runs);
}

/// A core dump's Python frames hold the locals the program reported just
/// before it aborted.
#[tokio::test]
async fn a_cores_python_locals_are_the_ones_cpython_reported() {
    let mut runs = Vec::new();
    for interpreter in CORES {
        let name = format!("python-locals-{interpreter}.core");
        let core = Scenario::fixture(&name);
        let log = std::fs::read_to_string(core.with_extension("core.log"))
            .expect("the core's log; run `just build-test-programs`");
        let truth = Report::parse(&log)
            .checkpoint("parked")
            .expect("the checkpoint the core was taken at")
            .to_vec();

        let mut scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
        let InferiorState::Stopped { stop_id, .. } = scenario.snapshot().await.inferior else {
            panic!("{name}: the core is not stopped");
        };

        let stop = Stop {
            scenario: &scenario,
            id: stop_id,
            context: name.clone(),
        };
        runs.push((name.clone(), assert_locals(&stop, &truth).await));
        assert_numbers(&stop, &truth).await;
        assert_children(&stop, &truth).await;
        assert_evaluated(&stop, &truth).await;
        scenario.shutdown().await;
    }

    assert_agreement(&runs);
}
