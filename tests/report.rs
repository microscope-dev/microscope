#[path = "../src/cli/profile/report.rs"]
mod report;

use std::io::Write as _;

use microscope::profiler::{Granularity, Look, Profile, Source};
use prost::Message as _;

#[derive(Clone, prost::Message)]
struct Pprof {
    #[prost(message, repeated, tag = "1")]
    types: Vec<ValueType>,
    #[prost(message, repeated, tag = "2")]
    samples: Vec<Sample>,
    #[prost(message, repeated, tag = "4")]
    locations: Vec<Location>,
    #[prost(message, repeated, tag = "5")]
    functions: Vec<Function>,
    #[prost(string, repeated, tag = "6")]
    strings: Vec<String>,
}

#[derive(Clone, prost::Message)]
struct ValueType {
    #[prost(int64, tag = "1")]
    name: i64,
    #[prost(int64, tag = "2")]
    unit: i64,
}

#[derive(Clone, prost::Message)]
struct Sample {
    #[prost(uint64, repeated, tag = "1")]
    locations: Vec<u64>,
    #[prost(uint64, repeated, tag = "2")]
    values: Vec<u64>,
}

#[derive(Clone, prost::Message)]
struct Location {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(message, repeated, tag = "4")]
    lines: Vec<Line>,
}

#[derive(Clone, prost::Message)]
struct Line {
    #[prost(uint64, tag = "1")]
    function: u64,
    #[prost(uint64, tag = "2")]
    number: u64,
}

#[derive(Clone, prost::Message)]
struct Function {
    #[prost(uint64, tag = "1")]
    id: u64,
    #[prost(uint64, tag = "2")]
    name: u64,
    #[prost(uint64, tag = "4")]
    file: u64,
    #[prost(uint64, tag = "5")]
    declared: u64,
}

const fn options() -> report::Options<'static> {
    report::Options {
        tree: false,
        limit: 20,
        min_percent: 0.0,
        granularity: Granularity::Function,
        source: Source::Samples,
        look: Look::Native,
        focus: None,
        invert: false,
    }
}

fn fixture(unit: &str, paths: &[(&[(&str, u32)], u64)]) -> Profile {
    let mut profile = Pprof {
        strings: vec![
            String::new(),
            "cost".into(),
            unit.into(),
            "fixture.c".into(),
        ],
        types: vec![ValueType { name: 1, unit: 2 }],
        ..Pprof::default()
    };
    let mut functions = std::collections::BTreeMap::new();
    let mut known = std::collections::BTreeMap::new();
    for &(path, weight) in paths {
        let locations = path
            .iter()
            .rev()
            .map(|&(name, line)| {
                let next = known.len() as u64 + 1;
                *known.entry((name, line)).or_insert_with(|| {
                    let id = functions.len() as u64 + 1;
                    let function = *functions.entry(name).or_insert_with(|| {
                        let (name, file) = name.split_once('@').unwrap_or((name, "fixture.c"));
                        let index = profile.strings.len() as u64;
                        profile.strings.push(name.into());
                        let file_index = profile.strings.len() as u64;
                        profile.strings.push(file.into());
                        profile.functions.push(Function {
                            id,
                            name: index,
                            file: file_index,
                            declared: 1,
                        });
                        id
                    });
                    profile.locations.push(Location {
                        id: next,
                        lines: vec![Line {
                            function,
                            number: u64::from(line),
                        }],
                    });
                    next
                })
            })
            .collect();
        profile.samples.push(Sample {
            locations,
            values: vec![weight],
        });
    }
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&profile.encode_to_vec()).unwrap();
    Profile::import(&gzip.finish().unwrap()).unwrap().profile
}

fn render(current: &Profile, base: Option<&Profile>, options: &report::Options<'_>) -> String {
    let mut out = Vec::new();
    report::write(current, base, options, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

fn rows(text: &str) -> Vec<Vec<&str>> {
    text.lines()
        .map(|line| line.split_whitespace().collect())
        .collect()
}

#[test]
fn report_diff_uses_absolute_own_costs_and_keeps_added_and_removed_functions() {
    let base = fixture(
        "nanoseconds",
        &[
            (&[("main", 1), ("leaf", 2)], 3_000_000_000),
            (&[("main", 1), ("removed", 3)], 1_000_000_000),
        ],
    );
    let current = fixture(
        "nanoseconds",
        &[
            (&[("main", 1), ("added", 4)], 500_000_000),
            (&[("main", 1), ("leaf", 2)], 1_000_000_000),
        ],
    );
    let text = render(&current, Some(&base), &options());
    assert!(text.contains("Before: 4.000 s"), "{text}");
    assert!(text.contains("After: 1.500 s"), "{text}");
    let rows = rows(&text);
    for expected in [
        vec!["3.000", "s", "1.000", "s", "-2.000", "s", "leaf"],
        vec!["1.000", "s", "0.000", "s", "-1.000", "s", "removed"],
        vec!["0.000", "s", "0.500", "s", "+0.500", "s", "added"],
        vec!["0.000", "s", "0.000", "s", "0.000", "s", "main"],
    ] {
        assert!(rows.contains(&expected), "{text}");
    }
    assert!(text.find("leaf\n") < text.find("removed\n"), "{text}");
    let mut limited = options();
    limited.limit = 1;
    let text = render(&current, Some(&base), &limited);
    assert!(
        text.contains("leaf\n") && !text.contains("removed\n"),
        "{text}"
    );
}

#[test]
fn report_diff_lines_match_names_across_different_key_orders() {
    let base = fixture(
        "executions",
        &[(&[("leaf", 10)], 30), (&[("leaf", 20)], 10)],
    );
    let current = fixture("executions", &[(&[("leaf", 20)], 25), (&[("leaf", 10)], 5)]);
    let mut options = options();
    options.granularity = Granularity::Line;
    let text = render(&current, Some(&base), &options);
    assert!(text.contains("fixture.c:10"), "{text}");
    assert!(text.contains("fixture.c:20"), "{text}");
    assert!(text.contains("-25 executions"), "{text}");
    assert!(text.contains("+15 executions"), "{text}");
    assert!(!text.contains('%'), "{text}");
}

#[test]
fn report_diff_tree_matches_paths_and_keeps_removed_branches() {
    let base = fixture(
        "executions",
        &[
            (&[("main", 1), ("a", 2), ("leaf", 4)], 30),
            (&[("main", 1), ("b", 3), ("leaf", 4)], 10),
        ],
    );
    let current = fixture("executions", &[(&[("main", 1), ("a", 2), ("leaf", 4)], 10)]);
    let mut options = options();
    options.tree = true;
    let text = render(&current, Some(&base), &options);
    assert_eq!(
        text.lines().filter(|line| line.ends_with("leaf")).count(),
        2,
        "{text}"
    );
    assert!(text.contains("    leaf"), "{text}");
    assert!(text.contains("-30 executions"), "{text}");
    assert!(text.contains("-20 executions"), "{text}");
    assert!(text.contains("-10 executions"), "{text}");
    options.min_percent = 30.0;
    let text = render(&current, Some(&base), &options);
    assert!(!text.lines().any(|line| line.ends_with("  b")), "{text}");
}

#[test]
fn report_diff_refuses_mismatched_units_without_printing_a_table() {
    let base = fixture("executions", &[(&[("leaf", 1)], 10)]);
    let current = fixture("nanoseconds", &[(&[("leaf", 1)], 10)]);
    let mut out = Vec::new();
    let error = report::write(&current, Some(&base), &options(), &mut out).unwrap_err();
    assert!(
        error.to_string().contains("nanoseconds") && error.to_string().contains("executions"),
        "{error}"
    );
    assert!(out.is_empty());
}

#[test]
fn report_focus_leaf_has_two_callers_and_no_callees() {
    let profile = fixture(
        "nanoseconds",
        &[
            (&[("main", 1), ("a", 2), ("leaf", 4)], 3_000_000_000),
            (&[("main", 1), ("b", 3), ("leaf", 4)], 1_000_000_000),
            (&[("unrelated", 5)], 2_000_000_000),
        ],
    );
    let mut options = options();
    options.focus = Some("leaf");
    let text = render(&profile, None, &options);
    assert!(text.contains("Total: 4.000 s"), "{text}");
    assert!(text.contains("Own: 4.000 s"), "{text}");
    assert!(rows(&text).contains(&vec!["3.000", "s", "a"]), "{text}");
    assert!(rows(&text).contains(&vec!["1.000", "s", "b"]), "{text}");
    assert!(
        text.contains("Callees (outermost occurrence):\n  (none)"),
        "{text}"
    );
    assert!(!text.contains("unrelated"), "{text}");
}

#[test]
fn report_focus_tree_merges_paths_without_changing_recursive_costs() {
    let profile = fixture(
        "executions",
        &[
            (&[("a", 1), ("f", 2), ("f", 2), ("g", 3)], 30),
            (&[("b", 1), ("f", 2), ("g", 3)], 10),
            (&[("a", 1), ("f", 2)], 5),
            (&[("a", 1), ("f", 2), ("f", 2)], 7),
        ],
    );
    let mut options = options();
    options.focus = Some("f");
    let text = render(&profile, None, &options);
    assert!(text.contains("Total: 52 executions"), "{text}");
    assert!(text.contains("Own: 12 executions"), "{text}");
    let incoming = text
        .split("Callers (outermost occurrence):")
        .nth(1)
        .unwrap()
        .split("Callees")
        .next()
        .unwrap();
    assert!(
        rows(incoming).contains(&vec!["42", "executions", "a"]),
        "{text}"
    );
    assert!(
        rows(incoming).contains(&vec!["10", "executions", "b"]),
        "{text}"
    );
    let outgoing = text
        .split("Callees (outermost occurrence):")
        .nth(1)
        .unwrap();
    assert!(
        rows(outgoing).contains(&vec!["37", "executions", "f"]),
        "{text}"
    );
    assert!(
        rows(outgoing).contains(&vec!["10", "executions", "g"]),
        "{text}"
    );
    options.tree = true;
    let text = render(&profile, None, &options);
    assert!(text.lines().any(|line| line.ends_with("    g")), "{text}");
    assert!(
        !text
            .lines()
            .any(|line| line.ends_with(" a") || line.ends_with(" b")),
        "{text}"
    );
    assert!(text.contains("52 executions"), "{text}");
    assert_eq!(
        text.lines().filter(|line| line.ends_with(" g")).count(),
        2,
        "{text}"
    );
}

#[test]
fn report_focus_missing_and_ambiguous_names_are_errors_with_candidates() {
    let profile = fixture(
        "executions",
        &[(&[("same@a.c", 10)], 30), (&[("same@b.c", 20)], 10)],
    );
    let mut options = options();
    for name in ["sam", "same"] {
        options.focus = Some(name);
        let mut out = Vec::new();
        let error = report::write(&profile, None, &options, &mut out)
            .unwrap_err()
            .to_string();
        assert!(out.is_empty());
        if name == "same" {
            assert!(error.contains("2 things are named same"), "{error}");
            assert!(
                error.contains("same [a.c:1]") && error.contains("same [b.c:1]"),
                "{error}"
            );
        } else {
            assert!(
                error.contains("nothing in the profile is named sam"),
                "{error}"
            );
        }
    }
    options.focus = Some("same [b.c:1]");
    let text = render(&profile, None, &options);
    assert!(text.contains("Total: 10 executions"), "{text}");
    options.granularity = Granularity::Line;
    options.focus = Some("same a.c:10");
    let text = render(&profile, None, &options);
    assert!(text.contains("Total: 30 executions"), "{text}");
}

#[test]
fn report_focus_diff_keeps_zero_for_a_function_missing_from_the_base() {
    let current = fixture("executions", &[(&[("a", 1), ("new", 2)], 10)]);
    let base = fixture("executions", &[(&[("a", 1), ("old", 2)], 20)]);
    let mut options = options();
    options.focus = Some("new");
    options.tree = true;
    let text = render(&current, Some(&base), &options);
    assert!(text.contains("+10 executions"), "{text}");
    assert!(!text.contains(" old") && !text.contains(" a\n"), "{text}");
}

#[test]
fn report_focus_butterfly_compares_each_relation_by_absolute_cost() {
    let base = fixture(
        "executions",
        &[
            (&[("a", 1), ("f", 2), ("g", 3)], 20),
            (&[("b", 1), ("f", 2)], 10),
        ],
    );
    let current = fixture(
        "executions",
        &[
            (&[("b", 1), ("f", 2), ("g", 3)], 5),
            (&[("c", 1), ("f", 2)], 15),
        ],
    );
    let mut options = options();
    options.focus = Some("f");
    let text = render(&current, Some(&base), &options);
    let rows = rows(&text);
    for (before, after, change, name) in [
        ("30", "20", "-10", "total"),
        ("10", "15", "+5", "own"),
        ("20", "0", "-20", "a"),
        ("10", "5", "-5", "b"),
        ("0", "15", "+15", "c"),
        ("20", "5", "-15", "g"),
    ] {
        assert!(
            rows.contains(&vec![
                before,
                "executions",
                after,
                "executions",
                change,
                "executions",
                name
            ]),
            "{text}"
        );
    }
}

#[test]
fn report_focus_refuses_an_ambiguous_base_before_writing_output() {
    let current = fixture("executions", &[(&[("f", 1)], 10)]);
    let base = fixture(
        "executions",
        &[(&[("f@one.c", 1)], 10), (&[("f@two.c", 1)], 20)],
    );
    let mut options = options();
    options.focus = Some("f");
    let mut out = Vec::new();
    let error = report::write(&current, Some(&base), &options, &mut out).unwrap_err();
    assert!(
        format!("{error:#}").contains("2 things are named f"),
        "{error:#}"
    );
    assert!(out.is_empty());
}
