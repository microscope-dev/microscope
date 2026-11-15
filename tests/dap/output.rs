use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture};

#[test]
fn program_output_arrives_whole_in_order_and_on_its_own_stream() {
    let mut dap = Dap::start("output");
    let started = dap.launch(
        Profile::VsCode,
        &fixture("output-streams"),
        json!({}),
        &Configuration::default(),
    );
    assert_eq!(
        dap.event(started.mark, "exited", |_| true),
        json!({"exitCode": 0})
    );
    let stdout = dap.output_text(started.mark, "stdout");
    let stderr = dap.output_text(started.mark, "stderr");
    let burst = "x".repeat(1024 * 1024);
    assert_eq!(
        stdout,
        format!(
            "out 1\nout 2\nout 3\nbad \u{fffd}\u{fffd} bytes, then ✓\n{burst}\nburst done\nstdin: eof\n"
        )
    );
    assert_eq!(stderr, "err 1\nerr 2\nerr 3\n");

    for event in dap.events(started.mark, "output") {
        let length = event["output"].as_str().expect("output").len();
        assert!(
            length <= 32 * 1024,
            "an event of {length} bytes in {} {}",
            event["category"],
            &event["output"].as_str().expect("output")[..40]
        );
    }

    dap.finish();
}
