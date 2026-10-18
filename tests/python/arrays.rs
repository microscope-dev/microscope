//! numpy's arrays, whose elements are numbers kept as the machine keeps
//! them: shown as `array(ELEMENTS, dtype=NAME)`, their elements or rows as
//! their children, and their numbers read at once where they lie next to
//! each other, for drawings.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use microscope::{
    ExpressionErrorKind, NumberKind, ValueChildRelationship, ValueChildren, ValueChildrenReference,
    VariableState,
};

use crate::expressions::{
    Case, agrees, all_children, check, local, outcome, refuses, reported_results,
};
use crate::locals::{Stop, presented, reported_locals};
use crate::session::PythonSession;
use crate::support::python;

/// The interpreters numpy is built for in the flake.
const NUMPY: [&str; 3] = ["3.12", "3.13", "3.14"];

/// Every expression the fixture evaluates in its frame of `values`.
const CASES: &[Case] = &[
    agrees("len(floats)"),
    agrees("len(grid)"),
    agrees("len(empty)"),
    agrees("len(tagged)"),
    agrees("len(scalar)"),
    agrees("floats"),
    agrees("grid"),
    // An item of an array is a new object, numpy's scalar or a view of the
    // array, and its shape is a property.
    refuses("floats[0]", ExpressionErrorKind::Unsupported),
    refuses("grid[1]", ExpressionErrorKind::Unsupported),
    refuses("floats.shape", ExpressionErrorKind::Unsupported),
    refuses("floats == floats", ExpressionErrorKind::Unsupported),
];

/// The directory numpy is in for `interpreter`, which the flake must give.
fn numpy_directory(interpreter: &str) -> PathBuf {
    let site = python::interpreter(interpreter).with_file_name("site");
    assert!(
        site.join("numpy").is_dir(),
        "{interpreter}: the flake gives no numpy at {}",
        site.display()
    );
    site
}

/// The children CPython reported of each local array, by its name.
fn reported_children(truth: &[Vec<String>]) -> BTreeMap<&str, Vec<&str>> {
    let mut locals: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for fields in truth.iter().filter(|fields| fields[0] == "child") {
        locals
            .entry(fields[1].as_str())
            .or_default()
            .push(fields[2].as_str());
    }

    locals
}

/// The capability of a presented value's children.
fn children_of(state: &VariableState) -> Arc<ValueChildrenReference> {
    match state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => match &presentation.children {
            ValueChildren::Available(children) => Arc::clone(children),
            children => panic!("no children: {children:?}"),
        },
        state => panic!("not presented: {state:?}"),
    }
}

/// The numbers of a value read at once, as a drawing reads them.
async fn numbers(
    stop: &Stop<'_>,
    reference: Arc<ValueChildrenReference>,
) -> Option<microscope::Numbers> {
    stop.scenario
        .operation(
            "numbers",
            stop.scenario
                .handle()
                .numbers(reference, microscope::MAX_DRAWING_BYTES),
        )
        .await
}

/// Each local array is shown as `canonical()` writes it, its elements or
/// rows are its children, its numbers are read at once only where they lie
/// next to each other, and every expression gives what CPython gave or is
/// refused as its case says.
#[tokio::test]
async fn each_array_shows_its_elements_and_reads_its_numbers_at_once() {
    for interpreter in NUMPY {
        let site = numpy_directory(interpreter);
        let mut arguments = vec![site.to_str().expect("a UTF-8 path")];
        arguments.extend(CASES.iter().map(|case| case.python));
        let mut session = PythonSession::launch(interpreter, "arrays.py", &arguments).await;
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
        let mut lies = Vec::new();

        let variables = stop.variables(printer, "values").await;
        let locals = reported_locals(&truth);
        let reported = locals
            .get(&(printer, "values".to_owned()))
            .filter(|reported| !reported.is_empty())
            .unwrap_or_else(|| panic!("{}: CPython reported no values frame", stop.context));
        for (name, text) in reported {
            let variable = local(&variables, name, &stop);
            if presented(&variable.state) != Some(text.as_str()) {
                lies.push(format!(
                    "{name} is {:?} where CPython reported {text}",
                    variable.state
                ));
            }
        }

        let children = reported_children(&truth);
        assert!(
            !children.is_empty(),
            "{}: CPython reported no children",
            stop.context
        );
        for (name, reported) in children {
            let variable = local(&variables, name, &stop);
            let shown: Vec<String> = all_children(&stop, &variable.state)
                .await
                .iter()
                .filter(|child| {
                    matches!(child.relationship, ValueChildRelationship::Element { .. })
                })
                .map(|child| {
                    presented(&child.state)
                        .map_or_else(|| format!("{:?}", child.state), str::to_owned)
                })
                .collect();
            if shown != reported {
                lies.push(format!(
                    "{name}: children {shown:?} where CPython reported {reported:?}"
                ));
            }
        }

        bulk(&stop, &variables, &mut lies).await;

        let results = reported_results(&truth);
        let at = stop.frame(printer, "values").await;
        for case in CASES {
            let reported = results.get(case.python).unwrap_or_else(|| {
                panic!("{}: CPython gave nothing for {}", stop.context, case.python)
            });
            let outcome = outcome(&stop, at, case.microscope).await;
            if let Err(lie) = check(case, reported, &outcome) {
                lies.push(lie);
            }
        }

        drawn(&stop, printer, &mut lies).await;
        assert!(lies.is_empty(), "{}:\n{}", stop.context, lies.join("\n"));
        session.shutdown().await;
    }
}

/// Numbers next to each other are read at once, and no others: of
/// arrays of one axis, of a row of a grid, and not of a list.
async fn bulk(stop: &Stop<'_>, variables: &[microscope::Variable], lies: &mut Vec<String>) {
    let state = |name: &str| local(variables, name, stop).state.clone();
    let floats = numbers(stop, children_of(&state("floats"))).await;
    let expected: Vec<u8> = [1.5_f64, -2.0, 0.1, 1e16]
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    match floats {
        Some(read) if read.kind == NumberKind::F64 && read.count == 4 => {
            if *read.bytes != *expected {
                lies.push(format!("floats read {:?}", read.bytes));
            }
        }
        other => lies.push(format!("floats read at once as {other:?}")),
    }

    let singles = numbers(stop, children_of(&state("singles"))).await;
    if !singles.is_some_and(|read| read.kind == NumberKind::F32 && read.count == 3) {
        lies.push("singles are not read at once as float32s".to_owned());
    }

    let long = numbers(stop, children_of(&state("long"))).await;
    if long.is_none_or(|read| read.count != 100) {
        lies.push("long's 100 numbers are not read at once".to_owned());
    }

    // A list's numbers are objects, which a drawing reads one by one.
    for name in [
        "strided",
        "backwards",
        "grid",
        "transposed",
        "flags",
        "listed",
    ] {
        let read = numbers(stop, children_of(&state(name))).await;
        if read.is_some() {
            lies.push(format!(
                "{name}'s numbers do not lie next to each other: {read:?}"
            ));
        }
    }

    // A row of a grid lies next to itself, and a row of a transposed
    // grid does not.
    let row = |name: &'static str| {
        let state = state(name);
        async move {
            let rows = all_children(stop, &state).await;
            let row = rows
                .get(1)
                .unwrap_or_else(|| panic!("{name} has no second row"));
            numbers(stop, children_of(&row.state)).await
        }
    };
    match row("grid").await {
        Some(read) if read.kind == NumberKind::I32 && read.count == 4 => {
            let expected: Vec<u8> = (4_i32..8).flat_map(i32::to_le_bytes).collect();
            if *read.bytes != *expected {
                lies.push(format!("grid[1] read {:?}", read.bytes));
            }
        }
        other => lies.push(format!("grid[1] read at once as {other:?}")),
    }
    if let Some(read) = row("transposed").await {
        lies.push(format!("transposed[1] is read at once: {read:?}"));
    }
}

/// A view of a Python class that draws its objects offers the drawing, and
/// its inputs read through the runtime: an array's numbers at once.
async fn drawn(stop: &Stop<'_>, printer: u64, lies: &mut Vec<String>) {
    let handle = stop.scenario.handle();
    let errors = stop
        .scenario
        .operation(
            "load views",
            handle.load_views(
                &[(
                    "arrays.views",
                    "microscope-views 1\nview python numpy::ndarray {\n    visualize \"line-plot\" { values = self }\n}\n",
                )],
                &[],
                &[],
            ),
        )
        .await;
    assert!(errors.is_empty(), "the view loads: {errors:?}");

    let variables = stop.variables(printer, "values").await;
    let floats = local(&variables, "floats", stop);
    let VariableState::Available {
        presentation: Some(presentation),
        ..
    } = &floats.state
    else {
        lies.push(format!("floats is not presented: {:?}", floats.state));
        return;
    };

    if presentation.summary.as_ref() != "array([1.5, -2.0, 0.1, 1e+16], dtype=float64)" {
        lies.push(format!(
            "a view without show changes floats: {}",
            presentation.summary
        ));
    }

    let [drawing] = &*presentation.visualizers else {
        lies.push(format!("floats offers {:?}", presentation.visualizers));
        return;
    };

    let inputs = stop
        .scenario
        .operation(
            "visualizer inputs",
            handle.visualizer_inputs(
                Arc::clone(&drawing.inputs),
                microscope::InspectionLimits::default(),
            ),
        )
        .await;
    let [input] = &*inputs.inputs else {
        lies.push(format!("the drawing's inputs are {:?}", inputs.inputs));
        return;
    };

    let microscope::VisualizerValue::Value(value) = &input.value else {
        lies.push(format!("`values` is {:?}", input.value));
        return;
    };

    let read = numbers(stop, children_of(&value.state)).await;
    if !read.is_some_and(|read| read.kind == NumberKind::F64 && read.count == 4) {
        lies.push("the drawing's values are not read at once".to_owned());
    }
}
