use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture};

#[test]
fn the_system_call_filter_stops_at_the_calls_it_names_until_turned_off() {
    let mut dap = Dap::start("system calls");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("catch-gcc-o0"),
        json!({}),
        &Configuration {
            exceptions: Some(json!({
                "filters": [],
                "filterOptions": [{"filterId": "syscalls", "condition": "write"}],
            })),
            ..Configuration::default()
        },
    );
    let stop = dap.stopped(started.mark);
    assert_eq!(stop.reason, "exception", "{stop:?}");
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert_eq!(
        (&info["exceptionId"], &info["description"]),
        (
            &json!("system call"),
            &json!(r#"write(2, "to stderr\n", 10)"#)
        ),
        "{info}"
    );

    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    let stop = dap.stopped(resumed.mark);
    let info = dap.request("exceptionInfo", json!({"threadId": stop.thread}));
    assert_eq!(
        info["description"],
        json!(r#"write(2, "to stderr\n", 10) = 10"#),
        "{info}"
    );

    dap.request("setExceptionBreakpoints", json!({"filters": []}));
    let resumed = dap.send("continue", json!({"threadId": stop.thread}));
    dap.success(resumed);
    assert_eq!(
        dap.event(resumed.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    dap.finish();
}
