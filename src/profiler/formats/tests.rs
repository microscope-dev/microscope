use std::collections::BTreeMap;

use proptest::prelude::*;

use super::Format;
use crate::analysis::Granularity;
use crate::capture::builder::{CaptureBuilder, InterpretedPlace, LineInput};
use crate::capture::records::{FrameKind, MarkerKind, NONE};
use crate::model::{Pid, Tid};
use crate::profiler::{Profile, StackView, query};

const NAMES: [&str; 5] = ["main", "a", "b::<[u8; 4]>", "c", "d"];

/// A profile of samples of stacks of [`NAMES`], innermost first, in
/// nanoseconds.
fn profile(samples: &[(Vec<usize>, u64)]) -> Profile {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/app", None, [0; 16], 0);
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let frames: Vec<u32> = (0..NAMES.len())
        .map(|at| {
            let frame = builder.frame(FrameKind::Native, module, 0x10 * at as u64, 0);
            let line = LineInput {
                function: NAMES[at],
                file: Some("/src/app.rs"),
                line: u32::try_from(at).unwrap_or(0) + 1,
                column: 0,
                declared: None,
                role: 0,
            };

            builder.symbolize(frame, module, &[line]);
            frame
        })
        .collect();

    for (time, (stack, weight)) in samples.iter().enumerate() {
        let stack: Vec<u32> = stack.iter().map(|&at| frames[at]).collect();
        let stack = builder.stack(&stack);
        builder.sample(
            thread,
            i64::try_from(time).unwrap_or(0),
            *weight,
            stack,
            NONE,
        );
    }

    builder.setting("weight", "nanoseconds");
    Profile {
        capture: builder.finish(),
    }
}

/// Each stack's weight by its functions' names, outermost first, as the
/// views aggregate it.
fn paths(profile: &Profile) -> BTreeMap<Vec<String>, u64> {
    let keys = profile.keys(Granularity::Function);
    let tree = keys.aggregate(&query(StackView::Native));
    let mut paths = BTreeMap::new();

    for node in (0..tree.len()).filter(|&node| tree.own[node] > 0) {
        let mut path = Vec::new();
        let mut at = u32::try_from(node).unwrap_or(NONE);

        while at != NONE {
            path.push(keys.name(tree.key[at as usize]));
            at = tree.parent[at as usize];
        }

        path.reverse();
        *paths.entry(path).or_default() += tree.own[node];
    }

    paths
}

fn samples() -> impl Strategy<Value = Vec<(Vec<usize>, u64)>> {
    proptest::collection::vec(
        (
            proptest::collection::vec(0..NAMES.len(), 1..6),
            1..1_000_000_u64,
        ),
        0..30,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// pprof and folded stacks keep every stack and its weight, and pprof
    /// its unit; what a name's `;` becomes is all that folding changes.
    #[test]
    fn exports_read_back_as_the_stacks_they_were(samples in samples()) {
        let original = profile(&samples);
        let mut expected: BTreeMap<Vec<String>, u64> = BTreeMap::new();

        for (stack, weight) in &samples {
            let path = stack.iter().rev().map(|&at| NAMES[at].to_owned()).collect();
            *expected.entry(path).or_default() += weight;
        }

        prop_assert_eq!(&paths(&original), &expected);

        let pprof = original.export(Format::Pprof, StackView::Native).unwrap();
        let read = Profile::import(&pprof).expect("pprof").profile;
        prop_assert_eq!(&paths(&read), &expected);
        prop_assert_eq!(read.setting("weight"), Some("nanoseconds"));
        prop_assert!(!read.timed());

        let folded = original.export(Format::Folded, StackView::Native).unwrap();
        let read = Profile::import(&folded).expect("folded").profile;
        let unfolded: BTreeMap<Vec<String>, u64> = expected
            .into_iter()
            .map(|(path, weight)| (path.into_iter().map(|name| name.replace(';', ":")).collect(), weight))
            .collect();
        prop_assert_eq!(paths(&read), unfolded);
        prop_assert_eq!(read.setting("weight"), Some("samples"));
    }

    /// No bytes make an importer panic or run away.
    #[test]
    fn importers_fail_on_any_bytes_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = Profile::import(&bytes);
        let mut gzipped = vec![0x1f, 0x8b];
        gzipped.extend_from_slice(&bytes);
        let _ = Profile::import(&gzipped);
    }
}

/// An export in the Python look writes each stack as its Python code sees
/// it, its interpreted frames by their functions, files, and lines.
#[test]
fn a_python_look_exports_its_interpreted_frames_by_line() {
    let mut builder = CaptureBuilder::new();
    let module = builder.module("/bin/python3", None, [0; 16], 0);
    let evaluate = builder.frame(FrameKind::Native, module, 0x10, 0);
    builder.symbolize(
        evaluate,
        module,
        &[LineInput {
            function: "_PyEval_EvalFrameDefault",
            file: None,
            line: 0,
            column: 0,
            declared: None,
            role: crate::capture::records::role_number(crate::CodeRole::Interpreter),
        }],
    );

    let mut python = |function: &str, line: u32| {
        builder.interpreted(&InterpretedPlace {
            function,
            file: Some("/srv/app.py"),
            line,
            declared: Some(("/srv/app.py", 1)),
        })
    };

    let handle = python("handle", 7);
    let outer = python("<module>", 20);
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let stack = builder.stack(&[handle, evaluate, outer, evaluate]);
    builder.sample(thread, 0, 5, stack, NONE);
    builder.setting("weight", "nanoseconds");
    let original = Profile {
        capture: builder.finish(),
    };

    let expected = BTreeMap::from([(vec!["<module>".to_owned(), "handle".to_owned()], 5)]);
    for format in [Format::Pprof, Format::Folded] {
        let bytes = original.export(format, StackView::Python).unwrap();
        let read = Profile::import(&bytes).expect("an export").profile;
        assert_eq!(paths(&read), expected, "{format:?}");
    }

    let pprof = original.export(Format::Pprof, StackView::Python).unwrap();
    let read = Profile::import(&pprof).expect("pprof").profile;
    let keys = read.keys(Granularity::Line);
    let lines: Vec<String> = (0..u32::try_from(read.capture.frames.len()).unwrap_or(0))
        .flat_map(|frame| keys.frame(frame).to_vec())
        .map(|key| keys.name(key))
        .collect();
    assert_eq!(lines, ["handle app.py:7", "<module> app.py:20"]);
}

/// Weights that add up past what a count holds are refused, as a total of
/// them could not be taken.
#[test]
fn weights_that_add_up_past_a_count_are_refused() {
    let text = format!("main;a {}\nmain;b 1\n", u64::MAX);
    assert!(Profile::import(text.as_bytes()).is_err());
}

/// A file's format is its own magic, gzip's, or its name's.
#[test]
fn a_file_says_its_format_by_its_bytes_or_its_name() {
    use std::path::Path;
    let usprof = profile(&[]).to_bytes().unwrap();
    assert_eq!(
        Format::of_file(Path::new("x"), &usprof[..8]),
        Some(Format::Usprof)
    );
    assert_eq!(
        Format::of_file(Path::new("cpu"), &[0x1f, 0x8b, 8]),
        Some(Format::Pprof)
    );
    assert_eq!(
        Format::of_file(Path::new("cpu.prof"), b"\x0a\x04"),
        Some(Format::Pprof)
    );
    assert_eq!(
        Format::of_file(Path::new("x.folded"), b"a;b 1"),
        Some(Format::Folded)
    );
    assert_eq!(Format::of_file(Path::new("trace.json"), b"{"), None);
    assert_eq!(Format::of_file(Path::new("program"), b"\x7fELF"), None);
}

/// A tid one process's thread had and another's took after it ended: each
/// mark is on the thread that made it, in its process.
#[test]
fn a_mark_is_on_its_own_thread_when_a_tid_is_reused() {
    let mut builder = CaptureBuilder::new();
    let site = builder.site("frame", "/src/app.rs", 1, NONE, 0);
    let first = builder.thread(Pid::new(10), Tid::new(7), 100);
    builder.thread_marker((first, site), 150, MarkerKind::Frame, None);
    builder.thread_exit(Tid::new(7), 200);
    builder.thread(Pid::new(20), Tid::new(7), 300);
    let profile = Profile::from_capture(builder.finish());
    let trace: serde_json::Value =
        serde_json::from_str(&super::chrome::export(&profile)).expect("a JSON trace");
    let marks = trace["traceEvents"]
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["ph"] == "i")
        .map(|event| (event["pid"].as_u64(), event["tid"].as_u64()))
        .collect::<Vec<_>>();

    assert_eq!(marks, [(Some(10), Some(7))]);
}
