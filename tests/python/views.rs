//! The standard library's values, each shown as its type's own `repr`
//! writes it: the types only C defines, which microscope reads itself, and
//! those Python defines, through the built-in views. Their items are their
//! children, and expressions reach into them as Python's own lookups do.

use std::collections::BTreeMap;

use microscope::{ExpressionErrorKind, ValueChild, ValueChildRelationship, VariableState};

use crate::expressions::{
    Case, agrees, all_children, check, local, outcome, refuses, reported_results,
};
use crate::locals::{Stop, presented, reported_locals};
use crate::session::{MATRIX, PythonSession};

/// Every expression the fixture evaluates in its frame of `values`.
const CASES: &[Case] = &[
    // A deque's items, across its blocks, and its length.
    agrees("len(queue)"),
    agrees("queue[0]"),
    agrees("queue[-1]"),
    agrees("queue[3]"),
    agrees("queue[\"a\"]"),
    agrees("long_queue[0]"),
    agrees("long_queue[1]"),
    agrees("long_queue[59]"),
    agrees("long_queue[-1]"),
    agrees("len(long_queue)"),
    agrees("nested_queue[1][0]"),
    agrees("queue_sub[1]"),
    agrees("len(queue_sub)"),
    agrees("queue_sub.label"),
    // A defaultdict's and an OrderedDict's keys, as a dict's; a key a
    // defaultdict holds no value for makes one, by code.
    agrees("grouped[\"a\"]"),
    agrees("len(grouped)"),
    agrees("ordered[\"d\"]"),
    agrees("ordered[\"b\"]"),
    agrees("len(ordered)"),
    agrees("len(empty_ordered)"),
    agrees("ordered_sub[\"x\"]"),
    refuses("no_factory[\"zz\"]", ExpressionErrorKind::Unsupported),
    // Values of their own, which have no items, and whose fields their
    // types' code computes.
    agrees("day"),
    agrees("zoned"),
    agrees("price"),
    agrees("len(day)"),
    agrees("price[0]"),
    refuses("day.year", ExpressionErrorKind::Unsupported),
    refuses("queue.maxlen", ExpressionErrorKind::Unsupported),
    // Values the built-in views present, and their attributes.
    agrees("color"),
    agrees("color._value_"),
    agrees("half._numerator"),
    agrees("mixed[2]"),
];

/// The children CPython reported of each local of a collection type, by
/// its name: each child's key, or `-` for an element, and its text.
fn reported_children(truth: &[Vec<String>]) -> BTreeMap<&str, Vec<(&str, &str)>> {
    let mut locals: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for fields in truth.iter().filter(|fields| fields[0] == "child") {
        locals
            .entry(fields[1].as_str())
            .or_default()
            .push((fields[2].as_str(), fields[3].as_str()));
    }

    locals
}

/// Each element's or entry's key, `-` for an element, and its text: the
/// children that are items, before any attributes.
fn shown_items(children: &[ValueChild]) -> Vec<(String, String)> {
    let text = |state: &VariableState| {
        presented(state).map_or_else(|| format!("{state:?}"), str::to_owned)
    };
    children
        .iter()
        .filter_map(|child| match &child.relationship {
            ValueChildRelationship::Element { .. } => Some(("-".to_owned(), text(&child.state))),
            ValueChildRelationship::Entry { key, .. } => {
                Some((text(&key.state), text(&child.state)))
            }
            _ => None,
        })
        .collect()
}

/// Every local is shown as CPython's canonical text, every collection's
/// items are its children in its own order, and every expression gives
/// what CPython gave, or is refused as its case says, on every interpreter
/// in the matrix.
#[tokio::test]
async fn each_library_value_is_shown_as_its_own_repr_writes_it() {
    let texts: Vec<&str> = CASES.iter().map(|case| case.python).collect();
    for interpreter in MATRIX {
        let mut session = PythonSession::launch(interpreter, "views.py", &texts).await;
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

        for (name, reported) in reported_children(&truth) {
            let variable = local(&variables, name, &stop);
            let shown = shown_items(&all_children(&stop, &variable.state).await);
            let expected: Vec<(String, String)> = reported
                .iter()
                .map(|(key, text)| ((*key).to_owned(), (*text).to_owned()))
                .collect();
            if shown != expected {
                lies.push(format!(
                    "{name}: children {shown:?} where CPython reported {expected:?}"
                ));
            }
        }

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

        assert!(lies.is_empty(), "{}:\n{}", stop.context, lies.join("\n"));
        session.shutdown().await;
    }
}
