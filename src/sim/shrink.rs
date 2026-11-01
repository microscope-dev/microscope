//! Shrinks a failing seed's run to a smaller one that fails the same way.
//!
//! A seed has no structure to shrink, so the shrinker works on what the run
//! chose. First it makes the run's shape simpler, one part at a time; the
//! streams are independent, so this barely disturbs the rest of the run.
//! Then it shrinks the tape of choices the run drew, as Hypothesis does:
//! it deletes runs of choices and lowers single ones toward zero. It keeps
//! each change whose run fails with the same [`Failure::signature`] and is
//! no larger, so the result is never a larger run than the seed's.
//!
//! [`Failure::signature`]: super::report::Failure::signature

use std::fmt::Write as _;

use super::Corpus;
use super::choices::{Stream, Tape};
use super::swarm::Simpler;
use super::world::{Outcome, Settings, run};

/// The smallest failing run found, and how to run it again.
pub struct Shrunk {
    pub seed: u64,
    pub simpler: Vec<Simpler>,
    pub tape: Tape,
    pub outcome: Outcome,

    /// How many runs shrinking took.
    pub runs: u64,
}

impl Shrunk {
    /// The run as a file `replay --shrunk` reads.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = format!("seed {:#x}\n", self.seed);
        for simpler in &self.simpler {
            let _ = writeln!(text, "simpler {simpler}");
        }

        for stream in Stream::ALL {
            let choices = &self.tape.streams[stream.index()];
            if !choices.is_empty() {
                let choices = choices.iter().map(u64::to_string).collect::<Vec<_>>();
                let _ = writeln!(text, "tape {} {}", stream.name(), choices.join(" "));
            }
        }

        text
    }
}

/// The seed, simpler choices, and tape a file `Shrunk::to_text` wrote
/// names.
///
/// # Errors
///
/// When a line is not one that function writes.
pub fn read(text: &str) -> Result<(u64, Vec<Simpler>, Tape), String> {
    let mut seed = None;
    let mut simpler = Vec::new();
    let mut tape = Tape::default();

    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("seed") => {
                let word = words.next().unwrap_or_default();
                let parsed = word
                    .strip_prefix("0x")
                    .map_or_else(|| word.parse(), |hex| u64::from_str_radix(hex, 16));
                seed = Some(parsed.map_err(|_| format!("'{word}' is not a seed"))?);
            }
            Some("simpler") => {
                for word in words {
                    simpler.push(word.parse()?);
                }
            }
            Some("tape") => {
                let name = words.next().unwrap_or_default();
                let stream = Stream::ALL
                    .into_iter()
                    .find(|stream| stream.name() == name)
                    .ok_or_else(|| format!("'{name}' is not a stream"))?;
                for word in words {
                    let choice = word
                        .parse()
                        .map_err(|_| format!("'{word}' is not a choice"))?;
                    tape.streams[stream.index()].push(choice);
                }
            }
            _ => return Err(format!("'{line}' is not a line of a shrunk run")),
        }
    }

    Ok((seed.ok_or("a shrunk run names its seed")?, simpler, tape))
}

/// How large a run is: its actions, then how many choices it drew, then
/// their sum.
fn size(outcome: &Outcome, tape: &Tape) -> (u64, usize, u128) {
    let sum = tape
        .streams
        .iter()
        .flatten()
        .map(|&choice| u128::from(choice))
        .sum();
    (outcome.steps, tape.len(), sum)
}

struct Shrinker<'a> {
    corpus: &'a Corpus,
    settings: &'a Settings,
    signature: String,
    best: Shrunk,
    most_runs: u64,
}

impl Shrinker<'_> {
    const fn exhausted(&self) -> bool {
        self.best.runs >= self.most_runs
    }

    /// Runs with `simpler` and `tape`, and keeps the run when it fails the
    /// same way and is smaller, or, when `ties` keep, no larger.
    fn attempt(&mut self, simpler: Vec<Simpler>, tape: Tape, ties: bool) -> bool {
        if self.exhausted() {
            return false;
        }

        self.best.runs += 1;
        let settings = Settings {
            simpler: simpler.clone(),
            tape: Some(tape),
            ..self.settings.clone()
        };
        let outcome = run(self.best.seed, self.corpus, &settings);
        let same = outcome
            .failure
            .as_ref()
            .is_some_and(|failure| failure.signature() == self.signature);
        let Some(drawn) = outcome.tape.clone() else {
            return false;
        };

        let (new, old) = (
            size(&outcome, &drawn),
            size(&self.best.outcome, &self.best.tape),
        );
        if !same || new > old || (new == old && !ties) {
            return false;
        }

        self.best.simpler = simpler;
        self.best.tape = drawn;
        self.best.outcome = outcome;
        true
    }

    /// Tries each simpler choice of the run's shape, the fewest requests
    /// that still fail first.
    fn simplify_shape(&mut self) {
        let mut fewer_requests = false;
        for candidate in Simpler::for_swarm(&self.best.outcome.swarm) {
            let requests = matches!(candidate, Simpler::Requests(_));
            if requests && fewer_requests {
                continue;
            }

            let mut simpler = self.best.simpler.clone();
            simpler.retain(|kept| {
                !(requests && matches!(kept, Simpler::Requests(_))) && *kept != candidate
            });
            simpler.push(candidate);
            if self.attempt(simpler, Tape::default(), true) && requests {
                fewer_requests = true;
            }
        }
    }

    /// Deletes runs of choices from each stream's tape, the longest first,
    /// and lowers each choice that remains toward zero, until nothing
    /// shrinks the run.
    fn shrink_tape(&mut self) {
        loop {
            let before = size(&self.best.outcome, &self.best.tape);

            for stream in Stream::ALL {
                let index = stream.index();
                let mut chunk = self.best.tape.streams[index].len().next_power_of_two();

                while chunk > 0 && !self.exhausted() {
                    let mut start = 0;
                    while start < self.best.tape.streams[index].len() && !self.exhausted() {
                        let mut tape = self.best.tape.clone();
                        let end = (start + chunk).min(tape.streams[index].len());
                        tape.streams[index].drain(start..end);
                        if !self.attempt(self.best.simpler.clone(), tape, false) {
                            start += chunk;
                        }
                    }

                    chunk /= 2;
                }

                let mut position = 0;
                while position < self.best.tape.streams[index].len() && !self.exhausted() {
                    let choice = self.best.tape.streams[index][position];
                    let half = (choice / 2 > 0).then_some(choice / 2);
                    for lower in std::iter::once(0).chain(half) {
                        let mut tape = self.best.tape.clone();
                        tape.streams[index][position] = lower;
                        if lower < choice && self.attempt(self.best.simpler.clone(), tape, false) {
                            break;
                        }
                    }

                    position += 1;
                }
            }

            if self.exhausted() || size(&self.best.outcome, &self.best.tape) == before {
                return;
            }
        }
    }
}

/// The smallest run found within `most_runs` runs that fails as `seed`'s
/// does, or `None` when `seed`'s run passes.
#[must_use]
pub fn shrink(seed: u64, corpus: &Corpus, settings: &Settings, most_runs: u64) -> Option<Shrunk> {
    let first = Settings {
        simpler: Vec::new(),
        tape: Some(Tape::default()),
        ..settings.clone()
    };
    let outcome = run(seed, corpus, &first);
    let signature = outcome.failure.as_ref()?.signature();
    let tape = outcome.tape.clone().unwrap_or_default();

    let mut shrinker = Shrinker {
        corpus,
        settings,
        signature,
        best: Shrunk {
            seed,
            simpler: Vec::new(),
            tape,
            outcome,
            runs: 1,
        },
        most_runs,
    };

    shrinker.simplify_shape();
    shrinker.shrink_tape();
    Some(shrinker.best)
}
