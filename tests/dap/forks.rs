//! Following the processes a program forks into child sessions.

use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture};

/// The breakpoint both processes of the fork fixture reach.
fn shared_work() -> Configuration {
    Configuration {
        functions: vec!["shared_work".to_owned()],
        ..Configuration::default()
    }
}

#[test]
fn a_forked_child_is_debugged_in_a_child_session() {
    let mut dap = Dap::start("followed fork");
    dap.follow_children(shared_work());
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"followForks": true}),
        &shared_work(),
    );
    let parent = dap.stopped(started.mark);
    assert_eq!(parent.reason, "function breakpoint");

    let (mut child, mark) = dap.child();
    let note = dap.output_containing(started.mark, "console", "session of its own");
    let process = child.event(mark, "process", |_| true);
    assert_eq!(process["startMethod"], "attach");
    assert!(
        note.contains(&format!("forked process {}", process["systemProcessId"])),
        "{note}"
    );

    // The child ran nothing before its session took it: it stops at the
    // breakpoint after its fork, which it no longer inherits as a trap.
    let stop = child.stopped(mark);
    assert_eq!(stop.reason, "function breakpoint");
    let frames = child.inspect_as(Profile::VsCode, &stop);
    assert_eq!(frames[0]["name"], "shared_work");
    let resumed = child.send("continue", json!({"threadId": stop.thread}));
    child.success(resumed);
    assert_eq!(child.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    child.event(resumed.mark, "terminated", |_| true);
    child.finish();

    // The parent sees its child exit normally.
    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}

/// A child no session takes runs on, and the user is told why.
#[test]
fn a_child_no_session_takes_runs_on_and_the_user_is_told() {
    for (profile, refused) in [(Profile::Helix, false), (Profile::VsCode, true)] {
        let mut dap = Dap::start(format!("unfollowed fork, {profile:?}"));
        if refused {
            dap.refuse_children("no sessions today");
        }

        let started = dap.launch(
            profile,
            &fixture("fork"),
            json!({"followForks": true}),
            &shared_work(),
        );
        let explanation = if refused {
            "the client did not debug it: no sessions today"
        } else {
            "this client cannot start child sessions"
        };

        dap.output_containing(started.mark, "important", explanation);
        let parent = dap.stopped(started.mark);
        let resumed = dap.send("continue", json!({"threadId": parent.thread}));
        dap.success(resumed);
        assert_eq!(
            dap.event(resumed.mark, "exited", |_| true)["exitCode"],
            0,
            "{profile:?}: the child exits normally"
        );
        dap.finish();
    }
}

/// A child whose client answered for a session that never attaches, as
/// nvim-dap answers once it starts one, runs on once the wait for it ends.
#[test]
fn a_child_whose_session_never_attaches_runs_on() {
    let mut dap = Dap::start_in("abandoned fork", &[("MICROSCOPE_ADOPTION_TIMEOUT", "200")]);
    dap.abandon_children();
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"followForks": true}),
        &shared_work(),
    );
    dap.output_containing(started.mark, "important", "no session attached to it");
    let parent = dap.stopped(started.mark);
    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}

/// A child session that cannot attach releases its child at once, which
/// then runs on, and says so.
#[test]
fn a_child_session_that_cannot_attach_releases_its_child() {
    let mut dap = Dap::start("unattachable fork");
    dap.fail_children();
    let started = dap.launch(
        Profile::VsCode,
        &fixture("fork"),
        json!({"followForks": true}),
        &shared_work(),
    );
    let failure = dap.child_failure();
    assert!(failure.contains("; it runs on its own"), "{failure}");
    let parent = dap.stopped(started.mark);
    let resumed = dap.send("continue", json!({"threadId": parent.thread}));
    dap.success(resumed);
    assert_eq!(dap.event(resumed.mark, "exited", |_| true)["exitCode"], 0);
    dap.finish();
}
