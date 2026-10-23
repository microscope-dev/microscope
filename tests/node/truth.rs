//! What a Node fixture reports at a checkpoint (`tests/fixtures/node/truth.js`),
//! and the comparisons that hold a reader of its threads to it, each shown
//! to fail on the lies it exists to catch.

use crate::session::NodeSession;
use crate::support::truth::Report;

/// One frame as V8 reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The function's name, or `-` where V8 has none.
    pub function: String,
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,

    /// The frame's tier, as `turbofan`, `inlined`, or `turbofan>ignition`,
    /// where the frame recorded one.
    pub tier: Option<String>,
}

/// One JavaScript thread and its frames, innermost first, from the frame
/// that called into `truth.js`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// The Linux thread id.
    pub native: u64,
    pub name: String,
    pub printer: bool,
    pub frames: Vec<Frame>,

    pub values: Vec<Named>,
}

/// A value the fixture named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    /// `util.inspect`'s text with the default options.
    pub text: String,
    /// The text with every entry on one line (`compact: true,
    /// breakLength: Infinity`).
    pub line: String,
    /// Whether the value is an error V8 made.
    pub error: bool,
}

fn optional(field: &str) -> Option<String> {
    (field != "-").then(|| field.to_owned())
}

fn number<T: std::str::FromStr>(field: &str) -> T {
    field
        .parse()
        .unwrap_or_else(|_| panic!("a number, not {field:?}"))
}

/// The threads a checkpoint's lines report, in the order it printed them:
/// the printer first.
pub fn threads(lines: &[Vec<String>]) -> Vec<Thread> {
    let mut threads: Vec<Thread> = Vec::new();
    for fields in lines {
        match fields.first().map(String::as_str) {
            Some("thread") => threads.push(Thread {
                native: number(&fields[1]),
                name: fields[2].clone(),
                printer: fields[3] == "printer",
                frames: Vec::new(),
                values: Vec::new(),
            }),
            Some("frame") => {
                let thread = threads.last_mut().expect("a frame of a reported thread");
                assert_eq!(thread.native, number::<u64>(&fields[1]), "{fields:?}");
                assert_eq!(
                    thread.frames.len(),
                    number::<usize>(&fields[2]),
                    "{fields:?}"
                );
                thread.frames.push(Frame {
                    function: fields[3].clone(),
                    file: fields[4].clone(),
                    line: optional(&fields[5]).map(|line| number(&line)),
                    column: optional(&fields[6]).map(|column| number(&column)),
                    tier: optional(&fields[7]),
                });
            }
            Some("value") => {
                let thread = threads.last_mut().expect("a value of a reported thread");
                assert_eq!(thread.native, number::<u64>(&fields[1]), "{fields:?}");
                thread.values.push(Named {
                    name: fields[3].clone(),
                    text: fields[4].clone(),
                    line: fields[5].clone(),
                    error: fields[6] == "error",
                });
            }
            _ => {}
        }
    }
    threads
}

/// Each tier a tier fixture reported, anywhere in `report`, it could not
/// put on the stack.
pub fn unavailable(report: &Report) -> Vec<String> {
    report
        .lines()
        .filter(|fields| fields[0] == "unavailable")
        .map(|fields| fields[1].clone())
        .collect()
}

/// Whether `observed` are the frames `truth` reports, in order: each
/// frame's function, file, line, and column, and its tier where the truth
/// has one. An error says the first that differs.
pub fn check_frames(observed: &[Frame], truth: &[Frame]) -> Result<(), String> {
    fn place(frame: &Frame) -> (&str, &str, Option<u32>, Option<u32>) {
        (&frame.function, &frame.file, frame.line, frame.column)
    }

    for (depth, (seen, real)) in observed.iter().zip(truth).enumerate() {
        if place(seen) != place(real) {
            return Err(format!("frame {depth} is {seen:?}, not {real:?}"));
        }
        if real.tier.is_some() && seen.tier != real.tier {
            return Err(format!(
                "frame {depth}, {}, runs {:?}, not {:?}",
                real.function, seen.tier, real.tier
            ));
        }
    }
    if observed.len() != truth.len() {
        return Err(format!(
            "{} frames, not {}: {observed:?}",
            observed.len(),
            truth.len()
        ));
    }
    Ok(())
}

/// Whether `observed` are the threads `truth` reports, by native id, each
/// with its frames.
pub fn check_threads(observed: &[Thread], truth: &[Thread]) -> Result<(), String> {
    // Every thread's first lie, so that one thread's hides no other's.
    let mut lies = Vec::new();
    for real in truth {
        let Some(seen) = observed.iter().find(|thread| thread.native == real.native) else {
            lies.push(format!("no thread {} ({})", real.native, real.name));
            continue;
        };
        if let Err(lie) = check_frames(&seen.frames, &real.frames) {
            lies.push(format!("thread {} ({}): {lie}", real.native, real.name));
        }
    }
    if observed.len() != truth.len() {
        lies.push(format!("{} threads, not {}", observed.len(), truth.len()));
    }
    if lies.is_empty() {
        Ok(())
    } else {
        Err(lies.join("; "))
    }
}

/// The names of a thread's frames in the fixture `file`, in order.
pub fn functions_in<'a>(thread: &'a Thread, file: &str) -> Vec<&'a str> {
    thread
        .frames
        .iter()
        .filter(|frame| {
            frame
                .file
                .ends_with(&format!("/tests/fixtures/node/{file}"))
        })
        .map(|frame| frame.function.as_str())
        .collect()
}

/// The comparisons fail on a dropped frame, a wrong line, a swapped thread,
/// and a wrong tier, and hold for the truth itself.
#[tokio::test]
async fn the_truth_fails_on_the_lies_it_looks_for() {
    let mut session = NodeSession::launch("26", "stacks.js", &[], &[]).await;
    let truth = threads(&session.stop_at("parked").await);
    session.shutdown().await;

    let mut session = NodeSession::launch("26", "tiers.js", &["--allow-natives-syntax"], &[]).await;
    let tiers = threads(&session.stop_at("tiers").await);
    session.shutdown().await;

    check_threads(&truth, &truth).expect("the truth holds");
    check_threads(&tiers, &tiers).expect("the tiers hold");

    let mut dropped = truth.clone();
    dropped[1].frames.remove(1);
    assert!(check_threads(&dropped, &truth).is_err(), "a dropped frame");

    let mut moved = truth.clone();
    let line = moved[0].frames[0].line.as_mut().expect("a line");
    *line += 1;
    assert!(check_threads(&moved, &truth).is_err(), "a wrong line");

    let mut swapped = truth.clone();
    let (first, second) = (swapped[1].native, swapped[2].native);
    swapped[1].native = second;
    swapped[2].native = first;
    assert!(check_threads(&swapped, &truth).is_err(), "a swapped thread");

    let mut retiered = tiers.clone();
    let frame = retiered[0]
        .frames
        .iter_mut()
        .find(|frame| frame.tier.as_deref() == Some("turbofan"))
        .expect("a Turbofan frame");
    frame.tier = Some("maglev".to_owned());
    assert!(check_threads(&retiered, &tiers).is_err(), "a wrong tier");

    let mut untiered = tiers.clone();
    untiered[0].frames[0].tier = None;
    assert!(check_threads(&untiered, &tiers).is_err(), "a missing tier");
}
