//! A recording's report: JSON for scripts and for trace viewers, which
//! open its `traceEvents` as Chrome trace events, and compact text for
//! people and agents in a terminal.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::Event;
use super::alloc::Totals;

/// The report format's version, which readers check.
pub const SCHEMA: u32 = 1;

/// The most phases a summary lists unless asked for all.
const SUMMARY_ROWS: usize = 40;

pub(super) enum CounterStatus {
    Off,
    Counted,
    Multiplexed,
    Unavailable(String),
}

pub(super) struct Raw {
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub peak_rss_bytes: Option<u64>,
    pub allocations: Totals,
    pub events: Vec<Event>,
    pub marks: Vec<(&'static str, u64)>,
    pub loose: Vec<(&'static str, u64)>,
    pub dropped: u64,
    pub instructions: CounterStatus,
}

/// What one recording measured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub summary: Summary,

    /// Every span name's totals, the longest first.
    pub phases: Vec<Phase>,

    /// Each thread's busy time.
    pub threads: Vec<ThreadBusy>,

    /// Every counter's total over the whole recording.
    pub counters: BTreeMap<String, u64>,

    /// Marked instants, in milliseconds since the recording began.
    pub marks: BTreeMap<String, f64>,

    /// Spans the recording had no room for.
    pub dropped_events: u64,

    #[serde(rename = "traceEvents")]
    pub trace_events: Vec<TraceEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub wall_ms: f64,

    /// Every thread's CPU time over the recording.
    pub cpu_ms: f64,

    /// When the session was ready for its first command, if it got there.
    pub ready_ms: Option<f64>,

    /// The process's peak resident set over its whole life.
    pub peak_rss_bytes: Option<u64>,

    /// Absent unless the binary counts its allocations.
    pub allocations: Option<u64>,
    pub allocated_bytes: Option<u64>,

    /// The most the heap grew during the recording.
    pub peak_heap_growth_bytes: Option<i64>,

    /// `off`, `counted`, `multiplexed`, or why they could not be counted.
    pub instructions: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    pub name: String,
    pub spans: u64,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub instructions: Option<u64>,
    pub allocations: u64,
    pub allocated_bytes: u64,
    pub counters: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadBusy {
    pub thread: u32,

    /// The wall time of the thread's outermost spans.
    pub busy_ms: f64,
    pub cpu_ms: f64,
    pub spans: u64,

    /// Its longest outermost span.
    pub longest: Option<String>,
    pub longest_ms: f64,
}

/// One Chrome trace event: a complete span (`X`), an instant (`i`), or
/// a thread's name (`M`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEvent {
    pub name: String,
    pub ph: String,

    /// Microseconds since the recording began.
    pub ts: f64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur: Option<f64>,
    pub pid: u32,
    pub tid: u32,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, serde_json::Value>,
}

#[expect(
    clippy::cast_precision_loss,
    reason = "times are reported approximately"
)]
fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

#[expect(
    clippy::cast_precision_loss,
    reason = "times are reported approximately"
)]
fn us(ns: u64) -> f64 {
    ns as f64 / 1e3
}

impl Report {
    #[expect(
        clippy::too_many_lines,
        reason = "one pass builds every part of the report"
    )]
    pub(super) fn new(raw: Raw) -> Self {
        let counted = super::alloc::installed();
        let mut phases = BTreeMap::<&'static str, Phase>::new();
        let mut counters = BTreeMap::<String, u64>::new();
        let mut threads = BTreeMap::<u32, ThreadBusy>::new();
        let mut trace_events = Vec::with_capacity(raw.events.len() + raw.marks.len());

        for event in &raw.events {
            let phase = phases.entry(event.name).or_insert_with(|| Phase {
                name: event.name.to_owned(),
                spans: 0,
                wall_ms: 0.0,
                cpu_ms: 0.0,
                instructions: None,
                allocations: 0,
                allocated_bytes: 0,
                counters: BTreeMap::new(),
            });
            phase.spans += 1;
            phase.wall_ms += ms(event.wall_ns);
            phase.cpu_ms += ms(event.cpu_ns);
            if let Some(instructions) = event.instructions {
                *phase.instructions.get_or_insert(0) += instructions;
            }

            phase.allocations += event.allocations.blocks;
            phase.allocated_bytes += event.allocations.bytes;
            for (name, amount) in &event.counters {
                *phase.counters.entry((*name).to_owned()).or_default() += amount;
                *counters.entry((*name).to_owned()).or_default() += amount;
            }

            if event.depth == 0 {
                let busy = threads.entry(event.thread).or_insert_with(|| ThreadBusy {
                    thread: event.thread,
                    busy_ms: 0.0,
                    cpu_ms: 0.0,
                    spans: 0,
                    longest: None,
                    longest_ms: 0.0,
                });
                busy.busy_ms += ms(event.wall_ns);
                busy.cpu_ms += ms(event.cpu_ns);
                busy.spans += 1;
                if ms(event.wall_ns) > busy.longest_ms {
                    busy.longest_ms = ms(event.wall_ns);
                    busy.longest = Some(label_of(event));
                }
            }

            let mut args = BTreeMap::new();

            if let Some(label) = &event.label {
                args.insert("label".to_owned(), label.as_ref().into());
            }

            if let Some(parent) = event.parent {
                args.insert("parent".to_owned(), parent.into());
            }

            args.insert("id".to_owned(), event.id.into());
            args.insert("cpu_us".to_owned(), us(event.cpu_ns).into());
            if let Some(instructions) = event.instructions {
                args.insert("instructions".to_owned(), instructions.into());
            }

            if counted {
                args.insert("allocations".to_owned(), event.allocations.blocks.into());
                args.insert("allocated_bytes".to_owned(), event.allocations.bytes.into());
            }

            for (name, amount) in &event.counters {
                args.insert((*name).to_owned(), (*amount).into());
            }

            trace_events.push(TraceEvent {
                name: event.name.to_owned(),
                ph: "X".to_owned(),
                ts: us(event.start_ns),
                dur: Some(us(event.wall_ns)),
                pid: 1,
                tid: event.thread,
                args,
            });
        }

        for (name, amount) in &raw.loose {
            *counters.entry((*name).to_owned()).or_default() += amount;
        }

        for thread in threads.keys() {
            trace_events.push(TraceEvent {
                name: "thread_name".to_owned(),
                ph: "M".to_owned(),
                ts: 0.0,
                dur: None,
                pid: 1,
                tid: *thread,
                args: BTreeMap::from([("name".to_owned(), format!("thread {thread}").into())]),
            });
        }

        let mut marks = BTreeMap::new();

        for (name, at) in &raw.marks {
            marks.entry((*name).to_owned()).or_insert_with(|| ms(*at));
            trace_events.push(TraceEvent {
                name: (*name).to_owned(),
                ph: "i".to_owned(),
                ts: us(*at),
                dur: None,
                pid: 1,
                tid: 0,
                args: BTreeMap::from([("s".to_owned(), "g".into())]),
            });
        }

        trace_events.sort_by(|left, right| left.ts.total_cmp(&right.ts));
        let mut phases = phases.into_values().collect::<Vec<_>>();
        phases.sort_by(|left, right| {
            right
                .wall_ms
                .total_cmp(&left.wall_ms)
                .then_with(|| left.name.cmp(&right.name))
        });

        Self {
            schema: SCHEMA,
            summary: Summary {
                wall_ms: ms(raw.wall_ns),
                cpu_ms: ms(raw.cpu_ns),
                ready_ms: marks.get("ready").copied(),
                peak_rss_bytes: raw.peak_rss_bytes,
                allocations: counted.then_some(raw.allocations.allocated.blocks),
                allocated_bytes: counted.then_some(raw.allocations.allocated.bytes),
                peak_heap_growth_bytes: counted.then_some(raw.allocations.peak_bytes),
                instructions: match raw.instructions {
                    CounterStatus::Off => "off".to_owned(),
                    CounterStatus::Counted => "counted".to_owned(),
                    CounterStatus::Multiplexed => "multiplexed".to_owned(),
                    CounterStatus::Unavailable(reason) => format!("unavailable: {reason}"),
                },
            },
            phases,
            threads: threads.into_values().collect(),
            counters,
            marks,
            dropped_events: raw.dropped,
            trace_events,
        }
    }

    /// Reads a report `--timings` wrote.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let report: Self = serde_json::from_str(text).map_err(|error| error.to_string())?;
        if report.schema != SCHEMA {
            return Err(format!(
                "the report has schema {}, not {SCHEMA}",
                report.schema
            ));
        }

        Ok(report)
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// The report as text: its summary, then its phases, the longest first.
    #[must_use]
    pub fn text(&self, all: bool) -> String {
        let mut text = String::new();
        let summary = &self.summary;
        let _ = write!(
            text,
            "wall {:.1} ms, cpu {:.1} ms",
            summary.wall_ms, summary.cpu_ms
        );
        if let Some(ready) = summary.ready_ms {
            let _ = write!(text, ", ready at {ready:.1} ms");
        }

        if let Some(rss) = summary.peak_rss_bytes {
            let _ = write!(text, ", peak rss {}", bytes(rss));
        }

        text.push('\n');
        if let (Some(blocks), Some(allocated)) = (summary.allocations, summary.allocated_bytes) {
            let _ = writeln!(
                text,
                "allocations {blocks} ({}), peak heap growth {}",
                bytes(allocated),
                summary
                    .peak_heap_growth_bytes
                    .map_or_else(|| "?".to_owned(), signed_bytes)
            );
        } else {
            text.push_str("allocations not counted\n");
        }

        let _ = writeln!(text, "instructions {}", summary.instructions);
        if self.dropped_events > 0 {
            let _ = writeln!(text, "dropped {} spans", self.dropped_events);
        }

        let _ = writeln!(
            text,
            "\n{:<36} {:>6} {:>10} {:>10} {:>12} {:>9} {:>10}",
            "phase", "spans", "wall ms", "cpu ms", "instructions", "allocs", "bytes"
        );

        let shown = if all { usize::MAX } else { SUMMARY_ROWS };

        for phase in self.phases.iter().take(shown) {
            let _ = writeln!(
                text,
                "{:<36} {:>6} {:>10.2} {:>10.2} {:>12} {:>9} {:>10}",
                truncate(&phase.name, 36),
                phase.spans,
                phase.wall_ms,
                phase.cpu_ms,
                phase
                    .instructions
                    .map_or_else(|| "-".to_owned(), |count| count.to_string()),
                phase.allocations,
                bytes(phase.allocated_bytes),
            );
            for (name, amount) in &phase.counters {
                let _ = writeln!(text, "    {name} {amount}");
            }
        }

        if self.phases.len() > shown {
            let _ = writeln!(
                text,
                "... {} more phases (--all)",
                self.phases.len() - shown
            );
        }

        text
    }

    /// Each thread's busy time and longest span, and how parallel the
    /// recording was.
    #[must_use]
    pub fn threads_text(&self) -> String {
        let mut text = String::new();
        let busy = self
            .threads
            .iter()
            .map(|thread| thread.busy_ms)
            .sum::<f64>();

        let _ = writeln!(
            text,
            "{} threads, {busy:.1} ms busy over {:.1} ms wall: {:.2}x parallel",
            self.threads.len(),
            self.summary.wall_ms,
            if self.summary.wall_ms > 0.0 {
                busy / self.summary.wall_ms
            } else {
                0.0
            }
        );
        let _ = writeln!(
            text,
            "{:>6} {:>10} {:>10} {:>6}  longest",
            "thread", "busy ms", "cpu ms", "spans"
        );

        for thread in &self.threads {
            let _ = writeln!(
                text,
                "{:>6} {:>10.2} {:>10.2} {:>6}  {} ({:.2} ms)",
                thread.thread,
                thread.busy_ms,
                thread.cpu_ms,
                thread.spans,
                thread.longest.as_deref().unwrap_or("-"),
                thread.longest_ms
            );
        }

        // The spans that ran for another, grouped by the span they ran for:
        // the parent waits for the last of them, which bounds its time.
        let mut children = BTreeMap::<u64, Vec<&TraceEvent>>::new();
        let mut by_id = BTreeMap::<u64, &TraceEvent>::new();

        for event in self.trace_events.iter().filter(|event| event.ph == "X") {
            if let Some(id) = event.args.get("id").and_then(serde_json::Value::as_u64) {
                by_id.insert(id, event);
            }

            if let Some(parent) = event.args.get("parent").and_then(serde_json::Value::as_u64) {
                children.entry(parent).or_default().push(event);
            }
        }

        for (parent, events) in &children {
            let Some(parent) = by_id.get(parent) else {
                continue;
            };

            let work = events.iter().filter_map(|event| event.dur).sum::<f64>() / 1e3;
            let wall = parent.dur.unwrap_or(0.0) / 1e3;
            let last = events.iter().max_by(|left, right| {
                (left.ts + left.dur.unwrap_or(0.0))
                    .total_cmp(&(right.ts + right.dur.unwrap_or(0.0)))
            });
            let longest = events
                .iter()
                .max_by(|left, right| left.dur.unwrap_or(0.0).total_cmp(&right.dur.unwrap_or(0.0)));

            let _ = writeln!(
                text,
                "{}: {} parts, {work:.2} ms of work in {wall:.2} ms ({:.2}x); longest {} {:.2} ms, last to finish {}",
                parent.name,
                events.len(),
                if wall > 0.0 { work / wall } else { 0.0 },
                longest.map_or("-", |event| event_label(event)),
                longest.and_then(|event| event.dur).unwrap_or(0.0) / 1e3,
                last.map_or("-", |event| event_label(event)),
            );
        }

        text
    }

    /// What changed from `base` to this report: every summary measure and
    /// every phase whose wall time, instructions, or allocations moved.
    #[must_use]
    pub fn diff_text(&self, base: &Self, all: bool) -> String {
        let mut text = String::new();
        let mut row = |name: &str, before: f64, after: f64, unit: &str| {
            let _ = writeln!(text, "{}", change_row(name, before, after, unit));
        };

        row("wall ms", base.summary.wall_ms, self.summary.wall_ms, "");
        row("cpu ms", base.summary.cpu_ms, self.summary.cpu_ms, "");
        if let (Some(before), Some(after)) = (base.summary.ready_ms, self.summary.ready_ms) {
            row("ready ms", before, after, "");
        }

        #[expect(clippy::cast_precision_loss, reason = "reported approximately")]
        if let (Some(before), Some(after)) =
            (base.summary.peak_rss_bytes, self.summary.peak_rss_bytes)
        {
            row("peak rss MB", before as f64 / 1e6, after as f64 / 1e6, "");
        }

        #[expect(clippy::cast_precision_loss, reason = "reported approximately")]
        if let (Some(before), Some(after)) = (base.summary.allocations, self.summary.allocations) {
            row("allocations", before as f64, after as f64, "");
        }

        text.push('\n');
        let base_phases = base
            .phases
            .iter()
            .map(|phase| (phase.name.as_str(), phase))
            .collect::<BTreeMap<_, _>>();
        let mut names = self
            .phases
            .iter()
            .map(|phase| phase.name.as_str())
            .chain(base.phases.iter().map(|phase| phase.name.as_str()))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        let new_phases = self
            .phases
            .iter()
            .map(|phase| (phase.name.as_str(), phase))
            .collect::<BTreeMap<_, _>>();
        let mut rows = names
            .into_iter()
            .map(|name| {
                let before = base_phases.get(name);
                let after = new_phases.get(name);
                let wall = |phase: Option<&&Phase>| phase.map_or(0.0, |phase| phase.wall_ms);
                (name, wall(before), wall(after), before, after)
            })
            .collect::<Vec<_>>();

        rows.sort_by(|left, right| {
            (right.2 - right.1)
                .abs()
                .total_cmp(&(left.2 - left.1).abs())
                .then_with(|| left.0.cmp(right.0))
        });

        let _ = writeln!(
            text,
            "{:<36} {:>10} {:>10} {:>9}",
            "phase wall ms", "base", "new", "change"
        );

        let shown = if all { usize::MAX } else { SUMMARY_ROWS };

        for (name, before, after, base_phase, new_phase) in rows.iter().take(shown) {
            let _ = writeln!(text, "{}", change_row(name, *before, *after, ""));

            #[expect(clippy::cast_precision_loss, reason = "reported approximately")]
            if let (Some(base_phase), Some(new_phase)) = (base_phase, new_phase) {
                if base_phase.allocations != new_phase.allocations {
                    let _ = writeln!(
                        text,
                        "{}",
                        change_row(
                            "    allocations",
                            base_phase.allocations as f64,
                            new_phase.allocations as f64,
                            ""
                        )
                    );
                }

                if let (Some(before), Some(after)) =
                    (base_phase.instructions, new_phase.instructions)
                    && before != after
                {
                    let _ = writeln!(
                        text,
                        "{}",
                        change_row("    instructions", before as f64, after as f64, "")
                    );
                }
            }
        }

        text
    }
}

fn change_row(name: &str, before: f64, after: f64, unit: &str) -> String {
    let change = if before == 0.0 {
        if after == 0.0 {
            "=".to_owned()
        } else {
            "new".to_owned()
        }
    } else {
        format!("{:+.1}%", (after - before) / before * 100.0)
    };

    format!(
        "{:<36} {:>10.2} {:>10.2} {:>9}{unit}",
        truncate(name, 36),
        before,
        after,
        change
    )
}

fn label_of(event: &Event) -> String {
    event.label.as_deref().map_or_else(
        || event.name.to_owned(),
        |label| format!("{} {label}", event.name),
    )
}

fn event_label(event: &TraceEvent) -> &str {
    event
        .args
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&event.name)
}

fn truncate(text: &str, width: usize) -> &str {
    match text.char_indices().nth(width) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "sizes are reported approximately"
)]
fn bytes(count: u64) -> String {
    match count {
        0..1_000 => format!("{count} B"),
        1_000..1_000_000 => format!("{:.1} kB", count as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", count as f64 / 1e6),
        _ => format!("{:.2} GB", count as f64 / 1e9),
    }
}

fn signed_bytes(count: i64) -> String {
    if count < 0 {
        format!("-{}", bytes(count.unsigned_abs()))
    } else {
        bytes(count.unsigned_abs())
    }
}
