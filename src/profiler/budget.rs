use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    pub cpu: f64,

    pub period: Duration,
}

impl Budget {
    pub const PRODUCTION: Self = Self {
        cpu: 0.01,
        period: Duration::from_secs(1),
    };
}

pub const WARM_UP: u32 = 4;

pub const FEWEST_SHARE: f64 = 1.0 / 1_048_576.0;

const HEADROOM: f64 = 0.9;

const FEWEST_KEPT_A_SECOND: f64 = 2.0;

const MOST_OWED: u32 = 4;

const FEWEST_COSTED: u64 = 64;

pub const UNFILTERED_SAMPLE_COST: Duration = Duration::from_micros(16);

#[must_use]
pub fn unfiltered_rate(budget: Budget, cpus: usize) -> u32 {
    let affordable = budget.cpu * HEADROOM / UNFILTERED_SAMPLE_COST.as_secs_f64();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "a rate is a small positive number of samples a second"
    )]
    let rate = (affordable / cpus.max(1) as f64).floor().max(1.0) as u32;
    rate
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub period: Duration,

    pub cpu: Duration,

    pub processing: Duration,

    pub raised: u64,

    pub kept: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Model {
    share: f64,

    cost: Option<Duration>,

    owed: Duration,
}

impl Model {
    #[must_use]
    pub fn new(budget: Budget, rate: Option<u32>, cpus: usize) -> Self {
        let share = rate.map_or(1.0, |rate| {
            #[expect(clippy::cast_precision_loss, reason = "a count of CPUs")]
            let worst = UNFILTERED_SAMPLE_COST.as_secs_f64() * f64::from(rate) * cpus.max(1) as f64;
            (budget.cpu * HEADROOM / worst).clamp(FEWEST_SHARE, 1.0)
        });
        Self {
            share,
            cost: None,
            owed: Duration::ZERO,
        }
    }

    #[must_use]
    pub const fn share(&self) -> f64 {
        self.share
    }

    #[must_use]
    pub const fn cost(&self) -> Option<Duration> {
        self.cost
    }

    pub const fn forgive(&mut self) {
        self.owed = Duration::ZERO;
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "sample counts and seconds need no more precision than f64's"
    )]
    pub fn step(&mut self, budget: Budget, observed: Observation) -> f64 {
        debug_assert!(budget.cpu > 0.0, "a budget of {} of a CPU", budget.cpu);
        debug_assert!(
            observed.kept <= observed.raised,
            "kept {} of {} samples",
            observed.kept,
            observed.raised
        );
        if observed.period.is_zero() {
            return self.share;
        }

        if observed.kept >= FEWEST_COSTED {
            let measured = observed.processing / u32::try_from(observed.kept).unwrap_or(u32::MAX);
            self.cost = Some(self.cost.map_or(measured, |before| (before + measured) / 2));
        }

        let allowed = observed.period.mul_f64(budget.cpu);
        self.owed = (self.owed + observed.cpu)
            .saturating_sub(allowed)
            .min(budget.period.mul_f64(budget.cpu) * MOST_OWED);

        let cost = self.cost.unwrap_or(UNFILTERED_SAMPLE_COST);
        if cost.is_zero() || observed.raised == 0 {
            return self.share;
        }

        let seconds = observed.period.as_secs_f64();
        let sampling = cost.as_secs_f64() * observed.kept as f64;
        let fixed = (observed.cpu.as_secs_f64() - sampling).max(0.0) / seconds;
        let owed = self.owed.as_secs_f64() / budget.period.as_secs_f64();
        let room = budget.cpu.mul_add(HEADROOM, -fixed) - owed;
        let raised = observed.raised as f64 / seconds;

        let fewest = (FEWEST_KEPT_A_SECOND / raised).clamp(FEWEST_SHARE, 1.0);
        self.share = (room / (cost.as_secs_f64() * raised)).clamp(fewest, 1.0);
        self.share
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Budget = Budget::PRODUCTION;

    struct Recorder {
        fixed: f64,
        cost: f64,
    }

    impl Recorder {
        fn run(&self, model: &mut Model, raised: u64, share: f64) -> (Observation, f64) {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "an expected count of samples"
            )]
            let kept = (raised as f64 * share).round() as u64;
            #[expect(clippy::cast_precision_loss, reason = "a count of samples")]
            let processing = self.cost * kept as f64;
            let observed = Observation {
                period: BUDGET.period,
                cpu: Duration::from_secs_f64(self.fixed + processing),
                processing: Duration::from_secs_f64(processing),
                raised,
                kept,
            };
            let next = model.step(BUDGET, observed);
            (observed, next)
        }
    }

    #[test]
    fn the_share_holds_spending_to_the_budget_from_the_second_period() {
        for (raised, fixed) in [
            (500, 0.002),
            (1_600, 0.002),
            (49 * 32, 0.003),
            (999 * 192, 0.004),
            (9_999 * 64, 0.0),
        ] {
            let recorder = Recorder { fixed, cost: 11e-6 };
            let mut model = Model::new(BUDGET, None, 1);
            let mut share = 1.0;
            let mut spent = Vec::new();
            for _ in 0..10 {
                let (observed, next) = recorder.run(&mut model, raised, share);
                spent.push(observed.cpu.as_secs_f64());
                share = next;
            }

            let affordable = 0.01_f64.mul_add(HEADROOM, -fixed) / 11e-6;
            #[expect(clippy::cast_precision_loss, reason = "a count of samples")]
            let expected = (affordable / raised as f64).min(1.0);
            assert!(
                (share - expected).abs() < 1e-3 * expected,
                "{raised} raised: share {share}, expected {expected}"
            );
            assert!(
                spent[1..].iter().all(|cpu| *cpu <= 0.01 + 1e-9),
                "{raised} raised overspent: {spent:?}"
            );
        }
    }

    #[test]
    fn an_affordable_program_keeps_every_sample() {
        let recorder = Recorder {
            fixed: 0.001,
            cost: 11e-6,
        };
        let mut model = Model::new(BUDGET, None, 1);
        let (_, share) = recorder.run(&mut model, 300, 1.0);
        assert!((share - 1.0).abs() < f64::EPSILON, "share {share}");
    }

    #[test]
    fn overspending_is_paid_back_and_then_forgotten() {
        let recorder = Recorder {
            fixed: 0.002,
            cost: 11e-6,
        };
        let mut model = Model::new(BUDGET, None, 1);
        let (_, first) = recorder.run(&mut model, 10_000, 1.0);
        let steady = 0.01_f64.mul_add(HEADROOM, -0.002) / 11e-6 / 10_000.0;
        assert!(first < steady, "the first share {first} did not pay back");

        let mut share = first;
        for _ in 0..MOST_OWED * 4 {
            share = recorder.run(&mut model, 10_000, share).1;
        }
        assert!((share - steady).abs() < 1e-3 * steady, "share {share}");
    }

    #[test]
    fn a_fixed_cost_over_the_budget_still_keeps_two_samples_a_second() {
        let recorder = Recorder {
            fixed: 0.02,
            cost: 11e-6,
        };
        let mut model = Model::new(BUDGET, None, 1);
        let (_, share) = recorder.run(&mut model, 10_000, 1.0);
        assert!(
            (share * 10_000.0 / FEWEST_KEPT_A_SECOND - 1.0).abs() < 1e-6,
            "share {share}"
        );
        let (_, share) = recorder.run(&mut model, 1 << 30, share);
        assert!((share - FEWEST_SHARE).abs() < f64::EPSILON, "share {share}");
    }

    #[test]
    fn a_cost_is_learned_only_from_enough_kept_samples() {
        let mut model = Model::new(BUDGET, None, 1);
        let few = Observation {
            period: BUDGET.period,
            cpu: Duration::from_millis(5),
            processing: Duration::from_millis(4),
            raised: FEWEST_COSTED - 1,
            kept: FEWEST_COSTED - 1,
        };
        model.step(BUDGET, few);
        assert_eq!(model.cost(), None);

        let enough = Observation {
            raised: FEWEST_COSTED,
            kept: FEWEST_COSTED,
            ..few
        };
        model.step(BUDGET, enough);
        assert_eq!(model.cost(), Some(Duration::from_millis(4) / 64));
    }

    #[test]
    fn the_first_period_fits_the_budget_with_every_cpu_busy() {
        let model = Model::new(BUDGET, Some(9_999), 32);
        let worst = model.share() * 9_999.0 * 32.0 * UNFILTERED_SAMPLE_COST.as_secs_f64();
        assert!(
            BUDGET.cpu.mul_add(-HEADROOM, worst).abs() < 1e-9,
            "the first share {} costs {worst}",
            model.share()
        );
        assert!((Model::new(BUDGET, Some(49), 4).share() - 1.0).abs() < f64::EPSILON);
        assert!((Model::new(BUDGET, None, 32).share() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn without_a_filter_the_rate_fits_every_cpu_busy_at_once() {
        let rate = unfiltered_rate(BUDGET, 32);
        let worst = f64::from(rate) * 32.0 * UNFILTERED_SAMPLE_COST.as_secs_f64();
        assert!(worst <= BUDGET.cpu, "{rate} Hz on 32 CPUs costs {worst}");
        assert!(
            f64::from(rate + 1) * 32.0 * UNFILTERED_SAMPLE_COST.as_secs_f64() > BUDGET.cpu * 0.9
        );
        assert_eq!(unfiltered_rate(BUDGET, 100_000), 1);
    }
}
