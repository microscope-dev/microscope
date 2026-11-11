//! Each thread's time off its CPU, from its context switches.

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

    const PHASES: [&str; 6] = [
        "running",
        "came back",
        "blocked",
        "preempted",
        "blocked in a pause",
        "blocked across a gap",
    ];

    const EVENTS: [&str; 8] = [
        "back",
        "out again",
        "thread exits",
        "process exits",
        "gap then back",
        "pause then back",
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
            "blocked in a pause" => {
                processor.process(switch(THREAD, true, false, 4_000));
                processor.pause(5_000);
            }
            "blocked across a gap" => {
                processor.process(switch(THREAD, true, false, 4_000));
                processor.gap(5_000, 6_000, GapReason::Behind);
            }
            _ => unreachable!("no phase {name}"),
        }

        processor
    }

    /// The thread's intervals in `profile`, as `start+duration`, with a `p`
    /// for each it was preempted from.
    fn intervals(profile: &Profile) -> String {
        profile
            .off_cpu()
            .iter()
            .filter(|interval| interval.tid == THREAD.get())
            .map(|interval| {
                let preempted = if interval.preempted { " p" } else { "" };
                format!("{}+{}{preempted}", interval.time, interval.duration)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// One event from 7,000 in one phase: the thread's intervals in a
    /// snapshot taken during it, if any, and in the finished recording,
    /// which a record of another thread at 9,000 ends.
    fn outcome(name: &str, event: &str) -> Option<String> {
        if event == "pause then back" && name == "blocked in a pause" {
            return None;
        }

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
            "pause then back" => {
                processor.pause(7_000);
                processor.process(switch(THREAD, false, false, 8_000));
                processor.resume(8_500);
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
        Some(format!(
            "{name} / {event}: {snapshot}end: {}",
            intervals(&finished)
        ))
    }

    /// Every event, in each phase of a thread's time off its CPU, ends its
    /// intervals as it always has.
    #[test]
    fn each_event_in_each_phase_ends_a_threads_time_off_its_cpu_the_same() {
        let actual: Vec<String> = PHASES
            .iter()
            .flat_map(|name| EVENTS.iter().filter_map(|event| outcome(name, event)))
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
        "running / out again: end: 7000+2000 p",
        "running / thread exits: end: ",
        "running / process exits: end: ",
        "running / gap then back: end: ",
        "running / pause then back: end: ",
        "running / snapshot then back: snapshot:  | end: ",
        "running / end: end: ",
        "came back / back: end: 2000+1000",
        "came back / out again: end: 2000+1000, 7000+2000 p",
        "came back / thread exits: end: 2000+1000",
        "came back / process exits: end: 2000+1000",
        "came back / gap then back: end: 2000+1000",
        "came back / pause then back: end: 2000+1000",
        "came back / snapshot then back: snapshot: 2000+1000 | end: 2000+1000",
        "came back / end: end: 2000+1000",
        "blocked / back: end: 4000+3000",
        "blocked / out again: end: 7000+2000 p",
        "blocked / thread exits: end: ",
        "blocked / process exits: end: ",
        "blocked / gap then back: end: 4000+3000",
        "blocked / pause then back: end: 4000+3000",
        "blocked / snapshot then back: snapshot: 4000+3000 | end: 4000+4000",
        "blocked / end: end: 4000+5000",
        "preempted / back: end: 4000+3000 p",
        "preempted / out again: end: 7000+2000 p",
        "preempted / thread exits: end: ",
        "preempted / process exits: end: ",
        "preempted / gap then back: end: 4000+3000 p",
        "preempted / pause then back: end: 4000+3000 p",
        "preempted / snapshot then back: snapshot: 4000+3000 p | end: 4000+4000 p",
        "preempted / end: end: 4000+5000 p",
        "blocked in a pause / back: end: 4000+1000",
        "blocked in a pause / out again: end: 7000+2000 p",
        "blocked in a pause / thread exits: end: ",
        "blocked in a pause / process exits: end: ",
        "blocked in a pause / gap then back: end: 4000+1000",
        "blocked in a pause / snapshot then back: snapshot: 4000+1000 | end: 4000+1000",
        "blocked in a pause / end: end: 4000+1000",
        "blocked across a gap / back: end: 4000+1000",
        "blocked across a gap / out again: end: 7000+2000 p",
        "blocked across a gap / thread exits: end: ",
        "blocked across a gap / process exits: end: ",
        "blocked across a gap / gap then back: end: 4000+1000",
        "blocked across a gap / pause then back: end: 4000+1000",
        "blocked across a gap / snapshot then back: snapshot: 4000+1000 | end: 4000+1000",
        "blocked across a gap / end: end: 4000+1000",
    ];
}
