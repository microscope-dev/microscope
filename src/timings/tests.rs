use super::*;

#[test]
fn a_recording_collects_every_threads_spans_and_counts() {
    let recording = Recording::start(Options::default()).expect("no other recording");

    assert_eq!(
        Recording::start(Options::default()).err(),
        Some(AlreadyRecording),
        "one recording at a time"
    );
    {
        let load = crate::span!("load");
        let parent = load.id();
        assert!(parent.is_some(), "a recorded span has an identifier");
        crate::count!("units", 2_usize);
        std::thread::scope(|scope| {
            for unit in 0..2 {
                scope.spawn(move || {
                    let _unit = crate::span!(parent = parent, "unit", "{unit:#x}");
                    {
                        let _inner = crate::span!("decode");
                        crate::count!("dies", 10_u32);
                    }

                    crate::count!("dies", 1_u32);
                });
            }
        });
    }

    crate::count!("loose", 3_u64);
    mark("ready");
    let report = recording.finish().unwrap();

    let phase = |name: &str| {
        report
            .phases
            .iter()
            .find(|phase| phase.name == name)
            .unwrap_or_else(|| panic!("no phase {name} in {:?}", report.phases))
    };

    assert_eq!(phase("load").spans, 1);
    assert_eq!(phase("load").counters.get("units"), Some(&2));
    assert_eq!(phase("unit").spans, 2);
    assert_eq!(
        phase("unit").counters.get("dies"),
        Some(&2),
        "counts attach to the innermost span"
    );
    assert_eq!(phase("decode").counters.get("dies"), Some(&20));
    assert_eq!(report.counters.get("dies"), Some(&22));
    assert_eq!(report.counters.get("loose"), Some(&3));
    assert!(report.summary.ready_ms.is_some());
    assert_eq!(report.threads.len(), 3, "the caller and two workers");

    let units = report
        .trace_events
        .iter()
        .filter(|event| event.name == "unit")
        .collect::<Vec<_>>();

    let load_id = report
        .trace_events
        .iter()
        .find(|event| event.name == "load")
        .and_then(|event| event.args.get("id").cloned())
        .expect("the load span's identifier");
    assert!(
        units
            .iter()
            .all(|unit| unit.args.get("parent") == Some(&load_id)),
        "work on other threads names the span it ran for"
    );

    let mut labels = units
        .iter()
        .filter_map(|unit| unit.args.get("label")?.as_str())
        .collect::<Vec<_>>();
    labels.sort_unstable();
    assert_eq!(labels, ["0x0", "0x1"]);

    let decoded = Report::from_json(&report.to_json().unwrap()).expect("the report reads back");
    assert_eq!(decoded.phases.len(), report.phases.len());
    assert!(decoded.text(false).contains("decode"));
    assert!(decoded.threads_text().contains("load: 2 parts"));
}

#[test]
fn nothing_is_measured_without_a_recording() {
    let span = span(
        "idle",
        None,
        Some(&|| panic!("a label formatted while not recording")),
    );
    assert_eq!(span.id(), None);
    count("idle", 1);
    drop(span);

    // Spans begun before a recording are not reported by it.
    let before = crate::span!("before");
    let recording = Recording::start(Options::default()).expect("no other recording");
    drop(before);
    let report = recording.finish().unwrap();
    assert!(report.phases.is_empty(), "{:?}", report.phases);
}

/// A thread keeps the spans it finished inside an open one until that one
/// ends. When it ends under a later recording, the spans belong to the
/// recording that ended, not to the later one.
#[test]
fn a_later_recording_reports_no_span_of_an_earlier_one() {
    let (finished_inner, inner_finished) = std::sync::mpsc::channel();
    let (end_outer, outer_ends) = std::sync::mpsc::channel::<()>();
    let earlier = Recording::start(Options::default()).expect("no other recording");
    let worker = std::thread::spawn(move || {
        let outer = crate::span!("outer");
        drop(crate::span!("earlier"));
        finished_inner.send(()).expect("the test waits");
        outer_ends.recv().expect("the test ends the outer span");
        drop(outer);
    });
    inner_finished
        .recv()
        .expect("the worker finished its inner span");
    drop(earlier.finish().unwrap());
    let later = Recording::start(Options::default()).expect("the earlier one ended");
    end_outer.send(()).expect("the worker waits");
    worker.join().expect("the worker ends");
    let report = later.finish().unwrap();
    assert!(report.phases.is_empty(), "{:?}", report.phases);
}

#[test]
fn a_report_bounds_its_labels() {
    let recording = Recording::start(Options::default()).expect("no other recording");
    drop(crate::span!("long", "{}", "é".repeat(MAX_LABEL)));
    let report = recording.finish().unwrap();
    let label = report
        .trace_events
        .iter()
        .find_map(|event| event.args.get("label")?.as_str().map(str::to_owned))
        .expect("a label");
    assert!(label.len() <= MAX_LABEL, "{}", label.len());
}

#[test]
fn a_report_says_whether_it_counted_instructions() {
    let recording = Recording::start(Options { instructions: true }).expect("no recording");
    {
        let _work = crate::span!("work");
        std::hint::black_box((0..10_000_u64).sum::<u64>());
    }

    let report = recording.finish().unwrap();
    let work = &report.phases[0];

    match report.summary.instructions.as_str() {
        "counted" | "multiplexed" => assert!(work.instructions.is_some_and(|count| count > 0)),
        unavailable => {
            assert!(unavailable.starts_with("unavailable: "), "{unavailable}");
            assert_eq!(
                work.instructions, None,
                "an unavailable counter is not zero"
            );
        }
    }
}
