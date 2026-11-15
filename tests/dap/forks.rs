use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture};

fn shared_work() -> Configuration {
    Configuration {
        functions: vec!["shared_work".to_owned()],
        ..Configuration::default()
    }
}

fn child_work() -> Configuration {
    Configuration {
        functions: vec!["child_work".to_owned()],
        ..Configuration::default()
    }
}

#[test]
fn a_forked_child_is_shown_in_a_session_the_same_adapter_serves() {
    let mut dap = Dap::start("followed fork");
    dap.follow_children(shared_work());
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"follow": {"mode": "all", "stop": "process"}}),
        &shared_work(),
    );
    let parent = dap.stopped(started.mark);
    assert_eq!(parent.reason, "function breakpoint");

    let (mut child, mark) = dap.child();
    let process = child.event(mark, "process", |_| true);
    assert_eq!(process["startMethod"], "attach");

    let stop = child.stopped(mark);
    assert_eq!(stop.reason, "function breakpoint");
    let frames = child.inspect_as(Profile::VsCode, &stop);
    assert_eq!(frames[0]["name"], "shared_work");
    let resumed = child.send("continue", json!({"threadId": stop.thread}));
    child.success(resumed);
    assert_eq!(child.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    child.finish();

    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}

#[test]
fn a_child_session_run_as_a_program_relays_to_the_groups_socket() {
    let mut dap = Dap::start("relayed fork");
    dap.follow_children_relayed(shared_work());
    let started = dap.launch(
        Profile::Neovim,
        &fixture("fork"),
        json!({"follow": {"mode": "all", "stop": "process"}}),
        &shared_work(),
    );
    let parent = dap.stopped(started.mark);

    let (mut child, mark) = dap.child();
    let stop = child.stopped(mark);
    let frames = child.inspect_as(Profile::Neovim, &stop);
    assert_eq!(frames[0]["name"], "shared_work");
    let resumed = child.send("continue", json!({"threadId": stop.thread}));
    child.success(resumed);
    assert_eq!(child.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    child.finish();

    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}

#[test]
fn a_childs_session_shows_the_groups_breakpoint_as_its_own() {
    let mut dap = Dap::start("followed breakpoints");
    dap.follow_children(shared_work());
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"follow": {"mode": "all", "stop": "process"}}),
        &shared_work(),
    );
    let parent = dap.stopped(started.mark);

    let (mut child, mark) = dap.child();
    let stop = child.stopped(mark);
    assert_eq!(stop.reason, "function breakpoint");
    let set = child.request(
        "setFunctionBreakpoints",
        json!({"breakpoints": [{"name": "shared_work"}]}),
    );
    let id = &set["breakpoints"][0]["id"];
    assert_eq!(stop.body["hitBreakpointIds"], json!([id]));

    let frames = child.inspect_as(Profile::VsCode, &stop);
    let listed = child.request(
        "evaluate",
        json!({"expression": "info breakpoints", "frameId": frames[0]["id"], "context": "repl"}),
    )["result"]
        .as_str()
        .expect("result")
        .to_owned();
    assert_eq!(listed.matches("shared_work").count(), 1, "{listed}");

    let resumed = child.send("continue", json!({"threadId": stop.thread}));
    child.success(resumed);
    assert_eq!(child.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    child.finish();
    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}

#[test]
fn a_members_stop_pauses_the_program_and_its_continue_resumes_both() {
    let mut dap = Dap::start("group stop");
    dap.follow_children(child_work());
    let started = dap.launch(
        Profile::VsCode,
        &fixture("process-tree"),
        json!({"args": ["1", "direct"], "follow": "all"}),
        &child_work(),
    );

    let (mut child, mark) = dap.child();
    let stop = child.stopped(mark);
    assert_eq!(stop.reason, "function breakpoint");
    let frames = child.inspect_as(Profile::VsCode, &stop);
    assert_eq!(frames[0]["name"], "child_work");
    let parent = dap.stopped(started.mark);
    assert_eq!(parent.reason, "pause");

    let resumed = child.send("continue", json!({"threadId": stop.thread}));
    child.success(resumed);
    assert_eq!(child.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    child.finish();

    assert_eq!(dap.event(started.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}

#[test]
fn a_member_no_session_shows_is_named_and_the_program_still_runs() {
    for (profile, refused) in [(Profile::Helix, false), (Profile::VsCode, true)] {
        let mut dap = Dap::start(format!("unshown member, {profile:?}"));
        if refused {
            dap.refuse_children("no sessions today");
        }

        let started = dap.launch(
            profile,
            &fixture("fork"),
            json!({"follow": {"mode": "all", "stop": "process"}}),
            &shared_work(),
        );
        let explanation = if refused {
            "the client did not start it: no sessions today"
        } else {
            "this client cannot start sessions"
        };

        let told = dap.output_containing(started.mark, "important", explanation);
        assert!(told.contains("no session shows it"), "{told}");
        assert!(told.contains("\"follow\" to \"none\""), "{told}");
        let parent = dap.stopped(started.mark);
        assert_eq!(parent.reason, "function breakpoint");
        dap.finish();
    }
}

#[test]
fn a_members_session_in_another_adapter_explains_how_to_route_it() {
    let mut dap = Dap::start("unrouted member");
    dap.fail_children();
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"follow": {"mode": "all", "stop": "process"}}),
        &shared_work(),
    );
    let failure = dap.child_failure();
    assert!(
        failure.contains("must connect it to that socket"),
        "{failure}"
    );
    dap.output_containing(started.mark, "important", "no session shows it");
    let parent = dap.stopped(started.mark);
    assert_eq!(parent.reason, "function breakpoint");
    dap.finish();
}

#[test]
fn follow_none_asks_for_no_sessions() {
    let mut dap = Dap::start("unfollowed");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"follow": "none"}),
        &shared_work(),
    );
    dap.output_containing(started.mark, "console", "which runs without microscope");
    let parent = dap.stopped(started.mark);
    assert_eq!(parent.reason, "function breakpoint");
    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}
