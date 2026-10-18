//! The agent is loaded by the fixture itself until P4.7 supplies injection.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use object::{Object as _, ObjectSection as _, ObjectSegment as _, ObjectSymbol as _};

use crate::support::{ScratchDir, python};

/// Own a fixture before the first fallible wait, including panic paths.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[path = "../../src/runtime_model/python/hook.rs"]
mod hook;

#[path = "../../src/runtime_model/python/agent.rs"]
pub mod agent_source;

#[test]
fn the_hook_is_a_loadable_exported_function() {
    let bytes = hook::library();
    let file = object::File::parse(bytes.as_slice()).expect("parse the generated hook");
    assert_eq!(file.kind(), object::ObjectKind::Dynamic);
    let symbol = file
        .dynamic_symbols()
        .find(|symbol| symbol.name() == Ok(hook::SYMBOL))
        .expect("the exported hook");
    assert!(symbol.is_global());
    assert!(symbol.is_definition());
    assert_eq!(symbol.kind(), object::SymbolKind::Text);
}

/// glibc before 2.35 relocates a library's dynamic section in place, so a
/// read-only one crashes the target as it loads the hook.
#[test]
fn the_hook_loads_under_older_glibc() {
    let scratch = ScratchDir::new("python-hook-glibc");
    let library = scratch.path().join("hook.so");
    std::fs::write(&library, hook::library()).expect("write the hook");
    for version in ["2.31", "2.35"] {
        let program =
            crate::support::Scenario::fixture(&format!("tls-modules-single-thread-glibc{version}"));
        let output = Command::new(&program)
            .env("LD_PRELOAD", &library)
            .stdin(Stdio::null())
            .output()
            .expect("run the program");
        assert!(output.status.success(), "glibc {version}: {output:?}");
    }
}

#[test]
fn the_command_slot_and_atomic_reader_have_the_declared_abi() {
    let bytes = hook::library();
    let file = object::File::parse(bytes.as_slice()).expect("parse companion");
    let control = file
        .dynamic_symbols()
        .find(|symbol| symbol.name() == Ok(hook::CONTROL_SYMBOL))
        .expect("control object");
    assert_eq!(control.kind(), object::SymbolKind::Data);
    assert_eq!(control.address(), hook::CONTROL_OFFSET as u64);
    assert_eq!(control.address() % 8, 0);
    assert_eq!(control.size(), hook::CONTROL_SIZE as u64);
    let data = file
        .section_by_name(".data")
        .expect("writable section")
        .data()
        .expect("data bytes");
    assert_eq!(&data[..8], hook::CONTROL_MAGIC);
    assert_eq!(&data[20..24], hook::CONTROL_CAPACITY.to_le_bytes());
    assert_eq!(&data[8..20], &[0; 12]);
    assert_eq!(&data[24..hook::CONTROL_HEADER], &[0; 8]);
    let segment = file
        .segments()
        .find(|segment| segment.address() == control.address())
        .expect("control mapping");
    assert_eq!(
        segment.flags(),
        object::SegmentFlags::Elf {
            p_flags: object::elf::PF_R | object::elf::PF_W
        }
    );
    let sequence = file
        .dynamic_symbols()
        .find(|symbol| symbol.name() == Ok(hook::SEQUENCE_SYMBOL))
        .expect("sequence reader");
    assert_eq!(sequence.kind(), object::SymbolKind::Text);
    let text = file.section_by_name(".text").expect("text");
    let offset = usize::try_from(sequence.address() - text.address()).expect("code offset");
    let code = &text.data().expect("code bytes")
        [offset..offset + usize::try_from(sequence.size()).expect("code length")];
    let mut decoder =
        iced_x86::Decoder::with_ip(64, code, sequence.address(), iced_x86::DecoderOptions::NONE);
    assert_eq!(decoder.decode().mnemonic(), iced_x86::Mnemonic::Endbr64);
    let load = decoder.decode();
    assert_eq!(load.mnemonic(), iced_x86::Mnemonic::Mov);
    assert_eq!(load.op0_register(), iced_x86::Register::RAX);
    assert_eq!(load.op1_kind(), iced_x86::OpKind::Memory);
    assert_eq!(load.memory_size(), iced_x86::MemorySize::UInt64);
    assert_eq!(load.ip_rel_memory_address(), control.address() + 8);
    assert_eq!(decoder.decode().mnemonic(), iced_x86::Mnemonic::Ret);
    assert!(!decoder.can_decode(), "exactly one native data load");
}

#[test]
fn monitoring_reconfiguration_and_cleanup() {
    let scratch = ScratchDir::new("python-agent");
    let library = scratch.path().join("hook.so");
    std::fs::write(&library, hook::library()).expect("write the hook");
    let agent = scratch.path().join("agent.py");
    std::fs::write(&agent, agent_source::bootstrap().expect("agent bootstrap"))
        .expect("write the agent");
    let standalone = scratch.path().join("standalone.py");
    std::fs::write(
        &standalone,
        agent_source::script(library.to_str().expect("hook path"), 100, &[], None, 0)
            .expect("whole-state script"),
    )
    .expect("write script");
    for interpreter in ["3.12", "3.13", "3.13t", "3.14", "3.14t", "3.15", "3.15t"] {
        let output = scratch.path().join(format!("{interpreter}.out"));
        let output = fixture_output(
            interpreter,
            "agent.py",
            &[
                agent.as_os_str(),
                library.as_os_str(),
                standalone.as_os_str(),
            ],
            &output,
        );
        assert!(output.contains("AGENT OK"), "{interpreter}: {output}");
    }
}

fn fixture_output(
    interpreter: &str,
    fixture: &str,
    arguments: &[&std::ffi::OsStr],
    output: &std::path::Path,
) -> String {
    let stdout = std::fs::File::create(output).expect("fixture output");
    let mut child = Reaped(
        Command::new(python::interpreter(interpreter))
            .args(["-I", "-B"])
            .arg(python::fixture(fixture))
            .args(arguments)
            .stdout(stdout.try_clone().expect("share output"))
            .stderr(stdout)
            .stdin(Stdio::null())
            .spawn()
            .expect("run the agent fixture"),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("wait for the fixture") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "{interpreter}: {fixture} timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(output).expect("read fixture output");
    assert!(
        status.success(),
        "{interpreter} {fixture} {arguments:?} {status:?}: {output}"
    );
    output
}

#[test]
fn monitoring_callbacks_survive_reconfiguration_and_code_reuse() {
    let scratch = ScratchDir::new("python-agent-races");
    let library = scratch.path().join("hook.so");
    std::fs::write(&library, hook::library()).expect("write control companion");
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime_model/python/agent.py");
    let fixtures = [
        ("agent_races.py", &["capture", "disable"][..]),
        ("agent_control.py", &["chain", "batch"][..]),
        (
            "agent_safety.py",
            &[
                "drain",
                "drain_together",
                "drain_race",
                "hook_interrupt",
                "arming_fault",
                "reentry",
                "keyboard",
                "exit",
                "custom",
                "cancelled",
                "timeout",
                "malformed",
                "interrupt_race",
                "reentry_race",
                "fork",
            ][..],
        ),
        (
            "agent_stepping.py",
            &[
                "over",
                "into",
                "out",
                "recursion",
                "generator",
                "coroutine",
                "exception",
                "outermost",
                "mismatch",
                "late",
                "threads",
                "reconfigured",
                "malformed",
            ][..],
        ),
        (
            "agent_exceptions.py",
            &["raised", "uncaught", "off", "internal", "malformed"][..],
        ),
        (
            "agent_discovery.py",
            &[
                "repeat",
                "fault",
                "resumed",
                "delegated",
                "identity",
                "gc",
                "reused",
                "coexist",
                "coexist_matching",
                "running",
                "together",
                "matching",
            ][..],
        ),
    ];
    // Every mode is a process of its own, and each build's modes run at
    // once, as many as the machine runs.
    let jobs: Vec<_> = ["3.12", "3.13", "3.14", "3.14t", "3.15", "3.15t"]
        .into_iter()
        .flat_map(|interpreter| {
            fixtures.iter().flat_map(move |(fixture, modes)| {
                modes.iter().map(move |mode| (interpreter, *fixture, *mode))
            })
        })
        .collect();
    run_modes(&jobs, &scratch, &library, &source);
}

/// Runs each fixture mode of `jobs`, by build, fixture, and mode, as a
/// process of its own, as many at once as the machine runs, and checks
/// each reports it passed.
fn run_modes(
    jobs: &[(&str, &str, &str)],
    scratch: &ScratchDir,
    library: &std::path::Path,
    source: &std::path::Path,
) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            scope.spawn(move || {
                while let Some(&(interpreter, fixture, mode)) =
                    jobs.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                {
                    let output = scratch
                        .path()
                        .join(format!("{interpreter}-{fixture}-{mode}.out"));
                    let output = fixture_output(
                        interpreter,
                        fixture,
                        &[
                            source.as_os_str(),
                            std::ffi::OsStr::new(mode),
                            library.as_os_str(),
                        ],
                        &output,
                    );
                    let prefix = match fixture {
                        "agent_races.py" => "RACE",
                        "agent_control.py" => "CONTROL AGENT",
                        "agent_safety.py" => "SAFETY AGENT",
                        "agent_stepping.py" => "STEPPING AGENT",
                        "agent_exceptions.py" => "EXCEPTIONS AGENT",
                        _ => "DISCOVERY",
                    };
                    assert!(
                        output
                            .lines()
                            .any(|line| line == format!("{prefix} OK {mode}")),
                        "{interpreter}: {output}"
                    );
                }
            });
        }
    });
}

/// Ready reports are stepped over internally, a real callback delivers its
/// three C arguments, and a malformed report becomes a visible runtime stop.
#[tokio::test]
async fn native_reports_cross_the_public_debugger_boundary() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, BreakpointSpec, Condition, ExitStatus, StopReason};

    let scratch = ScratchDir::new("python-agent-native");
    let library = scratch.path().join("hook.so");
    let script = scratch.path().join("agent.py");
    std::fs::write(&library, hook::library()).expect("write hook");
    std::fs::write(&script, agent_source::bootstrap().expect("bootstrap")).expect("write agent");
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "agent.py",
            &[
                script.to_str().expect("script path"),
                library.to_str().expect("hook path"),
                "--native",
            ],
            vec![(
                BreakpointSpec::Function(hook::SYMBOL.to_owned()),
                BreakpointOptions {
                    condition: Some(Condition::parse("$rdi == 1").expect("hook condition")),
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}"
        );
        let trace = session
            .scenario
            .operation("hook backtrace", session.scenario.handle().backtrace())
            .await;
        assert!(
            trace.frames.iter().any(|frame| frame
                .recorded
                .as_ref()
                .is_some_and(|recorded| &*recorded.function == "target")),
            "{interpreter}: {trace:#?}"
        );
        assert_eq!(
            trace.termination,
            microscope::UnwindTermination::Complete,
            "{interpreter}"
        );
        assert!(trace.unfollowed.is_empty(), "{interpreter}: {trace:#?}");
        let registers = session
            .scenario
            .operation("hook arguments", session.scenario.handle().registers())
            .await;
        for (name, expected) in [("rdi", 1_u64), ("rsi", 42), ("rdx", 0)] {
            let value = registers
                .registers
                .iter()
                .find(|value| &*value.register.name == name)
                .and_then(|value| value.bytes.as_ref())
                .expect("hook register");
            assert_eq!(
                value.as_ref(),
                expected.to_le_bytes(),
                "{interpreter}: {name}"
            );
        }
        let reason = session.scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::RuntimeEvent { ref description } if description.contains("invalid Python agent")),
            "{interpreter}: {reason:?}"
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn bootstrap_failure_reports_its_generation_through_the_owned_library() {
    use crate::session::PythonSession;
    use microscope::{ExitStatus, StopReason};
    let scratch = ScratchDir::new("python-agent-failed-bootstrap");
    let library = scratch.path().join("hook.so");
    let bootstrap = scratch.path().join("bootstrap.py");
    std::fs::write(&library, hook::library()).expect("write hook");
    std::fs::write(&bootstrap, agent_source::bootstrap().expect("bootstrap"))
        .expect("write script");
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "agent.py",
            &[
                bootstrap.to_str().expect("script"),
                library.to_str().expect("library"),
                "--failed-bootstrap",
            ],
            Vec::new(),
        )
        .await;
        assert!(
            matches!(reason, StopReason::RuntimeEvent { ref description } if description.contains("generation 1") && description.contains("initialization failed")),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert!(session.output().contains("FAILED BOOTSTRAP OK"));
        session.shutdown().await;
    }
}

// One test for each build, which the test runner runs at once: a process
// holds one debugger session.

#[tokio::test]
async fn user_reentry_and_interruptions_are_visible_at_the_native_boundary_on_3_14() {
    user_reentry_and_interruptions_are_visible_at_the_native_boundary("3.14").await;
}

#[tokio::test]
async fn user_reentry_and_interruptions_are_visible_at_the_native_boundary_on_3_14t() {
    user_reentry_and_interruptions_are_visible_at_the_native_boundary("3.14t").await;
}

#[tokio::test]
async fn user_reentry_and_interruptions_are_visible_at_the_native_boundary_on_3_15() {
    user_reentry_and_interruptions_are_visible_at_the_native_boundary("3.15").await;
}

#[tokio::test]
async fn user_reentry_and_interruptions_are_visible_at_the_native_boundary_on_3_15t() {
    user_reentry_and_interruptions_are_visible_at_the_native_boundary("3.15t").await;
}

async fn user_reentry_and_interruptions_are_visible_at_the_native_boundary(interpreter: &str) {
    use crate::session::PythonSession;
    use microscope::{ExitStatus, StopReason};
    let scratch = ScratchDir::new("python-agent-safety-native");
    let library = scratch.path().join("hook.so");
    std::fs::write(&library, hook::library()).expect("write companion");
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime_model/python/agent.py");
    {
        for (mode, description) in [
            ("reentry", "monitoring coverage is unconfirmed"),
            ("keyboard", "interruption is preserved"),
            ("exit", "interruption is preserved"),
            ("custom", "interruption is preserved"),
            ("cancelled", "interruption is preserved"),
            ("timeout", "interruption is preserved"),
        ] {
            let (mut session, reason, _) = PythonSession::launch_to_stop(
                interpreter,
                "agent_safety.py",
                &[
                    source.to_str().expect("source path"),
                    mode,
                    library.to_str().expect("library path"),
                    "--native",
                ],
                Vec::new(),
            )
            .await;
            assert!(
                matches!(reason, StopReason::RuntimeEvent { description: ref reported } if reported.contains(description)),
                "{interpreter} {mode}: {reason:?}: {}",
                session.output()
            );
            assert_eq!(
                session.scenario.resume_to_stop().await,
                StopReason::Exited(ExitStatus::Code(0)),
                "{interpreter} {mode}"
            );
            assert!(
                session
                    .output()
                    .lines()
                    .any(|line| line == format!("SAFETY AGENT OK {mode}")),
                "{interpreter} {mode}: {}",
                session.output()
            );
            session.shutdown().await;
        }
    }
}

#[tokio::test]
async fn a_python_line_breakpoint_installs_without_cooperation_and_retires_its_files() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, BreakpointSpec, ExitStatus, LineNumber, StopReason};
    let source = python::fixture("control.py");
    let text = std::fs::read_to_string(&source).expect("read fixture");
    let line = LineNumber::new(
        text.lines()
            .position(|line| line.contains("# TARGET_LINE"))
            .expect("marked line") as u64
            + 1,
    )
    .expect("nonzero line");
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "control.py",
            &[],
            vec![(
                BreakpointSpec::Source {
                    path: source.clone(),
                    line,
                },
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { address: None, ref hits } if hits.len() == 1),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        let trace = session
            .scenario
            .operation(
                "Python breakpoint backtrace",
                session.scenario.handle().backtrace(),
            )
            .await;
        assert!(
            trace
                .frames
                .iter()
                .any(
                    |frame| frame.recorded.as_ref().is_some_and(|recorded| recorded
                        .function
                        .as_ref()
                        == "target"
                        && recorded
                            .place
                            .as_ref()
                            .is_some_and(|place| u64::from(place.line) == line.get()))
                ),
            "{interpreter}: {trace:#?}"
        );
        let output = session.output();
        let library = output
            .lines()
            .find_map(|line| line.strip_prefix("AGENT LIBRARY "))
            .expect("the application reports the installed transport's private path");
        let directory = std::path::Path::new(library)
            .parent()
            .expect("private parent")
            .to_owned();
        let entries = std::fs::read_dir(&directory)
            .expect("agent directory")
            .map(|entry| entry.expect("owned file").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [std::ffi::OsString::from("hook.so")],
            "acknowledged source has been retired"
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{interpreter}"
        );
        assert!(
            session.output().contains("CONTROL RESULT 42"),
            "{interpreter}: {}",
            session.output()
        );
        session.shutdown().await;
        assert!(
            !directory.exists(),
            "{interpreter}: leaked {}",
            directory.display()
        );
    }
}

fn control_line(fixture: &str, marker: &str) -> microscope::BreakpointSpec {
    let path = python::fixture(fixture);
    let text = std::fs::read_to_string(&path).expect("read fixture");
    let line = text
        .lines()
        .position(|line| line.contains(marker))
        .expect("marked line") as u64
        + 1;
    microscope::BreakpointSpec::Source {
        path,
        line: microscope::LineNumber::new(line).expect("nonzero line"),
    }
}

#[tokio::test]
async fn automatic_configuration_is_pending_until_acknowledged_and_edits_supersede_old_sites() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, ExitStatus, StopReason};
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "control_reconfigure.py",
            &[],
            vec![(
                control_line("control_reconfigure.py", "# FIRST_LINE"),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}"
        );
        let snapshot = session.scenario.snapshot().await;
        let first = snapshot.breakpoints.first().expect("first site");
        assert!(
            first.pending_locations.is_empty(),
            "{interpreter}: {first:?}"
        );
        let first = first.id;
        let second = session
            .scenario
            .add_breakpoint_spec(control_line("control_reconfigure.py", "# SECOND_LINE"))
            .await;
        assert!(
            !second.pending_locations.is_empty(),
            "publication alone cannot establish readiness: {second:?}"
        );
        session.scenario.remove_breakpoint(first).await;
        for _ in 0..3 {
            let reason = session.scenario.resume_to_stop().await;
            assert!(
                matches!(reason, StopReason::Breakpoint { ref hits, .. } if hits.len() == 1 && hits[0].breakpoint == second.id),
                "{interpreter}: {reason:?}: {}",
                session.output()
            );
            let snapshot = session.scenario.snapshot().await;
            assert!(
                snapshot.breakpoints[0].pending_locations.is_empty(),
                "{interpreter}: {:?}",
                snapshot.breakpoints
            );
        }
        session.scenario.remove_breakpoint(second.id).await;
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{interpreter}: {}",
            session.output()
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn a_free_threaded_monitoring_agent_refuses_thread_only_resume_without_consuming_the_stop() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, ExceptionDisposition, ResumeScope, StopReason, ThreadId};
    for interpreter in ["3.14t", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "control.py",
            &[],
            vec![(
                control_line("control.py", "# TARGET_LINE"),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}"
        );
        let snapshot = session.scenario.snapshot().await;
        let stop = snapshot.stop_id.expect("public stop");
        let microscope::InferiorState::Stopped { process_id, .. } = snapshot.inferior else {
            panic!("not stopped")
        };
        let result = session
            .scenario
            .attempt(
                "thread-only resume refusal",
                session.scenario.handle().continue_execution(
                    stop,
                    ResumeScope::Thread(ThreadId::new(process_id.get())),
                    ExceptionDisposition::Pass,
                ),
            )
            .await;
        assert!(
            result.is_err(),
            "{interpreter}: unsafe thread-only resume was accepted: {result:?}"
        );
        assert!(result.unwrap_err().to_string().contains("process-wide"));
        assert_eq!(session.scenario.snapshot().await.stop_id, Some(stop));
        session.shutdown().await;
    }
}

/// A thread that only runs Python never calls the loop's entry: the agent
/// installs from where a pause finds it, among the loop's instructions,
/// which a tail-call build runs as a function each.
#[tokio::test]
async fn an_agent_installs_from_a_thread_paused_among_the_loops_instructions() {
    use crate::session::PythonSession;
    use microscope::{StopReason, ThreadId};
    for interpreter in ["3.14", "standalone-3.14", "3.15t"] {
        let mut session = PythonSession::launch(interpreter, "control_spin.py", &[]).await;
        let truth = session.stop_at("initial").await;
        let main = truth
            .iter()
            .find(|fields| fields[0] == "thread" && fields[3] == "printer")
            .and_then(|fields| fields[1].parse().ok())
            .map(ThreadId::new)
            .expect("the fixture names its main thread");
        session.go_on().await;
        session.wait_for_output("SPINNING").await;
        // A pause can find the thread in a function the loop calls, as
        // one that pops a frame: then let it run on a moment.
        let mut attempts = 0;
        loop {
            session.pause().await;
            let trace = session.backtrace(main).await;
            // The innermost native code, past the Python frames the loop
            // runs and the functions inlined into it.
            let looping = trace
                .frames
                .iter()
                .find_map(|frame| frame.symbol.as_ref())
                .is_some_and(|symbol| {
                    symbol.name.starts_with("_TAIL_CALL_")
                        || symbol.name.starts_with("_PyEval_EvalFrameDefault")
                });
            if looping {
                break;
            }
            attempts += 1;
            assert!(
                attempts < 100,
                "{interpreter}: never paused in the loop: {trace:#?}"
            );
            session.resume().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        session
            .scenario
            .add_breakpoint_spec(control_line("control_spin.py", "# TARGET_LINE"))
            .await;
        session.resume().await;
        let reason = session.finish_run().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn an_automatic_agent_detaches_disarms_and_can_be_installed_by_a_new_session() {
    use crate::session::PythonSession;
    use microscope::StopReason;
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, _) =
            PythonSession::attach_at(interpreter, "control_attach.py", "initial").await;
        session
            .scenario
            .add_breakpoint_spec(control_line("control_attach.py", "# TARGET_LINE"))
            .await;
        session.go_on().await;
        let reason = session.finish_run().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        let trace = session.scenario.snapshot().await;
        assert!(trace.breakpoints[0].pending_locations.is_empty());
        let mut session = session.reattach_at("detached").await;
        assert!(
            session
                .output()
                .contains("DETACHED TOOLS [None, None, None, None, None, None]"),
            "{interpreter}: {}",
            session.output()
        );
        session
            .scenario
            .add_breakpoint_spec(control_line("control_attach.py", "# TARGET_LINE"))
            .await;
        session.go_on().await;
        let reason = session.finish_run().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        session.detach_and_finish().await;
    }
}

#[tokio::test]
async fn exec_retires_old_agent_files_and_reinstalls_before_the_next_requested_line() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, ExitStatus, StopReason};
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "control_exec.py",
            &[],
            vec![(
                control_line("control_exec.py", "# TARGET_LINE"),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}"
        );
        let reason = session.scenario.resume_to_stop().await;
        assert_eq!(
            reason,
            StopReason::Exec { followed: true },
            "{interpreter}: {}",
            session.output()
        );
        let output = session.output();
        let old = output
            .lines()
            .find_map(|line| line.strip_prefix("AGENT LIBRARY "))
            .expect("first installation");
        let old = std::path::Path::new(old).parent().expect("agent parent");
        assert!(
            !old.exists(),
            "{interpreter}: exec retained {}",
            old.display()
        );
        let reason = session.scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{interpreter}: {}",
            session.output()
        );
        let output = session.output();
        let paths = output
            .lines()
            .filter_map(|line| line.strip_prefix("AGENT LIBRARY "))
            .collect::<Vec<_>>();
        assert_eq!(paths.len(), 2, "{interpreter}: {output}");
        assert_ne!(paths[0], paths[1]);
        assert!(paths.iter().all(|path| {
            !std::path::Path::new(path)
                .parent()
                .expect("agent parent")
                .exists()
        }));
        session.shutdown().await;
    }
}

#[tokio::test]
async fn exhausted_monitoring_tools_report_failure_and_preserve_application_tools() {
    use crate::session::PythonSession;
    use microscope::StopReason;
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, _) =
            PythonSession::attach_at(interpreter, "control_failure.py", "initial").await;
        session
            .scenario
            .add_breakpoint_spec(control_line("control_failure.py", "# TARGET_LINE"))
            .await;
        session.go_on().await;
        let reason = session.finish_run().await;
        assert!(
            matches!(reason, StopReason::RuntimeEvent { ref description } if description.contains("tool") && description.contains("generation")),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        let snapshot = session.scenario.snapshot().await;
        assert!(!snapshot.breakpoints[0].pending_locations.is_empty());
        assert!(
            snapshot.breakpoints[0].pending_locations[0]
                .reason
                .contains("tool"),
            "{interpreter}: {:?}",
            snapshot.breakpoints
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(microscope::ExitStatus::Code(0)),
            "{interpreter}: {}",
            session.output()
        );
        assert!(session.output().contains("APPLICATION TOOLS ['application-0', 'application-1', 'application-2', 'application-3', 'application-4', 'application-5']"), "{interpreter}: {}", session.output());
        session.shutdown().await;
    }
}

#[tokio::test]
async fn a_missing_agent_acknowledgement_has_a_bounded_visible_failure() {
    use crate::session::PythonSession;
    use microscope::StopReason;
    let (mut session, _) = PythonSession::attach_at("3.14", "control_noack.py", "initial").await;
    session
        .scenario
        .add_breakpoint_spec(control_line("control_noack.py", "# FIRST_LINE"))
        .await;
    session.go_on().await;
    let reason = session.finish_run().await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    session.resume().await;
    session.stop_at("faulted").await;
    session
        .scenario
        .add_breakpoint_spec(control_line("control_noack.py", "# SECOND_LINE"))
        .await;
    session.go_on().await;
    // The shared execution deadline allows scheduling under the stress
    // gate; unit tests separately prove the exact five-second running budget.
    let reason = session.finish_run().await;
    assert!(
        matches!(reason, StopReason::RuntimeEvent { ref description } if description.contains("acknowledgement") && description.contains("timed out")),
        "{reason:?}"
    );
    let snapshot = session.scenario.snapshot().await;
    assert!(
        snapshot
            .breakpoints
            .iter()
            .all(|site| !site.pending_locations.is_empty())
    );
    assert!(
        snapshot
            .breakpoints
            .iter()
            .all(|site| site.pending_locations[0].reason.contains("timed out"))
    );
    session.shutdown().await;
}

#[tokio::test]
async fn removing_all_sites_then_reactivating_uses_a_fresh_source_request() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, ExitStatus, StopReason};
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "control_reconfigure.py",
            &[],
            vec![(
                control_line("control_reconfigure.py", "# FIRST_LINE"),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}"
        );
        for marker in ["# FIRST_LINE", "# SECOND_LINE"] {
            session.scenario.remove_all_breakpoints().await;
            let site = session
                .scenario
                .add_breakpoint_spec(control_line("control_reconfigure.py", marker))
                .await;
            assert!(!site.pending_locations.is_empty());
            let reason = session.scenario.resume_to_stop().await;
            assert!(
                matches!(reason, StopReason::Breakpoint { ref hits, .. } if hits.len() == 1 && hits[0].breakpoint == site.id),
                "{interpreter}: {reason:?}: {}",
                session.output()
            );
        }
        session.scenario.remove_all_breakpoints().await;
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0)),
            "{interpreter}: {}",
            session.output()
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn unsupported_python_injection_is_visible_even_for_pending_breakpoints() {
    use crate::session::PythonSession;
    use microscope::BreakpointOptions;
    for interpreter in ["3.12", "3.13", "3.13t"] {
        let (session, _) =
            PythonSession::attach_at(interpreter, "control_attach.py", "initial").await;
        let site = session
            .scenario
            .operation(
                "unsupported pending breakpoint",
                session.scenario.handle().add_breakpoint_with(
                    control_line("control_attach.py", "# TARGET_LINE"),
                    BreakpointOptions {
                        pending: true,
                        ..BreakpointOptions::default()
                    },
                ),
            )
            .await;
        assert!(
            !site.pending_locations.is_empty(),
            "{interpreter}: unsupported injection lost its reason: {site:?}"
        );
        // 3.12 and 3.13 take the agent only through a launch's bootstrap,
        // and 3.13t not at all.
        let reason = if interpreter == "3.13t" {
            "free-threaded build stops listing code"
        } else {
            "need microscope to launch the interpreter"
        };
        assert!(
            site.pending_locations[0].reason.contains(reason),
            "{interpreter}: {site:?}"
        );
        session.shutdown().await;
    }
}

#[tokio::test]
async fn automatic_agents_use_target_process_ids_in_a_nested_pid_namespace() {
    use crate::session::PythonSession;
    use microscope::StopReason;
    for interpreter in ["3.14", "3.15t"] {
        let (mut session, _) = PythonSession::attach_in_pid_namespace_at(
            interpreter,
            "control_attach.py",
            "initial",
            &["--namespace"],
        )
        .await;
        session
            .scenario
            .add_breakpoint_spec(control_line("control_attach.py", "# TARGET_LINE"))
            .await;
        session.go_on().await;
        let reason = session.finish_run().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        let mut session = session.reattach_at("detached").await;
        assert!(
            session
                .output()
                .contains("DETACHED TOOLS [None, None, None, None, None, None]")
        );
        session
            .scenario
            .add_breakpoint_spec(control_line("control_attach.py", "# TARGET_LINE"))
            .await;
        session.go_on().await;
        let reason = session.finish_run().await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        session.detach_and_finish().await;
    }
}
