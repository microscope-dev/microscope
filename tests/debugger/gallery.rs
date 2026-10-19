//! Values agree with what their programs say they are. Each gallery prints
//! a tab-separated `TRUTH` line per value before it calls `reached()`, and
//! the values are read from `reached`'s caller, or from the stopped frame
//! at another breakpoint. A value microscope shows as available must equal the
//! truth; in optimized builds it may instead be unavailable for a typed
//! reason, unless the test requires it.

use std::process::Stdio;

use microscope::{
    FloatValue, IntegerValue, LaunchOptions, StackFrameId, TypeInfo, ValueChildRelationship,
    Variable, VariableKind, VariableValue, VariableValueSource,
};

use super::*;
use crate::support::ScratchDir;

/// One line a gallery printed: what one value at one checkpoint is.
#[derive(Debug, Clone)]
struct Truth {
    checkpoint: String,
    path: String,
    kind: String,
    value: String,
}

fn truths(output: &str) -> Vec<Truth> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.strip_prefix("TRUTH\t")?.split('\t');
            Some(Truth {
                checkpoint: fields.next()?.to_owned(),
                path: fields.next()?.to_owned(),
                kind: fields.next()?.to_owned(),
                value: fields.next().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// How one gallery build runs and what it must show.
struct Gallery<'a> {
    fixture: &'a str,

    /// The functions to stop in: `reached`, whose caller is inspected, and
    /// others inspected where they stop.
    breakpoints: &'a [&'a str],

    /// Checkpoints the run must reach, in order.
    checkpoints: &'a [&'a str],
    optimized: bool,

    /// `checkpoint:path` values an optimized build must show all the same.
    required: &'a [&'a str],

    /// What only the names of the compiler's own variables begin with,
    /// none of which may be listed.
    reserved: &'a [&'a str],

    /// `checkpoint:path` values a function returned that this build's
    /// calling convention does not say where to find, which must be shown
    /// as unknown for that reason.
    unknown: &'a [&'a str],
}

/// Runs a gallery through every checkpoint and checks every truth it
/// printed against what microscope shows there.
async fn check_gallery(gallery: &Gallery<'_>) {
    let fixture = gallery.fixture;
    let scratch = ScratchDir::new("gallery");
    let output_path = scratch.path().join("stdout");
    let output = std::fs::File::create(&output_path).expect("create the gallery's output");
    let mut scenario = Scenario::launch(fixture);
    for breakpoint in gallery.breakpoints {
        scenario.add_breakpoint(breakpoint).await;
    }

    let mut reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(output)),
            ..LaunchOptions::default()
        })
        .await;
    let mut visited = Vec::new();
    let mut failures = Vec::new();
    loop {
        match reason {
            StopReason::Breakpoint { .. } => {}

            // Go preempts its threads with SIGURG, which never stops a
            // program on its own.
            StopReason::Exception(ref exception) if exception.code == 23 => {
                reason = scenario.resume_to_stop().await;
                continue;
            }
            StopReason::Exited(ExitStatus::Code(0)) => break,
            other => panic!("{fixture} stopped unexpectedly: {other:?}"),
        }

        let printed = std::fs::read_to_string(&output_path).expect("read the gallery's output");
        let truths = truths(&printed);
        let checkpoint = truths
            .last()
            .unwrap_or_else(|| panic!("{fixture} stopped before printing any truth"))
            .checkpoint
            .clone();
        let variables = checkpoint_variables(&mut scenario, fixture, &checkpoint).await;
        let returned = checkpoint.starts_with("returned-");
        for variable in &variables {
            if gallery
                .reserved
                .iter()
                .any(|prefix| variable.name.starts_with(prefix))
            {
                failures.push(format!(
                    "{checkpoint}: lists the compiler's {}",
                    variable.name
                ));
            }
        }

        for truth in truths.iter().filter(|truth| truth.checkpoint == checkpoint) {
            if gallery
                .unknown
                .contains(&format!("{checkpoint}:{}", truth.path).as_str())
            {
                if let Err(failure) = check_unknown(&variables, truth) {
                    failures.push(format!("{checkpoint}: {}: {failure}", truth.path));
                }

                continue;
            }

            // The calling convention says where every returned value is.
            let required = returned
                || gallery
                    .required
                    .contains(&format!("{checkpoint}:{}", truth.path).as_str());
            if let Err(failure) =
                check_truth(&scenario, &variables, truth, gallery.optimized && !required).await
            {
                failures.push(format!("{checkpoint}: {}: {failure}", truth.path));
            }
        }

        visited.push(checkpoint);
        reason = scenario.resume_to_stop().await;
    }

    assert!(failures.is_empty(), "{fixture}:\n{}", failures.join("\n"));
    assert_eq!(visited, gallery.checkpoints, "{fixture}");
    assert_eq!(scenario.shutdown().await, Some(ExitStatus::Code(0)));
}

/// The gallery's check fails on a value other than the program's, and on
/// a variable listed where the program says there is none.
#[tokio::test]
async fn the_gallery_check_fails_on_values_its_program_did_not_report() {
    let fixture = "values-go-o0";
    let scratch = ScratchDir::new("gallery");
    let output_path = scratch.path().join("stdout");
    let output = std::fs::File::create(&output_path).expect("create the gallery's output");
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("main.reached").await;
    let mut reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(output)),
            ..LaunchOptions::default()
        })
        .await;
    while matches!(&reason, StopReason::Exception(exception) if exception.code == 23) {
        reason = scenario.resume_to_stop().await;
    }

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    let printed = std::fs::read_to_string(&output_path).expect("read the gallery's output");
    let truths = truths(&printed);
    let checkpoint = truths.last().expect("a truth").checkpoint.clone();
    let variables = checkpoint_variables(&mut scenario, fixture, &checkpoint).await;
    let truth = truths
        .iter()
        .find(|truth| {
            truth.checkpoint == checkpoint
                && !["absent", "hidden", "result", "type"].contains(&truth.kind.as_str())
        })
        .expect("a truth of a value");
    check_truth(&scenario, &variables, truth, false)
        .await
        .expect("the program's own value");
    let wrong = Truth {
        value: format!("{}1", truth.value),
        ..truth.clone()
    };

    assert!(
        check_truth(&scenario, &variables, &wrong, false)
            .await
            .is_err()
    );
    let absent = Truth {
        path: truth.path.split('.').next().expect("a name").to_owned(),
        kind: "absent".to_owned(),
        ..truth.clone()
    };

    assert!(
        check_truth(&scenario, &variables, &absent, false)
            .await
            .is_err()
    );
    scenario.shutdown().await;
}

/// The variables a checkpoint's truths are about: those of `reached`'s
/// caller, or of the stopped frame elsewhere, or, for a `returned-`
/// checkpoint, what that caller returned once it is finished.
async fn checkpoint_variables(
    scenario: &mut Scenario,
    fixture: &str,
    checkpoint: &str,
) -> Vec<Variable> {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let innermost = trace.frames[0]
        .function
        .as_ref()
        .map(|function| function.name.to_string())
        .unwrap_or_default();
    if innermost.ends_with("reached") {
        let caller: StackFrameId = trace.frames[1].id;
        scenario
            .operation("select caller", scenario.handle().select_frame(caller))
            .await;
    }

    let returned = checkpoint.starts_with("returned-");
    if returned {
        let reason = scenario.step_to_stop(StepKind::Out).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}: {checkpoint}"
        );
    }

    let snapshot = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    snapshot
        .variables
        .iter()
        .filter(|variable| !returned || variable.kind == VariableKind::Returned)
        .cloned()
        .collect()
}

/// Checks that the returned value a truth is about is listed, as unknown
/// because its calling convention does not say where it is.
fn check_unknown(variables: &[Variable], truth: &Truth) -> Result<(), String> {
    let name = truth
        .path
        .split('.')
        .next()
        .expect("a path names a variable");
    let variable = variables
        .iter()
        .find(|variable| variable.name.as_ref() == name)
        .ok_or("is not listed")?;
    match &variable.state {
        VariableState::Unavailable(microscope::VariableUnavailableReason::Unsupported(
            microscope::UnsupportedVariableFeature::ReturnPlace,
        )) => Ok(()),
        state => Err(format!(
            "is not unknown for its calling convention: {state:?}"
        )),
    }
}

/// Checks that a variable the compiler made for itself, named by the
/// whole path, which may begin with a dot, is not listed but its name
/// reaches it.
async fn check_hidden(
    scenario: &Scenario,
    variables: &[Variable],
    name: &str,
    may_be_unavailable: bool,
) -> Result<(), String> {
    if let Some(variable) = variables
        .iter()
        .find(|variable| variable.name.as_ref() == name)
    {
        return Err(format!("is listed: {variable:?}"));
    }

    match scenario.handle().variable(name).await {
        Ok(_) => Ok(()),
        Err(_) if may_be_unavailable => Ok(()),
        Err(error) => Err(format!("is not reachable by name: {error:?}")),
    }
}

/// Checks what a truth says of the variable rather than of its value: that
/// it is a result, or, for its type, that a generic value whose type
/// argument is unknown keeps its shape for a typed reason.
fn check_variable(
    variable: &Variable,
    truth: &Truth,
    may_be_unavailable: bool,
) -> Option<Result<(), String>> {
    if truth.kind == "result" {
        return Some(if variable.kind == VariableKind::Result {
            Ok(())
        } else {
            Err(format!("is listed as a {:?}", variable.kind))
        });
    }

    // A generic value whose type argument is unknown keeps its shape, for
    // a typed reason.
    if truth.kind == "type"
        && let Some(reason) = &variable.unresolved_shape
    {
        let shape = variable.type_info.as_ref().map(|info| info.name.as_ref());
        return Some(
            if !shape.is_some_and(|shape| shape.starts_with("go.shape.")) {
                Err(format!(
                    "is unresolved ({reason}) without its shape: {shape:?}"
                ))
            } else if may_be_unavailable {
                Ok(())
            } else {
                Err(format!("has its shape's type: {reason}"))
            },
        );
    }

    None
}

/// Checks one truth, accepting a typed unavailable or malformed state
/// when `may_be_unavailable`.
async fn check_truth(
    scenario: &Scenario,
    variables: &[Variable],
    truth: &Truth,
    may_be_unavailable: bool,
) -> Result<(), String> {
    if truth.kind == "hidden" {
        return check_hidden(scenario, variables, &truth.path, may_be_unavailable).await;
    }

    let mut segments = truth.path.split('.');
    let name = segments.next().expect("a path names a variable");
    let variable = variables
        .iter()
        .rev()
        .find(|variable| variable.name.as_ref() == name);
    if truth.kind == "absent" {
        return variable.map_or(Ok(()), |variable| Err(format!("is listed: {variable:?}")));
    }

    // Optimized code may describe no variable at all where its value is
    // gone.
    let Some(variable) = variable else {
        return if may_be_unavailable {
            Ok(())
        } else {
            Err("is not listed".to_owned())
        };
    };

    // A value microscope cannot show yet must say so.
    if truth.kind == "unsupported" {
        return match &variable.state {
            VariableState::Unavailable(microscope::VariableUnavailableReason::Unsupported(_)) => {
                Ok(())
            }
            state => Err(format!("is not unsupported: {state:?}")),
        };
    }

    if let Some(checked) = check_variable(variable, truth, may_be_unavailable) {
        return checked;
    }

    let mut type_info = variable.type_info.clone();
    let mut type_name = variable.type_info.as_ref().map(|info| info.name.clone());
    let mut state = variable.state.clone();
    for segment in segments {
        let VariableState::Available { .. } = &state else {
            break;
        };

        let child = child_named(scenario, &state, segment).await;

        // Optimized code may leave out what nothing reads, such as a
        // variable a closure captured.
        let child = match child {
            Ok(child) => child,
            Err(_) if may_be_unavailable => return Ok(()),
            Err(failure) => return Err(failure),
        };

        type_name = Some(child.type_info.name.clone());
        type_info = Some(child.type_info.clone());
        state = child.state.clone();
    }

    let VariableState::Available { .. } = &state else {
        return if may_be_unavailable
            && matches!(
                state,
                VariableState::Unavailable(_) | VariableState::Malformed(_)
            ) {
            Ok(())
        } else {
            Err(format!("is not available: {state:?}"))
        };
    };

    let shown = shown(&truth.kind, &state, type_info.as_ref());
    if shown == truth.value {
        Ok(())
    } else {
        Err(format!(
            "shows {shown} ({}) instead of {} {}",
            type_name.as_deref().unwrap_or("?"),
            truth.kind,
            truth.value
        ))
    }
}

/// The child a path's segment names among the value's own children, or
/// else among what a view shows of it.
async fn child_named(
    scenario: &Scenario,
    state: &VariableState,
    segment: &str,
) -> Result<microscope::ValueChild, String> {
    let VariableState::Available {
        children,
        presentation,
        ..
    } = state
    else {
        return Err("is not available".to_owned());
    };

    let presented = presentation
        .as_deref()
        .filter(|presentation| presentation.shape != microscope::PresentedShape::Raw)
        .map(|presentation| &presentation.children);
    let mut child = Err(format!("has no child {segment}"));
    for children in std::iter::once(children).chain(presented) {
        let microscope::ValueChildren::Available(reference) = children else {
            continue;
        };

        let page = scenario
            .operation(
                "value children",
                scenario.handle().value_children(
                    reference.clone(),
                    microscope::ValueChildQuery {
                        offset: 0,
                        limit: 256,
                    },
                ),
            )
            .await;
        if let Some(found) = page.children.iter().find(|child| named(child, segment)) {
            return Ok(found.clone());
        }

        child = Err(format!(
            "has no child {segment} among {} children",
            reference.total()
        ));
    }

    child
}

/// Whether a path's segment names `child`: a member by its name, an
/// element by its zero-based index or by its source indices in
/// parentheses.
fn named(child: &microscope::ValueChild, segment: &str) -> bool {
    match &child.relationship {
        ValueChildRelationship::Member(member) => member.name.as_deref() == Some(segment),
        ValueChildRelationship::SliceElement { index }
        | ValueChildRelationship::Element { index } => index.to_string() == segment,
        ValueChildRelationship::ArrayElement { index, indices } => segment
            .strip_prefix('(')
            .and_then(|rest| rest.strip_suffix(')'))
            .map_or_else(
                || index.to_string() == segment,
                |source| {
                    source
                        .split(',')
                        .map(str::parse)
                        .collect::<Result<Vec<i128>, _>>()
                        == Ok(indices.to_vec())
                },
            ),
        _ => false,
    }
}

/// What microscope shows of an available value, written as a truth of `kind`
/// writes it.
fn shown(kind: &str, state: &VariableState, type_info: Option<&TypeInfo>) -> String {
    let VariableState::Available { value, text, .. } = state else {
        unreachable!("only available values are shown");
    };

    let integer = |value: &IntegerValue| match value {
        IntegerValue::Signed(value) => value.to_string(),
        IntegerValue::Unsigned(value) => value.to_string(),
        _ => unreachable!("integers are signed or unsigned"),
    };

    match (kind, value) {
        ("int" | "uint" | "number", VariableValue::Scalar(ScalarValue::Signed(value))) => {
            value.to_string()
        }
        ("int" | "uint" | "number", VariableValue::Scalar(ScalarValue::Unsigned(value))) => {
            value.to_string()
        }
        ("int" | "uint", VariableValue::Enumeration { value, .. }) => integer(value),
        ("f32", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary32(bits)))) => {
            format!("{bits:#x}")
        }
        ("f64", VariableValue::Scalar(ScalarValue::Floating(FloatValue::Binary64(bits)))) => {
            format!("{bits:#x}")
        }

        (
            "c64",
            VariableValue::Scalar(ScalarValue::Complex {
                real: FloatValue::Binary32(real),
                imaginary: FloatValue::Binary32(imaginary),
            }),
        ) => format!("{real:#x}:{imaginary:#x}"),
        (
            "c128",
            VariableValue::Scalar(ScalarValue::Complex {
                real: FloatValue::Binary64(real),
                imaginary: FloatValue::Binary64(imaginary),
            }),
        ) => format!("{real:#x}:{imaginary:#x}"),
        ("summary", _) => microscope::value_summary(type_info, state),
        ("addressable", _) => match state {
            VariableState::Available {
                source: VariableValueSource::Memory(_),
                ..
            } => String::new(),
            VariableState::Available { source, .. } => format!("{source:?}"),
            _ => unreachable!("only available values are shown"),
        },
        ("func", VariableValue::Function { code, function }) => {
            microscope::function_text(*code, function.as_deref())
        }
        ("len", VariableValue::Slice { length, .. }) => length.to_string(),
        ("symbol", VariableValue::Enumeration { value, matches }) => {
            microscope::symbol_text(*value, matches).unwrap_or_else(|| format!("{value:?}"))
        }
        ("string", _) => text.as_ref().map_or_else(
            || format!("{value:?}"),
            |text| microscope::quoted_text(text),
        ),
        ("type", _) => type_info.map_or("?", |info| &info.name).to_owned(),
        _ => format!("{value:?}"),
    }
}

#[tokio::test]
async fn c_pieces_agree_with_their_program() {
    for (fixture, optimized, required) in [
        ("pieces-gcc-o0", false, &[][..]),
        (
            "pieces-gcc-o2",
            true,
            &[
                "split:local.first",
                "complex:small",
                "complex:large",
                "complex:product",
            ][..],
        ),
        (
            "pieces-clang-o2",
            true,
            &[
                "split:local.first",
                "split:local.second",
                "split:pair.second",
                "complex:large",
                "complex:product",
            ][..],
        ),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &["split", "complex"],
            optimized,
            required,
            reserved: &[],
            unknown: &[],
        })
        .await;
    }
}

#[tokio::test]
async fn go_values_agree_with_their_program() {
    for (fixture, optimized, required) in [
        ("values-go-o0", false, &[][..]),
        (
            "values-go-o2",
            true,
            &[
                "pieces:text",
                "pieces:numbers",
                "pieces:numbers.1",
                "pieces:pair.X",
                "pieces:pair.Y",
                "pieces:ratio",
                "complex:small",
                "complex:large",
                "funcs:closure",
                "funcs:closure.offset",
                "funcs:closure.total",
                "escape:counter",
            ][..],
        ),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["main.reached", "main.pieces"],
            checkpoints: &[
                "complex",
                "funcs",
                "escape",
                "results",
                "unnamed-results",
                "visibility-before",
                "visibility-after",
                "temporaries",
                "constants",
                "shape-int",
                "shape-float",
                "shape-celsius",
                "shape-point",
                "shape-other",
                "pieces",
                "returned-registers",
                "returned-stack",
                "returned-deferred",
                "returned-generic",
            ],
            optimized,
            required,
            reserved: &[".", "#", "&"],
            unknown: &[],
        })
        .await;
    }
}

/// Every checkpoint of a returns gallery, in order.
const C_RETURNS: &[&str] = &[
    "returned-int",
    "returned-char",
    "returned-bool",
    "returned-int128",
    "returned-enum",
    "returned-float",
    "returned-double",
    "returned-long-double",
    "returned-complex-float",
    "returned-complex-double",
    "returned-ints",
    "returned-pair",
    "returned-mixed",
    "returned-flipped",
    "returned-floats",
    "returned-doubles",
    "returned-vector",
    "returned-big",
    "returned-text",
    "returned-bits",
    "returned-union",
    "returned-void",
];

#[tokio::test]
async fn c_returned_values_agree_with_their_program() {
    for (fixture, optimized) in [
        ("returns-c-gcc-o0", false),
        ("returns-c-gcc-o2", true),
        ("returns-c-clang-o0", false),
        ("returns-c-clang-o2", true),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: C_RETURNS,
            optimized,
            required: &[],
            reserved: &[],
            unknown: &[],
        })
        .await;
    }
}

#[tokio::test]
async fn cpp_returned_values_agree_with_their_program() {
    // GCC does not record whether calls pass a class by value, which
    // decides where a small one is returned.
    let gcc = &[
        "returned-plain:r_plain.a",
        "returned-plain:r_plain.b",
        "returned-derived:r_derived.own",
        "returned-owner:r_owner.value",
    ][..];
    for (fixture, optimized, unknown) in [
        ("returns-cpp-gcc-o0", false, gcc),
        ("returns-cpp-gcc-o2", true, gcc),
        ("returns-cpp-clang-o2", true, &[][..]),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &[
                "returned-int",
                "returned-plain",
                "returned-derived",
                "returned-owner",
                "returned-large",
            ],
            optimized,
            required: &[],
            reserved: &[],
            unknown,
        })
        .await;
    }
}

#[tokio::test]
async fn rust_returned_values_agree_with_their_program() {
    for (fixture, optimized, unknown) in [
        (
            "returns-rust-o0",
            false,
            &["returned-triple:r_triple.a"][..],
        ),
        // Optimization stops returning the payload of `r_pending`, which
        // nothing reads, and says the convention no longer holds.
        (
            "returns-rust-o2",
            true,
            &["returned-triple:r_triple.a", "returned-pending:r_pending"][..],
        ),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &[
                "returned-int",
                "returned-bool",
                "returned-u128",
                "returned-f64",
                "returned-f32",
                "returned-pair",
                "returned-level",
                "returned-poll",
                "returned-pending",
                "returned-floats",
                "returned-option",
                "returned-triple",
                "returned-unit",
            ],
            optimized,
            required: &[],
            reserved: &[],
            // Rust's own convention is unspecified for aggregates of more
            // than two scalars.
            unknown,
        })
        .await;
    }
}

#[tokio::test]
async fn zig_returned_values_agree_with_their_program() {
    for (fixture, optimized, unknown) in [
        ("returns-zig-o0", false, &[][..]),
        ("returns-zig-o2", true, &[][..]),
        // Zig's own convention is unspecified for aggregates.
        (
            "returns-zig-self-hosted",
            false,
            &["returned-pair:r_pair"][..],
        ),
    ] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &[
                "returned-void",
                "returned-int",
                "returned-bool",
                "returned-u64",
                "returned-f64",
                "returned-f32",
                "returned-level",
                "returned-pair",
            ],
            optimized,
            required: &[],
            reserved: &[],
            unknown,
        })
        .await;
    }
}

#[tokio::test]
async fn odin_values_agree_with_their_program() {
    for (fixture, optimized) in [("values-odin-o0", false), ("values-odin-o2", true)] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["values::reached"],
            checkpoints: &[
                "scalars",
                "records",
                "slices",
                "unions",
                "returned-int",
                "returned-f64",
                "returned-bool",
            ],
            optimized,
            required: &[],
            reserved: &[],
            unknown: &[],
        })
        .await;
    }
}

#[tokio::test]
async fn fortran_values_agree_with_their_program() {
    for (fixture, optimized) in [("values-fortran-o0", false), ("values-fortran-o2", true)] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &[
                "scalars",
                "records",
                "descriptors",
                "section",
                "strings",
                "returned-int",
                "returned-double",
                "returned-logical",
            ],
            optimized,
            required: &[],
            reserved: &[".", "_"],
            unknown: &[],
        })
        .await;
    }
}

#[tokio::test]
async fn d_values_agree_with_their_program() {
    for (fixture, optimized) in [("values-d-o0", false), ("values-d-o2", true)] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &[
                "scalars",
                "records",
                "slices",
                "loop",
                "returned-int",
                "returned-double",
                "returned-bool",
            ],
            optimized,
            required: &[],
            reserved: &["__"],
            unknown: &[],
        })
        .await;
    }
}

#[tokio::test]
async fn nim_values_agree_with_their_program() {
    for (fixture, optimized) in [("values-nim-gcc-o0", false), ("values-nim-clang-o2", true)] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["values::reached"],
            checkpoints: &["scalars", "records", "strings", "seqs"],
            optimized,
            required: &[],
            reserved: &["colontmp", "nimErr_", "FR_"],
            unknown: &[],
        })
        .await;
    }
}

#[tokio::test]
async fn ada_values_agree_with_their_program() {
    for (fixture, optimized) in [("values-ada-o0", false), ("values-ada-o2", true)] {
        check_gallery(&Gallery {
            fixture,
            breakpoints: &["reached"],
            checkpoints: &[
                "scalars",
                "records",
                "bounded",
                "strings",
                "returned-int",
                "returned-float",
                "returned-boolean",
            ],
            optimized,
            required: &[],
            reserved: &["C", "S", "T"],
            unknown: &[],
        })
        .await;
    }
}
