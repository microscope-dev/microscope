//! A large real program: gofmt, built from the pinned toolchain's own
//! sources, formatting a file the test wrote. Its breakpoints are named by
//! package, its stacks run through many of the standard library's
//! packages, its values are what it parsed from that file, and it formats
//! the file under the debugger as it does alone.

use std::process::{Command, Stdio};

use microscope::{
    Evaluation, Expression, LaunchOptions, PresentedCount, StepKind, StopReason, ValueChildQuery,
    ValueChildRelationship, ValueChildren, VariableState, VariableValue,
};

use crate::invariants::checked;
use crate::stops::{integer, place};
use crate::support::{Scenario, ScratchDir};

const BUILDS: [&str; 2] = ["gofmt-go-o0", "gofmt-go-o2"];
const INPUT: &str = "tests/fixtures/go/gofmt/sample.go";

/// Go's keywords, which `go/token` maps to their tokens.
const KEYWORDS: [&str; 25] = [
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];

/// The names of the stop's frames, innermost first.
async fn frames(scenario: &Scenario) -> Vec<String> {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    trace
        .frames
        .iter()
        .map(|frame| {
            frame
                .function
                .as_ref()
                .map_or_else(String::new, |function| function.name.to_string())
        })
        .collect()
}

async fn evaluate(scenario: &Scenario, text: &str) -> VariableState {
    let expression = Expression::parse(text).expect("an expression");
    match scenario
        .operation(text, scenario.handle().evaluate(&expression))
        .await
    {
        Evaluation::Value { value, .. } => value.state,
        other => panic!("{text}: {other:?}"),
    }
}

/// A value as the debugger writes it: a string quoted, a constant by its
/// name, or the summary its view gives.
fn written(state: &VariableState) -> String {
    match state {
        VariableState::Available {
            text: Some(text), ..
        } => microscope::quoted_text(text),
        VariableState::Available {
            value: VariableValue::Enumeration { value, matches },
            ..
        } => microscope::symbol_text(*value, matches).unwrap_or_else(|| format!("{value:?}")),
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation.summary.to_string(),
        other => format!("{other:?}"),
    }
}

/// Checks the file gofmt parsed, at the printer's entry.
async fn check_parsed(scenario: &Scenario, fixture: &str) {
    assert_eq!(
        written(&evaluate(scenario, "src.Name.Name").await),
        "\"sample\"",
        "{fixture}"
    );
    assert_eq!(
        integer(scenario, "len(src.Decls)").await,
        Some(4),
        "{fixture}"
    );

    // Each declaration is an interface, shown as what it holds. A
    // position is one past the offset of the keyword that begins it.
    let source = std::fs::read_to_string(INPUT).expect("read the input");
    let position = |keyword: &str| {
        source
            .find(&format!("\n{keyword} "))
            .expect("a declaration")
            + 2
    };

    for (index, keyword) in ["import", "type", "var"].into_iter().enumerate() {
        let declaration = written(&evaluate(scenario, &format!("src.Decls[{index}]")).await);
        let shown = format!(
            "*go/ast.GenDecl *{{Doc: nil, TokPos: {}, Tok: go/token.{},",
            position(keyword),
            keyword.to_uppercase()
        );
        assert!(declaration.starts_with(&shown), "{fixture}: {declaration}");
    }

    let function = written(&evaluate(scenario, "src.Decls[3]").await);
    assert!(
        function.starts_with("*go/ast.FuncDecl *{Doc: nil, Recv: nil,"),
        "{fixture}: {function}"
    );
}

/// Checks `go/token`'s map of keywords: every keyword, each once, mapped
/// to the token its own name spells.
async fn check_keywords(scenario: &Scenario, fixture: &str) {
    let map = evaluate(scenario, "`go/token.keywords`").await;
    let VariableState::Available {
        presentation: Some(presentation),
        ..
    } = &map
    else {
        panic!("{fixture}: {map:?}");
    };

    assert_eq!(
        presentation.count,
        Some(PresentedCount::Exact(KEYWORDS.len() as u64)),
        "{fixture}"
    );
    let ValueChildren::Available(reference) = &presentation.children else {
        panic!("{fixture}: {presentation:?}");
    };

    let page = scenario
        .operation(
            "keywords",
            scenario.handle().value_children(
                std::sync::Arc::clone(reference),
                ValueChildQuery {
                    offset: 0,
                    limit: 64,
                },
            ),
        )
        .await;
    let mut entries = page
        .children
        .iter()
        .filter_map(|child| match &child.relationship {
            ValueChildRelationship::Entry { key, .. } => {
                Some((written(&key.state), written(&child.state)))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    entries.sort();
    let mut expected = KEYWORDS
        .iter()
        .map(|keyword| {
            (
                format!("\"{keyword}\""),
                format!("go/token.{}", keyword.to_uppercase()),
            )
        })
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(entries, expected, "{fixture}");
}

/// The lines of the body of the function the stop is at the start of,
/// through its closing brace.
async fn body(scenario: &Scenario) -> std::ops::RangeInclusive<u64> {
    let context = scenario
        .operation("source", scenario.handle().source_context(32))
        .await;
    let declaration = context.line.get();
    let closing = context
        .lines
        .iter()
        .find(|line| line.number.get() > declaration && line.text.as_ref() == "}")
        .expect("the function's closing brace");
    declaration + 1..=closing.number.get()
}

/// Steps over the printer's statements for a file, which stay in its
/// function and goroutine until it returns to its caller, and returns the
/// lines they stopped at.
async fn walk_the_printer(scenario: &mut Scenario, fixture: &str) -> Vec<u64> {
    let task = integer(scenario, "$task").await;
    let mut lines = Vec::new();
    for _ in 0..32 {
        let reason = scenario.step_to_stop(StepKind::OverSource).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        assert_eq!(integer(scenario, "$task").await, task, "{fixture}");
        let (function, line) = place(scenario).await;
        if function != "go/printer.(*printer).file" {
            assert_eq!(function, "go/printer.(*printer).printNode", "{fixture}");
            return lines;
        }

        lines.push(line);
    }

    panic!("{fixture}: the walk never left the printer: {lines:?}");
}

#[tokio::test]
async fn gofmt_formats_under_the_debugger_as_it_does_alone() {
    for fixture in BUILDS {
        let alone = Command::new(Scenario::fixture(fixture))
            .arg(INPUT)
            .stderr(Stdio::null())
            .output()
            .expect("run gofmt alone");
        assert!(alone.status.success(), "{fixture}: {alone:?}");

        let scratch = ScratchDir::new("gofmt");
        let output = scratch.path().join("stdout");
        let mut scenario = checked(fixture);

        // One by its package's path, one by its package's name.
        let printer = scenario.add_breakpoint("go/printer.(*printer).file").await;
        let parser = scenario.add_breakpoint("parser.ParseFile").await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec![INPUT.into()],
                stdout: Some(Stdio::from(
                    std::fs::File::create(&output).expect("create standard output"),
                )),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        let parsing = frames(&scenario).await;
        assert!(
            parsing.starts_with(&["go/parser.ParseFile".to_owned(), "main.parse".to_owned()]),
            "{fixture}: {parsing:?}"
        );
        assert!(
            parsing.contains(&"main.processFile".to_owned()),
            "{fixture}: {parsing:?}"
        );
        assert_eq!(
            written(&evaluate(&scenario, "filename").await),
            format!("\"{INPUT}\""),
            "{fixture}"
        );

        let reason = scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        let printing = frames(&scenario).await;
        for function in [
            "go/printer.(*printer).file",
            "go/printer.(*Config).Fprint",
            "main.format",
            "main.processFile",
            "runtime.goexit",
        ] {
            assert!(
                printing.contains(&function.to_owned()),
                "{fixture}: {function} in {printing:?}"
            );
        }

        check_parsed(&scenario, fixture).await;
        check_keywords(&scenario, fixture).await;

        // Unoptimized, the walk is the body's lines in order; optimized,
        // it may go back to a line whose work was interleaved with
        // another's, but it stops at every line of the body and no other.
        let body = body(&scenario).await;
        let walked = walk_the_printer(&mut scenario, fixture).await;
        if fixture.ends_with("o0") {
            assert_eq!(walked, body.clone().collect::<Vec<_>>(), "{fixture}");
        }

        assert!(
            walked.iter().all(|line| body.contains(line)),
            "{fixture}: {walked:?}"
        );
        assert!(
            body.clone().all(|line| walked.contains(&line)),
            "{fixture}: {walked:?}"
        );
        assert_eq!(walked.last(), Some(body.end()), "{fixture}");

        scenario.remove_breakpoint(printer.id).await;
        scenario.remove_breakpoint(parser.id).await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(microscope::ExitStatus::Code(0)),
            "{fixture}"
        );
        scenario.shutdown().await;
        assert_eq!(
            std::fs::read(&output).expect("read standard output"),
            alone.stdout,
            "{fixture}"
        );
    }
}
