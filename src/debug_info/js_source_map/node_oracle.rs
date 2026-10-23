//! Lookups compared with Node's own decoder, `SourceMap` from `node:module`,
//! for each official build in the dev shell's matrix, on hand-written maps
//! and generated ones.
//!
//! Node and this decoder disagree in two places, both counted and checked
//! to be nothing else:
//!
//! - Node's `findEntry` answers a position before its line's first segment
//!   with the last segment of an earlier line. This decoder answers none,
//!   as Mozilla's `source-map` does: the segment ended with its line.
//! - Node reads past the end of `mappings` when its last segment has 1 or
//!   4 fields, as if the missing characters were zero digits, which gives
//!   a 1-field segment the source of the segment before it and either one
//!   a name. ECMA-426 gives such a segment no source or no name.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use super::tests::{Written, encode_mappings};
use super::*;

const SCRIPT: &str = r"
const { SourceMap } = require('node:module');
let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => { input += chunk; });
process.stdin.on('end', () => {
  const answers = JSON.parse(input).map(({ json, queries }) => {
    const map = new SourceMap(JSON.parse(json));
    return queries.map(([line, column]) => [
      map.findEntry(line, column),
      map.findOrigin(line + 1, column + 1),
    ]);
  });
  process.stdout.write(JSON.stringify(answers));
});
";

struct Case {
    name: String,
    json: String,

    /// The generated start of each section's last segment, where Node's
    /// reading past the end shows.
    finals: Vec<Position>,
    lines: u32,
    columns: u32,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u32) -> u32 {
        u32::try_from(self.next() % u64::from(bound)).expect("below a u32")
    }
}

/// Lines of segments in column order, as generators write them, with
/// repeated columns, unmapped segments, and names.
fn random_lines(rng: &mut Rng) -> Vec<Vec<Written>> {
    (0..=rng.below(7))
        .map(|_| {
            let mut columns: Vec<u32> = (0..rng.below(7)).map(|_| rng.below(40)).collect();
            columns.sort_unstable();
            columns
                .into_iter()
                .map(|column| Written {
                    column,
                    original: (rng.below(5) != 0).then(|| {
                        (
                            rng.below(3),
                            rng.below(20),
                            rng.below(30),
                            (rng.below(2) == 0).then(|| rng.below(4)),
                        )
                    }),
                })
                .collect()
        })
        .collect()
}

fn section_json(lines: &[Vec<Written>]) -> String {
    format!(
        r#"{{"version":3,"sources":["s0.ts","s1.ts","s2.ts"],"names":["n0","n1","n2","n3"],"mappings":"{}"}}"#,
        encode_mappings(lines)
    )
}

fn final_position(lines: &[Vec<Written>], offset: Position) -> Option<Position> {
    let (line, segments) = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, segments)| !segments.is_empty())?;
    let column = segments.last()?.column;
    generated_position(offset, u32::try_from(line).ok()?, column)
}

fn hand_written() -> Vec<Case> {
    let at = |line, column| Position::new(Line(line), Column(column));
    vec![
        Case {
            name: "worked".into(),
            json: r#"{"version":3,"sources":["a.ts","b.ts"],"names":["foo","bar"],"mappings":"AAAA,IAAI,EAAEC,G;;KCCK,I,EDDF,CAAAD"}"#.into(),
            finals: vec![at(2, 12)],
            lines: 4,
            columns: 20,
        },
        Case {
            name: "index".into(),
            json: r#"{"version":3,"sections":[
                {"offset":{"line":0,"column":0},"map":{"version":3,"sources":["one.ts"],"names":["unused"],"mappings":"AAAA,EAAE;AACA"}},
                {"offset":{"line":1,"column":10},"map":{"version":3,"sources":["two.ts","one.ts"],"names":["x"],"mappings":"AAAAA,EAAE;CAAC,CCAA"}}]}"#.into(),
            finals: vec![at(1, 0), at(2, 2)],
            lines: 4,
            columns: 20,
        },
        Case {
            name: "empty lines and a trailing unmapped segment".into(),
            json: r#"{"version":3,"sources":["c.ts"],"names":[],"mappings":";;AAAA;;;CAAC,G"}"#.into(),
            finals: vec![at(5, 4)],
            lines: 7,
            columns: 10,
        },
        Case {
            name: "a trailing separator".into(),
            json: r#"{"version":3,"sources":["c.ts","d.ts"],"names":["n"],"mappings":"GAAG,ECCC,G;"}"#.into(),
            finals: vec![],
            lines: 2,
            columns: 10,
        },
    ]
}

fn generated() -> Vec<Case> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut cases = Vec::new();
    for index in 0..40 {
        let lines = random_lines(&mut rng);
        cases.push(Case {
            name: format!("random {index}"),
            json: section_json(&lines),
            finals: final_position(&lines, Position::default())
                .into_iter()
                .collect(),
            lines: u32::try_from(lines.len()).expect("few lines") + 1,
            columns: 44,
        });
    }

    for index in 0..10 {
        let mut offset = Position::default();
        let mut sections = Vec::new();
        let mut finals = Vec::new();
        for _ in 0..2 + rng.below(2) {
            let lines = random_lines(&mut rng);
            sections.push(format!(
                r#"{{"offset":{{"line":{},"column":{}}},"map":{}}}"#,
                offset.line.0,
                offset.column.0,
                section_json(&lines)
            ));
            finals.extend(final_position(&lines, offset));
            let height = u32::try_from(lines.len()).expect("few lines");
            offset = Position::new(Line(offset.line.0 + height), Column(rng.below(10)));
        }
        cases.push(Case {
            name: format!("random index {index}"),
            json: format!(r#"{{"version":3,"sections":[{}]}}"#, sections.join(",")),
            finals,
            lines: offset.line.0 + 1,
            columns: 54,
        });
    }

    cases
}

/// A segment's original place as Node reports it: source URL, line,
/// column, and name, each `null` where it has none.
fn ours(map: &SourceMap, place: Option<OriginalPlace>) -> Value {
    let Some(place) = place else {
        return Value::Null;
    };
    let source = map.source(place.source).expect("a source").name.as_deref();
    json!([
        source,
        place.line.0,
        place.column.0,
        place.name.and_then(|name| map.name(name)),
    ])
}

fn theirs(entry: &Value) -> Value {
    if entry.get("originalSource").is_none() {
        return Value::Null;
    }
    json!([
        entry["originalSource"],
        entry["originalLine"],
        entry["originalColumn"],
        entry.get("name").unwrap_or(&Value::Null),
    ])
}

#[derive(Default, Debug)]
struct Tally {
    queries: usize,
    agreed: usize,
    origins: usize,
    reverse: usize,
    earlier_line: usize,
    past_the_end: usize,
    mismatches: Vec<String>,
}

fn compare(
    case: &Case,
    map: &SourceMap,
    queries: &[(u32, u32)],
    answers: &[Value],
    tally: &mut Tally,
) {
    assert_eq!(answers.len(), queries.len(), "{}", case.name);
    for (&(line, column), answer) in queries.iter().zip(answers) {
        tally.queries += 1;
        let entry = &answer[0];
        let origin = &answer[1];
        let covering = map.covering(Position::new(Line(line), Column(column)));

        let Some(node_line) = entry.get("generatedLine").and_then(Value::as_u64) else {
            if covering.is_none() {
                tally.agreed += 1;
            } else {
                tally.mismatches.push(format!(
                    "{} {line}:{column}: Node finds nothing, this finds {covering:?}",
                    case.name
                ));
            }
            continue;
        };

        if node_line != u64::from(line) {
            if covering.is_none() {
                tally.earlier_line += 1;
            } else {
                tally.mismatches.push(format!(
                    "{} {line}:{column}: Node finds line {node_line}, this {covering:?}",
                    case.name
                ));
            }
            continue;
        }

        let Some(covering) = covering else {
            tally.mismatches.push(format!(
                "{} {line}:{column}: Node finds {entry}, this none",
                case.name
            ));
            continue;
        };

        let start = covering.start.0;
        let mine = ours(map, covering.original);
        let node = theirs(entry);
        if entry["generatedColumn"] != json!(start) {
            tally.mismatches.push(format!(
                "{} {line}:{column}: Node's segment starts at {entry}, this at {start}",
                case.name
            ));
            continue;
        }

        if mine != node {
            let at_end = case
                .finals
                .contains(&Position::new(Line(line), Column(start)));
            let only_added = match (&mine, &node) {
                (Value::Null, Value::Array(_)) => true,
                (Value::Array(mine), Value::Array(node)) => {
                    mine[..3] == node[..3] && mine[3].is_null() && !node[3].is_null()
                }
                _ => false,
            };
            if at_end && only_added {
                tally.past_the_end += 1;
            } else {
                tally.mismatches.push(format!(
                    "{} {line}:{column}: Node maps to {node}, this to {mine}",
                    case.name
                ));
            }
            continue;
        }

        tally.agreed += 1;
        if let Some(place) = covering.original {
            // findOrigin takes and gives one-based numbers, the column
            // extended from the segment's start.
            let expected_column = place.column.0 + (column - start) + 1;
            let expected_line = place.line.one_based();
            if origin["lineNumber"] == json!(expected_line)
                && origin["columnNumber"] == json!(expected_column)
            {
                tally.origins += 1;
            } else {
                tally.mismatches.push(format!(
                    "{} {line}:{column}: Node's findOrigin gives {origin}, this {expected_line:?}:{expected_column}",
                    case.name
                ));
            }
        }
    }
}

#[test]
fn lookups_agree_with_nodes_decoder() {
    let root = PathBuf::from(
        std::env::var_os("MICROSCOPE_NODE")
            .expect("MICROSCOPE_NODE names the Node builds; run tests in the dev shell"),
    );
    let base = Location::Path(PathBuf::from("/work/dist/app.js.map"));
    let cases: Vec<Case> = hand_written().into_iter().chain(generated()).collect();

    let mut maps = Vec::new();
    let mut input = Vec::new();
    let mut reverse = 0;
    for case in &cases {
        let map = SourceMap::parse(case.json.as_bytes(), &base).expect(&case.name);
        let mut queries: Vec<(u32, u32)> = (0..case.lines)
            .flat_map(|line| (0..case.columns).map(move |column| (line, column)))
            .collect();

        // Each position a breakpoint on an original line takes, which Node
        // must map back to that line.
        for (source, _) in map.sources() {
            for line in 0..20 {
                for found in map.generated_for_line(source, Line(line)) {
                    reverse += 1;
                    queries.push((found.generated.line.0, found.generated.column.0));
                }
            }
        }

        input.push(json!({ "json": case.json, "queries": queries }));
        maps.push((map, queries));
    }
    let input = serde_json::to_vec(&input).expect("JSON");

    for build in ["26", "24", "22"] {
        let mut child = Command::new(root.join(build).join("node"))
            .args(["-e", SCRIPT])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("node");
        let mut stdin = child.stdin.take().expect("stdin");
        let writer = {
            let input = input.clone();
            std::thread::spawn(move || stdin.write_all(&input))
        };
        let output = child.wait_with_output().expect("node's answers");
        writer.join().expect("the writer").expect("node's input");
        assert!(
            output.status.success(),
            "node {build}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let answers: Vec<Vec<Value>> = serde_json::from_slice(&output.stdout).expect("JSON");

        let mut tally = Tally {
            reverse,
            ..Tally::default()
        };
        for ((case, (map, queries)), answers) in cases.iter().zip(&maps).zip(&answers) {
            compare(case, map, queries, answers, &mut tally);
        }

        println!(
            "node {build}: {} maps, {} queries, {} agreed ({} findOrigin checks, {} reverse positions), {} before their line's first segment, {} past the end of mappings, {} mismatches",
            cases.len(),
            tally.queries,
            tally.agreed,
            tally.origins,
            tally.reverse,
            tally.earlier_line,
            tally.past_the_end,
            tally.mismatches.len(),
        );
        assert!(
            tally.mismatches.is_empty(),
            "node {build} disagrees:\n{}",
            tally.mismatches.join("\n")
        );
        assert!(tally.earlier_line > 0 && tally.past_the_end > 0);
    }
}
