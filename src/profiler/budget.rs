//! Holding microscope's own CPU time to a budget.
//!
//! Once a period, the controller reads how much CPU time recording took
//! and how long sampling was on, and chooses how much of the next period
//! to sample. Pausing is the one control that reaches every event: a new
//! sampling period would not reach the events a program's threads
//! inherited, and the stack copy is fixed when an event opens. Each paused
//! span is a gap in the capture, so weights stay true for the time sampled.

use std::time::Duration;

/// How much CPU time microscope may spend recording.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// A share of one CPU: 0.01 is 1%.
    pub cpu: f64,

    /// How often the budget is checked; each period is sampled for its
    /// share, in one span at a random offset.
    pub period: Duration,
}

impl Budget {
    /// One percent of a CPU, checked every second.
    pub const PRODUCTION: Self = Self {
        cpu: 0.01,
        period: Duration::from_secs(1),
    };
}

/// The finest share of a period the controller samples: 1/64.
pub const STEPS: u32 = 64;

/// What one period cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// CPU time recording took in the period, less what loading programs'
    /// debug information took, which no pause makes cheaper.
    pub cpu: Duration,

    /// How long sampling was on.
    pub sampled: Duration,

    /// How long the period lasted.
    pub period: Duration,
}

/// A change of the share sampled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// Sixty-fourths of each period now sampled.
    pub steps: u32,

    /// The share of a CPU the period that led to it used.
    pub used: f64,
}

/// The most a recording owes over its budget, in periods' worth of
/// budget: past it, an expensive start is forgiven rather than starving
/// sampling for longer than about this many periods.
const MOST_OWED: u32 = 4;

/// How much of each period is sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controller {
    steps: u32,

    /// What recording has spent beyond its budget so far. A cheap period
    /// pays it back; one under budget never banks credit for later.
    owed: Duration,

    /// Whether a period has passed. The first warms what recording builds
    /// once, such as each module's unwind tables, so what it cost is owed
    /// but says nothing of what sampling costs.
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
    /// Sixty-fourths of each period sampled.
    #[must_use]
    pub const fn steps(self) -> u32 {
        self.steps
    }

    /// Chooses the share of the next period from what the last one cost,
    /// and says so when it changes. What fits is what the budget leaves,
    /// with a margin, once what is owed is paid back. Over budget, the
    /// share falls at once to what fits. Otherwise it at most doubles,
    /// toward what fits, and falls only while something is owed. Every
    /// period's cost is owed, the first's too, though only later ones say
    /// what sampling costs.
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

        // CPU per second of sampling, and the share of a period that fits.
        let cost = observed.cpu.as_secs_f64() / observed.sampled.as_secs_f64();
        let left = budget.cpu - self.owed.as_secs_f64() / budget.period.as_secs_f64();
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

    /// A period of a recording whose sampling costs `cost` of a CPU while
    /// it is on, sampled for `steps` sixty-fourths.
    fn period(cost: f64, steps: u32) -> Observation {
        let sampled = BUDGET.period * steps / STEPS;
        Observation {
            cpu: sampled.mul_f64(cost),
            sampled,
            period: BUDGET.period,
        }
    }

    /// Whatever sampling costs, the controller settles on a share that fits
    /// the budget, or on the least share, and stays: within a few periods,
    /// or a few more when a costly start must be paid back first.
    #[test]
    fn the_share_settles_where_it_fits_and_stays() {
        for cost in [0.001, 0.009, 0.011, 0.05, 0.2, 1.0, 4.0] {
            let mut controller = Controller::default();
            let mut changes = Vec::new();

            for index in 0..20 {
                if let Some(decision) = controller.step(BUDGET, period(cost, controller.steps())) {
                    changes.push((index, decision.steps));
                }
            }

            let steps = controller.steps();
            let used = cost * f64::from(steps) / f64::from(STEPS);

            assert!(
                used <= BUDGET.cpu || steps == 1,
                "{cost}: {steps} use {used}"
            );
            assert!(
                changes
                    .iter()
                    .all(|(index, _)| *index < 3 + MOST_OWED as usize * 2),
                "{cost} changed late: {changes:?}"
            );
        }
    }

    /// Once the cost falls and what was owed is paid, the share doubles
    /// back toward the whole period, never past what fits.
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

        // 0.9 of the budget fits 0.72 of a period: 46 sixty-fourths.
        assert_eq!(shares, [1, 1, 1, 2, 4, 8, 16, 32, 46, 46]);
    }

    /// The first period warms what recording builds once, such as each
    /// module's unwind tables, so it is not judged.
    #[test]
    fn the_first_period_is_not_judged() {
        let mut controller = Controller::default();
        assert_eq!(controller.step(BUDGET, period(1.0, STEPS)), None);
        assert_eq!(controller.steps(), STEPS);
        assert!(controller.step(BUDGET, period(1.0, STEPS)).is_some());
    }

    /// What `perft.py 5` under `--preset prod --python bpf` cost: its first
    /// period, sampled in full, took 14-36 ms as recording warmed up, and
    /// later ones 9-10 ms a second of sampling. A run of 5 s is still held
    /// to the budget, the warm-up included.
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

    /// A period that sampled nothing says nothing of the cost.
    #[test]
    fn a_period_without_sampling_changes_nothing() {
        let mut controller = Controller::default();
        let idle = Observation {
            cpu: Duration::from_millis(500),
            sampled: Duration::ZERO,
            period: BUDGET.period,
        };

        assert_eq!(controller.step(BUDGET, idle), None);
        assert_eq!(controller.steps(), STEPS);
    }
}
