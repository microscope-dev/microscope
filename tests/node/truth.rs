use crate::support::Scenario;
use crate::support::truth::Report;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub function: String,
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,

    pub tier: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    pub native: u64,
    pub name: String,
    pub printer: bool,
    pub frames: Vec<Frame>,

    pub awaiting: Vec<Frame>,

    pub values: Vec<Named>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub text: String,
    pub line: String,
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

pub fn threads(lines: &[Vec<String>]) -> Vec<Thread> {
    let mut threads: Vec<Thread> = Vec::new();
    for fields in lines {
        match fields.first().map(String::as_str) {
            Some("thread") => threads.push(Thread {
                native: number(&fields[1]),
                name: fields[2].clone(),
                printer: fields[3] == "printer",
                frames: Vec::new(),
                awaiting: Vec::new(),
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
            Some("async") => {
                let thread = threads
                    .last_mut()
                    .expect("an async frame of a reported thread");
                assert_eq!(thread.native, number::<u64>(&fields[1]), "{fields:?}");
                assert_eq!(
                    thread.awaiting.len(),
                    number::<usize>(&fields[2]),
                    "{fields:?}"
                );
                thread.awaiting.push(Frame {
                    function: fields[3].clone(),
                    file: fields[4].clone(),
                    line: optional(&fields[5]).map(|line| number(&line)),
                    column: optional(&fields[6]).map(|column| number(&column)),
                    tier: None,
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

pub fn unavailable(report: &Report) -> Vec<String> {
    report
        .lines()
        .filter(|fields| fields[0] == "unavailable")
        .map(|fields| fields[1].clone())
        .collect()
}

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

pub fn check_threads(observed: &[Thread], truth: &[Thread]) -> Result<(), String> {
    let mut lies = Vec::new();
    for real in truth {
        let Some(seen) = observed.iter().find(|thread| thread.native == real.native) else {
            lies.push(format!("no thread {} ({})", real.native, real.name));
            continue;
        };
        if let Err(lie) = check_frames(&seen.frames, &real.frames) {
            lies.push(format!("thread {} ({}): {lie}", real.native, real.name));
        }
        if let Err(lie) = check_frames(&seen.awaiting, &real.awaiting) {
            lies.push(format!(
                "thread {} ({}) awaiting: {lie}",
                real.native, real.name
            ));
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

pub fn saved(fixture: &str, checkpoint: &str) -> Vec<Thread> {
    let core = Scenario::fixture(&format!("node-{fixture}-26.core"));
    let log = std::fs::read_to_string(core.with_extension("core.log"))
        .expect("the core's log; run `just build-test-programs`");
    threads(
        Report::parse(&log)
            .checkpoint(checkpoint)
            .expect("the checkpoint the core was taken at"),
    )
}

#[test]
fn the_truth_fails_on_the_lies_it_looks_for() {
    let truth = saved("stacks", "parked");
    let tiers = saved("tiers", "tiers");

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
