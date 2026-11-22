use std::sync::Arc;

use serde_json::json;

use crate::capture::builder::{CaptureBuilder, LineInput};
use crate::capture::records::{FrameKind, NONE};
use crate::model::{Pid, Tid};
use crate::sanitize::{Builtin, HashKey, Sanitizer, sanitize_capture};

use super::{AnswerOptions, CaptureView};

fn sample_view() -> CaptureView {
    let mut builder = CaptureBuilder::new();
    builder.setting("program", "/usr/bin/demo");
    builder.setting("start", "1000");
    builder.setting("end", "3000");
    builder.setting(
        "arguments 0",
        r#"["/usr/bin/demo","--token","ghp_1234567890abcdef1234567890abcdef1234"]"#,
    );
    builder.setting(
        "command 0",
        "/usr/bin/demo --token ghp_1234567890abcdef1234567890abcdef1234",
    );
    builder.setting("weight", "nanoseconds");

    let module = builder.module("/usr/bin/demo", Some("aabbccdd"), [1; 16], 4096);
    let main_frame = builder.frame(FrameKind::Native, module, 0x1000, 0);
    let work_frame = builder.frame(FrameKind::Native, module, 0x1100, 0);
    builder.symbolize(
        main_frame,
        NONE,
        &[LineInput {
            function: "main",
            file: Some("/src/demo.rs"),
            line: 10,
            column: 1,
            declared: Some(("/src/demo.rs", 1)),
            role: 0,
        }],
    );
    builder.symbolize(
        work_frame,
        NONE,
        &[LineInput {
            function: "do_work",
            file: Some("/src/demo.rs"),
            line: 42,
            column: 1,
            declared: Some(("/src/demo.rs", 35)),
            role: 0,
        }],
    );
    let stack = builder.stack(&[work_frame, main_frame]);
    let thread = builder.thread(Pid::new(100), Tid::new(100), 1000);
    builder.sample(thread, 1500, 20_000, stack, NONE);
    builder.sample(thread, 2500, 30_000, stack, NONE);

    CaptureView::new(
        Arc::new(builder.finish()),
        "capture:1".to_owned(),
        "demo.usprof".to_owned(),
        None,
    )
}

#[test]
fn capture_view_answers_core_profile_queries() {
    let view = sample_view();
    let opts = AnswerOptions::hosted();

    let (summary, bin) = view.answer("profile", json!({}), opts).unwrap();
    assert!(bin.is_empty());
    assert_eq!(summary["name"], "demo.usprof");
    assert_eq!(summary["samples"], 2);
    assert_eq!(summary["weight"], 50_000);

    let (inspect, _) = view.answer("profileInspect", json!({}), opts).unwrap();
    assert_eq!(inspect["frames"], 2);
    assert_eq!(inspect["distinctStacks"], 1);

    let (tree, bytes) = view
        .answer("profileTree", json!({ "profile": "capture:1" }), opts)
        .unwrap();
    assert_eq!(tree["nodes"], 2);
    assert_eq!(tree["weight"], 50_000);
    assert!(!bytes.is_empty());

    let (funcs, _) = view
        .answer("profileFunctions", json!({ "profile": "capture:1" }), opts)
        .unwrap();
    assert_eq!(funcs["weight"], 50_000);
    assert_eq!(funcs["rows"].as_array().unwrap().len(), 2);

    let (timeline, _) = view
        .answer(
            "profileTimeline",
            json!({ "profile": "capture:1", "buckets": 16 }),
            opts,
        )
        .unwrap();
    assert_eq!(timeline["lanes"].as_array().unwrap().len(), 1);

    let (lines, _) = view
        .answer(
            "profileLines",
            json!({ "profile": "capture:1", "function": "do_work" }),
            opts,
        )
        .unwrap();
    assert_eq!(lines["files"].as_array().unwrap().len(), 1);
    assert!(lines["files"][0]["unread"].as_str().is_some());

    let (code, _) = view
        .answer(
            "profileCode",
            json!({ "profile": "capture:1", "function": "do_work" }),
            opts,
        )
        .unwrap();
    assert!(code["unavailable"].as_str().is_some());

    let (sc, _) = view
        .answer(
            "syscallSummary",
            json!({ "profile": "capture:1", "group": "syscall" }),
            opts,
        )
        .unwrap();
    assert_eq!(sc["calls"], 0);
}

#[test]
fn sanitize_capture_scrubs_secrets_and_counts_firings() {
    let view = sample_view();
    let sanitizer = Sanitizer::new(&Builtin::ALL, Vec::new(), HashKey::new(b"test-key"));
    let cleaned = sanitize_capture((*view.capture).clone(), &sanitizer);
    assert!(cleaned.total_fired() > 0);
    let args = cleaned.capture.setting("arguments 0").unwrap();
    assert!(!args.contains("ghp_1234567890abcdef1234567890abcdef1234"));
    assert!(args.contains("[redacted]"));
}
