use super::*;

fn exit_code(child: support::ExternalProcess) -> Option<i32> {
    wait_for_zombie(child.process_id());
    child.wait().code()
}

#[tokio::test]
async fn attach_discovers_the_executable_and_detaches_without_harming_the_process() {
    let fixture = Scenario::fixture("attach");
    let mut child = support::ExternalProcess::spawn(&fixture);

    let process = child.process_id();
    let debugger = child.attach().await;
    let handle = debugger.handle();
    assert_eq!(handle.executable(), fixture);
    let snapshot = handle.snapshot().await.expect("attached snapshot");
    assert!(matches!(
        snapshot.inferior,
        InferiorState::Stopped {
            process_id,
            reason: StopReason::Attach,
            ..
        } if process_id == process
    ));

    let modules = handle.loaded_modules().await.expect("attached modules");
    assert_eq!(
        support::vdso_module(&modules).module.load_bias.get(),
        support::vdso_mapping(process).start
    );

    handle
        .add_breakpoint(microscope::BreakpointSpec::Function(
            "attach_breakpoint".to_owned(),
        ))
        .await
        .expect("set attached breakpoint");
    child.release();
    assert!(matches!(
        timeout(support::DEADLINE, handle.resume())
            .await
            .expect("attached resume timed out")
            .expect("resume attached process"),
        StopReason::Breakpoint { .. }
    ));

    debugger.shutdown().await.expect("detach debugger");
    assert_eq!(exit_code(child), Some(23));
}

#[tokio::test]
async fn a_process_in_another_process_group_is_attached_and_stops() {
    use std::os::unix::process::CommandExt as _;
    let fixture = Scenario::fixture("attach");
    let mut child = support::ExternalProcess::spawn_command(
        std::process::Command::new(&fixture).process_group(0),
    );
    let process = child.process_id();
    assert_ne!(
        nix::unistd::getpgid(Some(Pid::from_raw(
            i32::try_from(process.get()).expect("PID fits i32")
        )))
        .expect("the fixture's group"),
        nix::unistd::getpgrp(),
        "the fixture leads a group of its own"
    );

    let debugger = child.attach().await;
    let handle = debugger.handle();
    handle
        .add_breakpoint(microscope::BreakpointSpec::Function(
            "attach_breakpoint".to_owned(),
        ))
        .await
        .expect("set attached breakpoint");
    child.release();
    assert!(matches!(
        timeout(support::DEADLINE, handle.resume())
            .await
            .expect("attached resume timed out")
            .expect("resume attached process"),
        StopReason::Breakpoint { .. }
    ));

    debugger.shutdown().await.expect("detach debugger");
    assert_eq!(exit_code(child), Some(23));
}

#[tokio::test]
async fn attached_group_stops_are_classified_and_resumable() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach"));
    let process = child.process_id();
    let pid = Pid::from_raw(i32::try_from(process.get()).expect("PID fits i32"));
    let mut scenario = Scenario::attached("attached group stop", child.attach().await);

    let stopping = scenario.start_resuming().await;
    kill(pid, Signal::SIGSTOP).expect("stop attached process");
    assert!(matches!(
        timeout(support::DEADLINE, stopping)
            .await
            .expect("signal stop timed out")
            .expect("join resume")
            .expect("resume attached process"),
        StopReason::Exception(exception) if exception.code == 19
    ));
    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Exception(exception) if exception.code == 19
    ));

    scenario.shutdown().await;
    kill(pid, Signal::SIGCONT).expect("continue detached process");
    child.release();
    assert_eq!(exit_code(child), Some(23));
}

#[tokio::test]
async fn attach_stops_and_detaches_every_existing_native_thread() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach-threads"));
    let debugger = child.attach().await;
    let snapshot = debugger
        .handle()
        .snapshot()
        .await
        .expect("attached snapshot");
    assert_eq!(snapshot.threads.len(), 3);
    assert!(
        snapshot
            .threads
            .iter()
            .all(|thread| { matches!(thread.state, ThreadState::Stopped { .. }) })
    );

    child.release();
    debugger.shutdown().await.expect("detach every thread");
    assert_eq!(exit_code(child), Some(0));
}

#[tokio::test]
async fn attach_traces_a_process_whose_main_thread_exited() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach-exited-leader"));

    wait_for_zombie(child.process_id());
    let mut scenario = Scenario::attached("attach without a leader", child.attach().await);
    let snapshot = scenario.snapshot().await;
    assert!(matches!(
        snapshot.inferior,
        InferiorState::Stopped {
            reason: StopReason::Attach,
            ..
        }
    ));
    assert_eq!(snapshot.threads.len(), 1, "{:?}", snapshot.threads);

    let (_, value) = super::globals::tls_location_by_name(&scenario, "worker_value").await;
    assert_eq!(value, 31);

    scenario.shutdown().await;
    child.release();
    assert_eq!(child.wait().code(), Some(31));
}

#[tokio::test]
async fn detaching_waits_for_no_main_thread_that_exited_after_attaching() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach-leader-exits"));
    let mut scenario = Scenario::attached("detach after the leader exits", child.attach().await);
    let _resumed = scenario.start_resuming().await;
    child.release();
    wait_for_zombie(child.process_id());

    scenario.shutdown().await;
    child.release();
    assert_eq!(child.wait().code(), Some(7));
}

const PF_EXITING: u64 = 0x4;

#[tokio::test]
async fn attaching_while_threads_are_created_traces_every_thread() {
    let child = support::ExternalProcess::spawn_running(&Scenario::fixture("attach-clones"));
    let tasks = format!("/proc/{}/task", child.process_id().get());
    support::wait_until("the fixture runs every worker", || {
        fs::read_dir(&tasks).is_ok_and(|entries| entries.count() > 128)
    });

    for _ in 0..20 {
        let scenario = Scenario::attached("attach while cloning", child.attach().await);

        let untraced = fs::read_dir(&tasks)
            .expect("list threads")
            .filter_map(|entry| {
                let path = entry.expect("thread entry").path();

                let stat = fs::read_to_string(path.join("stat")).ok()?;
                let status = fs::read_to_string(path.join("status")).ok()?;

                let fields = stat
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .collect::<Vec<_>>();
                let flags = fields[6].parse::<u64>().expect("stat flags");
                if matches!(fields[0], "Z" | "X") || flags & PF_EXITING != 0 {
                    return None;
                }

                let tracer = status
                    .lines()
                    .find_map(|line| line.strip_prefix("TracerPid:"))
                    .expect("status names the tracer")
                    .trim();
                let ours = tracer != "0"
                    && fs::exists(format!("/proc/self/task/{tracer}")).is_ok_and(|exists| exists);
                (!ours).then_some(path)
            })
            .collect::<Vec<_>>();
        assert!(untraced.is_empty(), "untraced threads: {untraced:?}");
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_detaches_a_running_attached_process_and_cancels_waiters() {
    let mut child = support::ExternalProcess::spawn(&Scenario::fixture("attach"));
    let pid = child.process_id();
    let mut scenario = Scenario::attached("detach running", child.attach().await);
    let attached = scenario.snapshot().await.inferior;
    assert!(
        matches!(
            attached,
            InferiorState::Stopped {
                reason: StopReason::Attach,
                ..
            }
        ),
        "{attached:?}"
    );

    let resuming = scenario.start_resuming().await;
    scenario.shutdown().await;
    assert!(matches!(
        timeout(support::DEADLINE, resuming)
            .await
            .expect("resume waiter outlived the session")
            .expect("join resume waiter"),
        Err(Error::RequestCancelled)
    ));

    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read target status");
    assert!(status.contains("\nTracerPid:\t0\n"), "{status}");

    child.release();
    assert_eq!(exit_code(child), Some(23));
}

#[tokio::test]
async fn attach_reads_an_unlinked_executable_through_proc() {
    let directory = support::ScratchDir::new("attach-unlinked");
    let executable = directory.path().join("deleted-fixture");
    fs::copy(Scenario::fixture("attach"), &executable).expect("copy attach fixture");
    let mut child = support::ExternalProcess::spawn(&executable);
    fs::remove_file(&executable).expect("unlink running fixture");

    let debugger = child.attach().await;
    assert!(
        debugger
            .handle()
            .executable()
            .to_string_lossy()
            .ends_with(" (deleted)")
    );
    child.release();
    debugger.shutdown().await.expect("detach deleted fixture");
    assert_eq!(exit_code(child), Some(23));
}

fn c_library_of(process: ProcessId) -> std::path::PathBuf {
    let maps = fs::read_to_string(format!("/proc/{}/maps", process.get())).expect("fixture maps");
    maps.lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .find(|path| path.ends_with("/libc.so.6"))
        .map(std::path::PathBuf::from)
        .expect("the fixture maps a C library")
}

#[tokio::test]
async fn attach_finds_the_libraries_of_a_process_in_another_mount_namespace() {
    let fixture = Scenario::fixture("attach");
    let mut plain = support::ExternalProcess::spawn(&fixture);
    let library = c_library_of(plain.process_id());
    plain.release();
    assert_eq!(exit_code(plain), Some(23));

    let directory = support::ScratchDir::new("attach-mount-namespace");
    let copy = directory.path().join("libc.so.6");
    fs::copy(&library, &copy).expect("copy the C library");
    let mut child = support::ExternalProcess::spawn_command(
        std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c"])
            .arg(r#"mount --bind "$1" "$2" && exec "$3""#)
            .arg("sh")
            .arg(&copy)
            .arg(&library)
            .arg(&fixture),
    );
    let process = child.process_id();
    assert_eq!(c_library_of(process), library);

    let debugger = child.attach().await;
    let handle = debugger.handle();
    let rooted = std::path::Path::new(&format!("/proc/{}/root", process.get()))
        .join(library.strip_prefix("/").expect("an absolute path"));
    let modules = handle.loaded_modules().await.expect("attached modules");
    assert!(
        modules.modules.iter().any(|module| *module.path == rooted),
        "no module at {}: {:?}",
        rooted.display(),
        modules
            .modules
            .iter()
            .map(|module| module.path.display().to_string())
            .collect::<Vec<_>>()
    );

    child.release();
    debugger
        .shutdown()
        .await
        .expect("detach the namespaced fixture");
    assert_eq!(exit_code(child), Some(23));
}

#[tokio::test]
async fn invalid_or_missing_attach_targets_fail_without_starting_a_session() {
    assert!(matches!(
        Debugger::attach(ProcessId::new(0)).await,
        Err(Error::InvalidProcessId(0))
    ));
    assert!(matches!(
        Debugger::attach(ProcessId::new(i32::MAX as u64)).await,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
    ));

    let debugger = Debugger::new(Scenario::fixture("basic"))
        .expect("failed attach did not retain the Linux session lease");
    debugger
        .shutdown()
        .await
        .expect("shutdown replacement debugger");
}

#[tokio::test]
async fn an_attached_threads_stack_moved_into_unused_stack_does_not_end_whole() {
    attached_stacks_moved_into_unused_stack_do_not_end_whole("attach-threads").await;
}

#[tokio::test]
async fn an_attached_musl_threads_stack_moved_into_unused_stack_does_not_end_whole() {
    attached_stacks_moved_into_unused_stack_do_not_end_whole("attach-threads-musl").await;
}

async fn attached_stacks_moved_into_unused_stack_do_not_end_whole(fixture: &str) {
    use microscope::{EvaluationMode, Expression, InspectionLimits, StackFrameId, StopContext};

    let mut child = support::ExternalProcess::spawn(&Scenario::fixture(fixture));
    let mut scenario = Scenario::attached("attach threads", child.attach().await);
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
        panic!("not stopped: {:?}", snapshot.inferior);
    };
    assert_eq!(snapshot.threads.len(), 3);

    for thread in snapshot.threads.iter() {
        let view = scenario.handle().at(StopContext {
            stop: stop_id,
            execution: thread.id.into(),
            frame: StackFrameId::INNERMOST,
        });
        let before = scenario.operation("backtrace", view.backtrace()).await;
        assert_eq!(
            before.termination,
            UnwindTermination::Complete,
            "{before:#?}"
        );

        let assign = |text: String| async move {
            let expression = Expression::parse(&text).expect("an expression");
            let assigned = view
                .evaluate_with(
                    &expression,
                    EvaluationMode::Assign,
                    InspectionLimits::default(),
                )
                .await;
            assert!(
                matches!(assigned, Ok(microscope::Evaluation::Value { .. })),
                "{text}: {assigned:?}"
            );
        };
        let registers = scenario.operation("registers", view.registers()).await;
        let stack = register_u64(&registers, RegisterRole::StackPointer);
        let frame = register_u64(&registers, RegisterRole::FramePointer);
        let moved = stack - 0x8000;
        assign(format!("$rsp = {moved:#x}")).await;
        assign(format!("$rbp = {moved:#x}")).await;

        let after = scenario.operation("backtrace", view.backtrace()).await;
        assert!(
            matches!(after.termination, UnwindTermination::InvalidCaller { .. }),
            "thread {}: {after:#?}",
            thread.id
        );

        assign(format!("$rsp = {stack:#x}")).await;
        assign(format!("$rbp = {frame:#x}")).await;
    }

    child.release();
    scenario.shutdown().await;
    assert_eq!(exit_code(child), Some(0));
}
