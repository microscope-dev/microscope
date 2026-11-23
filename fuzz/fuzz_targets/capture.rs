#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use serde_json::json;

use microscope::capture_view::{AnswerOptions, CaptureView};
use microscope::object::{ObjectReader, decode_tables_budgeted};

const VIEW_METHODS: &[(&str, &str)] = &[
    ("profile", "{}"),
    ("profileInspect", "{}"),
    ("profileTree", r#"{"profile":"capture:fuzz"}"#),
    ("profileFunctions", r#"{"profile":"capture:fuzz"}"#),
    (
        "profileTimeline",
        r#"{"profile":"capture:fuzz","buckets":32}"#,
    ),
    ("profileZones", r#"{"profile":"capture:fuzz","width":64}"#),
    ("profileZoneStats", r#"{"profile":"capture:fuzz"}"#),
    (
        "profileLines",
        r#"{"profile":"capture:fuzz","function":"main"}"#,
    ),
    (
        "profileCode",
        r#"{"profile":"capture:fuzz","function":"main"}"#,
    ),
    (
        "syscallSummary",
        r#"{"profile":"capture:fuzz","group":"syscall"}"#,
    ),
    ("syscallHeatmap", r#"{"profile":"capture:fuzz"}"#),
    (
        "syscallDetail",
        r#"{"profile":"capture:fuzz","group":"syscall","key":"read"}"#,
    ),
    ("syscallLog", r#"{"profile":"capture:fuzz","limit":16}"#),
    ("syscallCall", r#"{"profile":"capture:fuzz","index":0}"#),
    ("syscallFiles", r#"{"profile":"capture:fuzz"}"#),
    ("syscallErrors", r#"{"profile":"capture:fuzz"}"#),
    ("syscallProcesses", r#"{"profile":"capture:fuzz"}"#),
    (
        "syscallStacks",
        r#"{"profile":"capture:fuzz","group":"syscall","key":"read"}"#,
    ),
];

fuzz_target!(|data: &[u8]| {
    microscope::fuzz_capture(data);

    if let Ok(reader) = ObjectReader::open_unverified(data) {
        for idx in 0..reader.parts().len().min(2) {
            if let Ok(sections) = reader.part_sections_unverified(idx)
                && let Ok(chunk) = decode_tables_budgeted(&sections, 8 << 20)
            {
                let view = CaptureView::new(
                    Arc::new(chunk.capture),
                    "capture:fuzz".to_owned(),
                    "fuzz.usprof".to_owned(),
                    None,
                );
                let opts = AnswerOptions::hosted();
                for &(method, params) in VIEW_METHODS {
                    let parsed = serde_json::from_str(params).unwrap_or_else(|_| json!({}));
                    let _ = view.answer(method, parsed, opts);
                }
            }
        }
    }
});
