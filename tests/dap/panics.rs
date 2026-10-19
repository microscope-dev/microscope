//! A Rust panic, as an editor debugs it: the default filters stop on a
//! panic in a tokio task, which tokio would catch, with the program's
//! frame that panicked shown as the exception's.

use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture, line_of, source};

#[test]
fn the_default_filters_stop_on_a_task_panic_at_its_caller() {
    let mut dap = Dap::start("rust panics");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("tokio-panics-o0"),
        json!({"args": ["unwrap"]}),
        &Configuration::default(),
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "exception", "{stop:?}");
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert_eq!(
        info["description"], "panicked: called `Option::unwrap()` on a `None` value",
        "{info}"
    );
    assert_eq!(info["breakMode"], "always", "{info}");

    // Clients focus the first frame whose source is not deemphasized.
    let frames = dap.inspect_as(Profile::VsCode, &stop);
    let panicked = frames
        .iter()
        .position(|frame| frame["source"]["presentationHint"] != "deemphasize")
        .unwrap_or_else(|| panic!("{frames:#?}"));

    let main = source("rust/tokio/panics/src/main.rs");
    assert_eq!(
        frames[panicked]["line"],
        line_of(&main, "// PANIC: unwrap"),
        "{frames:#?}"
    );

    dap.finish();
}
