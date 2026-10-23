//! Views of JavaScript values, chosen by the classes their prototype chains
//! name: Node's library types through the built-in views
//! (`views/javascript-std.views`), and a program's own classes through a
//! views file, by the path of the script that defines them.

use std::sync::Arc;

use microscope::{
    Evaluation, Expression, InspectionLimits, NumberKind, Presentation, SourceLanguage, ThreadId,
    ValueChildQuery, ValueChildRelationship, ValueChildren, VariableState, VisualizerValue,
};

use crate::session::{MATRIX, NodeSession};
use crate::truth::{Named, threads};

/// Views of the fixture's classes, and of Node's by the identities its
/// scripts give them, ahead of the built-in views.
const PROGRAM_VIEWS: &str = "microscope-views 1
view javascript **::node::views::Point {
    show record { x = self.x, y = self.y }
}
view javascript **::node::views::Point3 {
    show record { x = self.x, y = self.y, z = self.z }
}
view javascript node::buffer::Buffer {
    show empty(\"a Buffer\")
}
view javascript node::internal::url::URL {
    show empty(\"a URL\")
}
";

/// Selects the frame of `views` on the printer's thread.
async fn select_views(session: &NodeSession, native: u64) {
    let handle = session.scenario.handle();
    session
        .scenario
        .operation("select", handle.select_context(ThreadId::new(native)))
        .await;
    let trace = session
        .scenario
        .operation("backtrace", handle.backtrace())
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| {
            frame.recorded.as_ref().is_some_and(|recorded| {
                recorded.language == SourceLanguage::JavaScript
                    && recorded.function.as_ref() == "views"
            })
        })
        .unwrap_or_else(|| panic!("{}: no frame of views", session.context()));
    session
        .scenario
        .operation("select frame", handle.select_frame(frame.id))
        .await;
}

/// How microscope presents the expression `text`.
async fn presentation(session: &NodeSession, text: &str) -> Arc<Presentation> {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("{text}: {error}"));
    match session.scenario.handle().evaluate(&expression).await {
        Ok(Evaluation::Value { value, .. }) => match value.state {
            VariableState::Available {
                presentation: Some(presentation),
                ..
            } => presentation,
            state => panic!("{}: {text} is {state:?}", session.context()),
        },
        other => panic!("{}: {text} is {other:?}", session.context()),
    }
}

/// The one-line text Node gave the value named `name`.
fn line<'v>(values: &'v [Named], name: &str) -> &'v str {
    &values
        .iter()
        .find(|value| value.name == name)
        .unwrap_or_else(|| panic!("the fixture names no {name}"))
        .line
}

/// The view that presents `presented`, as its source and header.
fn view_of(presented: &Presentation) -> (String, String) {
    (
        presented.view.source.to_string(),
        presented.view.header.to_string(),
    )
}

fn std_view(header: &str) -> (String, String) {
    ("javascript-std.views".to_owned(), header.to_owned())
}

/// A typed array's drawing reads its elements at once, as their bytes.
async fn drawn(
    session: &NodeSession,
    name: &str,
    presented: &Presentation,
    expected: (NumberKind, &[u8]),
    lies: &mut Vec<String>,
) {
    let context = session.context();
    let renderers: Vec<&str> = presented
        .visualizers
        .iter()
        .map(|drawing| drawing.renderer.name.as_ref())
        .collect();
    let [drawing] = &*presented.visualizers else {
        lies.push(format!("{context}: {name} offers {renderers:?}"));
        return;
    };
    if renderers != ["line-plot"] {
        lies.push(format!("{context}: {name} offers {renderers:?}"));
    }
    let handle = session.scenario.handle();
    let inputs = session
        .scenario
        .operation(
            "visualizer inputs",
            handle.visualizer_inputs(Arc::clone(&drawing.inputs), InspectionLimits::default()),
        )
        .await;
    let [input] = &*inputs.inputs else {
        lies.push(format!(
            "{context}: {name}'s inputs are {:?}",
            inputs.inputs
        ));
        return;
    };
    let VisualizerValue::Value(value) = &input.value else {
        lies.push(format!("{context}: {name}'s values are {:?}", input.value));
        return;
    };
    let VariableState::Available {
        presentation: Some(values),
        ..
    } = &value.state
    else {
        lies.push(format!("{context}: {name}'s values are {:?}", value.state));
        return;
    };
    let ValueChildren::Available(children) = &values.children else {
        lies.push(format!("{context}: {name}'s values have no children"));
        return;
    };
    let read = session
        .scenario
        .operation(
            "numbers",
            handle.numbers(Arc::clone(children), microscope::MAX_DRAWING_BYTES),
        )
        .await;
    let (kind, bytes) = expected;
    if !read
        .as_ref()
        .is_some_and(|read| read.kind == kind && *read.bytes == *bytes)
    {
        lies.push(format!(
            "{context}: {name}'s numbers read at once as {read:?}"
        ));
    }
}

/// The summary of the field `field` a view gives `presented`.
async fn field(session: &NodeSession, presented: &Presentation, field: &str) -> Option<String> {
    let ValueChildren::Available(children) = &presented.children else {
        return None;
    };
    let total = u32::try_from(children.total()).ok()?;
    let page = session
        .scenario
        .operation(
            "value children",
            session.scenario.handle().value_children(
                Arc::clone(children),
                ValueChildQuery {
                    offset: 0,
                    limit: total,
                },
            ),
        )
        .await;
    page.children
        .iter()
        .find_map(|child| match (&child.relationship, &child.state) {
            (
                ValueChildRelationship::Field { name },
                VariableState::Available {
                    presentation: Some(presentation),
                    ..
                },
            ) if name.as_ref() == field => Some(presentation.summary.to_string()),
            _ => None,
        })
}

/// The built-in views present typed arrays, a Buffer among them, with a
/// line plot that reads their elements at once, and emitters, a subclass's
/// too, with their listeners, while each still shows as `util.inspect`
/// writes it. No built-in view presents a URL.
async fn built_in(session: &NodeSession, values: &[Named], lies: &mut Vec<String>) {
    let context = session.context();
    let floats: Vec<u8> = [1.5_f64, -2.0, 0.25]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    for (name, kind, bytes) in [
        ("values.buffer", NumberKind::U8, &[1_u8, 2, 3, 250][..]),
        ("values.uint8", NumberKind::U8, &[4, 5, 6][..]),
        ("values.floats", NumberKind::F64, &floats[..]),
    ] {
        let presented = presentation(session, name).await;
        if view_of(&presented) != std_view("javascript TypedArray") {
            lies.push(format!(
                "{context}: {name} is presented by {:?}",
                presented.view
            ));
        }
        if presented.summary.as_ref() != line(values, name) {
            lies.push(format!("{context}: {name} shows {}", presented.summary));
        }
        drawn(session, name, &presented, (kind, bytes), lies).await;
    }

    for (name, events) in [
        ("values.emitter", Some("values.emitter._events")),
        ("values.derived", Some("values.derived._events")),
        ("values.bare", None),
    ] {
        let presented = presentation(session, name).await;
        if view_of(&presented) != std_view("javascript node::events::EventEmitter") {
            lies.push(format!(
                "{context}: {name} is presented by {:?}",
                presented.view
            ));
        }
        if name != "values.bare" && presented.summary.as_ref() != line(values, name) {
            lies.push(format!("{context}: {name} shows {}", presented.summary));
        }
        let listeners = field(session, &presented, "listeners").await;
        let expected = events.map_or("undefined", |events| line(values, events));
        if listeners.as_deref() != Some(expected) {
            lies.push(format!(
                "{context}: {name}'s listeners are {listeners:?}, not {expected:?}"
            ));
        }
    }

    // A URL's state is in private fields that only its own code reads.
    let url = presentation(session, "values.url").await;
    if url.view.source.as_ref() != "microscope" {
        lies.push(format!(
            "{context}: values.url is presented by {:?}",
            url.view
        ));
    }
}

/// A views file presents the program's classes, each by its own view
/// before its base class's, and Node's by the scripts that define them.
async fn program(session: &NodeSession, lies: &mut Vec<String>) {
    let context = session.context();
    let handle = session.scenario.handle();
    let errors = session
        .scenario
        .operation(
            "load views",
            handle.load_views(&[("program.views", PROGRAM_VIEWS)], &[], &[]),
        )
        .await;
    assert!(errors.is_empty(), "{context}: the views load: {errors:?}");
    for (name, header, summary) in [
        (
            "values.point",
            "javascript **::node::views::Point",
            "{x: 1, y: 2}",
        ),
        (
            "values.point3",
            "javascript **::node::views::Point3",
            "{x: 1, y: 2, z: 3}",
        ),
        (
            "values.buffer",
            "javascript node::buffer::Buffer",
            "a Buffer",
        ),
        ("values.url", "javascript node::internal::url::URL", "a URL"),
    ] {
        let presented = presentation(session, name).await;
        let shown = (view_of(&presented), presented.summary.to_string());
        let expected = (
            ("program.views".to_owned(), header.to_owned()),
            summary.to_owned(),
        );
        if shown != expected {
            lies.push(format!("{context}: {name} is {shown:?}, not {expected:?}"));
        }
    }
}

/// On every build, views present Node's library types and the program's
/// classes by the classes their prototype chains name.
#[tokio::test]
async fn views_choose_javascript_classes_by_their_scripts_and_prototypes() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, "views.js", &[], &[]).await;
        let reported = threads(&session.stop_at("views").await);
        let printer = reported
            .iter()
            .find(|thread| thread.printer)
            .expect("the printer's thread");
        select_views(&session, printer.native).await;
        built_in(&session, &printer.values, &mut lies).await;
        program(&session, &mut lies).await;
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{} lies:\n{}", lies.len(), lies.join("\n"));
}
