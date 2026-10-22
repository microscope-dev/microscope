//! Expressions evaluated at real stops agree with what the program itself
//! computes. Each fixture prints `EXPECT` lines before it calls `barrier()`,
//! and every expression is evaluated from `barrier`'s caller. Besides
//! values, a fixture may expect `text`, the bytes a string holds; `range`,
//! the elements of a range, joined by commas; and `error`, the kind of
//! error the expression is.

use std::process::Stdio;

use microscope::{Evaluation, Expression, FloatValue, IntegerValue, StackFrameId, VariableValue};

use super::*;
use crate::support::ScratchDir;

/// One line a fixture printed: an expression and what it must evaluate to.
struct Expectation {
    expression: String,
    kind: String,
    value: String,
}

fn expectations(output: &str) -> Vec<Expectation> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("EXPECT\t")?.split('\t');
            Some(Expectation {
                expression: fields.next()?.to_owned(),
                kind: fields.next()?.to_owned(),
                value: fields.next()?.to_owned(),
            })
        })
        .collect()
}

/// A value as a fixture prints it, for the kind it printed.
fn rendered(kind: &str, value: &VariableValue) -> Option<String> {
    let integer = |value: &IntegerValue| match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
        _ => unreachable!("integer values are signed or unsigned"),
    };

    Some(match (kind, value) {
        ("int", VariableValue::Scalar(ScalarValue::Signed(value))) => value.to_string(),
        ("int", VariableValue::Scalar(ScalarValue::Unsigned(value))) => value.to_string(),
        ("int", VariableValue::Enumeration { value, .. }) => integer(value),
        ("bool", VariableValue::Scalar(ScalarValue::Boolean(value))) => value.to_string(),
        ("address", VariableValue::Address(address)) => format!("{:#x}", address.address.get()),
        ("f32", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary32(bits)))) => {
            format!("{bits:#x}")
        }
        ("f64", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(bits)))) => {
            format!("{bits:#x}")
        }

        (
            "f80",
            VariableValue::Scalar(ScalarValue::Floating(FloatValue::X87Extended {
                significand,
                sign_exponent,
            })),
        ) => format!("{sign_exponent:#x}:{significand:#x}"),
        _ => return None,
    })
}

/// Runs a fixture to `barrier`, selects its caller, and checks every
/// expectation. In optimized builds a row may be explicitly unavailable, or
/// refused for want of an address; a different value always fails.
async fn check_fixture(fixture: &str, barrier: &str, optimized: bool) {
    let scratch = ScratchDir::new("expressions");
    let output_path = scratch.path().join("stdout");
    let output = std::fs::File::create(&output_path).expect("create the fixture's output");
    let errors = output.try_clone().expect("share the fixture's output");
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint(barrier).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(output)),
            stderr: Some(Stdio::from(errors)),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );

    // The fixture flushes its expectations before it calls barrier.
    let printed = std::fs::read_to_string(&output_path).expect("read the fixture's output");
    let expectations = expectations(&printed);
    assert!(
        expectations.len() >= 10,
        "{fixture} printed its expectations: {printed}"
    );

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let caller: StackFrameId = trace.frames[1].id;

    scenario
        .operation("select caller", scenario.handle().select_frame(caller))
        .await;

    let mut failures = Vec::new();

    for expectation in &expectations {
        let expression = match Expression::parse(&expectation.expression) {
            Ok(expression) => expression,
            Err(error) => {
                failures.push(format!("`{}`: {error}", expectation.expression));
                continue;
            }
        };

        let result = scenario.handle().evaluate(&expression).await;
        if let Some(message) = disagreement(expectation, result, optimized) {
            failures.push(format!("`{}`: {message}", expectation.expression));
        }
    }

    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    scenario.shutdown().await;
}

/// Why an expression's result disagrees with what the program expects, if
/// it does. In optimized builds a value may be explicitly unavailable.
fn disagreement(
    expectation: &Expectation,
    result: Result<Evaluation, Error>,
    optimized: bool,
) -> Option<String> {
    let message = match (expectation.kind.as_str(), result) {
        ("error", Err(Error::Expression(error))) if error.kind.name() == expectation.value => {
            return None;
        }
        ("range", Ok(Evaluation::Range(page))) => {
            let elements = page
                .children
                .iter()
                .map(|child| match &child.state {
                    VariableState::Available { value, .. } => {
                        rendered("int", value).unwrap_or_else(|| format!("{value:?}"))
                    }
                    state => format!("{state:?}"),
                })
                .collect::<Vec<_>>()
                .join(",");
            if elements == expectation.value {
                return None;
            }

            format!("{elements} instead of {}", expectation.value)
        }
        ("text", Ok(Evaluation::Value { value, cause })) => match &value.state {
            VariableState::Available {
                text: Some(text), ..
            } if text.completion == microscope::TextCompletion::Complete
                && text.bytes.as_ref() == expectation.value.as_bytes() =>
            {
                return None;
            }
            VariableState::Unavailable(_) if optimized && cause.is_some() => return None,
            state => format!("{state:?} instead of {:?}", expectation.value),
        },
        (_, result) => match result {
            Ok(Evaluation::Value { value, cause }) => match &value.state {
                VariableState::Available { value: decoded, .. } => {
                    match rendered(&expectation.kind, decoded) {
                        Some(actual) if actual == expectation.value => return None,
                        actual => format!(
                            "{} ({}) instead of {}",
                            actual.unwrap_or_else(|| format!("{decoded:?}")),
                            value
                                .type_info
                                .map(|info| info.name)
                                .as_deref()
                                .unwrap_or("?"),
                            expectation.value
                        ),
                    }
                }
                VariableState::Unavailable(_) if optimized && cause.is_some() => return None,
                state => format!("{state:?} at {cause:?}"),
            },
            // An optimized value may be kept where it has no address.
            Err(Error::Expression(error))
                if optimized && error.kind == microscope::ExpressionErrorKind::NotAnLvalue =>
            {
                return None;
            }
            other => format!("{other:?}"),
        },
    };

    Some(message)
}

#[tokio::test]
async fn c_expressions_agree_with_the_program_across_the_compiler_matrix() {
    for compiler in ["gcc", "clang"] {
        for optimization in ["o0", "o2"] {
            for linking in ["pie", "nopie"] {
                let fixture = format!("expressions-c-{compiler}-{optimization}-{linking}");
                check_fixture(&fixture, "barrier", optimization == "o2").await;
            }
        }
    }
}

#[tokio::test]
async fn cpp_rust_go_and_zig_expressions_agree_with_their_programs() {
    for (fixture, barrier, optimized) in [
        ("expressions-cpp-gcc-o0", "barrier", false),
        ("expressions-cpp-clang-o2", "barrier", true),
        ("expressions-rust-o0", "barrier", false),
        ("expressions-rust-o2", "barrier", true),
        ("expressions-go-o0", "main.barrier", false),
        ("expressions-go-o2", "main.barrier", true),
        ("expressions-zig-o0", "barrier", false),
        ("expressions-zig-o2", "barrier", true),
    ] {
        check_fixture(fixture, barrier, optimized).await;
    }
}

/// Maps keyed by tuples, pairs among them, are indexed by tuple keys as
/// each program's own lookup indexes them, in each standard library.
#[tokio::test]
async fn tuple_keys_find_what_each_programs_maps_find() {
    for (fixture, optimized) in [
        ("keys-rust-o0", false),
        ("keys-rust-o2", true),
        ("keys-cpp-gcc-o0", false),
        ("keys-cpp-clang-o2", true),
        ("keys-cpp-libcxx-o0", false),
        ("keys-zig-o0", false),
        ("keys-zig-o2", true),
    ] {
        check_fixture(fixture, "barrier", optimized).await;
    }
}
