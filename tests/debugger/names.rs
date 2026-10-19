//! Go function locations, written the ways Go programmers write them.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use microscope::{
    BreakpointHit, BreakpointId, BreakpointSpec, Error, ExecutionContext, ExitStatus,
    InferiorState, LineNumber, StackFrameId, StopContext, StopReason, ThreadId, ThreadState,
};

use crate::support::{Scenario, source_line};

const MAIN: &str = "tests/fixtures/go/names/main.go";
const SUM: &str = "tests/fixtures/go/names/sum.go";

/// What a location names: the functions its breakpoint must stop in, each
/// at least once, the file they are in, and for the fixture's own code the
/// marker of the lines between which every stop must be.
struct Named {
    location: &'static str,
    functions: &'static [&'static str],
    file: &'static str,
    lines: Option<(&'static str, &'static str)>,
}

const NAMED: &[Named] = &[
    // A generic function's name binds every instantiation.
    Named {
        location: "main.Sum",
        functions: &["main.Sum[go.shape.int]", "main.Sum[go.shape.float64]"],
        file: "names/sum.go",
        lines: Some((SUM, "Sum")),
    },
    // A method, unqualified in the stopless session.
    Named {
        location: "(*Counter).Add",
        functions: &["main.(*Counter).Add"],
        file: "names/main.go",
        lines: Some((MAIN, "Add")),
    },
    // A receiver written without its pointer still names its method, here
    // a generic type's, in each instantiation.
    Named {
        location: "main.Stack.Push",
        functions: &[
            "main.(*Stack[go.shape.int]).Push",
            "main.(*Stack[go.shape.string]).Push",
        ],
        file: "names/main.go",
        lines: Some((MAIN, "Push")),
    },
    // And a pointer receiver names a value receiver's method, not the
    // wrapper the compiler generates for pointers.
    Named {
        location: "main.(*Celsius).Fahrenheit",
        functions: &["main.Celsius.Fahrenheit"],
        file: "names/main.go",
        lines: Some((MAIN, "Fahrenheit")),
    },
    Named {
        location: "main.main.func1",
        functions: &["main.main.func1"],
        file: "names/main.go",
        lines: Some((MAIN, "closure")),
    },
    // A package by its name, here unlike its path's last element, and
    // inlined into main by an optimized build.
    Named {
        location: "rand.IntN",
        functions: &["math/rand/v2.IntN"],
        file: "math/rand/v2/rand.go",
        lines: None,
    },
    Named {
        location: "hex.Dump",
        functions: &["encoding/hex.Dump"],
        file: "encoding/hex/hex.go",
        lines: None,
    },
    // A package by its path, with a pointer method's receiver plain.
    Named {
        location: "encoding/hex.dumper.Close",
        functions: &["encoding/hex.(*dumper).Close"],
        file: "encoding/hex/hex.go",
        lines: None,
    },
    // The runtime's function, never the ABI wrapper sharing its name.
    Named {
        location: "runtime.newstack",
        functions: &["runtime.newstack"],
        file: "runtime/stack.go",
        lines: None,
    },
];

#[tokio::test]
async fn go_function_locations_stop_in_every_function_they_name() {
    for fixture in ["names-go-o0", "names-go-o2"] {
        let mut scenario = Scenario::launch(fixture);
        let mut named = BTreeMap::new();

        for entry in NAMED {
            let breakpoint = scenario.add_breakpoint(entry.location).await;
            assert!(
                !breakpoint.locations.is_empty(),
                "{fixture}: {} bound nothing",
                entry.location
            );
            named.insert(breakpoint.id, entry);
        }

        let newstack = scenario.snapshot().await.breakpoints;
        let newstack = newstack
            .iter()
            .find(|breakpoint| {
                breakpoint.spec == BreakpointSpec::Function("runtime.newstack".into())
            })
            .expect("runtime.newstack breakpoint");

        assert_eq!(
            newstack.locations.len(),
            1,
            "{fixture}: the ABI wrapper was chosen too"
        );

        let mut hit = BTreeMap::<BreakpointId, BTreeSet<String>>::new();
        let mut reason = scenario.run_to_stop().await;

        for _ in 0..1000 {
            match reason {
                // Each thread that hit a breakpoint at once with the one the
                // stop reports keeps its own hit as its reason.
                StopReason::Breakpoint { .. } => {
                    for (thread, hits) in thread_hits(&scenario).await {
                        let (function, file, line) = stopped_frame(&scenario, thread).await;
                        for breakpoint in hits.iter().map(|hit| hit.breakpoint) {
                            let entry = named[&breakpoint];
                            assert!(
                                entry.functions.contains(&function.as_str())
                                    && file.ends_with(entry.file),
                                "{fixture}: {} stopped thread {thread} in {function} at {file}:{line}",
                                entry.location
                            );

                            if let Some((path, marker)) = entry.lines {
                                let begins = source_line(path, &format!("names: {marker} begins"));
                                let ends = source_line(path, &format!("names: {marker} ends"));
                                assert!(
                                    (begins..=ends).contains(&line),
                                    "{fixture}: {} stopped at line {line}, outside {begins}..={ends}",
                                    entry.location
                                );
                            }

                            hit.entry(breakpoint).or_default().insert(function.clone());
                        }
                    }

                    reason = scenario.resume_to_stop().await;
                }

                // The runtime's preemption signal.
                StopReason::Exception(ref exception) if exception.code == 23 => {
                    reason = scenario.resume_to_stop().await;
                }
                StopReason::Exited(ExitStatus::Code(0)) => break,
                _ => panic!("{fixture} stopped unexpectedly: {reason:?}"),
            }
        }

        assert!(
            matches!(reason, StopReason::Exited(_)),
            "{fixture} never exited"
        );

        for (id, entry) in &named {
            let expected = entry
                .functions
                .iter()
                .map(|&function| function.to_owned())
                .collect::<BTreeSet<_>>();
            assert_eq!(
                hit.get(id),
                Some(&expected),
                "{fixture}: {} did not stop in every function it names",
                entry.location
            );
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn go_locations_that_name_too_much_or_nothing_say_why() {
    let fixture = "names-go-o0";
    let mut scenario = Scenario::launch(fixture);

    // Without a stop, a bare name is looked up in every package.
    match scenario
        .handle()
        .add_breakpoint(BreakpointSpec::Function("main".into()))
        .await
    {
        Err(Error::AmbiguousFunction { name, candidates }) => {
            assert_eq!(name, "main");
            assert_eq!(candidates, ["main.main", "runtime.main"]);
        }
        other => panic!("bare main was not ambiguous: {other:?}"),
    }

    // A file narrows a name to the functions declared in it.
    let file_function = |path: &str, function: &str| BreakpointSpec::FileFunction {
        path: path.into(),
        function: function.into(),
    };

    assert!(matches!(
        scenario
            .handle()
            .add_breakpoint(file_function("main.go", "Sum"))
            .await,
        Err(Error::FunctionNotFound(_))
    ));

    for function in ["Sum", "main.Sum"] {
        let breakpoint = scenario
            .add_breakpoint_spec(file_function("sum.go", function))
            .await;
        assert_eq!(breakpoint.locations.len(), 2, "sum.go:{function}");
    }

    // A line without a statement is refused with its neighbours that have
    // one, never moved to either.
    let line = source_line(MAIN, "names: no statement");

    match scenario
        .handle()
        .add_breakpoint(BreakpointSpec::Source {
            path: "main.go".into(),
            line: LineNumber::new(line).expect("line"),
        })
        .await
    {
        Err(Error::SourceLineWithoutStatement {
            line: refused,
            before,
            after,
            ..
        }) => {
            assert_eq!(refused, line);
            assert_eq!(before, Some(source_line(MAIN, "names: before blank")));
            assert_eq!(after, Some(source_line(MAIN, "names: after blank")));
        }
        other => panic!("a line without a statement was not refused: {other:?}"),
    }

    // At a stop, a bare name is first looked up in the stopped frame's
    // package, and the breakpoint keeps the name it resolved to.
    scenario.add_breakpoint("main.apply").await;

    assert!(matches!(
        scenario.run_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let breakpoint = scenario.add_breakpoint("main").await;
    assert_eq!(
        breakpoint.spec,
        BreakpointSpec::Function("main.main".into())
    );
    assert_eq!(breakpoint.locations.len(), 1);
    scenario.shutdown().await;
}

/// Each stopped thread whose own reason is a breakpoint hit, with its
/// hits.
async fn thread_hits(scenario: &Scenario) -> Vec<(ThreadId, Arc<[BreakpointHit]>)> {
    scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await
        .threads
        .iter()
        .filter_map(|thread| match &thread.state {
            ThreadState::Stopped {
                reason: Some(StopReason::Breakpoint { hits, .. }),
            } => Some((thread.id, Arc::clone(hits))),
            _ => None,
        })
        .collect()
}

/// The function, file, and line `thread` is stopped at.
async fn stopped_frame(scenario: &Scenario, thread: ThreadId) -> (String, String, u64) {
    let snapshot = scenario
        .operation("snapshot", scenario.handle().snapshot())
        .await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("the program is not stopped");
    };

    let at = StopContext {
        stop: stop_id,
        execution: ExecutionContext::Thread(thread),
        frame: StackFrameId::INNERMOST,
    };

    let backtrace = scenario
        .operation("backtrace", scenario.handle().at(at).backtrace())
        .await;

    let frame = &backtrace.frames[0];
    let function = frame
        .function
        .as_ref()
        .map_or_else(String::new, |function| function.name.to_string());
    let source = frame.source.as_ref().expect("stopped frame has a source");
    let image = scenario.handle().module_image();
    let file = image
        .source_file(source.file)
        .expect("source file")
        .path
        .display()
        .to_string();

    (function, file, source.line.get())
}
