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

    #[must_use]
    pub fn period_for(self, toggling: Duration) -> Duration {
        let stretched = toggling.div_f64(self.cpu / 10.0);
        stretched.clamp(self.period, self.period * MOST_STRETCH)
    }
}

pub const MOST_STRETCH: u32 = 8;

pub const STEPS: u32 = 64;

pub const WARM_UP: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub cpu: Duration,

    pub sampled: Duration,

    pub period: Duration,

    pub fixed: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    pub steps: u32,

    pub used: f64,
}

const MOST_OWED: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controller {
    steps: u32,

    owed: Duration,

    warm: bool,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            steps: STEPS,
            owed: Duration::ZERO,
            warm: false,
        }
    }
}

impl Controller {
    #[must_use]
    pub const fn steps(self) -> u32 {
        self.steps
    }

    pub const fn whole(&mut self) {
        self.steps = STEPS;
    }

    pub const fn forgive(&mut self) {
        self.owed = Duration::ZERO;
    }

    #[must_use]
    pub const fn warm(self) -> bool {
        self.warm
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a share is a small fraction, floored to sixty-fourths"
    )]
    pub fn step(&mut self, budget: Budget, observed: Observation) -> Option<Decision> {
        if observed.sampled.is_zero() || observed.period.is_zero() {
            return None;
        }

        let allowed = observed.period.mul_f64(budget.cpu);
        self.owed = (self.owed + observed.cpu)
            .saturating_sub(allowed)
            .min(budget.period.mul_f64(budget.cpu) * MOST_OWED);
        if !std::mem::replace(&mut self.warm, true) {
            return None;
        }

        let used = observed.cpu.as_secs_f64() / observed.period.as_secs_f64();

        let sampling = observed.cpu.saturating_sub(observed.fixed);
        let cost = sampling.as_secs_f64() / observed.sampled.as_secs_f64();
        let left = budget.cpu
            - self.owed.as_secs_f64() / budget.period.as_secs_f64()
            - observed.fixed.as_secs_f64() / observed.period.as_secs_f64();
        let fits = if cost > 0.0 {
            ((left / cost * 0.9 * f64::from(STEPS)).floor().max(0.0) as u32).clamp(1, STEPS)
        } else {
            STEPS
        };

        let steps = if used > budget.cpu {
            fits.min(self.steps)
        } else if !self.owed.is_zero() {
            fits.min(self.steps.saturating_mul(2))
        } else {
            fits.min(self.steps.saturating_mul(2)).max(self.steps)
        };

        (steps != self.steps).then(|| {
            self.steps = steps;
            Decision { steps, used }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Budget = Budget {
        cpu: 0.01,
        period: Duration::from_secs(1),
    };

    fn period(cost: f64, steps: u32) -> Observation {
        let sampled = BUDGET.period * steps / STEPS;
        Observation {
            cpu: sampled.mul_f64(cost),
            sampled,
            period: BUDGET.period,
            fixed: Duration::ZERO,
        }
    }

    #[test]
    fn a_period_lasts_until_toggling_costs_a_tenth_of_the_budget() {
        let period = |ms| BUDGET.period_for(Duration::from_millis(ms));
        assert_eq!(period(0), BUDGET.period);
        assert_eq!(period(1), BUDGET.period, "a tenth of 1% of a second");
        assert_eq!(period(3).as_millis(), 3_000);
        assert_eq!(
            period(1_000),
            Duration::from_secs(8),
            "at most MOST_STRETCH periods"
        );
    }

    #[test]
    fn the_share_settles_where_it_fits_and_stays() {
        for (cost, settled) in [(0.009, 64), (0.011, 52), (0.2, 2), (1.0, 1)] {
            let mut controller = Controller::default();
            let mut changes = Vec::new();

            for index in 0..20 {
                if let Some(decision) = controller.step(BUDGET, period(cost, controller.steps())) {
                    changes.push((index, decision.steps));
                }
            }

            assert_eq!(controller.steps(), settled, "{cost}: {changes:?}");
            assert!(
                changes
                    .iter()
                    .all(|(index, _)| *index < 3 + MOST_OWED as usize * 2),
                "{cost} changed late: {changes:?}"
            );
        }
    }

    #[test]
    fn a_cost_sampling_does_not_cause_is_paid_before_sampling() {
        let mut controller = Controller::default();
        for _ in 0..20 {
            let sampled = BUDGET.period * controller.steps() / STEPS;
            let fixed = BUDGET.period.mul_f64(0.006);
            controller.step(
                BUDGET,
                Observation {
                    cpu: fixed + sampled.mul_f64(0.02),
                    sampled,
                    period: BUDGET.period,
                    fixed,
                },
            );
        }

        assert_eq!(controller.steps(), 11);
    }

    #[test]
    fn a_cheaper_recording_samples_more_again() {
        let mut controller = Controller::default();
        controller.step(BUDGET, period(1.0, STEPS));
        controller.step(BUDGET, period(1.0, STEPS));
        assert_eq!(controller.steps(), 1);
        let mut shares = Vec::new();

        for _ in 0..10 {
            controller.step(BUDGET, period(0.0125, controller.steps()));
            shares.push(controller.steps());
        }

        assert_eq!(shares, [1, 1, 1, 2, 4, 8, 16, 32, 46, 46]);
    }

    #[test]
    fn the_first_period_is_not_judged() {
        let mut controller = Controller::default();
        assert_eq!(controller.step(BUDGET, period(1.0, STEPS)), None);
        assert_eq!(controller.steps(), STEPS);
        assert!(controller.step(BUDGET, period(1.0, STEPS)).is_some());
    }

    #[test]
    fn a_costly_first_period_is_paid_back() {
        for first in [16.0, 36.0] {
            let mut controller = Controller::default();
            let mut spent = 0.0;

            for index in 0..5 {
                let steps = controller.steps();
                let observed = if index == 0 {
                    Observation {
                        cpu: Duration::from_secs_f64(first / 1000.0),
                        sampled: BUDGET.period,
                        period: BUDGET.period,
                        fixed: Duration::ZERO,
                    }
                } else {
                    period(0.0095, steps)
                };
                spent += observed.cpu.as_secs_f64();
                controller.step(BUDGET, observed);
            }

            assert!(
                spent <= 5.0 * BUDGET.cpu,
                "{first} ms first: {:.1} ms in 5 s",
                spent * 1000.0
            );
        }
    }

    #[test]
    fn a_period_without_sampling_changes_nothing() {
        let mut controller = Controller::default();
        let idle = Observation {
            cpu: Duration::from_millis(500),
            sampled: Duration::ZERO,
            period: BUDGET.period,
            fixed: Duration::ZERO,
        };

        assert_eq!(controller.step(BUDGET, idle), None);
        assert_eq!(controller.steps(), STEPS);
    }
}
