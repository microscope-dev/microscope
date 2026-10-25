//! TypeScript run through source maps: tsx compiling each file as Node
//! loads it, with the map inline, and esbuild bundles, with the map in a
//! file beside them, as `CommonJS` and as an ES module. A breakpoint on a
//! line of the TypeScript stops there, including in top-level code that
//! runs as the script loads, frames show the TypeScript's lines, a step
//! goes to the next TypeScript line, and an exception stops where it is
//! thrown.

use std::path::{Path, PathBuf};
use std::process::Command;

use microscope::{
    BreakpointOptions, BreakpointSpec, ExceptionStops, ExitStatus, LineNumber, SourceLanguage,
    StepKind, StopReason,
};

use crate::session::{MATRIX, NodeSession};
use crate::support::{ScratchDir, node};

const SOURCE: &str = "maps/app.ts";

/// How the TypeScript reaches Node.
#[derive(Debug, Clone, Copy)]
enum Compiler {
    /// tsx's loader, which compiles each file as Node loads it.
    Tsx,

    /// An esbuild bundle in `CommonJS`, its map in a file.
    Bundle,

    /// An esbuild bundle as an ES module.
    ModuleBundle,
}

const COMPILERS: [Compiler; 3] = [Compiler::Tsx, Compiler::Bundle, Compiler::ModuleBundle];

impl Compiler {
    /// The Node arguments and the script that run the source, compiled
    /// into `scratch` where it is compiled ahead.
    fn script(self, scratch: &Path) -> (Vec<String>, PathBuf) {
        let bundle = |name: &str, format: &str| {
            let out = scratch.join(name);
            let status = Command::new(esbuild())
                .arg(node::fixture(SOURCE))
                .args([
                    "--bundle",
                    "--platform=node",
                    "--sourcemap",
                    "--log-level=warning",
                ])
                .arg(format!("--format={format}"))
                .arg(format!("--outfile={}", out.display()))
                .status()
                .expect("run esbuild");
            assert!(status.success(), "esbuild failed");
            (Vec::new(), out)
        };
        match self {
            Self::Tsx => (
                vec![
                    "--import".to_owned(),
                    tsx().join("dist/loader.mjs").display().to_string(),
                ],
                node::fixture(SOURCE),
            ),
            Self::Bundle => bundle("out.js", "cjs"),
            Self::ModuleBundle => bundle("out.mjs", "esm"),
        }
    }
}

fn tsx() -> PathBuf {
    std::env::var_os("MICROSCOPE_TSX")
        .expect("MICROSCOPE_TSX names tsx; run tests in the dev shell")
        .into()
}

fn esbuild() -> PathBuf {
    std::env::var_os("MICROSCOPE_ESBUILD")
        .expect("MICROSCOPE_ESBUILD names esbuild; run tests in the dev shell")
        .into()
}

/// The line of the source that carries `marker`.
fn line_of(marker: &str) -> u32 {
    let text = std::fs::read_to_string(node::fixture(SOURCE)).expect("read the source");
    let index = text
        .lines()
        .position(|line| line.contains(&format!("// {marker}")))
        .expect("a marked line");
    u32::try_from(index + 1).expect("a small line")
}

fn at(marker: &str) -> (BreakpointSpec, BreakpointOptions) {
    (
        BreakpointSpec::Source {
            path: node::fixture(SOURCE),
            line: LineNumber::new(u64::from(line_of(marker))).expect("a line"),
        },
        BreakpointOptions {
            pending: true,
            ..BreakpointOptions::default()
        },
    )
}

/// The selected frame's file, line, and language, and its local `name`.
async fn where_stopped(session: &NodeSession, name: &str) -> (String, u32, SourceLanguage, String) {
    let scenario = &session.scenario;
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let selected = snapshot.selected_frame.expect("a selected frame");
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let frame = trace
        .frames
        .iter()
        .find(|frame| frame.id == selected)
        .expect("the selected frame is in the backtrace");
    let recorded = frame.recorded.as_ref().expect("a script frame");
    let (path, line) = recorded.place.as_ref().map_or_else(
        || (format!("no place: {:?}", recorded.problem), 0),
        |place| (place.path.to_string(), place.line),
    );
    let variables = scenario
        .operation("variables", scenario.handle().variables())
        .await;
    let value = variables
        .variables
        .iter()
        .find(|variable| &*variable.name == name)
        .map_or_else(
            || "absent".to_owned(),
            |variable| match &variable.state {
                microscope::VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } => presentation.summary.to_string(),
                other => format!("{other:?}"),
            },
        );
    (path, line, recorded.language, value)
}

/// On every build and through every compiler, breakpoints on the
/// TypeScript's lines stop there, top-level code included, with the
/// frame's locals; a step over goes to the next TypeScript line; and the
/// program prints what it computed.
#[tokio::test]
async fn typescript_stops_at_its_own_lines_through_every_compiler() {
    let source = node::fixture(SOURCE).display().to_string();
    let mut lies = Vec::new();
    for entry in MATRIX {
        for compiler in COMPILERS {
            let scratch = ScratchDir::new("node-maps");
            let (arguments, script) = compiler.script(scratch.path());
            let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
            let script = script.display().to_string();
            let session = NodeSession::unlaunched_with_node(entry, &script, &arguments);
            let (mut session, reason, _events) =
                session.run_to_stop(vec![at("TOP"), at("NORM")]).await;
            let mut seen = Vec::new();
            let mut reason = reason;
            // TOP, then NORM, then a step over to RETURN, then the end.
            for (local, next) in [
                ("origin", None),
                ("p", Some(StepKind::OverSource)),
                ("squared", None),
            ] {
                if !matches!(
                    reason,
                    StopReason::Breakpoint { .. } | StopReason::Step { .. }
                ) {
                    break;
                }
                seen.push(where_stopped(&session, local).await);
                reason = match next {
                    None => session.scenario.resume_to_stop().await,
                    Some(kind) => session.scenario.step_to_stop(kind).await,
                };
            }
            let expected = vec![
                (
                    source.clone(),
                    line_of("TOP"),
                    SourceLanguage::TypeScript,
                    // tsx keeps the `const`, not yet bound, in the module's
                    // function; esbuild makes it a `var`, which an ES module
                    // keeps in its module scope rather than the frame's.
                    match compiler {
                        Compiler::Tsx => "Unavailable(Unbound)",
                        Compiler::Bundle => "undefined",
                        Compiler::ModuleBundle => "absent",
                    }
                    .to_owned(),
                ),
                (
                    source.clone(),
                    line_of("NORM"),
                    SourceLanguage::TypeScript,
                    "{ x: 3, y: 4 }".to_owned(),
                ),
                (
                    source.clone(),
                    line_of("RETURN"),
                    SourceLanguage::TypeScript,
                    "25".to_owned(),
                ),
            ];
            let output = session.output();
            if seen != expected
                || reason != StopReason::Exited(ExitStatus::Code(0))
                || !output.contains("NORM 5 1")
            {
                lies.push(format!(
                    "node {entry} {compiler:?}: saw {seen:#?}, then {reason:?}; output:\n{output}"
                ));
            }
            session.shutdown().await;
        }
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

/// On every build, an exception thrown in a bundle stops at the
/// TypeScript line that throws it.
#[tokio::test]
async fn an_exception_stops_at_the_typescript_that_throws_it() {
    let source = node::fixture(SOURCE).display().to_string();
    let mut lies = Vec::new();
    for entry in MATRIX {
        let scratch = ScratchDir::new("node-maps");
        let (arguments, script) = Compiler::Bundle.script(scratch.path());
        assert!(arguments.is_empty());
        let script = script.display().to_string();
        let session = NodeSession::unlaunched_with_node(entry, &script, &[]);
        let stops = ExceptionStops::NONE
            .with("javascript-thrown", true)
            .expect("a filter");
        session
            .scenario
            .operation(
                "choose exceptions",
                session.scenario.handle().set_exception_stops(stops),
            )
            .await;
        let (mut session, reason, _events) = session.run_to_stop(Vec::new()).await;
        let stopped = where_stopped(&session, "reason").await;
        let expected = (
            source.clone(),
            line_of("THROW"),
            SourceLanguage::TypeScript,
            "'mapped'".to_owned(),
        );
        if !matches!(reason, StopReason::LanguageException(_)) || stopped != expected {
            lies.push(format!("node {entry}: {reason:?} at {stopped:?}"));
        }
        let end = session.scenario.resume_to_stop().await;
        if end != StopReason::Exited(ExitStatus::Code(0)) {
            lies.push(format!("node {entry}: then {end:?}"));
        }
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}

#[tokio::test]
async fn an_unreadable_source_map_keeps_generated_breakpoints_and_steps() {
    let scratch = ScratchDir::new("node-missing-map");
    let script = scratch.path().join("app.js");
    std::fs::write(&script, "function run(n) {\n  const squared = n * n;\n  return squared + 1;\n}\nconsole.log(run(9));\n//# sourceMappingURL=missing.js.map\n")
        .expect("write the script");
    for entry in MATRIX {
        let pending = NodeSession::unlaunched_with_node(entry, &script.display().to_string(), &[]);
        let breakpoint = (
            BreakpointSpec::Source {
                path: script.clone(),
                line: LineNumber::new(2).expect("a line"),
            },
            BreakpointOptions {
                pending: true,
                ..BreakpointOptions::default()
            },
        );
        let (mut session, reason, _) = pending.run_to_stop(vec![breakpoint]).await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{entry}: {reason:?}"
        );
        let place = where_stopped(&session, "n").await;
        assert_eq!(
            (place.0, place.1, place.3),
            (script.display().to_string(), 2, "9".to_owned())
        );
        assert!(matches!(
            session.scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step { .. }
        ));
        let place = where_stopped(&session, "squared").await;
        assert_eq!(
            (place.0, place.1, place.3),
            (script.display().to_string(), 3, "81".to_owned())
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        session.shutdown().await;
    }
}
