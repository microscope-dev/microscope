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

#[test]
fn hosted_timeline_bounds_lanes_and_cells() {
    let mut builder = CaptureBuilder::new();
    builder.setting("start", "0");
    builder.setting("end", "10000");
    builder.setting("weight", "nanoseconds");
    let frame = builder.frame(FrameKind::Native, NONE, 0x1000, 0);
    let stack = builder.stack(&[frame]);
    for i in 1..=300_u32 {
        let t = builder.thread(Pid::new(1), Tid::new(i), 0);
        builder.sample(t, i64::from(i), 100, stack, NONE);
    }
    let view = CaptureView::new(
        Arc::new(builder.finish()),
        "capture:many".into(),
        "many.usprof".into(),
        None,
    );

    let (clamped, _) = view
        .answer(
            "profileTimeline",
            json!({ "profile": "capture:many", "buckets": 4096 }),
            AnswerOptions::hosted(),
        )
        .unwrap();
    let lanes = clamped["lanes"].as_array().unwrap();
    assert_eq!(lanes.len(), 300);
    let bucket_len = lanes[0]["weights"].as_array().unwrap().len();
    assert!(
        bucket_len * lanes.len() <= crate::view::HOSTED_MAX_TIMELINE_CELLS,
        "expected cells bounded by hosted budget, got {} * {}",
        lanes.len(),
        bucket_len
    );

    let strict = AnswerOptions {
        max_timeline_lanes: Some(100),
        max_timeline_cells: Some(1000),
        ..AnswerOptions::hosted()
    };
    let err = view
        .answer(
            "profileTimeline",
            json!({ "profile": "capture:many", "buckets": 4096 }),
            strict,
        )
        .unwrap_err();
    assert!(matches!(err, crate::view::ViewError::Invalid(_)), "{err:?}");

    let (narrow, _) = view
        .answer(
            "profileTimeline",
            json!({ "profile": "capture:many", "buckets": 128, "threads": [0] }),
            strict,
        )
        .unwrap();
    assert_eq!(narrow["lanes"].as_array().unwrap().len(), 1);
}

proptest::proptest! {
    #[test]
    fn hosted_endpoints_survive_adversarial_numeric_params(
        start in proptest::prelude::prop_oneof![
            proptest::prelude::Just(i64::MIN),
            proptest::prelude::Just(i64::MAX),
            proptest::prelude::Just(0_i64),
            proptest::prelude::any::<i64>(),
        ],
        end in proptest::prelude::prop_oneof![
            proptest::prelude::Just(i64::MIN),
            proptest::prelude::Just(i64::MAX),
            proptest::prelude::Just(0_i64),
            proptest::prelude::any::<i64>(),
        ],
        width in proptest::prelude::prop_oneof![
            proptest::prelude::Just(0_u32),
            proptest::prelude::Just(1_u32),
            proptest::prelude::Just(u32::MAX),
            0_u32..50_000,
        ],
        buckets in proptest::prelude::prop_oneof![
            proptest::prelude::Just(0_u32),
            proptest::prelude::Just(1_u32),
            proptest::prelude::Just(u32::MAX),
            0_u32..100_000,
        ],
    ) {
        let view = sample_view();
        let opts = AnswerOptions::hosted();
        let _ = view.answer(
            "profileTimeline",
            json!({ "profile": "capture:1", "during": { "from": start, "to": end }, "buckets": buckets }),
            opts,
        );
        let _ = view.answer(
            "profileZones",
            json!({ "profile": "capture:1", "from": start, "to": end, "width": width }),
            opts,
        );
        let _ = view.answer(
            "profileZoneStats",
            json!({ "profile": "capture:1", "from": start, "to": end }),
            opts,
        );
        let _ = view.answer(
            "syscallHeatmap",
            json!({ "profile": "capture:1", "during": { "from": start, "to": end }, "columns": buckets }),
            opts,
        );
        let _ = view.answer(
            "syscallLog",
            json!({ "profile": "capture:1", "from": start, "to": end, "limit": width }),
            opts,
        );
    }
}

#[test]
fn hosted_timeline_focus_transform_excludes_filtered_lanes() {
    let mut focus_builder = CaptureBuilder::new();
    focus_builder.setting("start", "0");
    focus_builder.setting("end", "1000");
    focus_builder.setting("weight", "nanoseconds");
    let f1 = focus_builder.frame(FrameKind::Native, NONE, 0x1000, 0);
    focus_builder.symbolize(
        f1,
        NONE,
        &[LineInput {
            function: "alpha",
            file: None,
            line: 0,
            column: 0,
            declared: None,
            role: 0,
        }],
    );
    let f2 = focus_builder.frame(FrameKind::Native, NONE, 0x2000, 0);
    focus_builder.symbolize(
        f2,
        NONE,
        &[LineInput {
            function: "beta",
            file: None,
            line: 0,
            column: 0,
            declared: None,
            role: 0,
        }],
    );
    let s1 = focus_builder.stack(&[f1]);
    let s2 = focus_builder.stack(&[f2]);
    let t1 = focus_builder.thread(Pid::new(1), Tid::new(1), 0);
    let t2 = focus_builder.thread(Pid::new(1), Tid::new(2), 0);
    focus_builder.sample(t1, 100, 10, s1, NONE);
    focus_builder.sample(t2, 200, 10, s2, NONE);
    let focus_view = CaptureView::new(
        Arc::new(focus_builder.finish()),
        "capture:focus".into(),
        "focus.usprof".into(),
        None,
    );
    let one_lane = AnswerOptions {
        max_timeline_lanes: Some(1),
        max_timeline_cells: Some(64),
        ..AnswerOptions::hosted()
    };
    let (focused, _) = focus_view
        .answer(
            "profileTimeline",
            json!({
                "profile": "capture:focus",
                "buckets": 32,
                "transforms": [{ "op": "focus", "name": "alpha" }]
            }),
            one_lane,
        )
        .unwrap();
    assert_eq!(focused["lanes"].as_array().unwrap().len(), 1);
}
