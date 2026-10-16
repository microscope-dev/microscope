//! Python frames' locals at native stops, each the value CPython reported
//! for it, in the text CPython's own `repr` gives it.

use std::collections::BTreeMap;
use std::sync::Arc;

use microscope::{
    CoreDumpOptions, ExecutionContext, FloatValue, FrameKind, InferiorState, ScalarValue,
    StackFrameId, StopContext, StopId, ThreadId, UnsupportedVariableFeature, ValueChildQuery,
    ValueChildRelationship, ValueChildren, ValueChildrenReference, Variable, VariableKind,
    VariableState, VariableUnavailableReason, VariableValue,
};

use crate::session::{CORES, MATRIX, PythonSession};
use crate::support::Scenario;
use crate::support::truth::Report;

/// The fixture's functions, each parked on a thread of its own, and the
/// parameters of each, in order.
const PARKED: [(&str, &[&str]); 4] = [
    ("scalars", &["gate"]),
    ("containers", &["gate"]),
    ("unbound_and_shared", &["gate"]),
    (
        "calls_with_arguments",
        &["gate", "positional", "keyword", "rest", "options"],
    ),
];

/// The locals a closure shares, through a cell the frame keeps.
const SHARED: [&str; 1] = ["shared"];

/// The locals not yet assigned at the checkpoint, which CPython leaves out
/// of a frame's locals, and the function whose they are.
const UNBOUND: [&str; 1] = ["later"];
const UNBOUND_IN: &str = "unbound_and_shared";

/// Each frame's locals as CPython reported them, by its thread's native id
/// and its qualified name: each local's name and canonical text.
fn reported_locals(truth: &[Vec<String>]) -> BTreeMap<(u64, String), Vec<(String, String)>> {
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
struct Stop<'a> {
    scenario: &'a Scenario,
    id: StopId,
    context: String,
}

impl Stop<'_> {
    /// The variables of the frame of `thread` that runs `function`.
    async fn variables(&self, thread: u64, function: &str) -> Vec<Variable> {
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
        let snapshot = self
            .scenario
            .operation(
                "frame variables",
                self.scenario.handle().at(at(frame.id)).variables(),
            )
            .await;
        snapshot.variables.to_vec()
    }

    /// Every child of a value, in order.
    async fn children(
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
fn summary(state: &VariableState) -> &str {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => &presentation.summary,
        _ => panic!("no presented value: {state:?}"),
    }
}

/// The capability of a value's children.
fn children_of(state: &VariableState) -> &Arc<ValueChildrenReference> {
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

/// Checks the locals of every parked function's frame against CPython's.
async fn assert_locals(stop: &Stop<'_>, truth: &[Vec<String>]) {
    let reported = reported_locals(truth);
    for (function, parameters) in PARKED {
        let (thread, expected) = reported
            .iter()
            .find_map(|((thread, name), locals)| (name == function).then_some((*thread, locals)))
            .unwrap_or_else(|| panic!("{}: CPython reported no {function}", stop.context));
        let variables = stop.variables(thread, function).await;
        let context = format!("{} {function}: {variables:#?}", stop.context);

        // CPython leaves unbound locals out; the debugger says they are.
        let bound: Vec<&Variable> = variables
            .iter()
            .filter(|variable| {
                if UNBOUND.contains(&&*variable.name) {
                    assert_eq!(
                        variable.state,
                        VariableState::Unavailable(VariableUnavailableReason::Unbound),
                        "{context}"
                    );
                    return false;
                }

                true
            })
            .collect();
        let unbound = variables.len() - bound.len();
        let expected_unbound = if function == UNBOUND_IN {
            UNBOUND.len()
        } else {
            0
        };
        assert_eq!(unbound, expected_unbound, "{context}");
        let names: Vec<&str> = bound.iter().map(|variable| &*variable.name).collect();
        let reported_names: Vec<&str> = expected.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, reported_names, "{context}");

        for (variable, (_, text)) in bound.iter().zip(expected) {
            let kind = if parameters.contains(&&*variable.name) {
                VariableKind::Parameter
            } else {
                VariableKind::Local
            };
            assert_eq!(variable.kind, kind, "{} {}", variable.name, context);
            if SHARED.contains(&&*variable.name) {
                assert_eq!(
                    variable.state,
                    VariableState::Unavailable(VariableUnavailableReason::Unsupported(
                        UnsupportedVariableFeature::SharedCell
                    )),
                    "{context}"
                );
                continue;
            }

            // An object of a type the debugger does not present is shown
            // by its type, which CPython reported as `<module.name ...>`.
            if let Some(shown) = text.strip_prefix('<') {
                let qualified = shown.split([' ', '>']).next().unwrap_or_default();
                let name = qualified.rsplit('.').next().unwrap_or_default();
                let type_name = &variable.type_info.as_ref().expect(&context).name;
                assert!(
                    **type_name == *name || **type_name == *qualified,
                    "{}: {type_name} for {text}: {context}",
                    variable.name
                );
                continue;
            }

            assert_eq!(
                summary(&variable.state),
                text,
                "{}: {context}",
                variable.name
            );
        }
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

/// Every parked frame's locals are the ones CPython reports, presented as
/// its `repr` presents them, on every interpreter in the matrix.
#[tokio::test]
async fn each_frames_locals_are_the_ones_cpython_reports() {
    for interpreter in MATRIX {
        let mut session = PythonSession::launch(interpreter, "locals.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let stop = Stop {
            id: session.stop().await,
            scenario: &session.scenario,
            context: session.context(),
        };
        assert_locals(&stop, &truth).await;
        assert_numbers(&stop, &truth).await;
        assert_children(&stop, &truth).await;
        session.shutdown().await;
    }
}

/// A core dump's Python frames hold the locals the program reported just
/// before it aborted.
#[tokio::test]
async fn a_cores_python_locals_are_the_ones_cpython_reported() {
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
        assert_locals(&stop, &truth).await;
        assert_numbers(&stop, &truth).await;
        assert_children(&stop, &truth).await;
        scenario.shutdown().await;
    }
}
