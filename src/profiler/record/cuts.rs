use std::collections::{BTreeMap, VecDeque};

use crate::backend::perf;
use crate::profiler::Cuts;

pub(super) struct Mark {
    pub(super) time: u64,
    pub(super) accounts: BTreeMap<u32, u64>,
}

pub(super) enum Cut {
    /// Everything is kept, and counts count as they are read.
    Whole,

    /// Only the span since the newest mark at least `length` before the
    Window { length: u64, observed: Observed },

    Segments {
        start: Option<Mark>,
        observed: Observed,
    },
}

#[derive(Default)]
pub(super) struct Observed {
    waiting: VecDeque<(u64, perf::Counts)>,
    marks: VecDeque<Mark>,
}

pub(super) enum End<'a> {
    Records,

    Unobserved,

    At(&'a Mark),
}

impl Cut {
    pub(super) fn new(cuts: Cuts) -> Self {
        match cuts {
            Cuts::Whole => Self::Whole,
            Cuts::Window(length) => Self::Window {
                length: u64::try_from(length.as_nanos()).unwrap_or(u64::MAX),
                observed: Observed::default(),
            },
            Cuts::Segments(_) => Self::Segments {
                start: None,
                observed: Observed::default(),
            },
        }
    }

    const fn observed_mut(&mut self) -> Option<&mut Observed> {
        match self {
            Self::Whole => None,
            Self::Window { observed, .. } | Self::Segments { observed, .. } => Some(observed),
        }
    }

    pub(super) fn observe(&mut self, time: u64, counts: perf::Counts) -> Option<perf::Counts> {
        let Some(observed) = self.observed_mut() else {
            return Some(counts);
        };

        debug_assert!(
            observed
                .waiting
                .back()
                .map(|(at, _)| at)
                .or_else(|| observed.marks.back().map(|mark| &mark.time))
                .is_none_or(|last| *last <= time),
            "counts read at {time} are older than the last read"
        );
        observed.waiting.push_back((time, counts));
        None
    }

    pub(super) fn ripe(&mut self, time: u64) -> Option<(u64, perf::Counts)> {
        self.observed_mut()?
            .waiting
            .pop_front_if(|(at, _)| *at <= time)
    }

    pub(super) fn mark(&mut self, time: u64, accounts: &BTreeMap<u32, u64>) -> Option<u64> {
        debug_assert!(
            !matches!(self, Self::Whole),
            "a whole recording counts what it reads at once"
        );
        let mark = Mark {
            time,
            accounts: accounts.clone(),
        };

        match self {
            Self::Whole => None,
            Self::Segments { observed, .. } => {
                observed.marks.clear();
                observed.marks.push_back(mark);
                None
            }
            Self::Window { length, observed } => {
                let marks = &mut observed.marks;
                marks.push_back(mark);
                let passed = time.saturating_sub(*length);
                while marks.get(1).is_some_and(|mark| mark.time <= passed) {
                    marks.pop_front();
                }

                marks
                    .front()
                    .map(|mark| mark.time)
                    .filter(|start| *start <= passed)
            }
        }
    }

    pub(super) fn observed(&self, at: u64) -> bool {
        match self.end() {
            End::Records => true,
            End::Unobserved => false,
            End::At(mark) => mark.time >= at,
        }
    }

    pub(super) fn end(&self) -> End<'_> {
        match self {
            Self::Whole => End::Records,
            Self::Window { observed, .. } | Self::Segments { observed, .. } => {
                observed.marks.back().map_or(End::Unobserved, End::At)
            }
        }
    }

    pub(super) fn start(&self) -> Option<&Mark> {
        match self {
            Self::Whole => None,
            Self::Segments { start, .. } => start.as_ref(),
            Self::Window { length, observed } => {
                let end = observed.marks.back()?.time;
                let passed = end.saturating_sub(*length);
                observed.marks.front().filter(|mark| mark.time <= passed)
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use std::time::Duration;

    use proptest::prelude::*;

    use crate::backend::perf;
    use crate::capture::{AccountKind, Capture, OFF_CPU_BEGAN_UNSEEN, OFF_CPU_ENDED_UNSEEN};
    use crate::debug_info::DebugFileSearch;
    use crate::profiler::Cuts;
    use crate::profiler::record::Processor;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Mode {
        Whole,
        Window,
        Segments,
    }

    const WINDOW: u64 = 30;

    const SAMPLED: u32 = 8;
    const SLEEPER: u32 = 9;

    struct Recording {
        mode: Mode,
        processor: Processor,
    }

    impl Recording {
        fn new(mode: Mode) -> Self {
            let mut processor = Processor::new(DebugFileSearch::default(), 64);
            processor.cuts(match mode {
                Mode::Whole => Cuts::Whole,
                Mode::Window => Cuts::Window(Duration::from_nanos(WINDOW)),
                Mode::Segments => Cuts::Segments(Duration::from_secs(1)),
            });

            Self { mode, processor }
        }

        fn observe(&mut self, time: u64) {
            let counts = perf::Counts {
                counted: time,
                ..perf::Counts::default()
            };
            self.processor.observe(time, counts);
        }

        fn observed(&self, at: u64) -> bool {
            self.processor.observed(at)
        }

        fn save(&mut self) -> Option<Capture> {
            self.processor.save()
        }

        fn end(mut self, time: u64) -> Capture {
            self.observe(time);
            if self.mode != Mode::Whole {
                self.processor.ingest(&[], None);
            }
            self.processor.finish()
        }

        fn ingest(&mut self, records: &[Vec<u8>], watermark: u64) {
            self.processor.ingest(&records.concat(), Some(watermark));
        }
    }

    fn record(kind: u32, misc: u16, body: &[u64]) -> Vec<u8> {
        let mut record = kind.to_le_bytes().to_vec();
        record.extend_from_slice(&misc.to_le_bytes());
        let size = u16::try_from(8 + body.len() * 8).expect("small");
        record.extend_from_slice(&size.to_le_bytes());
        for word in body {
            record.extend_from_slice(&word.to_le_bytes());
        }
        record
    }

    fn sample(time: u64) -> Vec<u8> {
        let thread = 7 | (u64::from(SAMPLED) << 32);
        record(9, 0, &[1, thread, time, 3, 1_000, 0, 0, 0])
    }

    fn switch(time: u64, out: bool) -> Vec<u8> {
        let thread = 7 | (u64::from(SLEEPER) << 32);
        record(14, if out { 1 << 13 } else { 0 }, &[thread, time, 1, 3])
    }

    const PHASES: [&str; 8] = [
        "whole",
        "window waiting",
        "window shorter than its length",
        "window rolling",
        "segments waiting",
        "segments before a cut",
        "segments after a cut",
        "segments cut at its last record",
    ];

    const EVENTS: [&str; 7] = [
        "snapshot",
        "save",
        "save asked",
        "records arrive",
        "late record",
        "end",
        "save then end",
    ];

    fn phase(name: &str) -> Recording {
        let mode = match name.split(' ').next() {
            Some("whole") => Mode::Whole,
            Some("window") => Mode::Window,
            _ => Mode::Segments,
        };
        let mut recording = Recording::new(mode);
        recording.observe(5);
        let early = [sample(10), sample(20), switch(22, true), sample(30)];

        match name {
            "whole" => recording.ingest(&early, 35),

            "window waiting" | "segments waiting" => recording.ingest(&early, 4),
            "window shorter than its length" | "segments before a cut" => {
                recording.observe(25);
                recording.ingest(&early, 35);
            }
            "window rolling" => {
                for time in [15, 25, 35, 45, 55] {
                    recording.observe(time);
                }
                recording.ingest(&early, 35);
                recording.ingest(&[sample(40), sample(50)], 60);
            }
            "segments after a cut" => {
                recording.observe(25);
                recording.ingest(&early, 35);
                recording.save().expect("a segment to 25");
                recording.observe(35);
                recording.ingest(&[sample(40)], 45);
            }
            "segments cut at its last record" => {
                recording.observe(25);
                recording.ingest(&early[..3], 30);
                recording.save().expect("a segment to 25");
            }
            _ => unreachable!("{name}"),
        }

        recording
    }

    fn describe(capture: &Capture) -> String {
        let (start, end) = capture.span();
        let samples: Vec<_> = capture
            .samples
            .iter()
            .map(|sample| sample.time.get())
            .collect();
        let off: Vec<_> = capture
            .off_cpu
            .iter()
            .map(|interval| {
                let began = if interval.flags & OFF_CPU_BEGAN_UNSEEN == 0 {
                    ""
                } else {
                    "<"
                };
                let ended = if interval.flags & OFF_CPU_ENDED_UNSEEN == 0 {
                    ""
                } else {
                    ">"
                };
                format!(
                    "{began}{}+{}{ended}",
                    interval.time.get(),
                    interval.duration.get()
                )
            })
            .collect();
        let late = capture.account(AccountKind::LateRecords as u32);
        let late = if late == 0 {
            String::new()
        } else {
            format!(" late {late}")
        };

        format!(
            "{start}..{end} counted {} samples {samples:?} off {off:?}{late}",
            capture.account(AccountKind::Counted as u32),
        )
    }

    fn saved(capture: Option<&Capture>) -> String {
        capture.map_or_else(|| "none".to_owned(), describe)
    }

    fn outcome(name: &str, event: &str) -> String {
        let mut recording = phase(name);
        let later = [sample(60), switch(62, false), sample(70)];
        let captures = match event {
            "snapshot" => describe(&recording.processor.snapshot()),
            "save" => saved(recording.save().as_ref()),
            "save asked" => {
                recording.observe(65);
                let before = recording.observed(65);
                recording.ingest(&later, 75);
                let after = recording.observed(65);
                format!(
                    "observed {before} then {after}: {}",
                    saved(recording.save().as_ref())
                )
            }
            "records arrive" => {
                recording.observe(65);
                recording.ingest(&later, 75);
                describe(&recording.processor.snapshot())
            }
            "late record" => {
                recording.observe(65);
                recording.ingest(&[sample(21)], 75);
                describe(&recording.processor.snapshot())
            }
            "end" => describe(&recording.end(80)),
            "save then end" => {
                let first = saved(recording.save().as_ref());
                format!("{first}, then {}", describe(&recording.end(80)))
            }
            _ => unreachable!("{event}"),
        };

        format!("{name} / {event}: {captures}")
    }

    #[derive(Clone, Debug)]
    enum Step {
        Record { after: u64, kind: u8 },

        Observe { after: u64 },

        Drain { lag: u64 },

        Save,

        Late { before: u64 },
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            4 => (0..20_u64, 0..4_u8).prop_map(|(after, kind)| Step::Record { after, kind }),
            2 => (1..20_u64).prop_map(|after| Step::Observe { after }),
            2 => (0..40_u64).prop_map(|lag| Step::Drain { lag }),
            1 => Just(Step::Save),
            1 => (1..30_u64).prop_map(|before| Step::Late { before }),
        ]
    }

    #[derive(Default)]
    struct Run {
        samples: Vec<u64>,
        observations: Vec<u64>,

        off: Vec<(u64, Option<u64>)>,
        saves: Vec<(u64, Capture)>,

        skipped: Vec<(u64, Option<i64>)>,

        late: Vec<u64>,
    }

    #[derive(Default)]
    struct Unprocessed {
        records: Vec<(u64, bool)>,
        latest: u64,
        watermark: u64,
    }

    impl Unprocessed {
        fn process(&mut self, watermark: Option<u64>, late: &mut Vec<u64>) {
            self.records.sort_unstable();
            let ready = self
                .records
                .partition_point(|(time, _)| watermark.is_none_or(|watermark| *time <= watermark));
            for (time, is_sample) in self.records.drain(..ready) {
                if time < self.latest.max(self.watermark) && is_sample {
                    late.push(time);
                }
                self.latest = self.latest.max(time);
            }
            self.watermark = self.watermark.max(watermark.unwrap_or(0));
        }
    }

    fn run(mode: Mode, steps: &[Step]) -> (Run, Capture) {
        let mut recording = Recording::new(mode);
        let mut run = Run::default();
        let (mut now, mut watermark) = (1, 0);
        let mut pending = Vec::new();
        let mut unprocessed = Unprocessed::default();
        recording.observe(now);
        run.observations.push(now);

        for step in steps {
            match *step {
                Step::Record { after, kind } => {
                    now += after;
                    let out = run.off.last().is_none_or(|(_, back)| back.is_some());
                    match kind {
                        0 | 1 => {
                            pending.push(sample(now));
                            run.samples.push(now);
                            unprocessed.records.push((now, true));
                        }
                        _ if out => {
                            pending.push(switch(now, true));
                            run.off.push((now, None));
                            unprocessed.records.push((now, false));
                        }
                        _ => {
                            pending.push(switch(now, false));
                            unprocessed.records.push((now, false));
                            if let Some((_, back)) = run.off.last_mut() {
                                *back = Some(now);
                            }
                        }
                    }
                }
                Step::Observe { after } => {
                    now += after;
                    recording.observe(now);
                    run.observations.push(now);
                }
                Step::Drain { lag } => {
                    watermark = now.saturating_sub(lag).max(watermark);
                    recording.ingest(&std::mem::take(&mut pending), watermark);
                    unprocessed.process(Some(watermark), &mut run.late);
                }
                Step::Late { before } => {
                    let time = now.saturating_sub(before).max(1);
                    pending.push(sample(time));
                    run.samples.push(time);
                    unprocessed.records.push((time, true));
                }
                Step::Save => {
                    if recording.observed(0) {
                        if let Some(capture) = recording.save() {
                            run.saves.push((watermark, capture));
                        } else {
                            let before = run.saves.last().map(|(_, capture)| capture.span().1);
                            run.skipped.push((watermark, before));
                        }
                    }
                }
            }
        }

        recording.ingest(&pending, watermark);
        unprocessed.process(Some(watermark), &mut run.late);
        unprocessed.process(None, &mut run.late);
        let end = now + 1;
        run.observations.push(end);
        (run, recording.end(end))
    }

    fn observed_by(run: &Run, time: u64) -> Option<u64> {
        run.observations
            .iter()
            .copied()
            .filter(|observed| *observed <= time)
            .max()
    }

    fn sample_times(capture: &Capture) -> Vec<u64> {
        capture
            .samples
            .iter()
            .map(|sample| u64::try_from(sample.time.get()).expect("positive"))
            .collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Segments tile the recording: each ends at the latest
        /// observation its records had all arrived by and the next begins
        /// there, and together they hold every sample once, count what
        /// the events counted, and hold the sleeper's time off its CPU.
        #[test]
        fn segments_tile_the_recording_and_add_up_to_it(
            steps in proptest::collection::vec(step(), 0..60),
        ) {
            let (run, last) = run(Mode::Segments, &steps);
            let captures: Vec<&Capture> = run
                .saves
                .iter()
                .map(|(_, capture)| capture)
                .chain(std::iter::once(&last))
                .collect();

            for ((watermark, capture), next) in run.saves.iter().zip(&captures[1..]) {
                let end = observed_by(&run, *watermark).expect("observed");
                prop_assert_eq!(capture.span().1, i64::try_from(end).expect("small"));
                prop_assert_eq!(capture.span().1, next.span().0);
            }



            for (watermark, capture) in &run.saves {
                let (from, to) = capture.span();
                prop_assert!(from < to, "segment {:?} saved by {}", (from, to), watermark);
            }
            for (watermark, before) in &run.skipped {
                let end = observed_by(&run, *watermark).expect("observed");
                if let Some(before) = before {
                    prop_assert!(i64::try_from(end).expect("small") <= *before, "{} after {}", end, before);
                }
            }




            let mut on_time = run.samples.clone();
            on_time.sort_unstable();
            for time in &run.late {
                let at = on_time.binary_search(time).expect("a late sample was recorded");
                on_time.remove(at);
            }
            let mut kept: Vec<u64> = Vec::new();
            for capture in &captures {
                let (from, to) = capture.span();
                for time in sample_times(capture) {
                    let at = i64::try_from(time).expect("small");
                    prop_assert!(
                        from <= at && at <= to,
                        "{} outside {:?}",
                        time,
                        (from, to)
                    );
                    kept.push(time);
                }
            }
            kept.sort_unstable();
            let mut missing = on_time.clone();
            for time in &kept {
                if let Ok(at) = missing.binary_search(time) {
                    missing.remove(at);
                } else {
                    prop_assert!(run.late.contains(time), "{} kept twice", time);
                }
            }
            prop_assert!(missing.is_empty(), "lost {:?}", missing);
            prop_assert!(kept.len() <= run.samples.len());
            let late: u64 = captures
                .iter()
                .map(|capture| capture.account(AccountKind::LateRecords as u32))
                .sum();
            prop_assert_eq!(late, run.late.len() as u64);

            let end = *run.observations.last().expect("an end");
            let counted: u64 = captures
                .iter()
                .map(|capture| capture.account(AccountKind::Counted as u32))
                .sum();
            prop_assert_eq!(counted, end);

            let off: u64 = captures
                .iter()
                .flat_map(|capture| &capture.off_cpu)
                .map(|interval| interval.duration.get())
                .sum();
            let expected: u64 = run.off.iter().map(|(left, back)| back.unwrap_or(end) - left).sum();
            prop_assert_eq!(off, expected);
        }

        /// A window ends at the latest observation its records had all
        /// arrived by, starts at the latest observation at least its
        /// length before that, and holds the samples and counts between.
        #[test]
        fn a_window_holds_its_last_span_from_an_observation(
            steps in proptest::collection::vec(step(), 0..60),
        ) {
            let (run, last) = run(Mode::Window, &steps);
            let end = *run.observations.last().expect("an end");
            let saves = run.saves.iter().map(|(watermark, capture)| (*watermark, capture));

            for (watermark, capture) in saves.chain(std::iter::once((end, &last))) {
                let end = observed_by(&run, watermark).expect("observed");
                let start = observed_by(&run, end.saturating_sub(WINDOW));
                let (from, to) = capture.span();
                prop_assert_eq!(to, i64::try_from(end).expect("small"));
                if let Some(start) = start {
                    prop_assert_eq!(from, i64::try_from(start).expect("small"));
                }

                let start = start.unwrap_or(0);
                let within = |time: &u64| start <= *time && *time < end;
                let mut on_time: Vec<u64> = run.samples.iter().copied().filter(within).collect();
                for time in run.late.iter().filter(|time| within(time)) {
                    let at = on_time.iter().position(|kept| kept == time).expect("recorded");
                    on_time.remove(at);
                }

                let mut kept = sample_times(capture);
                for time in &on_time {
                    let at = kept.iter().position(|kept| kept == time);
                    prop_assert!(at.is_some(), "lost {} from {:?}", time, (start, end));
                    kept.remove(at.expect("checked"));
                }
                for time in &kept {
                    prop_assert!(within(time) && run.late.contains(time), "kept {}", time);
                }
                prop_assert_eq!(capture.account(AccountKind::Counted as u32), end - start);


                let off: u64 = capture.off_cpu.iter().map(|interval| interval.duration.get()).sum();
                let expected: u64 = run
                    .off
                    .iter()
                    .map(|(left, back)| {
                        let back = back.unwrap_or(u64::MAX).min(end);
                        back.saturating_sub((*left).max(start))
                    })
                    .sum();
                prop_assert_eq!(off, expected);
            }
        }
    }

    #[test]
    fn each_cut_mode_keeps_the_same_span_counts_and_records_after_each_event() {
        let actual: Vec<String> = PHASES
            .iter()
            .flat_map(|name| EVENTS.iter().map(move |event| outcome(name, event)))
            .collect();

        let expected = [
            "whole / snapshot: 10..30 counted 5 samples [10, 20, 30] off [\"22+8>\"]",
            "whole / save: 10..30 counted 5 samples [10, 20, 30] off [\"22+8>\"]",
            "whole / save asked: observed true then true: 10..70 counted 65 samples [10, 20, 30, 60, 70] off [\"22+40\"]",
            "whole / records arrive: 10..70 counted 65 samples [10, 20, 30, 60, 70] off [\"22+40\"]",
            "whole / late record: 10..30 counted 65 samples [10, 20, 21, 30] off [\"22+8>\"] late 1",
            "whole / end: 10..30 counted 80 samples [10, 20, 30] off [\"22+8>\"]",
            "whole / save then end: 10..30 counted 5 samples [10, 20, 30] off [\"22+8>\"], then 10..30 counted 80 samples [10, 20, 30] off [\"22+8>\"]",
            "window waiting / snapshot: 0..0 counted 0 samples [] off []",
            "window waiting / save: 0..0 counted 0 samples [] off []",
            "window waiting / save asked: observed false then true: 5..65 counted 60 samples [10, 20, 30, 60] off [\"22+40\"]",
            "window waiting / records arrive: 5..65 counted 60 samples [10, 20, 30, 60] off [\"22+40\"]",
            "window waiting / late record: 5..65 counted 60 samples [10, 20, 21, 30] off [\"22+43>\"]",
            "window waiting / end: 5..80 counted 75 samples [10, 20, 30] off [\"22+58>\"]",
            "window waiting / save then end: 0..0 counted 0 samples [] off [], then 5..80 counted 75 samples [10, 20, 30] off [\"22+58>\"]",
            "window shorter than its length / snapshot: 10..25 counted 25 samples [10, 20] off [\"22+3>\"]",
            "window shorter than its length / save: 10..25 counted 25 samples [10, 20] off [\"22+3>\"]",
            "window shorter than its length / save asked: observed false then true: 25..65 counted 40 samples [30, 60] off [\"<25+37\"]",
            "window shorter than its length / records arrive: 25..65 counted 40 samples [30, 60] off [\"<25+37\"]",
            "window shorter than its length / late record: 25..65 counted 40 samples [30] off [\"<25+40>\"] late 1",
            "window shorter than its length / end: 25..80 counted 55 samples [30] off [\"<25+55>\"]",
            "window shorter than its length / save then end: 10..25 counted 25 samples [10, 20] off [\"22+3>\"], then 25..80 counted 55 samples [30] off [\"<25+55>\"]",
            "window rolling / snapshot: 25..55 counted 30 samples [30, 40, 50] off [\"<25+30>\"]",
            "window rolling / save: 25..55 counted 30 samples [30, 40, 50] off [\"<25+30>\"]",
            "window rolling / save asked: observed false then true: 35..65 counted 30 samples [40, 50, 60] off [\"<35+27\"]",
            "window rolling / records arrive: 35..65 counted 30 samples [40, 50, 60] off [\"<35+27\"]",
            "window rolling / late record: 35..65 counted 30 samples [40, 50] off [\"<35+30>\"] late 1",
            "window rolling / end: 45..80 counted 35 samples [50] off [\"<45+35>\"]",
            "window rolling / save then end: 25..55 counted 30 samples [30, 40, 50] off [\"<25+30>\"], then 45..80 counted 35 samples [50] off [\"<45+35>\"]",
            "segments waiting / snapshot: 0..0 counted 0 samples [] off []",
            "segments waiting / save: none",
            "segments waiting / save asked: observed false then true: 10..65 counted 65 samples [10, 20, 30, 60] off [\"22+40\"]",
            "segments waiting / records arrive: 10..65 counted 65 samples [10, 20, 30, 60] off [\"22+40\"]",
            "segments waiting / late record: 10..65 counted 65 samples [10, 20, 21, 30] off [\"22+43>\"]",
            "segments waiting / end: 10..80 counted 80 samples [10, 20, 30] off [\"22+58>\"]",
            "segments waiting / save then end: none, then 10..80 counted 80 samples [10, 20, 30] off [\"22+58>\"]",
            "segments before a cut / snapshot: 10..25 counted 25 samples [10, 20] off [\"22+3>\"]",
            "segments before a cut / save: 10..25 counted 25 samples [10, 20] off [\"22+3>\"]",
            "segments before a cut / save asked: observed false then true: 10..65 counted 65 samples [10, 20, 30, 60] off [\"22+40\"]",
            "segments before a cut / records arrive: 10..65 counted 65 samples [10, 20, 30, 60] off [\"22+40\"]",
            "segments before a cut / late record: 10..65 counted 65 samples [10, 20, 21, 30] off [\"22+43>\"] late 1",
            "segments before a cut / end: 10..80 counted 80 samples [10, 20, 30] off [\"22+58>\"]",
            "segments before a cut / save then end: 10..25 counted 25 samples [10, 20] off [\"22+3>\"], then 25..80 counted 55 samples [30] off [\"<25+55>\"]",
            "segments after a cut / snapshot: 25..35 counted 10 samples [30] off [\"<25+10>\"]",
            "segments after a cut / save: 25..35 counted 10 samples [30] off [\"<25+10>\"]",
            "segments after a cut / save asked: observed false then true: 25..65 counted 40 samples [30, 40, 60] off [\"<25+37\"]",
            "segments after a cut / records arrive: 25..65 counted 40 samples [30, 40, 60] off [\"<25+37\"]",
            "segments after a cut / late record: 25..65 counted 40 samples [30, 40] off [\"<25+40>\"] late 1",
            "segments after a cut / end: 25..80 counted 55 samples [30, 40] off [\"<25+55>\"]",
            "segments after a cut / save then end: 25..35 counted 10 samples [30] off [\"<25+10>\"], then 35..80 counted 45 samples [40] off [\"<35+45>\"]",
            "segments cut at its last record / snapshot: 25..25 counted 0 samples [] off []",
            "segments cut at its last record / save: none",
            "segments cut at its last record / save asked: observed false then true: 25..65 counted 40 samples [60] off [\"<25+37\"]",
            "segments cut at its last record / records arrive: 25..65 counted 40 samples [60] off [\"<25+37\"]",
            "segments cut at its last record / late record: 25..65 counted 40 samples [] off [\"<25+40>\"] late 1",
            "segments cut at its last record / end: 25..80 counted 55 samples [] off [\"<25+55>\"]",
            "segments cut at its last record / save then end: none, then 25..80 counted 55 samples [] off [\"<25+55>\"]",
        ];
        assert_eq!(actual, expected, "\n{}", actual.join("\n"));
    }
}
