use std::collections::BTreeMap;

use proptest::prelude::*;

use super::Format;
use super::proto::Message;
use crate::analysis::Granularity;
use crate::capture::builder::{CaptureBuilder, InterpretedPlace, LineInput};
use crate::capture::records::{FrameKind, MarkerKind, NONE};
use crate::model::{Pid, Tid};
use crate::profiler::{Profile, StackView, query};

/// Each frame's module and lines, innermost first, each a function's name
/// and a line: functions inlined two and three deep, one name at several
/// lines and in two modules, and names with spaces, digits after a space,
/// a trailing space, no name, and the characters folded stacks use.
const FRAMES: [(&str, &[(&str, u32)]); 10] = [
    ("/bin/app", &[("main", 1)]),
    ("/bin/app", &[("a", 10)]),
    ("/bin/app", &[("b::<[u8; 4]>", 20), ("a", 11)]),
    ("/bin/app", &[("c", 30), ("d", 40), ("main", 2)]),
    ("/lib/libx.so", &[("a", 5)]),
    ("/bin/app", &[("with space", 1)]),
    ("/bin/app", &[("digits 123", 1)]),
    ("/bin/app", &[("trailing ", 1)]),
    ("/bin/app", &[("", 7)]),
    ("/bin/app", &[("x;y\nz", 1)]),
];

/// A profile of samples of stacks of [`FRAMES`], innermost first, in
/// nanoseconds.
fn profile(samples: &[(Vec<usize>, u64)]) -> Profile {
    let mut builder = CaptureBuilder::new();
    let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
    let frames: Vec<u32> = (0..)
        .zip(FRAMES)
        .map(|(at, (path, lines))| {
            let module = builder.module(path, None, [0; 16], 0);
            let frame = builder.frame(FrameKind::Native, module, 0x10 * at, 0);
            let lines: Vec<LineInput<'_>> = lines
                .iter()
                .map(|&(function, line)| LineInput {
                    function,
                    file: Some("/src/app.rs"),
                    line,
                    column: 0,
                    declared: None,
                    role: 0,
                })
                .collect();

            builder.symbolize(frame, module, &lines);
            frame
        })
        .collect();

    for (time, (stack, weight)) in (0..).zip(samples) {
        let stack: Vec<u32> = stack.iter().map(|&at| frames[at]).collect();
        let stack = builder.stack(&stack);
        builder.sample(thread, time, *weight, stack, NONE);
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

/// Each stack's weight by the names its frames' lines show, outermost
/// first, each name as `shown` shows it with its line.
fn expected(
    samples: &[(Vec<usize>, u64)],
    shown: impl Fn(&str, u32) -> String,
) -> BTreeMap<Vec<String>, u64> {
    let mut expected: BTreeMap<Vec<String>, u64> = BTreeMap::new();
    for (stack, weight) in samples {
        let path = stack
            .iter()
            .rev()
            .flat_map(|&at| FRAMES[at].1.iter().rev())
            .map(|&(name, line)| shown(name, line))
            .collect();
        *expected.entry(path).or_default() += weight;
    }

    expected
}

fn samples() -> impl Strategy<Value = Vec<(Vec<usize>, u64)>> {
    proptest::collection::vec(
        (
            proptest::collection::vec(0..FRAMES.len(), 1..6),
            1..1_000_000_u64,
        ),
        0..30,
    )
}

/// microscope's own format keeps the capture; pprof and folded stacks keep
/// every stack and its weight, inlined functions and recursion too, and
/// pprof its unit. A name pprof cannot hold, the empty one, reads back
/// named for its line, and one folded stacks cannot hold, with a `;` or a
/// line break, has them as `:`; an empty name reads back as anonymous.
fn exports_read_back(samples: &[(Vec<usize>, u64)]) -> Result<(), TestCaseError> {
    let original = profile(samples);
    prop_assert_eq!(
        paths(&original),
        expected(samples, |name, _| name.to_owned())
    );

    let usprof = original.export(Format::Usprof, StackView::Native).unwrap();
    let read = Profile::import(&usprof).expect("usprof").profile;
    prop_assert_eq!(&read.capture, &original.capture);

    let pprof = original.export(Format::Pprof, StackView::Native).unwrap();
    let read = Profile::import(&pprof).expect("pprof").profile;
    let named = |name: &str, line| {
        if name.is_empty() {
            format!("[function at {line}]")
        } else {
            name.to_owned()
        }
    };
    prop_assert_eq!(paths(&read), expected(samples, named));
    prop_assert_eq!(read.setting("weight"), Some("nanoseconds"));
    prop_assert!(!read.timed());

    let folded = original.export(Format::Folded, StackView::Native).unwrap();
    let read = Profile::import(&folded).expect("folded").profile;
    let folded = |name: &str, _| {
        if name.is_empty() {
            "<anonymous>".to_owned()
        } else {
            name.replace([';', '\n'], ":")
        }
    };
    prop_assert_eq!(paths(&read), expected(samples, folded));
    prop_assert_eq!(read.setting("weight"), Some("samples"));
    Ok(())
}

/// Found by the property below: a name's trailing space, which the reader
/// took for the space before the weight.
#[test]
fn a_folded_name_keeps_its_trailing_space() {
    exports_read_back(&[(vec![7], 1)]).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn exports_read_back_as_the_stacks_they_were(samples in samples()) {
        exports_read_back(&samples)?;
    }
}

/// A pprof profile Go wrote of inlined code: one location holds the
/// function inlined into another, the inlined one first.
#[test]
fn a_location_of_inlined_functions_reads_innermost_first() {
    let strings = ["", "cpu", "nanoseconds", "leaf", "middle", "root", "a.go"];
    let mut profile = Message::default();
    profile.message(1, |kind| {
        kind.number(1, 1);
        kind.number(2, 2);
    });
    profile.message(2, |sample| {
        sample.packed(1, [1, 2]);
        sample.packed(2, [7]);
    });

    for (location, lines) in [(1, &[(1, 10), (2, 20)][..]), (2, &[(3, 30)][..])] {
        profile.message(4, |message| {
            message.number(1, location);
            for &(function, line) in lines {
                message.message(4, |message| {
                    message.number(1, function);
                    message.number(2, line);
                });
            }
        });
    }

    for function in 1..=3 {
        profile.message(5, |message| {
            message.number(1, function);
            message.number(2, function + 2);
            message.number(4, 6);
        });
    }

    for string in strings {
        profile.bytes(6, string.as_bytes());
    }

    let read = Profile::import(&profile.bytes).expect("a profile").profile;
    let path = |path: &[&str]| path.iter().map(|name| (*name).to_owned()).collect();
    assert_eq!(
        paths(&read),
        BTreeMap::from([(path(&["root", "middle", "leaf"]), 7)])
    );

    let keys = read.keys(Granularity::Line);
    let lines: Vec<String> = keys.frame(0).iter().map(|&key| keys.name(key)).collect();
    assert_eq!(lines, ["leaf a.go:10", "middle a.go:20"]);
}

/// A pprof profile of functions, locations, and samples by id, which may
/// name ids that are not there or are there twice, and values of any
/// count: function ids, names; location ids, function ids; sample
/// location ids, values.
type Ids = (
    Vec<(u64, usize)>,
    Vec<(u64, Vec<u64>)>,
    Vec<(Vec<u64>, u64)>,
);

/// A profile whose ids name what it has, numbered from 1, with up to two
/// of its ids or values then changed to any number, often one of its ids.
fn ids() -> impl Strategy<Value = Ids> {
    let valid = (1..4_usize, 1..5_usize).prop_flat_map(|(functions, locations)| {
        (
            prop::collection::vec(0..3_usize, functions),
            prop::collection::vec(prop::collection::vec(1..=functions as u64, 0..3), locations),
            prop::collection::vec(
                (
                    prop::collection::vec(1..=locations as u64, 0..4),
                    0..100_u64,
                ),
                0..6,
            ),
        )
    });
    let change = (
        0..5_u8,
        any::<prop::sample::Index>(),
        any::<prop::sample::Index>(),
        prop_oneof![3 => 0..6_u64, 1 => crate::test_strategies::u64_edges()],
    );

    (valid, prop::collection::vec(change, 0..3)).prop_map(|((names, lines, samples), changes)| {
        let mut functions: Vec<(u64, usize)> = (1..).zip(names).collect();
        let mut locations: Vec<(u64, Vec<u64>)> = (1..).zip(lines).collect();
        let mut samples = samples;

        for (field, row, at, number) in changes {
            match field {
                0 => {
                    let row = row.index(functions.len());
                    functions[row].0 = number;
                }
                1 => {
                    let row = row.index(locations.len());
                    locations[row].0 = number;
                }
                2 => {
                    let row = row.index(locations.len());
                    let lines = &mut locations[row].1;
                    if !lines.is_empty() {
                        let at = at.index(lines.len());
                        lines[at] = number;
                    }
                }
                _ if samples.is_empty() => {}
                3 => {
                    let row = row.index(samples.len());
                    let ids = &mut samples[row].0;
                    if !ids.is_empty() {
                        let at = at.index(ids.len());
                        ids[at] = number;
                    }
                }
                _ => {
                    let row = row.index(samples.len());
                    samples[row].1 = number;
                }
            }
        }

        (functions, locations, samples)
    })
}

/// The profile [`Ids`] describe, in nanoseconds, its functions named
/// `f0` to `f2`.
fn encoded((functions, locations, samples): &Ids) -> Vec<u8> {
    let mut profile = Message::default();
    profile.message(1, |kind| {
        kind.number(1, 1);
        kind.number(2, 2);
    });

    for (ids, value) in samples {
        profile.message(2, |sample| {
            sample.packed(1, ids.iter().copied());
            sample.packed(2, [*value]);
        });
    }

    for (id, functions) in locations {
        profile.message(4, |location| {
            location.number(1, *id);
            for &function in functions {
                location.message(4, |line| line.number(1, function));
            }
        });
    }

    for &(id, name) in functions {
        profile.message(5, |function| {
            function.number(1, id);
            function.number(2, 3 + name as u64);
        });
    }

    for string in ["", "cpu", "nanoseconds", "f0", "f1", "f2"] {
        profile.bytes(6, string.as_bytes());
    }

    profile.bytes
}

/// What a reader of [`Ids`] reads: each stack's weight by its names,
/// outermost first, or why it is refused. A location with no lines is
/// named for its id; a sample with no locations has no stack.
fn read_ids(
    (functions, locations, samples): &Ids,
) -> Result<BTreeMap<Vec<String>, u64>, &'static str> {
    let mut names = BTreeMap::new();
    for &(id, name) in functions {
        if names.insert(id, format!("f{name}")).is_some() {
            return Err("two functions of one id");
        }
    }

    let mut frames: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for (id, lines) in locations {
        let lines = if lines.is_empty() {
            vec![format!("{:#x}", 1_u64 << 63 | id)]
        } else {
            lines
                .iter()
                .map(|function| names.get(function).cloned().ok_or("a line of no function"))
                .collect::<Result<_, _>>()?
        };

        if frames.insert(*id, lines).is_some() {
            return Err("two locations of one id");
        }
    }

    let mut paths: BTreeMap<Vec<String>, u64> = BTreeMap::new();
    let mut total = 0_u64;
    // A sample of no weight is passed over before its locations are found.
    for (ids, value) in samples {
        if value.cast_signed() < 0 {
            return Err("a negative value");
        }

        if *value == 0 {
            continue;
        }

        total = total.checked_add(*value).ok_or("values past a count")?;
        let mut path = Vec::new();
        for id in ids {
            let frame = frames.get(id).ok_or("a sample of no location")?;
            path.extend(frame.iter().cloned());
        }

        if path.is_empty() {
            path.push("[no stack]".to_owned());
        }

        path.reverse();
        *paths.entry(path).or_default() += value;
    }

    Ok(paths)
}

/// A pprof profile whose ids name each other or not, and whose values are
/// any count, reads as the stacks it describes, or is refused when it
/// names what it lacks, numbers two things alike, or weighs what no count
/// holds.
fn reads_as_its_ids_describe(ids: &Ids) -> Result<(), TestCaseError> {
    let read = Profile::import(&encoded(ids)).map(|read| read.profile);
    match (read_ids(ids), read) {
        (Ok(expected), Ok(read)) => {
            prop_assert_eq!(read.capture.validate(), Ok(()));
            prop_assert_eq!(paths(&read), expected);
        }
        (Err(_), Err(_)) => {}
        (expected, read) => {
            prop_assert!(
                false,
                "expected {:?}, read {:?}",
                expected,
                read.map(|read| paths(&read))
            );
        }
    }

    Ok(())
}

/// Found by the property below: a profile not gzipped whose bytes are all
/// text was read as folded stacks, and refused.
#[test]
fn a_pprof_profile_that_is_text_is_read_as_pprof() {
    reads_as_its_ids_describe(&(vec![(1, 0)], vec![(1, Vec::new())], Vec::new())).unwrap();
}

/// Found by the property below: two functions of one id, of which the
/// reader took the last.
#[test]
fn two_functions_of_one_id_are_refused() {
    reads_as_its_ids_describe(&(vec![(2, 0), (2, 0)], Vec::new(), Vec::new())).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn a_pprof_profile_reads_as_its_ids_describe_or_is_refused(ids in ids()) {
        reads_as_its_ids_describe(&ids)?;
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

proptest! {
    /// The fuzz target's oracle on damaged copies of its corpus.
    #[test]
    fn damaged_profiles_read_back_through_usprof(
        data in crate::test_strategies::damaged_corpus("profile-import"),
    ) {
        super::fuzz_import(&data);
    }
}
