use std::collections::BTreeMap;

use microscope::{ExpressionErrorKind, ValueChild, ValueChildRelationship, VariableState};

use crate::expressions::{
    Case, agrees, all_children, check, local, outcome, refuses, reported_results,
};
use crate::locals::{Stop, presented, reported_locals};
use crate::session::{PythonSession, VERSIONS};

const CASES: &[Case] = &[
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
    agrees("grouped[\"a\"]"),
    agrees("len(grouped)"),
    agrees("ordered[\"d\"]"),
    agrees("ordered[\"b\"]"),
    agrees("len(ordered)"),
    agrees("len(empty_ordered)"),
    agrees("ordered_sub[\"x\"]"),
    refuses("no_factory[\"zz\"]", ExpressionErrorKind::Unsupported),
    agrees("day"),
    agrees("zoned"),
    agrees("price"),
    agrees("len(day)"),
    agrees("price[0]"),
    refuses("day.year", ExpressionErrorKind::Unsupported),
    refuses("queue.maxlen", ExpressionErrorKind::Unsupported),
    agrees("color"),
    agrees("color._value_"),
    agrees("half._numerator"),
    agrees("mixed[2]"),
];

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

#[tokio::test]
async fn each_library_value_is_shown_as_its_own_repr_writes_it() {
    let texts: Vec<&str> = CASES.iter().map(|case| case.python).collect();
    for interpreter in VERSIONS {
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
