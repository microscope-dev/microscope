//! Chrome's trace event JSON, as Perfetto and `chrome://tracing` read it:
//! each thread's zones and labels as spans, and the program's marks as
//! instants. Samples are not written: pprof and folded stacks hold them.

use serde_json::{Value, json};

use crate::capture::records::{MarkerKind, ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, ZONE_MISMATCHED};
use crate::profiler::Profile;

/// The profile's instrumentation as a trace, its times in microseconds
/// from the profile's start.
pub fn export(profile: &Profile) -> String {
    let (start, _) = profile.span();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a trace's times are floating microseconds"
    )]
    let micros = |time: i64| (time - start) as f64 / 1000.0;
    let mut events: Vec<Value> = Vec::new();

    for thread in profile.threads() {
        if let Some(name) = thread.name {
            events.push(json!({
                "ph": "M", "name": "thread_name", "pid": thread.pid, "tid": thread.tid,
                "args": {"name": name},
            }));
        }
    }

    for zone in profile.zones() {
        let (name, category) = match &zone.value {
            Some(value) => (format!("{}={value}", zone.name), "label"),
            None => (zone.name.clone(), "zone"),
        };

        let mut event = json!({
            "ph": "X", "name": name, "cat": category, "pid": zone.pid, "tid": zone.tid,
            "ts": micros(zone.start), "dur": micros(zone.end) - micros(zone.start),
        });

        let unseen: Vec<&str> = [
            (ZONE_BEGAN_UNSEEN, "its begin was lost"),
            (ZONE_ENDED_UNSEEN, "its end was not seen"),
            (ZONE_MISMATCHED, "it does not nest"),
        ]
        .into_iter()
        .filter(|(flag, _)| zone.flags & flag != 0)
        .map(|(_, said)| said)
        .collect();
        if !unseen.is_empty() {
            event["args"] = json!({"unseen": unseen});
        }

        events.push(event);
    }

    for marker in profile.program_markers() {
        let category = match marker.kind {
            MarkerKind::Frame => "frame",
            MarkerKind::Trigger => "trigger",
            _ => "message",
        };

        let mut event = json!({
            "ph": "i", "s": "t", "name": marker.name, "cat": category,
            "pid": marker.pid, "tid": marker.tid,
            "ts": micros(marker.time),
        });
        if let Some(text) = marker.text {
            event["args"] = json!({"text": text});
        }

        events.push(event);
    }

    json!({
        "traceEvents": events,
        "displayTimeUnit": "ns",
        "otherData": {
            "program": profile.setting("program"),
            "samples": "not in a trace: export pprof or folded stacks for them",
        },
    })
    .to_string()
}
