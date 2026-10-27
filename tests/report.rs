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
}

const fn options() -> report::Options {
    report::Options {
        tree: false,
        limit: 20,
        min_percent: 0.0,
        granularity: Granularity::Function,
        source: Source::Samples,
        look: Look::Native,
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
                        let index = profile.strings.len() as u64;
                        profile.strings.push(name.into());
                        profile.functions.push(Function {
                            id,
                            name: index,
                            file: 3,
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

fn render(current: &Profile, base: Option<&Profile>, options: &report::Options) -> String {
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
