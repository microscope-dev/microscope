//! Choosing what to debug, running it, and sharing it between tabs.

use serde_json::{Value, json};

use crate::support::{ExternalProcess, Scenario, ScratchDir};
use crate::web::{Web, exists, wait_gone};

fn fixture(name: &str) -> String {
    Scenario::fixture(name).display().to_string()
}

fn pid(state: &Value) -> u64 {
    state["inferior"]["pid"].as_u64().expect("a pid")
}

#[tokio::test]
async fn a_loaded_program_waits_for_continue_then_runs_to_its_exit_with_output() {
    let program = fixture("output-streams");
    let web = Web::start("run", &[&program]);
    let mut tab = web.control("tab").await;
    let loaded = tab
        .state("the program loaded", |state| {
            state["target"]["kind"] == "launch"
        })
        .await;
    assert_eq!(loaded["inferior"]["state"], "notStarted");
    let session = loaded["session"].as_str().expect("a session").to_owned();

    tab.ok("continue", json!({})).await;

    // The program reads its input until it ends.
    tab.ok("input", json!({"text": "", "eof": true})).await;
    assert!(
        tab.output_until("stderr", "err 3")
            .await
            .contains("err 1\nerr 2\n")
    );
    let stdout = tab.output_until("stdout", "stdin: eof").await;
    assert!(
        stdout.contains("bad \u{fffd}\u{fffd} bytes, then \u{2713}"),
        "{stdout:.80}"
    );
    assert!(stdout.contains("burst done"));
    let exited = tab.inferior("exited").await;
    assert_eq!(exited["session"], session.as_str());
    assert!(
        exited["inferior"]["description"]
            .as_str()
            .is_some_and(|text| text.contains("exited")),
        "{exited}"
    );

    // A continue starts it again.
    tab.ok("continue", json!({})).await;
    tab.ok("input", json!({"text": "", "eof": true})).await;
    tab.output_until("stdout", "out 1").await;
    tab.inferior("exited").await;
    tab.save_traffic("run-to-exit");
}

#[tokio::test]
async fn two_tabs_share_one_session_and_only_one_continue_of_a_stop_wins() {
    let program = fixture("spin");
    let web = Web::start("race", &["--stop-at-entry", &program]);
    let mut first = web.control("first").await;
    let mut second = web.control("second").await;
    first.ok("continue", json!({})).await;
    let stopped = first.inferior("stopped").await;
    assert_eq!(stopped["inferior"]["reason"]["kind"], "entry");
    let seen = second.inferior("stopped").await;
    assert_eq!(
        seen["inferior"], stopped["inferior"],
        "both tabs see the same stop"
    );
    second
        .expect("a notice of who started it", |message| {
            message["type"] == "notice" && message["text"] == "started the program"
        })
        .await;

    let stop = stopped["inferior"]["stop"].clone();
    let one = first.send("continue", json!({"stop": stop})).await;
    let two = second.send("continue", json!({"stop": stop})).await;
    let one = first
        .expect("first's answer", |message| message["id"] == one)
        .await;
    let two = second
        .expect("second's answer", |message| message["id"] == two)
        .await;
    let answers = [&one, &two];
    assert_eq!(
        answers
            .iter()
            .filter(|answer| answer["type"] == "result")
            .count(),
        1,
        "{one} {two}"
    );
    let loser = answers
        .iter()
        .find(|answer| answer["type"] == "error")
        .expect("a loser");
    assert!(
        matches!(
            loser["error"]["kind"].as_str(),
            Some("staleStop" | "notStopped")
        ),
        "{loser}"
    );

    first.inferior("running").await;
    second.ok("pause", json!(null)).await;
    let paused = first.inferior("stopped").await;
    assert_eq!(paused["inferior"]["reason"]["kind"], "pause");
    assert!(paused["inferior"]["stop"].as_u64() > stop.as_u64());
    first.ok("kill", json!(null)).await;
    second
        .state("the program killed", |state| {
            !matches!(
                state["inferior"]["state"].as_str(),
                Some("running" | "stopped")
            )
        })
        .await;
    first.save_traffic("two-tabs");
}

#[tokio::test]
async fn a_tab_that_joins_late_sees_the_recent_output_once() {
    let program = fixture("output-streams");
    let web = Web::start("late", &["--run", &program]);
    let mut early = web.control("early").await;

    // Input reaches only a program that has been launched.
    early.inferior("running").await;
    early.ok("input", json!({"text": "", "eof": true})).await;
    early.output_until("stdout", "burst done").await;
    early.inferior("exited").await;
    let mut late = web.control("late").await;
    let output = late.output_until("stdout", "stdin: eof").await;

    // The burst pushed the first lines out of the history it keeps.
    assert!(!output.contains("out 1"), "{output:.80}");
    assert_eq!(output.matches("burst done").count(), 1);
    assert!(output.len() <= 256 * 1024 + 64, "{} bytes", output.len());
}

#[tokio::test]
async fn the_picker_replaces_one_session_with_another_only_when_asked() {
    let web = Web::start("picker", &[]);
    let mut tab = web.control("tab").await;
    let idle = tab
        .state("nothing debugged", |state| state["session"].is_null())
        .await;
    assert_eq!(idle["inferior"]["state"], "notStarted");

    let directory = Scenario::fixture("");
    let completions = tab
        .ok(
            "completePath",
            json!({"text": format!("{}/spi", directory.display())}),
        )
        .await;
    assert!(
        completions["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["text"]
                .as_str()
                .is_some_and(|text| text.ends_with("/spin"))
                && entry["kind"] == "executable"),
        "{completions}"
    );

    tab.ok("launch", json!({"program": fixture("spin"), "run": true}))
        .await;
    let first = tab.inferior("running").await;
    let first_pid = pid(&first);
    let first_session = first["session"].clone();

    let (kind, message) = tab
        .request("launch", json!({"program": fixture("basic")}))
        .await
        .expect_err("busy");
    assert_eq!(kind, "busy");
    assert!(message.contains("spin"), "{message}");
    assert!(
        exists(first_pid),
        "a refused launch leaves the session alone"
    );

    tab.ok(
        "launch",
        json!({"program": fixture("basic"), "replace": true}),
    )
    .await;

    // The answer follows the new session's state, so the page can open it.
    let second = tab.latest_state().expect("a state").clone();
    assert!(
        second["target"]["program"]
            .as_str()
            .is_some_and(|program| program.ends_with("/basic")),
        "answered before the new session's state: {second}"
    );
    assert_ne!(second["session"], first_session);
    wait_gone(first_pid);

    let (kind, message) = tab
        .request(
            "launch",
            json!({"program": "/nonexistent/program", "replace": true}),
        )
        .await
        .expect_err("a missing program");
    assert_eq!(kind, "failed");
    assert!(message.contains("/nonexistent/program"), "{message}");

    // An answer never arrives ahead of the state its request left behind.
    let state = tab.latest_state().expect("a state");
    assert!(state["session"].is_null(), "{state}");
    assert!(
        state["busy"].is_null(),
        "still busy after the failure: {state}"
    );
    tab.save_traffic("picker");
}

#[tokio::test]
async fn attaching_from_the_picker_and_ending_leaves_the_process_running() {
    let mut process = ExternalProcess::spawn(&Scenario::fixture("attach"));
    let pid = process.process_id().get();
    let web = Web::start("attach", &[]);
    let mut tab = web.control("tab").await;
    let listed = tab.ok("processes", json!(null)).await;
    assert!(
        listed["processes"]
            .as_array()
            .expect("processes")
            .iter()
            .any(|listed| listed["pid"] == pid
                && listed["command"]
                    .as_str()
                    .is_some_and(|command| command.ends_with("/attach"))),
        "{listed}"
    );
    tab.ok("attach", json!({"pid": pid})).await;
    let attached = tab.inferior("stopped").await;
    assert_eq!(attached["target"]["kind"], "attach");
    assert_eq!(attached["target"]["pid"], pid);
    assert_eq!(attached["inferior"]["reason"]["kind"], "attach");

    tab.ok("end", json!(null)).await;
    tab.state("nothing debugged", |state| state["session"].is_null())
        .await;

    // Detached, it runs on: released, it exits with its own status.
    process.release();
    assert_eq!(process.wait().code(), Some(23));
}

#[tokio::test]
async fn an_attached_process_resumes_at_once_with_resume_and_runs_on_after() {
    let mut process = ExternalProcess::spawn(&Scenario::fixture("attach"));
    let pid = process.process_id().get();
    let web = Web::start("resume", &["--attach", &pid.to_string(), "--resume"]);
    let mut tab = web.control("tab").await;
    let running = tab.inferior("running").await;
    assert_eq!(running["target"]["kind"], "attach");

    // Not in a tracing stop: blocked on its own read, as before the attach.
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("status");
    let state = status
        .lines()
        .find_map(|line| line.strip_prefix("State:"))
        .expect("a state")
        .trim();
    assert!(!state.starts_with('t'), "{state}");
    drop(tab);
    let mut web = web;
    assert!(web.interrupt().success());
    process.release();
    assert_eq!(process.wait().code(), Some(23));
}

#[tokio::test]
async fn interrupting_the_server_kills_the_program_it_launched() {
    let program = fixture("spin");
    let mut web = Web::start("shutdown", &["--run", &program]);
    let mut tab = web.control("tab").await;
    let running = tab.inferior("running").await;
    let pid = pid(&running);
    let status = web.interrupt();
    assert!(status.success(), "{status}");
    wait_gone(pid);
}

#[tokio::test]
async fn the_page_is_served_while_the_program_loads() {
    let scratch = ScratchDir::new("web-loading");
    let program = scratch.path().join("kvstore");

    // Loading reads the FIFO, which blocks until something writes it.
    nix::unistd::mkfifo(&program, nix::sys::stat::Mode::S_IRWXU).expect("create FIFO");
    let web = Web::start("loading", &[program.to_str().expect("a UTF-8 path")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loading", |state| {
        state["busy"]
            .as_str()
            .is_some_and(|busy| busy.starts_with("Loading"))
    })
    .await;

    let bytes = std::fs::read(Scenario::fixture("kvstore")).expect("read kvstore");
    tokio::task::spawn_blocking(move || std::fs::write(program, bytes))
        .await
        .expect("the writer")
        .expect("write the program");
    tab.state("the program loaded", |state| state["session"].is_string())
        .await;
}

/// A load that never ends, as of a FIFO no one writes or a hung network
/// file system, launches nothing, so the server ends without waiting for it.
#[tokio::test]
async fn terminating_the_server_while_the_program_loads_ends_it() {
    let scratch = ScratchDir::new("web-loading-stop");
    let program = scratch.path().join("kvstore");
    nix::unistd::mkfifo(&program, nix::sys::stat::Mode::S_IRWXU).expect("create FIFO");
    let mut web = Web::start("loading-stop", &[program.to_str().expect("a UTF-8 path")]);
    let mut tab = web.control("tab").await;
    tab.state("the program loading", |state| {
        state["busy"]
            .as_str()
            .is_some_and(|busy| busy.starts_with("Loading"))
    })
    .await;

    let status = web.terminate();
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn interrupting_the_server_while_it_starts_the_program_shuts_it_down() {
    let program = fixture("spin");

    // The link is printed before the program starts, so this lands mid-launch.
    let mut web = Web::start("early-shutdown", &["--run", &program]);
    let status = web.interrupt();
    assert!(status.success(), "{status}");
}

#[tokio::test]
async fn restart_runs_the_program_again() {
    let program = fixture("spin");
    let web = Web::start("restart", &["--run", &program]);
    let mut tab = web.control("tab").await;
    let first = pid(&tab.inferior("running").await);
    tab.ok("restart", json!(null)).await;
    let again = tab
        .state("a new process", |state| {
            state["inferior"]["state"] == "running" && state["inferior"]["pid"] != first
        })
        .await;
    wait_gone(first);
    let (kind, _) = tab
        .request("continue", json!({}))
        .await
        .expect_err("already running");
    assert_eq!(kind, "notStopped");
    assert!(exists(pid(&again)));
}

/// Copies each module the foreign core recorded, as its machine had it, to
/// where it lies under `root`.
fn foreign_sysroot(core: &std::path::Path, root: &std::path::Path) {
    let debugger = microscope::Debugger::open_core(&microscope::CoreDumpOptions {
        allow_module_mismatch: true,
        ..microscope::CoreDumpOptions::new(core.to_owned())
    })
    .expect("open the foreign core");
    let info = debugger
        .handle()
        .core_dump()
        .expect("a core session")
        .clone();
    for module in info
        .modules
        .iter()
        .filter(|module| module.recorded_path.as_os_str() != crate::support::VDSO)
    {
        let recorded = &*module.recorded_path;
        let source = if recorded
            .parent()
            .is_some_and(|directory| directory.ends_with("core-foreign"))
        {
            match recorded.file_name().and_then(|name| name.to_str()) {
                Some("libc.so.6") => Scenario::fixture("libc-foreign.so.6"),
                Some(name) => Scenario::fixture(name),
                None => panic!("unnamed module {}", recorded.display()),
            }
        } else {
            recorded.to_owned()
        };

        let target = root.join(
            recorded
                .strip_prefix("/")
                .expect("an absolute recorded path"),
        );
        std::fs::create_dir_all(target.parent().expect("a parent")).expect("create directories");
        std::fs::copy(&source, &target).expect("copy a module");
    }

    drop(debugger);
}

#[tokio::test]
async fn a_core_from_another_machine_opens_with_its_files_from_a_sysroot() {
    let core = Scenario::fixture("core-foreign/crash.core");
    let core_path = core.to_str().expect("a UTF-8 path").to_owned();

    // This machine has another build at the executable's recorded path.
    let web = Web::start("core-without-sysroot", &["--core", &core_path]);
    let mut tab = web.control("tab").await;
    tab.state("the core refused", |state| {
        state["session"].is_null() && state["busy"].is_null()
    })
    .await;
    drop(tab);
    drop(web);

    let root = ScratchDir::new("web-core-sysroot");
    foreign_sysroot(&core, root.path());
    let sysroot = root.path().to_str().expect("a UTF-8 path").to_owned();
    let web = Web::start(
        "core-sysroot",
        &["--core", &core_path, "--sysroot", &sysroot],
    );
    let mut tab = web.control("tab").await;
    let opened = tab
        .state("the core opened", |state| state["session"].is_string())
        .await;
    assert_eq!(opened["target"]["kind"], "core");
}
