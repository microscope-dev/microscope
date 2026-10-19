//! Attaching to running processes and detaching without harm.

use super::*;

/// Waits for a released attach fixture to exit, and returns its exit code. A
/// process left stopped by the debugger would never exit.
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

    // The vDSO, which no file backs, is found where the kernel mapped it.
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
        timeout(Duration::from_secs(5), handle.resume())
            .await
            .expect("attached resume timed out")
            .expect("resume attached process"),
        StopReason::Breakpoint { .. }
    ));

    debugger.shutdown().await.expect("detach debugger");
    assert_eq!(exit_code(child), Some(23));
}

/// A process in a process group of its own, as a daemon or a shell's job
/// is, reports its stops as any other: the debugger waits for every child,
/// not only those in its own group.
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

    let debugger = timeout(Duration::from_secs(5), child.attach())
        .await
        .expect("the attach timed out");
    let handle = debugger.handle();
    handle
        .add_breakpoint(microscope::BreakpointSpec::Function(
            "attach_breakpoint".to_owned(),
        ))
        .await
        .expect("set attached breakpoint");
    child.release();
    assert!(matches!(
        timeout(Duration::from_secs(5), handle.resume())
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

    // SIGSTOP is reported once on delivery and again as the seized
    // thread's group-stop; neither may become an unresumable stop.
    let stopping = scenario.start_resuming().await;
    kill(pid, Signal::SIGSTOP).expect("stop attached process");
    assert!(matches!(
        timeout(Duration::from_secs(5), stopping)
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

    // The zombie leader stays listed but can never be seized.
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

    // The TLS lookup reads the address space through the worker.
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

    // Linux reports the main thread's exit only after the worker's, so
    // detaching cannot wait for it.
    scenario.shutdown().await;
    child.release();
    assert_eq!(child.wait().code(), Some(7));
}

/// The kernel's flag for a thread that has begun to exit.
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

        // With every thread stopped none is inside clone, so the list is
        // complete. An untraced thread would run through breakpoints and
        // die of their traps.
        let untraced = fs::read_dir(&tasks)
            .expect("list threads")
            .filter_map(|entry| {
                let path = entry.expect("thread entry").path();

                // A thread may exit while the list is read.
                let stat = fs::read_to_string(path.join("stat")).ok()?;
                let status = fs::read_to_string(path.join("status")).ok()?;

                // An exiting thread runs no more of the program, and one that
                // has exited cannot be seized.
                let fields = stat
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .collect::<Vec<_>>();
                let flags = fields[6].parse::<u64>().expect("stat flags");
                if matches!(fields[0], "Z" | "X") || flags & PF_EXITING != 0 {
                    return None;
                }

                // The tracer is the debugger's controller thread.
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

    // A resume waiting for a stop that shutdown preempts must not hang.
    let resuming = scenario.start_resuming().await;
    scenario.shutdown().await;
    assert!(matches!(
        timeout(Duration::from_secs(5), resuming)
            .await
            .expect("resume waiter outlived the session")
            .expect("join resume waiter"),
        Err(Error::RequestCancelled)
    ));

    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("read target status");
    assert!(status.contains("\nTracerPid:\t0\n"), "{status}");

    // A stop left behind by the detach would keep it from finishing.
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

/// The C library a running fixture maps, by the path it maps it from.
fn c_library_of(process: ProcessId) -> std::path::PathBuf {
    let maps = fs::read_to_string(format!("/proc/{}/maps", process.get())).expect("fixture maps");
    maps.lines()
        .filter_map(|line| line.split_whitespace().nth(5))
        .find(|path| path.ends_with("/libc.so.6"))
        .map(std::path::PathBuf::from)
        .expect("the fixture maps a C library")
}

/// A process in a mount namespace of its own, as a container's is, names its
/// files by paths that name other files, or none, outside it. Its C library
/// is still found, under its root as `/proc` shows it, so backtraces and
/// thread-local storage go through it.
#[tokio::test]
async fn attach_finds_the_libraries_of_a_process_in_another_mount_namespace() {
    let fixture = Scenario::fixture("attach");
    let mut plain = support::ExternalProcess::spawn(&fixture);
    let library = c_library_of(plain.process_id());
    plain.release();
    assert_eq!(exit_code(plain), Some(23));

    // Inside the namespace, the library's path names a copy: same bytes,
    // another inode.
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
