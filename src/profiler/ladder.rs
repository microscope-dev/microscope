use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    Stacks,

    Strings,

    Sampled,

    ExceptionsOnly,

    Samples,
}

pub const HOLD: Duration = Duration::from_secs(5);

pub const SAMPLED_EVERY: u32 = 16;

impl Rung {
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Stacks => "system call stacks",
            Self::Strings => "system call strings past 16 bytes",
            Self::Sampled => "15 of every 16 system calls, which are counted",
            Self::ExceptionsOnly => "every system call that neither failed nor was slow",
            Self::Samples => "samples",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    Down(Rung),
    Up(Rung),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Period {
    pub over: bool,

    pub used: f64,

    pub calls: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ladder {
    rungs: Vec<Rung>,
    shed: usize,

    limits: Vec<f64>,

    calm_since: Option<u64>,
}

impl Ladder {
    #[must_use]
    pub fn new(rungs: impl IntoIterator<Item = Rung>) -> Self {
        let mut rungs: Vec<Rung> = rungs.into_iter().collect();
        rungs.sort_unstable();
        rungs.dedup();
        Self {
            rungs,
            shed: 0,
            limits: Vec::new(),
            calm_since: None,
        }
    }

    #[must_use]
    pub fn shed(&self) -> &[Rung] {
        self.rungs.get(..self.shed).unwrap_or_default()
    }

    #[must_use]
    pub fn sheds(&self, rung: Rung) -> bool {
        self.shed().contains(&rung)
    }

    pub fn judge(&mut self, period: Period, now: u64) -> Option<Move> {
        if period.over {
            self.calm_since = None;
            let rung = *self.rungs.get(self.shed)?;
            self.shed += 1;
            self.limits.push(if rung == Rung::Samples {
                f64::INFINITY
            } else {
                period.calls / period.used.max(1.0)
            });
            return Some(Move::Down(rung));
        }

        let since = *self.calm_since.get_or_insert(now);
        let held = now.saturating_sub(since) >= u64::try_from(HOLD.as_nanos()).unwrap_or(u64::MAX);
        let few = self
            .limits
            .last()
            .is_some_and(|&limit| period.calls <= limit);
        if !held || !few {
            return None;
        }

        self.shed -= 1;
        self.limits.pop();
        self.calm_since = Some(now);
        self.rungs.get(self.shed).map(|&rung| Move::Up(rung))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const ALL: [Rung; 5] = [
        Rung::Stacks,
        Rung::Strings,
        Rung::Sampled,
        Rung::ExceptionsOnly,
        Rung::Samples,
    ];

    const QUIET: Period = Period {
        over: false,
        used: 0.1,
        calls: 0.0,
    };

    const fn over(used: f64, calls: f64) -> Period {
        Period {
            over: true,
            used,
            calls,
        }
    }

    const fn under(calls: f64) -> Period {
        Period {
            over: false,
            used: 0.5,
            calls,
        }
    }

    fn rungs() -> impl Strategy<Value = Vec<Rung>> {
        prop::sample::subsequence(ALL.to_vec(), 0..=ALL.len()).prop_shuffle()
    }

    proptest! {
        /// Whatever the periods' costs, each moves at most one rung,
        /// down only when over budget, up only after a hold under it with
        /// calls few enough to fit.
        #[test]
        fn the_ladder_moves_one_rung_at_a_time(
            rungs in rungs(),
            periods in prop::collection::vec(
                (any::<bool>(), 0.0..50.0_f64, 0.0..1e6_f64, 1_u64..3_000_000_000),
                0..60,
            ),
        ) {
            let mut ladder = Ladder::new(rungs.clone());
            let hold = u64::try_from(HOLD.as_nanos()).expect("a short hold");
            let mut now = 0;
            let mut calm: Option<u64> = None;
            let mut shed_at: Vec<(f64, f64)> = Vec::new();
            for (over, used, calls, step) in periods {
                now += step;
                let before = ladder.shed().len();
                let moved = ladder.judge(Period { over, used, calls }, now);
                let after = ladder.shed().len();
                let calm_from = if over { None } else { Some(*calm.get_or_insert(now)) };
                match moved {
                    Some(Move::Down(rung)) => {
                        prop_assert!(over);
                        prop_assert_eq!(after, before + 1);
                        prop_assert_eq!(ladder.shed().last(), Some(&rung));
                        shed_at.push(if rung == Rung::Samples { (0.0, 1.0) } else { (calls, used) });
                    }
                    Some(Move::Up(rung)) => {
                        prop_assert!(!over);
                        prop_assert_eq!(after + 1, before);
                        prop_assert!(!ladder.sheds(rung));
                        prop_assert!(calm_from.is_some_and(|from| now - from >= hold));
                        let (then, cost) = shed_at.pop().expect("a rung given up");
                        prop_assert!(rung == Rung::Samples || calls * cost.max(1.0) <= then * (1.0 + 1e-9));
                        calm = Some(now);
                    }
                    None => prop_assert_eq!(after, before),
                }
                if over {
                    calm = None;
                }

                let mut sorted = rungs.clone();
                sorted.sort_unstable();
                sorted.dedup();
                prop_assert_eq!(ladder.shed(), &sorted[..after]);
            }
        }

        /// Under a quiet load the ladder returns to the top, a rung a
        /// hold.
        #[test]
        fn a_quiet_load_returns_to_the_top(
            rungs in rungs(),
            overs in prop::collection::vec((1.0..300.0_f64, 0.0..1e6_f64), 0..8),
        ) {
            let mut ladder = Ladder::new(rungs);
            let mut now = 0;
            for (used, calls) in overs {
                now += 1_000_000_000;
                ladder.judge(Period { over: true, used, calls }, now);
            }

            let shed = ladder.shed().len() as u64;
            let hold = u64::try_from(HOLD.as_nanos()).expect("a short hold");
            let mut periods = 0;
            while !ladder.shed().is_empty() {
                now += 1_000_000_000;
                periods += 1;
                ladder.judge(QUIET, now);
                prop_assert!(periods <= (shed + 1) * hold / 1_000_000_000);
            }
        }
    }

    #[test]
    fn rungs_are_given_up_in_order_and_come_back_in_reverse() {
        let mut ladder = Ladder::new([Rung::Samples, Rung::Stacks, Rung::ExceptionsOnly]);
        let second = 1_000_000_000;
        assert_eq!(
            ladder.judge(over(2.0, 0.0), second),
            Some(Move::Down(Rung::Stacks))
        );
        assert_eq!(
            ladder.judge(over(2.0, 0.0), 2 * second),
            Some(Move::Down(Rung::ExceptionsOnly))
        );
        assert_eq!(
            ladder.judge(over(2.0, 0.0), 3 * second),
            Some(Move::Down(Rung::Samples))
        );
        assert_eq!(ladder.judge(over(2.0, 0.0), 4 * second), None);

        assert_eq!(ladder.judge(QUIET, 5 * second), None);
        assert_eq!(ladder.judge(QUIET, 9 * second), None);
        assert_eq!(
            ladder.judge(QUIET, 10 * second),
            Some(Move::Up(Rung::Samples))
        );
        assert_eq!(ladder.judge(QUIET, 11 * second), None);
        assert_eq!(
            ladder.judge(over(2.0, 0.0), 12 * second),
            Some(Move::Down(Rung::Samples))
        );
    }

    #[test]
    fn call_detail_comes_back_once_the_calls_would_fit() {
        let mut ladder = Ladder::new([Rung::ExceptionsOnly, Rung::Samples]);
        let second = 1_000_000_000;
        assert_eq!(
            ladder.judge(over(300.0, 300_000.0), second),
            Some(Move::Down(Rung::ExceptionsOnly))
        );
        assert_eq!(
            ladder.judge(over(2.0, 300_000.0), 2 * second),
            Some(Move::Down(Rung::Samples))
        );

        assert_eq!(ladder.judge(under(300_000.0), 3 * second), None);
        assert_eq!(
            ladder.judge(under(300_000.0), 8 * second),
            Some(Move::Up(Rung::Samples))
        );
        assert_eq!(ladder.judge(under(300_000.0), 30 * second), None);
        assert_eq!(ladder.judge(under(1_001.0), 31 * second), None);
        assert_eq!(
            ladder.judge(under(1_000.0), 32 * second),
            Some(Move::Up(Rung::ExceptionsOnly))
        );
    }
}
