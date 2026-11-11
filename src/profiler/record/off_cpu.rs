//! Each thread's time off its CPU, from its context switches.
//!
//! A thread is on its CPU or off it since it left. Its switches come from
//! the sideband, which stays on through every gap in sampling, so an
//! interval ends where the thread came back. A return the kernel dropped
//! is no interval, as when the thread came back is unknown: it is counted
//! instead.

use std::collections::BTreeMap;

use crate::capture::builder::CaptureBuilder;
use crate::capture::records::{AccountKind, OFF_CPU_ENDED_UNSEEN};
use crate::model::Tid;

/// Each thread off its CPU: its row, when it left, and whether it was
/// preempted. A thread not here is on its CPU, or not yet seen to leave.
#[derive(Debug, Default)]
pub(super) struct OffCpu(BTreeMap<Tid, (u32, u64, bool)>);

impl OffCpu {
    /// Thread `tid`, of row `row`, left its CPU at `time`. Had it left
    /// before with no return seen, that return was lost.
    pub(super) fn left(
        &mut self,
        builder: &mut CaptureBuilder,
        tid: Tid,
        (row, time, preempted): (u32, u64, bool),
    ) {
        if self.0.insert(tid, (row, time, preempted)).is_some() {
            builder.count(AccountKind::OffCpuLost as u32, 1);
        }
    }

    /// Thread `tid` came back to its CPU at `time`, ending its interval.
    /// One that comes back unseen leaving left before the recording could
    /// see it, so when is unknown.
    pub(super) fn back(&mut self, builder: &mut CaptureBuilder, tid: Tid, time: u64) {
        if let Some((row, left, preempted)) = self.0.remove(&tid) {
            builder.off_cpu(
                row,
                left.cast_signed(),
                time.saturating_sub(left),
                preempted,
                0,
            );
        }
    }

    /// Thread `tid` exited. It came back to its CPU to exit, so if it is
    /// still off it, its return was lost.
    pub(super) fn exited(&mut self, builder: &mut CaptureBuilder, tid: Tid) {
        if self.0.remove(&tid).is_some() {
            builder.count(AccountKind::OffCpuLost as u32, 1);
        }
    }

    /// Ends each interval of a thread still off its CPU where the capture
    /// does, at `end`, unseen.
    pub(super) fn close(&self, builder: &mut CaptureBuilder, end: u64) {
        for &(row, left, preempted) in self.0.values() {
            builder.off_cpu(
                row,
                left.cast_signed(),
                end.saturating_sub(left),
                preempted,
                OFF_CPU_ENDED_UNSEEN,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::backend::perf::decode::Record;
    use crate::capture::records::GapReason;
    use crate::model::{Pid, Tid};
    use crate::profiler::Profile;
    use crate::profiler::record::Processor;

    const PID: Pid = Pid::new(100);
    const THREAD: Tid = Tid::new(101);
    const OTHER: Tid = Tid::new(102);

    fn switch(tid: Tid, out: bool, preempted: bool, time: u64) -> Record<'static> {
        Record::Switch {
            pid: PID,
            tid,
            out,
            preempted,
            time,
        }
    }

    const PHASES: [&str; 5] = [
        "running",
        "came back",
        "blocked",
        "preempted",
        "blocked across a gap",
    ];

    const EVENTS: [&str; 8] = [
        "back",
        "out again",
        "thread exits",
        "process exits",
        "gap then back",
        "back in a later gap",
        "snapshot then back",
        "end",
    ];

    /// A recording whose thread is in phase `name` at 6,000.
    fn phase(name: &str) -> Processor {
        let mut processor = Processor::new(crate::debug_info::DebugFileSearch::default(), 64);
        for tid in [PID.leader(), THREAD, OTHER] {
            processor.process(Record::Fork {
                pid: PID,
                ppid: Pid::new(1),
                tid,
                ptid: PID.leader(),
                time: 1_000,
            });
        }

        match name {
            "running" => {}
            "came back" => {
                processor.process(switch(THREAD, true, false, 2_000));
                processor.process(switch(THREAD, false, false, 3_000));
            }
            "blocked" => processor.process(switch(THREAD, true, false, 4_000)),
            "preempted" => processor.process(switch(THREAD, true, true, 4_000)),
            "blocked across a gap" => {
                processor.process(switch(THREAD, true, false, 4_000));
                processor.gap(5_000, 6_000, GapReason::Behind);
            }
            _ => unreachable!("no phase {name}"),
        }

        processor
    }

    /// The thread's intervals in `profile`, as `start+duration`, with a `p`
    /// for each it was preempted from and `>` for each the capture's end
    /// cut, and how many lost their return.
    fn intervals(profile: &Profile) -> String {
        let mut said: Vec<String> = profile
            .off_cpu()
            .iter()
            .filter(|interval| interval.tid == THREAD.get())
            .map(|interval| {
                let preempted = if interval.preempted { " p" } else { "" };
                let began = if interval.began_unseen { "<" } else { "" };
                let ended = if interval.ended_unseen { ">" } else { "" };
                format!(
                    "{began}{}+{}{ended}{preempted}",
                    interval.time, interval.duration
                )
            })
            .collect();
        if profile.off_cpu_lost() > 0 {
            said.push(format!("lost {}", profile.off_cpu_lost()));
        }

        said.join(", ")
    }

    /// One event from 7,000 in one phase: the thread's intervals in a
    /// snapshot taken during it, if any, and in the finished recording,
    /// which a record of another thread at 9,000 ends.
    fn outcome(name: &str, event: &str) -> String {
        let mut processor = phase(name);
        let mut snapshot = None;
        match event {
            "back" => processor.process(switch(THREAD, false, false, 7_000)),
            "out again" => processor.process(switch(THREAD, true, true, 7_000)),
            "thread exits" => processor.process(Record::Exit {
                pid: PID,
                tid: THREAD,
                time: 7_000,
            }),
            "process exits" => {
                processor.process(Record::Exit {
                    pid: PID,
                    tid: PID.leader(),
                    time: 7_000,
                });
                processor.exited(PID);
            }
            "gap then back" => {
                processor.gap(7_000, 8_000, GapReason::TaskSnapshot);
                processor.process(switch(THREAD, false, false, 8_500));
            }
            // A budget pause or a reader behind is recorded once it ends.
            "back in a later gap" => {
                processor.process(switch(THREAD, false, false, 8_000));
                processor.gap(7_000, 8_500, GapReason::Budget);
            }
            "snapshot then back" => {
                processor.process(switch(OTHER, false, false, 7_000));
                snapshot = Some(Profile {
                    capture: processor.snapshot(),
                });
                processor.process(switch(THREAD, false, false, 8_000));
            }
            "end" => {}
            _ => unreachable!("no event {event}"),
        }
        processor.process(switch(OTHER, false, false, 9_000));

        let finished = Profile {
            capture: processor.finish(),
        };
        let snapshot = snapshot.map_or_else(String::new, |snapshot| {
            format!("snapshot: {} | ", intervals(&snapshot))
        });
        format!("{name} / {event}: {snapshot}end: {}", intervals(&finished))
    }

    /// Every event, in each phase of a thread's time off its CPU, ends its
    /// intervals as it always has. Only sampling stops in a gap, so an
    /// interval goes on through one; a return the kernel dropped makes no
    /// interval, and is counted.
    #[test]
    fn each_event_in_each_phase_ends_a_threads_time_off_its_cpu_the_same() {
        let actual: Vec<String> = PHASES
            .iter()
            .flat_map(|name| EVENTS.iter().map(|event| outcome(name, event)))
            .collect();
        let rows = actual.join("\n");
        assert_eq!(
            EXPECTED,
            actual.as_slice(),
            "the off-CPU table changed; its rows are now:\n{rows}"
        );
    }

    const EXPECTED: &[&str] = &[
        "running / back: end: ",
        "running / out again: end: 7000+2000> p",
        "running / thread exits: end: ",
        "running / process exits: end: ",
        "running / gap then back: end: ",
        "running / back in a later gap: end: ",
        "running / snapshot then back: snapshot:  | end: ",
        "running / end: end: ",
        "came back / back: end: 2000+1000",
        "came back / out again: end: 2000+1000, 7000+2000> p",
        "came back / thread exits: end: 2000+1000",
        "came back / process exits: end: 2000+1000",
        "came back / gap then back: end: 2000+1000",
        "came back / back in a later gap: end: 2000+1000",
        "came back / snapshot then back: snapshot: 2000+1000 | end: 2000+1000",
        "came back / end: end: 2000+1000",
        "blocked / back: end: 4000+3000",
        "blocked / out again: end: 7000+2000> p, lost 1",
        "blocked / thread exits: end: lost 1",
        "blocked / process exits: end: lost 1",
        "blocked / gap then back: end: 4000+4500",
        "blocked / back in a later gap: end: 4000+4000",
        "blocked / snapshot then back: snapshot: 4000+3000> | end: 4000+4000",
        "blocked / end: end: 4000+5000>",
        "preempted / back: end: 4000+3000 p",
        "preempted / out again: end: 7000+2000> p, lost 1",
        "preempted / thread exits: end: lost 1",
        "preempted / process exits: end: lost 1",
        "preempted / gap then back: end: 4000+4500 p",
        "preempted / back in a later gap: end: 4000+4000 p",
        "preempted / snapshot then back: snapshot: 4000+3000> p | end: 4000+4000 p",
        "preempted / end: end: 4000+5000> p",
        "blocked across a gap / back: end: 4000+3000",
        "blocked across a gap / out again: end: 7000+2000> p, lost 1",
        "blocked across a gap / thread exits: end: lost 1",
        "blocked across a gap / process exits: end: lost 1",
        "blocked across a gap / gap then back: end: 4000+4500",
        "blocked across a gap / back in a later gap: end: 4000+4000",
        "blocked across a gap / snapshot then back: snapshot: 4000+3000> | end: 4000+4000",
        "blocked across a gap / end: end: 4000+5000>",
    ];
}
