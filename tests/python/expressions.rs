//! Expressions in a Python frame, each giving what CPython's own `eval`
//! gave in that frame, or refused where Python would run code microscope
//! does not run, or compute a result microscope does not compute.

use microscope::{
    Evaluation, Expression, ExpressionErrorKind, FloatValue, ScalarValue, StopContext,
    VariableState, VariableValue,
};

use crate::locals::Stop;
use crate::session::{MATRIX, PythonSession};

/// What microscope must do with an expression.
#[derive(Debug, Clone, Copy)]
enum Expect {
    /// Give what CPython gave: the same value, or the error that stands
    /// for the exception CPython raised.
    Agrees,

    /// Refuse it, whatever CPython gave.
    Refuses(ExpressionErrorKind),
}

/// An expression as each language writes it, and what microscope must do
/// with it.
#[derive(Debug, Clone, Copy)]
struct Case {
    python: &'static str,
    microscope: &'static str,
    expect: Expect,
}

/// An expression both languages write alike, on which they agree.
const fn agrees(text: &'static str) -> Case {
    Case {
        python: text,
        microscope: text,
        expect: Expect::Agrees,
    }
}

/// An expression both languages write alike, which microscope refuses.
const fn refuses(text: &'static str, kind: ExpressionErrorKind) -> Case {
    Case {
        python: text,
        microscope: text,
        expect: Expect::Refuses(kind),
    }
}

/// An expression each language writes its own way, on which they agree.
const fn spelled(python: &'static str, microscope: &'static str) -> Case {
    Case {
        python,
        microscope,
        expect: Expect::Agrees,
    }
}

/// Every expression the fixture evaluates in its frame of `values`.
const CASES: &[Case] = &[
    // Names: a local before a global, a global before a built-in.
    agrees("count"),
    agrees("LIMIT"),
    agrees("max"),
    agrees("None"),
    agrees("True"),
    // Numbers, with truth values counting as 0 and 1.
    agrees("count + negative"),
    agrees("count - LIMIT"),
    agrees("count * ratio"),
    agrees("-negative"),
    agrees("~count"),
    agrees("count & 3"),
    agrees("count | 8"),
    agrees("count ^ 1"),
    agrees("count << 2"),
    agrees("negative >> 1"),
    agrees("True + 1"),
    agrees("flag + flag"),
    agrees("count * 1.0 / 2"),
    agrees("count % 4"),
    agrees("big"),
    agrees("big - 1"),
    agrees("huge"),
    agrees("ratio / 0"),
    refuses("count / 2", ExpressionErrorKind::Arithmetic),
    refuses("negative % 5", ExpressionErrorKind::Arithmetic),
    refuses("ratio % 1", ExpressionErrorKind::Arithmetic),
    refuses("huge + 1", ExpressionErrorKind::Arithmetic),
    // Comparisons: numbers exactly, text by its characters, and values of
    // different types unequal.
    agrees("count == 7"),
    agrees("count != 7"),
    agrees("count > 6.5"),
    agrees("ratio < 1"),
    agrees("count == name"),
    agrees("count != None"),
    agrees("None == None"),
    agrees("name == \"ann\""),
    agrees("name < \"bob\""),
    agrees("wide == \"日本語\""),
    agrees("astral == \"snake 🐍\""),
    agrees("wide > name"),
    agrees("data == \"ab\""),
    refuses("count < name", ExpressionErrorKind::Type),
    refuses("numbers == numbers", ExpressionErrorKind::Unsupported),
    // Truth.
    spelled("not empty", "!empty"),
    spelled("not numbers", "!numbers"),
    spelled("not name", "!name"),
    spelled("not None", "!None"),
    refuses("point", ExpressionErrorKind::Type).truth(),
    // Items of sequences, from their ends when negative.
    agrees("numbers[0]"),
    agrees("numbers[-1]"),
    agrees("numbers[-3]"),
    agrees("numbers[flag]"),
    agrees("numbers[3]"),
    agrees("numbers[-4]"),
    agrees("pair[1]"),
    agrees("numbers[0] + numbers[2] * 10"),
    agrees("nested[\"a\"][1][\"b\"][0]"),
    refuses("name[0]", ExpressionErrorKind::Unsupported),
    agrees("count[0]"),
    // Keys of dicts, found by hash as Python finds them.
    agrees("ages[\"bob\"]"),
    agrees("ages[name]"),
    agrees("ages[wide]"),
    agrees("ages[astral]"),
    agrees("ages[\"eve\"]"),
    agrees("squares[2]"),
    agrees("squares[2.0]"),
    agrees("squares[True]"),
    agrees("squares[-1]"),
    agrees("squares[1099511627776]"),
    agrees("squares[0.5]"),
    agrees("squares[4]"),
    agrees("labels[None]"),
    agrees("labels[1]"),
    agrees("labels[1.0]"),
    agrees("labels[0]"),
    agrees("doubtful[\"s\"]"),
    agrees("doubtful[1.5]"),
    agrees("many[\"k5\"]"),
    agrees("many[\"k299\"]"),
    agrees("many[\"k3\"]"),
    refuses("doubtful[7]", ExpressionErrorKind::Unsupported),
    // Lengths.
    agrees("len(name)"),
    agrees("len(wide)"),
    agrees("len(astral)"),
    agrees("len(data)"),
    agrees("len(numbers)"),
    agrees("len(empty)"),
    agrees("len(pair)"),
    agrees("len(ages)"),
    agrees("len(many)"),
    agrees("len(count)"),
    // What only running the program's code could give.
    refuses("numbers + numbers", ExpressionErrorKind::Type),
    refuses("name + name", ExpressionErrorKind::Type),
    refuses("point.x", ExpressionErrorKind::Unsupported),
];

impl Case {
    /// The case for the truth of the expression, which Python asks with
    /// `not not`.
    const fn truth(self) -> Self {
        Self {
            python: "not not point",
            microscope: "!!point",
            ..self
        }
    }
}

/// What microscope gave for an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A value, in the text CPython's `repr` gives it.
    Value(String),

    /// An error of this kind.
    Error(ExpressionErrorKind),

    /// No value, for the reason given.
    Missing(String),
}

/// What microscope gives for `text` in the frame `at`.
pub async fn outcome(stop: &Stop<'_>, at: StopContext, text: &str) -> Outcome {
    let expression = Expression::parse(text).unwrap_or_else(|error| panic!("{text}: {error:?}"));
    match stop.scenario.handle().at(at).evaluate(&expression).await {
        Ok(Evaluation::Value { value, .. }) => match &value.state {
            VariableState::Available {
                value: scalar,
                presentation,
                ..
            } => match (presentation, scalar) {
                (Some(presentation), _) => Outcome::Value(presentation.summary.to_string()),
                (None, VariableValue::Scalar(scalar)) => Outcome::Value(match scalar {
                    ScalarValue::Signed(value) => value.to_string(),
                    ScalarValue::Unsigned(value) => value.to_string(),
                    ScalarValue::Boolean(true) => "True".to_owned(),
                    ScalarValue::Boolean(false) => "False".to_owned(),
                    ScalarValue::Floating(FloatValue::Binary64(bits)) => {
                        format!("{:?}", f64::from_bits(*bits))
                    }
                    other => format!("{other:?}"),
                }),
                (None, other) => Outcome::Missing(format!("no text for {other:?}")),
            },
            state => Outcome::Missing(format!("{state:?}")),
        },
        Ok(other) => Outcome::Missing(format!("{other:?}")),
        Err(microscope::Error::Expression(error)) => Outcome::Error(error.kind),
        Err(error) => Outcome::Missing(error.to_string()),
    }
}

/// The error kind that stands for the exception CPython raised, by its
/// type's name.
fn standing_for(exception: &str) -> Option<ExpressionErrorKind> {
    Some(match exception {
        "KeyError" => ExpressionErrorKind::MissingKey,
        "IndexError" => ExpressionErrorKind::Bounds,
        "TypeError" => ExpressionErrorKind::Type,
        "ZeroDivisionError" => ExpressionErrorKind::Arithmetic,
        _ => return None,
    })
}

/// Checks what microscope gave for a case against what CPython gave.
fn check(case: &Case, reported: &str, outcome: &Outcome) -> Result<(), String> {
    let expected = match (case.expect, reported.strip_prefix('!')) {
        (Expect::Refuses(kind), _) => Outcome::Error(kind),
        (Expect::Agrees, Some(exception)) => Outcome::Error(
            standing_for(exception)
                .ok_or_else(|| format!("no error stands for CPython's {exception}"))?,
        ),
        (Expect::Agrees, None) => Outcome::Value(reported.to_owned()),
    };

    if *outcome == expected {
        Ok(())
    } else {
        Err(format!(
            "`{}`: CPython gave {reported}, microscope must give {expected:?}, and gave {outcome:?}",
            case.microscope
        ))
    }
}

/// What CPython gave for each expression the fixture evaluated, by its
/// text.
fn reported_results(truth: &[Vec<String>]) -> std::collections::BTreeMap<&str, &str> {
    truth
        .iter()
        .filter(|fields| fields[0] == "evaluated")
        .map(|fields| (fields[1].as_str(), fields[2].as_str()))
        .collect()
}

/// Every expression gives what CPython gave in the same frame, or is
/// refused as its case says, on every interpreter in the matrix.
#[tokio::test]
async fn each_expression_gives_what_cpython_gave_in_the_frame() {
    let texts: Vec<&str> = CASES.iter().map(|case| case.python).collect();
    for interpreter in MATRIX {
        let mut session = PythonSession::launch(interpreter, "expressions.py", &texts).await;
        let truth = session.stop_at("parked").await;
        let printer = truth
            .iter()
            .find(|fields| fields[0] == "thread" && fields[3] == "printer")
            .and_then(|fields| fields[1].parse().ok())
            .expect("the printer's thread");
        let reported = reported_results(&truth);
        let stop = Stop {
            id: session.stop().await,
            scenario: &session.scenario,
            context: session.context(),
        };
        let at = stop.frame(printer, "values").await;
        let mut lies = Vec::new();
        for case in CASES {
            let reported = reported.get(case.python).unwrap_or_else(|| {
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

/// The comparison fails on a wrong value, a wrong error, a value where
/// CPython raised, an error where it gave a value, and a refusal of
/// another kind.
#[test]
fn the_comparison_fails_on_the_lies_it_looks_for() {
    let value = |text: &str| Outcome::Value(text.to_owned());
    let case = agrees("numbers[0]");
    check(&case, "10", &value("10")).expect("the truth holds");
    assert!(check(&case, "10", &value("11")).is_err(), "a wrong value");
    assert!(
        check(&case, "10", &Outcome::Error(ExpressionErrorKind::Bounds)).is_err(),
        "an error where CPython gave a value"
    );
    assert!(
        check(&case, "10", &Outcome::Missing("unreadable".to_owned())).is_err(),
        "no value where CPython gave one"
    );

    let raised = agrees("numbers[3]");
    check(
        &raised,
        "!IndexError",
        &Outcome::Error(ExpressionErrorKind::Bounds),
    )
    .expect("the truth holds");
    assert!(
        check(
            &raised,
            "!IndexError",
            &Outcome::Error(ExpressionErrorKind::MissingKey)
        )
        .is_err(),
        "a wrong error"
    );
    assert!(
        check(&raised, "!IndexError", &value("None")).is_err(),
        "a value where CPython raised"
    );
    assert!(
        check(
            &raised,
            "!AttributeError",
            &Outcome::Error(ExpressionErrorKind::Type)
        )
        .is_err(),
        "an exception no error stands for"
    );

    let refused = refuses("count / 2", ExpressionErrorKind::Arithmetic);
    check(
        &refused,
        "3.5",
        &Outcome::Error(ExpressionErrorKind::Arithmetic),
    )
    .expect("the truth holds");
    assert!(
        check(&refused, "3.5", &value("3.5")).is_err(),
        "a refusal computed"
    );
    assert!(
        check(&refused, "3.5", &Outcome::Error(ExpressionErrorKind::Type)).is_err(),
        "a refusal of another kind"
    );
}
