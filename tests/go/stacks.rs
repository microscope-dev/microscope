//! Stacks that continue from a thread's system stack onto a goroutine's
//! own, across each of the runtime's stack switches.

use microscope::{
    Backtrace, BreakpointSpec, CodeRole, Evaluation, ExecutionContext, Expression, FrameKind,
    InferiorState, ScalarValue, StackFrameId, StackSegment, StopContext, StopId, StopReason,
    ThreadActivity, ThreadId, ThreadState, UnwindTermination, VariableState,
    VariableUnavailableReason, VariableValue, VirtualAddress,
};

use crate::support::{self, Scenario};

const BUILDS: [&str; 2] = ["stacks-go-o0", "stacks-go-o2"];

/// The most stops a test passes over before the one it waits for.
const MAX_STOPS: usize = 200;

/// A backtrace's function names, grouped by the stack each run of frames
/// is on, innermost first.
fn segments(trace: &Backtrace) -> Vec<(StackSegment, Vec<String>)> {
    let mut segments: Vec<(StackSegment, Vec<String>)> = Vec::new();
    for frame in trace.frames.iter() {
        let name = frame
            .function
            .as_ref()
            .map_or_else(|| "?".to_owned(), |function| function.name.to_string());
        match segments.last_mut() {
            Some((segment, names)) if *segment == frame.segment => names.push(name),
            _ => segments.push((frame.segment, vec![name])),
        }
    }

    segments
}

/// Runs a fixture to a breakpoint on `function`, then on until `wanted`
/// accepts the backtrace of a thread the breakpoint stopped. Threads that
/// hit it together stop together, each with its own reason.
async fn stop_in(
    fixture: &str,
    function: &str,
    wanted: impl Fn(&[(StackSegment, Vec<String>)]) -> bool,
) -> (Scenario, Backtrace) {
    let mut scenario = crate::invariants::checked(fixture);
    scenario.add_breakpoint(function).await;
    let reason = scenario.run_to_stop().await;
    let trace = stop_where(&mut scenario, reason, fixture, wanted).await;
    (scenario, trace)
}

/// From a breakpoint stop, resumes until `wanted` accepts the backtrace of
/// a thread a breakpoint stopped.
async fn stop_where(
    scenario: &mut Scenario,
    mut reason: StopReason,
    fixture: &str,
    wanted: impl Fn(&[(StackSegment, Vec<String>)]) -> bool,
) -> Backtrace {
    for _ in 0..MAX_STOPS {
        // The program's own signals go on to it.
        if let StopReason::Exception(_) = reason {
            reason = scenario.resume_to_stop().await;
            continue;
        }

        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );

        let snapshot = scenario.snapshot().await;
        let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
            panic!("{fixture}: not stopped");
        };

        let hit = snapshot.threads.iter().filter(|thread| {
            matches!(
                thread.state,
                ThreadState::Stopped {
                    reason: Some(StopReason::Breakpoint { .. })
                }
            )
        });

        for thread in hit {
            let trace = scenario
                .operation(
                    "backtrace",
                    scenario
                        .handle()
                        .at(innermost(stop_id, thread.id))
                        .backtrace(),
                )
                .await;
            if wanted(&segments(&trace)) {
                return trace;
            }
        }

        reason = scenario.resume_to_stop().await;
    }

    panic!("{fixture}: no stop was the one sought");
}

const fn innermost(stop: StopId, thread: ThreadId) -> StopContext {
    StopContext {
        stop,
        execution: ExecutionContext::Thread(thread),
        frame: StackFrameId::INNERMOST,
    }
}

#[tokio::test]
async fn system_stack_calls_unwind_onto_their_goroutine() {
    for fixture in BUILDS {
        let (mut scenario, trace) = stop_in(fixture, "runtime.readmemstats_m", |_| true).await;
        assert_eq!(
            segments(&trace),
            [
                (
                    StackSegment::System,
                    names(&[
                        "runtime.readmemstats_m",
                        "runtime.ReadMemStats.func1",
                        "runtime.systemstack",
                    ])
                ),
                (
                    StackSegment::Task,
                    names(&[
                        "runtime.ReadMemStats",
                        "main.stats",
                        "main.main",
                        "runtime.main",
                        "runtime.goexit",
                    ])
                ),
            ],
            "{fixture}: {trace:#?}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{fixture}");
        check_every_thread(&mut scenario, fixture).await;
        scenario.shutdown().await;
    }
}

/// With the world stopped, every thread's stack unwinds to its first
/// frame, and a thread running the runtime's code for a goroutine goes on
/// to the goroutine's stack.
async fn check_every_thread(scenario: &mut Scenario, fixture: &str) {
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("{fixture}: not stopped");
    };

    for thread in snapshot.threads.iter() {
        let trace = scenario
            .operation(
                "thread backtrace",
                scenario
                    .handle()
                    .at(innermost(stop_id, thread.id))
                    .backtrace(),
            )
            .await;
        let context = format!("{fixture}: {:?}\n{trace:#?}", thread.activity);
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        let stacks = segments(&trace)
            .into_iter()
            .map(|(segment, _)| segment)
            .collect::<Vec<_>>();

        // A stack moves outward only, from a signal stack to a system stack
        // and from either to the task the thread runs, each once: runs of
        // frames on one stack are already merged.
        let order = [
            StackSegment::Signal,
            StackSegment::System,
            StackSegment::Task,
        ];
        let ranks = stacks
            .iter()
            .map(|segment| order.iter().position(|known| known == segment))
            .collect::<Vec<_>>();
        let innermost = trace
            .frames
            .first()
            .and_then(|frame| frame.function.as_ref())
            .map(|function| function.name.as_ref());

        let last = match thread.activity.as_ref().expect(&context) {
            ThreadActivity::Task { .. } => StackSegment::Task,
            // A thread `clone` just made has no g yet, and its stack
            // begins where `clone` made it.
            ThreadActivity::Idle if innermost == Some("runtime.clone") => StackSegment::Thread,
            // An idle thread runs the scheduler on its system stack.
            ThreadActivity::Idle => StackSegment::System,
            ThreadActivity::Unknown(reason) => panic!("{context}: {reason}"),
            // Every thread of a Go program runs the runtime's scheduler.
            ThreadActivity::Outside => panic!("{context}: outside the runtime"),
        };

        assert_eq!(stacks.last(), Some(&last), "{context}");
        if last != StackSegment::Thread {
            assert!(
                ranks.iter().all(Option::is_some) && ranks.is_sorted(),
                "{context}: {stacks:?}"
            );
        }

        if last == StackSegment::Task {
            let outermost = trace
                .frames
                .last()
                .and_then(|frame| frame.function.as_ref());
            assert_eq!(
                outermost.map(|function| function.name.as_ref()),
                Some("runtime.goexit"),
                "{context}"
            );
        }
    }
}

#[tokio::test]
async fn a_growing_stack_unwinds_onto_its_goroutine() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.newstack", |segments| {
            segments
                .get(1)
                .is_some_and(|(_, names)| names.first().is_some_and(|name| name == "main.grow"))
        })
        .await;
        let found = segments(&trace);
        let context = format!("{fixture}: {trace:#?}");
        assert_eq!(
            found[0],
            (
                StackSegment::System,
                names(&["runtime.newstack", "runtime.morestack"])
            ),
            "{context}"
        );

        // The goroutine is in the prologue of the call that needed more
        // stack, below every call before it.
        let (segment, task) = &found[1];
        assert_eq!(*segment, StackSegment::Task, "{context}");
        let calls = task.iter().take_while(|name| *name == "main.grow").count();
        assert!(calls > 1, "{context}");
        assert_eq!(
            task[calls..],
            names(&["main.main.func2", "runtime.goexit"]),
            "{context}"
        );
        assert_eq!(found.len(), 2, "{context}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_parking_goroutine_unwinds_onto_its_goroutine() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.park_m", |segments| {
            segments
                .get(1)
                .is_some_and(|(_, names)| names.iter().any(|name| name == "main.main.func1"))
        })
        .await;
        let context = format!("{fixture}: {trace:#?}");
        assert_eq!(
            segments(&trace),
            [
                (
                    StackSegment::System,
                    names(&["runtime.park_m", "runtime.mcall"])
                ),
                (
                    StackSegment::Task,
                    names(&[
                        "runtime.gopark",
                        "runtime.chanrecv",
                        "runtime.chanrecv1",
                        "main.main.func1",
                        "runtime.goexit",
                    ])
                ),
            ],
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_thread_switching_to_a_goroutine_unwinds_on_its_system_stack() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "gogo", |_| true).await;
        let found = segments(&trace);
        let context = format!("{fixture}: {trace:#?}");
        let [(segment, names)] = found.as_slice() else {
            panic!("{context}");
        };

        // The scheduler has made the goroutine the thread's own before it
        // switches to it.
        assert_eq!(*segment, StackSegment::System, "{context}");
        assert_eq!(names.first().map(String::as_str), Some("gogo"), "{context}");
        assert!(
            names.iter().any(|name| name == "runtime.schedule"),
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

/// A goroutine that parks switches to the system stack, where the
/// scheduler may then give the thread to another goroutine, as often to one
/// that has never run. The goroutine that switched has left the thread, so
/// its frames are not the thread's: the stack ends at the switch.
#[tokio::test]
async fn a_thread_given_to_another_goroutine_ends_where_the_last_switched() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.gogo", |segments| {
            segments.first().is_some_and(|(_, names)| {
                names.iter().any(|name| name == "runtime.execute")
                    && names.iter().any(|name| name == "runtime.mcall")
            })
        })
        .await;
        let found = segments(&trace);
        let context = format!("{fixture}: {trace:#?}");
        let [(StackSegment::System, names)] = found.as_slice() else {
            panic!("{context}");
        };

        assert_eq!(
            names.last().map(String::as_str),
            Some("runtime.mcall"),
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

/// The runtime reads the clock through the vDSO from the system stack, and
/// keeps the goroutine's stack pointer in a register the vDSO preserves.
#[tokio::test]
async fn a_vdso_call_unwinds_onto_its_caller() {
    for fixture in BUILDS {
        let mut scenario = crate::invariants::checked(fixture);
        let start = scenario.add_breakpoint("main.stats").await;
        scenario.run_to_stop().await;

        // The program holds the vDSO's clock_gettime itself.
        let expression = Expression::parse("runtime.vdsoClockgettimeSym").expect("an expression");
        let clock = scenario
            .operation("vDSO clock", scenario.handle().evaluate(&expression))
            .await;
        let Evaluation::Value { value, .. } = clock else {
            panic!("{fixture}: {clock:?}");
        };

        let VariableState::Available {
            value: VariableValue::Scalar(ScalarValue::Unsigned(address)),
            ..
        } = value.state
        else {
            panic!("{fixture}: {value:?}");
        };

        scenario.remove_breakpoint(start.id).await;
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(VirtualAddress::new(
                u64::try_from(address).expect("an address"),
            )))
            .await;
        let reason = scenario.resume_to_stop().await;

        // A goroutine's call, not the scheduler's on an idle thread.
        let trace = stop_where(&mut scenario, reason, fixture, |segments| {
            segments.windows(2).any(|pair| {
                pair[0].1.iter().any(|name| name == "runtime.nanotime1")
                    && pair[1].0 == StackSegment::Task
            })
        })
        .await;
        let context = format!("{fixture}: {trace:#?}");
        let functions = trace
            .frames
            .iter()
            .map(|frame| {
                frame
                    .function
                    .as_ref()
                    .map(|function| function.name.as_ref())
            })
            .collect::<Vec<_>>();

        // The vDSO's frame has no function, and its caller's is named.
        assert_eq!(functions[0], None, "{context}");
        assert_eq!(functions[1], Some("runtime.nanotime1"), "{context}");
        assert!(functions.len() > 3, "{context}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");

        let outermost = trace
            .frames
            .last()
            .and_then(|frame| frame.function.as_ref());
        assert_eq!(
            outermost.map(|function| function.role),
            Some(CodeRole::Outermost),
            "{context}"
        );
        scenario.shutdown().await;
    }
}

/// Until a vDSO call has saved its stack pointer, the register that will
/// hold it holds whatever its caller left there, and the frame is still
/// on the stack it was called on.
#[tokio::test]
async fn a_vdso_call_unwinds_before_it_saves_its_stack_pointer() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.nanotime1", |segments| {
            segments[0].0 == StackSegment::Task
        })
        .await;
        let context = format!("{fixture}: {trace:#?}");
        let functions = segments(&trace);
        assert_eq!(functions.len(), 1, "{context}");
        assert_eq!(functions[0].1[0], "runtime.nanotime1", "{context}");
        assert_eq!(
            functions[0].1.last().map(String::as_str),
            Some("runtime.goexit"),
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

/// A signal's handler runs on the thread's signal stack and returns
/// through the runtime's signal trampoline, above the frame the signal
/// interrupted, whose registers the kernel saved in the signal frame.
#[tokio::test]
async fn a_signal_handler_unwinds_onto_the_frame_it_interrupted() {
    for fixture in BUILDS {
        // The runtime's own signals, such as preemption's, may reach the
        // handler first, even within `main.interrupt`; wait for its kill.
        let (scenario, trace) = stop_in(fixture, "runtime.sighandler", |segments| {
            segments
                .iter()
                .any(|(_, names)| names.iter().any(|name| name == "syscall.Tgkill"))
        })
        .await;
        let found = segments(&trace);
        let context = format!("{fixture}: {trace:#?}");
        assert_eq!(
            found[0],
            (
                StackSegment::Signal,
                names(&[
                    "runtime.sighandler",
                    "runtime.sigtrampgo",
                    "runtime.sigtramp",
                    "runtime.sigreturn__sigaction",
                ])
            ),
            "{context}"
        );
        let (segment, task) = &found[1];
        assert_eq!(*segment, StackSegment::Task, "{context}");
        assert!(
            task.ends_with(&names(&[
                "syscall.Tgkill",
                "main.interrupt",
                "main.main",
                "runtime.main",
                "runtime.goexit",
            ])),
            "{context}"
        );

        // The trampoline's instruction and the interrupted frame's are
        // their own, not return addresses.
        let trampoline = found[0].1.len() - 1;

        for frame in &trace.frames[trampoline..=trampoline + 1] {
            assert_eq!(frame.kind, FrameKind::Signal, "{context}");
        }

        assert_eq!(found.len(), 2, "{context}");
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

/// A signal that interrupts a thread on its system stack unwinds through
/// its handler and the system stack onto the goroutine that switched there,
/// although the thread runs the signal's goroutine, not the system one.
#[tokio::test]
async fn a_signal_on_the_system_stack_unwinds_onto_its_goroutine() {
    for fixture in BUILDS {
        let (mut scenario, trace) = stop_in(fixture, "runtime.stopTheWorldWithSema", |segments| {
            segments
                .iter()
                .any(|(_, names)| names.iter().any(|name| name == "main.stats"))
        })
        .await;
        let ExecutionContext::Thread(thread) = trace.context else {
            panic!("{fixture}: {trace:#?}");
        };

        let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
            panic!("{fixture}: not stopped");
        };

        support::signal_thread(process_id, thread, nix::libc::SIGUSR1);
        scenario.remove_all_breakpoints().await;
        scenario.add_breakpoint("runtime.sighandler").await;
        let reason = scenario.resume_to_stop().await;
        let trace = stop_where(&mut scenario, reason, fixture, |segments| {
            segments.iter().any(|(_, names)| {
                names
                    .iter()
                    .any(|name| name == "runtime.stopTheWorldWithSema")
            })
        })
        .await;
        assert_eq!(trace.context, ExecutionContext::Thread(thread), "{fixture}");
        assert_eq!(
            segments(&trace),
            [
                (
                    StackSegment::Signal,
                    names(&[
                        "runtime.sighandler",
                        "runtime.sigtrampgo",
                        "runtime.sigtramp",
                        "runtime.sigreturn__sigaction",
                    ])
                ),
                (
                    StackSegment::System,
                    names(&[
                        "runtime.stopTheWorldWithSema",
                        "runtime.stopTheWorld.func1",
                        "runtime.systemstack",
                    ])
                ),
                (
                    StackSegment::Task,
                    names(&[
                        "runtime.stopTheWorld",
                        "runtime.ReadMemStats",
                        "main.stats",
                        "main.main",
                        "runtime.main",
                        "runtime.goexit",
                    ])
                ),
            ],
            "{fixture}: {trace:#?}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{fixture}");
        scenario.shutdown().await;
    }
}

/// The runtime turns a fault into a call to `sigpanic` from the faulting
/// instruction, which the caller's frame names exactly.
#[tokio::test]
async fn a_fault_unwinds_onto_the_instruction_that_faulted() {
    let line = support::source_line("tests/fixtures/go/stacks/main.go", "// the fault");
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "runtime.sigpanic", |_| true).await;
        let context = format!("{fixture}: {trace:#?}");
        assert_eq!(
            segments(&trace),
            [(
                StackSegment::Task,
                names(&[
                    "runtime.sigpanic",
                    "main.fault",
                    "main.main",
                    "runtime.main",
                    "runtime.goexit",
                ])
            )],
            "{context}"
        );

        let faulted = &trace.frames[1];
        assert_eq!(faulted.kind, FrameKind::Signal, "{context}");
        assert_eq!(
            faulted.source.as_ref().map(|source| source.line.get()),
            Some(line),
            "{context}"
        );
        assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");
        scenario.shutdown().await;
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A pointer below its frame's stack pointer points at memory only the
/// frame's callees use, which they may have freed: the pointer is stale,
/// and what it pointed at is not shown as if it were still there.
#[tokio::test]
async fn a_pointer_below_its_frames_stack_pointer_is_stale() {
    for fixture in BUILDS {
        let (scenario, trace) = stop_in(fixture, "main.hold", |_| true).await;
        scenario
            .operation(
                "select stale",
                scenario.handle().select_frame(trace.frames[1].id),
            )
            .await;

        let evaluate = |text: &'static str| {
            let expression = Expression::parse(text).expect("an expression");
            let handle = scenario.handle().clone();
            async move { handle.evaluate(&expression).await.expect(text) }
        };

        let Evaluation::Value { value, .. } = evaluate("pointer").await else {
            panic!("{fixture}: pointer is no value");
        };

        let VariableState::Available {
            value: VariableValue::Address(pointer),
            ..
        } = value.state
        else {
            panic!("{fixture}: {value:?}");
        };

        let Evaluation::Value { value, .. } = evaluate("*pointer").await else {
            panic!("{fixture}: *pointer is no value");
        };

        assert_eq!(
            value.state,
            VariableState::Unavailable(VariableUnavailableReason::BelowStackPointer {
                address: pointer.address,
            }),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}
