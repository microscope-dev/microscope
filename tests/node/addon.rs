//! JavaScript values in a Node-API addon's native frame: each `napi_value`
//! local is shown as the value it refers to, and `(js)` casts a pointer to
//! one, so that expressions reach into it as in a JavaScript frame.

use microscope::{FrameKind, ThreadId, VariableState};

use crate::session::{MATRIX, NodeSession};
use crate::support::Scenario;
use crate::truth::threads;
use crate::values::{lies_about, presented};

/// Selects the frame of the addon's C function `inspect` on `thread`.
async fn select_inspect(session: &NodeSession, thread: u64) {
    let handle = session.scenario.handle();
    session
        .scenario
        .operation("select", handle.select_context(ThreadId::new(thread)))
        .await;
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| {
            !matches!(frame.kind, FrameKind::Interpreted { .. })
                && frame
                    .function
                    .as_ref()
                    .is_some_and(|info| &*info.name == "inspect")
        })
        .unwrap_or_else(|| {
            panic!(
                "{}: no native frame of inspect: {trace:#?}",
                session.context()
            )
        });
    session
        .scenario
        .operation("select frame", handle.select_frame(frame.id))
        .await;
}

/// On every build, the addon's `napi_value` locals, in the frame's variables
/// and as expressions, are the text `util.inspect` gave the values the
/// driver passed, and `(js)` reaches the object through a `void *`.
#[tokio::test]
async fn an_addons_napi_values_are_the_values_node_gave_them() {
    let addon = Scenario::fixture("node-addon.node");
    let addon = addon.to_str().expect("a UTF-8 path");
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "addon.js", &[], &[addon]).await;
        let reported = threads(&session.stop_at("addon").await);
        let printer = reported
            .iter()
            .find(|thread| thread.printer)
            .expect("the printer's thread");
        select_inspect(&session, printer.native).await;
        let context = session.context();

        let variables = session
            .scenario
            .operation("variables", session.scenario.handle().variables())
            .await;
        let named = |name: &str| {
            printer
                .values
                .iter()
                .find(|value| value.name == name)
                .unwrap_or_else(|| panic!("{context}: the driver named no {name}"))
        };
        for name in ["object", "text"] {
            let shown = match variables
                .variables
                .iter()
                .find(|variable| &*variable.name == name)
                .map(|variable| &variable.state)
            {
                Some(VariableState::Available {
                    presentation: Some(presentation),
                    ..
                }) => (
                    presentation.summary.to_string(),
                    presentation.text().to_string(),
                ),
                other => (format!("<{other:?}>"), format!("<{other:?}>")),
            };
            lies.extend(lies_about(
                &format!("{context}, variable"),
                named(name),
                &shown,
            ));
        }

        for (text, name) in [("object", "object"), ("text", "text"), ("(js)text", "text")] {
            let shown = presented(&session, text).await;
            lies.extend(lies_about(
                &format!("{context}, `{text}`"),
                named(name),
                &shown,
            ));
        }
        // `opaque` is a `void *` that holds the object's handle.
        let shown = presented(&session, "(js)opaque").await;
        lies.extend(lies_about(
            &format!("{context}, `(js)opaque`"),
            named("object"),
            &shown,
        ));
        let member = presented(&session, "((js)opaque).a").await;
        if member.0 != "1" {
            lies.push(format!("{context}: `((js)opaque).a` is {member:?}, not 1"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{} lies:\n{}", lies.len(), lies.join("\n"));
}
