use microscope::{
    ExecutionContext, ExpressionErrorKind, FrameKind, StackFrameId, StopContext, ThreadId,
    ValueChildren, VariableState, VariableValue,
};

use crate::expressions::{Outcome, outcome};
use crate::locals::{Stop, presented};
use crate::session::PythonSession;
use crate::support::Scenario;

pub const INTERPRETERS: [&str; 4] = ["3.12", "3.14", "3.14t", "3.15"];

pub fn extension_directory(interpreter: &str) -> String {
    Scenario::fixture(&format!("python-extension-{interpreter}"))
        .to_string_lossy()
        .into_owned()
}

async fn native_frame(stop: &Stop<'_>, thread: u64, function: &str) -> StopContext {
    let at = |frame| StopContext {
        stop: stop.id,
        execution: ExecutionContext::Thread(ThreadId::new(thread)),
        frame,
    };
    let trace = stop
        .scenario
        .operation(
            "thread backtrace",
            stop.scenario
                .handle()
                .at(at(StackFrameId::INNERMOST))
                .backtrace(),
        )
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| {
            !matches!(frame.kind, FrameKind::Interpreted { .. })
                && frame
                    .function
                    .as_ref()
                    .is_some_and(|info| &*info.name == function)
        })
        .unwrap_or_else(|| {
            panic!(
                "{}: no native frame of {function}: {trace:#?}",
                stop.context
            )
        });

    at(frame.id)
}

fn reported_objects<'a>(truth: &'a [Vec<String>], context: &str) -> Vec<(&'a str, &'a str)> {
    let objects: Vec<_> = truth
        .iter()
        .filter(|fields| fields[0] == "object")
        .map(|fields| (fields[1].as_str(), fields[2].as_str()))
        .collect();
    assert!(
        !objects.is_empty(),
        "{context}: CPython reported no objects"
    );
    objects
}

#[tokio::test]
async fn a_native_frames_python_objects_are_the_values_cpython_reported() {
    for interpreter in INTERPRETERS {
        let directory = extension_directory(interpreter);
        let mut session = PythonSession::launch(interpreter, "extension.py", &[&directory]).await;
        let truth = session.stop_at("parked").await;
        let printer = truth
            .iter()
            .find(|fields| fields[0] == "thread" && fields[3] == "printer")
            .and_then(|fields| fields[1].parse().ok())
            .expect("the printer's thread");
        let stop = Stop {
            id: session.stop().await,
            scenario: &session.scenario,
            context: session.context(),
        };

        let at = native_frame(&stop, printer, "inspect").await;
        let variables = stop
            .scenario
            .operation("frame variables", stop.scenario.handle().at(at).variables())
            .await
            .variables
            .to_vec();

        let variable = |name: &str| {
            variables
                .iter()
                .find(|variable| &*variable.name == name)
                .unwrap_or_else(|| panic!("{}: no local {name}: {variables:#?}", stop.context))
        };

        for (name, text) in reported_objects(&truth, &stop.context) {
            assert_eq!(
                presented(&variable(name).state),
                Some(text),
                "{}: {name} is {:?}",
                stop.context,
                variable(name).state
            );
        }

        let VariableState::Available {
            presentation: Some(presentation),
            ..
        } = &variable("numbers").state
        else {
            panic!("{}: numbers is not presented", stop.context);
        };
        let ValueChildren::Available(reference) = &presentation.children else {
            panic!("{}: numbers has no children", stop.context);
        };
        let elements: Vec<String> = stop
            .children(reference, 0, 8)
            .await
            .iter()
            .map(|child| presented(&child.state).unwrap_or_default().to_owned())
            .collect();
        assert_eq!(elements, ["1", "2", "3"], "{}", stop.context);

        assert!(
            matches!(
                &variable("missing").state,
                VariableState::Available {
                    value: VariableValue::Address(address),
                    presentation: None,
                    ..
                } if address.address.get() == 0
            ),
            "{}: missing is {:?}",
            stop.context,
            variable("missing").state
        );

        let value = |text: &str| Outcome::Value(text.to_owned());
        for (text, expected) in [
            ("((py)point).x", value("3")),
            ("((py)point).x * ((py)point).y", value("12")),
            ("((py)numbers)[-1]", value("3")),
            ("len((py)numbers)", value("3")),
            ("((py)mapping)[\"b\"]", value("'two'")),
            ("(py)nothing", value("None")),
            (
                "((py)point).z",
                Outcome::Error(ExpressionErrorKind::MissingKey),
            ),
            (
                "(py)point + 1",
                Outcome::Error(ExpressionErrorKind::Unsupported),
            ),
        ] {
            assert_eq!(
                outcome(&stop, at, text).await,
                expected,
                "{}: `{text}`",
                stop.context
            );
        }

        assert!(
            matches!(outcome(&stop, at, "(py)missing").await, Outcome::Missing(_)),
            "{}: a null object is no object",
            stop.context
        );
        session.shutdown().await;
    }
}
