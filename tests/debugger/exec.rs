//! Following a process through exec(2): into its own program again, and
//! from a launcher that executes the program.

use std::io::Write as _;
use std::path::Path;

use super::libraries::{breakpoint, pending_function};
use super::support::ExternalProcess;
use super::*;

/// Asks the session to stop at every exec, as `catch exec` does.
async fn catch_exec(scenario: &Scenario) {
    scenario
        .operation("catch exec", scenario.handle().set_exec_stops(true))
        .await;
}

fn function_name(trace: &microscope::Backtrace, index: usize) -> Option<&str> {
    trace.frames[index]
        .function
        .as_ref()
        .map(|function| &*function.name)
}

/// Stops at the program's `reexecuted` breakpoint and checks its argument
/// count.
async fn assert_reexecuted(scenario: &Scenario, reason: &StopReason, argc: i128) {
    let StopReason::Breakpoint { hits, .. } = reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits.len(), 1);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_eq!(function_name(&trace, 0), Some("reexecuted"));
    assert_eq!(function_name(&trace, 1), Some("main"));
    let count = scenario
        .operation("argc", scenario.handle().variable("argc"))
        .await;
    assert_variable_value(&count, ScalarValue::Signed(argc));
}

/// Checks that the one vDSO module is the one the executed image mapped,
/// since exec(2) maps a new vDSO, anywhere under randomization.
async fn assert_vdso_follows_the_image(scenario: &mut Scenario) {
    let InferiorState::Stopped { process_id, .. } = scenario.snapshot().await.inferior else {
        panic!("the program is stopped");
    };

    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert_eq!(
        support::vdso_module(&modules).module.load_bias.get(),
        support::vdso_mapping(process_id).start
    );
}

#[tokio::test]
async fn a_program_that_executes_itself_again_is_followed_with_its_breakpoints() {
    let mut scenario = Scenario::launch("reexec");
    catch_exec(&scenario).await;
    scenario.add_breakpoint("reexecuted").await;
    let library = pending_function(&scenario, "puts").await;

    assert_eq!(
        scenario.run_to_stop().await,
        StopReason::Exec { followed: true }
    );

    // The first image's libraries went with it, and the new image's dynamic
    // loader has loaded none yet.
    assert!(
        breakpoint(&mut scenario, library.id)
            .await
            .locations
            .is_empty()
    );
    scenario
        .operation("backtrace at exec", scenario.handle().backtrace())
        .await;

    let reason = scenario.resume_to_stop().await;
    assert_reexecuted(&scenario, &reason, 2).await;
    assert_vdso_follows_the_image(&mut scenario).await;
    let reason = scenario.resume_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };

    assert_eq!(hits[0].breakpoint, library.id);
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(2))
    );

    scenario.shutdown().await;
}

/// Breakpoints waiting on indirect functions' resolvers, which a program
/// may never call, wait again in the image exec(2) maps: the first image's
/// resolver sites went with it.
#[tokio::test]
async fn indirect_functions_wait_again_after_exec() {
    let mut scenario = Scenario::launch("reexec");
    catch_exec(&scenario).await;
    for name in ["strlen", "strcasestr", "wcsnlen", "rawmemchr", "memrchr"] {
        pending_function(&scenario, name).await;
    }

    // The loader has functions of these names of its own.
    let mut reason = scenario.run_to_stop().await;
    while let StopReason::Breakpoint { .. } = reason {
        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exec { followed: true });

    // Libraries load again, which places the waiting breakpoints anew.
    loop {
        match scenario.resume_to_stop().await {
            StopReason::Exited(status) => {
                assert_eq!(status, ExitStatus::Code(2));
                break;
            }
            StopReason::Breakpoint { .. } => {}
            other => panic!("stopped for {other:?}"),
        }
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn a_launcher_that_executes_the_program_launches_it() {
    for stop_at_entry in [false, true] {
        let mut launcher =
            ExternalProcess::exec_gate(&Scenario::fixture("reexec"), &["one", "two"]);
        let mut scenario = Scenario::new(
            format!("launch by exec, stop at entry {stop_at_entry}"),
            Scenario::fixture("reexec"),
        );
        scenario.add_breakpoint("reexecuted").await;
        let mut release = launcher.take_stdin();
        let mut reason = scenario
            .launch_by_exec_to_stop(launcher.process_id(), stop_at_entry, move || {
                release.write_all(b"\n").expect("release the launcher");
            })
            .await;
        if stop_at_entry {
            assert_eq!(reason, StopReason::Entry);

            // The launcher runs randomized, as does the program it executes.
            assert_vdso_follows_the_image(&mut scenario).await;
            reason = scenario.resume_to_stop().await;
        }

        assert_reexecuted(&scenario, &reason, 3).await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(3))
        );
        launcher.reaped();
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_launcher_runs_as_it_would_untraced_until_it_executes_the_program() {
    let mut scenario = Scenario::launch("reexec");

    // Job control stopping it does not hold the launch back.
    let mut launcher = ExternalProcess::exec_gate(&Scenario::fixture("reexec"), &["one"]);
    let process = launcher.process_id();
    let mut release = launcher.take_stdin();
    let reason = scenario
        .launch_by_exec_to_stop(process, false, move || {
            kill(pid(process), Signal::SIGSTOP).expect("stop the launcher");
            release.write_all(b"\n").expect("release the launcher");
        })
        .await;
    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(2)));
    launcher.reaped();

    // A signal it does not handle is delivered and ends it, and the launch
    // with it.
    let launcher = ExternalProcess::exec_gate(&Scenario::fixture("reexec"), &["one"]);
    let process = launcher.process_id();
    let failed = scenario
        .attempt(
            "launch by exec",
            scenario.handle().launch_by_exec(process, false, move || {
                kill(pid(process), Signal::SIGTERM).expect("terminate the launcher");
            }),
        )
        .await;
    let error = failed.expect_err("the launcher never executed the program");
    assert!(error.to_string().contains("SIGTERM"), "{error}");
    launcher.reaped();
    scenario.shutdown().await;
}

/// Launches through a launcher of `program` that is released at once, and
/// returns why the launch failed. The debugger reaps the launcher, its
/// child, before the next one starts: its waiter runs until this process has
/// no children.
async fn failed_launch(program: &Path) -> Error {
    let scenario = Scenario::launch("reexec");
    let mut launcher = ExternalProcess::exec_gate(program, &[]);
    let mut release = launcher.take_stdin();
    let failed = scenario
        .attempt(
            "launch by exec",
            scenario
                .handle()
                .launch_by_exec(launcher.process_id(), false, move || {
                    release.write_all(b"\n").expect("release the launcher");
                }),
        )
        .await;
    scenario.shutdown().await;
    launcher.reaped();

    failed.expect_err("the launch fails")
}

#[tokio::test]
async fn a_launch_through_a_process_that_cannot_execute_the_program_fails() {
    // It executes another program, which is killed.
    let error = failed_launch(&Scenario::fixture("basic")).await;
    assert!(
        error.to_string().contains("executed a program other than"),
        "{error}"
    );

    // Its exec fails, so it exits first.
    let error = failed_launch(Path::new("/nonexistent/program")).await;
    assert!(error.to_string().contains("Code(127)"), "{error}");

    // A thread it started could execute the program untraced.
    let scenario = Scenario::launch("reexec");
    let threaded = ExternalProcess::spawn(&Scenario::fixture("attach-threads"));
    let failed = scenario
        .attempt(
            "launch by exec of a threaded process",
            scenario
                .handle()
                .launch_by_exec(threaded.process_id(), false, || {}),
        )
        .await;
    assert!(
        matches!(failed, Err(Error::ProcessHasThreads(_))),
        "{failed:?}"
    );
    scenario.shutdown().await;
}

fn pid(process: ProcessId) -> Pid {
    Pid::from_raw(i32::try_from(process.get()).expect("process id"))
}

/// The module path of each loaded module whose file is one of the
/// fixtures, which leaves out the C library, the loader, and the vDSO.
async fn fixture_modules(scenario: &Scenario) -> Vec<(microscope::ModuleId, String)> {
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    modules
        .modules
        .iter()
        .filter_map(|record| {
            let name = record.path.file_name()?.to_str()?;
            name.starts_with("exec-")
                .then(|| (record.module.id, name.to_owned()))
        })
        .collect()
}

/// A pending breakpoint at the line of exec-target.c that prints.
async fn pending_target_line(scenario: &Scenario) -> microscope::Breakpoint {
    scenario
        .operation(
            "add pending line breakpoint",
            scenario.handle().add_breakpoint_with(
                BreakpointSpec::Source {
                    path: "exec-target.c".into(),
                    line: microscope::LineNumber::new(support::source_line(
                        "tests/fixtures/c/exec-target.c",
                        "puts(argv[argc - 1]);",
                    ))
                    .expect("one-based line"),
                },
                microscope::BreakpointOptions {
                    pending: true,
                    ..microscope::BreakpointOptions::default()
                },
            ),
        )
        .await
}

/// Checks that the events report the old program's module unloaded before
/// the new program's loaded.
fn assert_program_replaced(
    events: &mut tokio::sync::broadcast::Receiver<microscope::DebuggerEvent>,
    old: microscope::ModuleId,
    new: microscope::ModuleId,
) {
    let mut unloaded = false;
    let mut loaded = false;
    while let Ok(event) = events.try_recv() {
        match event {
            microscope::DebuggerEvent::ModuleUnloaded { module, .. } if module.module.id == old => {
                unloaded = true;
            }
            microscope::DebuggerEvent::ModuleLoaded { module, .. } if module.module.id == new => {
                assert!(unloaded, "the old program unloads before the new one loads");
                loaded = true;
            }
            _ => {}
        }
    }
    assert!(unloaded && loaded);
}

/// A program that executes another is followed into it: breakpoints by
/// name, by line, and in a library are placed in the new program before
/// its first instruction, one only the old program has stays pending, and
/// one set by address is never planted in the new program.
async fn follows_an_exec_into_another_program(arguments: &[&str]) {
    let mut scenario = Scenario::launch("exec-other");
    catch_exec(&scenario).await;
    let old = scenario.add_breakpoint("before_exec").await;
    let by_name = pending_function(&scenario, "target_work").await;
    let by_line = pending_target_line(&scenario).await;
    assert!(by_name.locations.is_empty() && by_line.locations.is_empty());

    let mut events = scenario.handle().subscribe();
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: arguments.iter().map(Into::into).collect(),
            ..LaunchOptions::default()
        })
        .await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, old.id);
    let by_address = scenario
        .add_breakpoint_spec(BreakpointSpec::Address(support::breakpoint_address(
            &reason,
        )))
        .await;
    let before = fixture_modules(&scenario).await;
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!(before[0].1, "exec-other");

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exec { followed: true }
    );

    // The new program is the main module under a new id; the old one is
    // gone.
    let after = fixture_modules(&scenario).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].1, "exec-target");
    assert_ne!(after[0].0, before[0].0);
    let modules = scenario
        .operation("modules", scenario.handle().loaded_modules())
        .await;
    assert_eq!(modules.main, after[0].0);

    assert_program_replaced(&mut events, before[0].0, after[0].0);

    // Breakpoints only the old program had wait for a program that has
    // them; one by address is never planted in another program.
    for id in [old.id, by_address.id] {
        assert!(
            breakpoint(&mut scenario, id).await.locations.is_empty(),
            "breakpoint {id:?} kept a location in the old program"
        );
    }
    assert_eq!(
        breakpoint(&mut scenario, by_name.id).await.locations.len(),
        1
    );
    assert_eq!(
        breakpoint(&mut scenario, by_line.id).await.locations.len(),
        1
    );

    let reason = scenario.resume_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, by_name.id);
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    assert_eq!(function_name(&trace, 0), Some("target_work"));
    assert_eq!(function_name(&trace, 1), Some("main"));
    let argc = scenario
        .operation("argc", scenario.handle().variable("argc"))
        .await;
    assert_variable_value(&argc, ScalarValue::Signed(2));

    let reason = scenario.resume_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, by_line.id);

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(42))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn an_exec_into_another_program_is_followed_into_it() {
    follows_an_exec_into_another_program(&[]).await;
}

#[tokio::test]
async fn an_exec_from_a_second_thread_into_another_program_is_followed_into_it() {
    follows_an_exec_into_another_program(&["thread"]).await;
}

/// Without `catch exec`, a run through an exec into another program goes on
/// to the breakpoint in it, and the handle describes the program the
/// process runs while still naming the one launched.
#[tokio::test]
async fn a_followed_exec_runs_on_unless_caught() {
    let mut scenario = Scenario::launch("exec-other");
    let target = pending_function(&scenario, "target_work").await;
    let mut events = scenario.handle().subscribe();

    let reason = scenario.run_to_stop().await;
    let StopReason::Breakpoint { hits, .. } = &reason else {
        panic!("stopped for {reason:?}");
    };
    assert_eq!(hits[0].breakpoint, target.id);

    let mut changed = None;
    while let Ok(event) = events.try_recv() {
        if let microscope::DebuggerEvent::ProgramChanged {
            previous, program, ..
        } = event
        {
            assert!(changed.is_none(), "one exec changed the program once");
            changed = Some((previous, program));
        }
    }
    let (previous, program) = changed.expect("the program changed");
    assert_eq!(previous.path.file_name().unwrap(), "exec-other");
    assert_eq!(program.path.file_name().unwrap(), "exec-target");

    let handle = scenario.handle();
    assert_eq!(handle.module_image().path(), program.path.as_path());
    assert_eq!(handle.executable(), *program.path);
    assert_eq!(handle.launched().file_name().unwrap(), "exec-other");

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(42))
    );
    scenario.shutdown().await;
}
