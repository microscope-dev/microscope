use std::collections::BTreeMap;
use std::fmt;

use super::choices::{Choices, Stream};
use super::kernel::Tid;
use super::swarm::Weights;

pub type SessionId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Run(Tid),

    Collect(SessionId),

    Deliver(SessionId),

    Poll(SessionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Actor {
    Thread(Tid),
    Waiter(SessionId),
    Controller(SessionId),
    Client(SessionId),
}

impl Action {
    const fn actor(self) -> Actor {
        match self {
            Self::Run(tid) => Actor::Thread(tid),
            Self::Collect(session) => Actor::Waiter(session),
            Self::Deliver(session) => Actor::Controller(session),
            Self::Poll(session) => Actor::Client(session),
        }
    }

    const fn kind(self) -> usize {
        match self {
            Self::Run(_) => 0,
            Self::Collect(_) => 1,
            Self::Deliver(_) => 2,
            Self::Poll(_) => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Walk(Weights),

    /// actions.
    Pct {
        depth: u64,
        horizon: u64,
    },
}

impl fmt::Display for Policy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Walk(Weights {
                run,
                collect,
                deliver,
                poll,
            }) => write!(
                formatter,
                "walk(run={run} collect={collect} deliver={deliver} poll={poll})"
            ),
            Self::Pct { depth, horizon } => write!(formatter, "pct(d={depth} k={horizon})"),
        }
    }
}

pub struct Scheduler {
    policy: Policy,

    priorities: BTreeMap<Actor, i64>,

    change_points: Vec<u64>,

    floor: i64,

    waiting: BTreeMap<Actor, u64>,
}

const HIGH: i64 = 1 << 32;

const PATIENCE: u64 = 10_000;

impl Scheduler {
    pub fn new(policy: Policy, choices: &mut Choices) -> Self {
        let mut change_points = Vec::new();
        if let Policy::Pct { depth, horizon } = policy {
            let points = depth.saturating_sub(1).min(horizon);
            while (change_points.len() as u64) < points {
                let point = choices.below(Stream::Schedule, horizon) + 1;
                if !change_points.contains(&point) {
                    change_points.push(point);
                }
            }
        }

        change_points.sort_unstable();

        Self {
            policy,
            priorities: BTreeMap::new(),
            change_points,
            floor: 0,
            waiting: BTreeMap::new(),
        }
    }

    pub fn choose(&mut self, enabled: &[Action], step: u64, choices: &mut Choices) -> Action {
        match self.policy {
            Policy::Walk(weights) => walk(weights, enabled, choices),
            Policy::Pct { .. } => {
                for action in enabled {
                    self.priorities.entry(action.actor()).or_insert_with(|| {
                        HIGH + i64::try_from(choices.below(Stream::Schedule, 1 << 32))
                            .expect("a priority fits")
                    });
                }

                self.waiting
                    .retain(|actor, _| enabled.iter().any(|action| action.actor() == *actor));

                for action in enabled {
                    self.waiting.entry(action.actor()).or_insert(step);
                }

                let starved = enabled
                    .iter()
                    .filter(|action| step - self.waiting[&action.actor()] >= PATIENCE)
                    .min_by_key(|action| (self.waiting[&action.actor()], action.actor()));
                let action = *starved.unwrap_or_else(|| {
                    enabled
                        .iter()
                        .max_by_key(|action| (self.priorities[&action.actor()], action.actor()))
                        .expect("an action is enabled")
                });
                self.waiting.remove(&action.actor());

                if self
                    .change_points
                    .first()
                    .is_some_and(|&point| point <= step)
                {
                    self.change_points.remove(0);
                    self.drop_actor(action.actor());
                }

                action
            }
        }
    }

    pub fn yielded(&mut self, tid: Tid) {
        if matches!(self.policy, Policy::Pct { .. }) {
            self.drop_actor(Actor::Thread(tid));
        }
    }

    fn drop_actor(&mut self, actor: Actor) {
        self.floor -= 1;
        self.priorities.insert(actor, self.floor);
    }
}

fn walk(weights: Weights, enabled: &[Action], choices: &mut Choices) -> Action {
    let weights = [weights.run, weights.collect, weights.deliver, weights.poll];
    let kinds = std::array::from_fn::<_, 4, _>(|kind| {
        if enabled.iter().any(|action| action.kind() == kind) {
            weights[kind]
        } else {
            0
        }
    });
    let kind = choices.weighted(Stream::Schedule, &kinds);
    let of_kind = enabled
        .iter()
        .filter(|action| action.kind() == kind)
        .copied()
        .collect::<Vec<_>>();

    match of_kind.as_slice() {
        [only] => *only,
        _ => *choices.pick(Stream::Schedule, &of_kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_runs_the_highest_priority_actor_until_it_drops() {
        let mut choices = Choices::new(3);
        let mut scheduler = Scheduler::new(
            Policy::Pct {
                depth: 2,
                horizon: 10,
            },
            &mut choices,
        );
        let change = scheduler.change_points[0];
        let enabled = [Action::Run(1), Action::Run(2), Action::Run(3)];
        let first = scheduler.choose(&enabled, 1, &mut choices);
        for step in 2..=change {
            assert_eq!(scheduler.choose(&enabled, step, &mut choices), first);
        }

        let second = scheduler.choose(&enabled, change + 1, &mut choices);
        assert_ne!(second, first);
        let Action::Run(tid) = second else {
            unreachable!("only threads are enabled")
        };

        scheduler.yielded(tid);
        let third = scheduler.choose(&enabled, change + 2, &mut choices);
        assert!(![first, second].contains(&third));
        let Action::Run(tid) = third else {
            unreachable!("only threads are enabled")
        };

        scheduler.yielded(tid);
        assert_eq!(scheduler.choose(&enabled, change + 3, &mut choices), first);
    }

    #[test]
    fn pct_honors_every_change_point() {
        for seed in 0..64 {
            let mut choices = Choices::new(seed);
            let scheduler = Scheduler::new(
                Policy::Pct {
                    depth: 4,
                    horizon: 4,
                },
                &mut choices,
            );
            let mut points = scheduler.change_points.clone();
            points.dedup();
            assert_eq!(points.len(), 3, "seed {seed}: {points:?}");
        }

        let mut choices = Choices::new(3);
        let mut scheduler = Scheduler::new(
            Policy::Pct {
                depth: 1,
                horizon: 8,
            },
            &mut choices,
        );
        scheduler.change_points = vec![3, 6];
        let enabled = [Action::Run(1), Action::Run(2), Action::Run(3)];
        let mut ran = Vec::new();

        for step in [1, 2, 4, 5, 7, 8] {
            ran.push(scheduler.choose(&enabled, step, &mut choices));
        }

        assert_eq!([ran[1], ran[2]], [ran[0]; 2]);
        assert_ne!(ran[3], ran[0]);
        assert_eq!(ran[4], ran[3]);
        assert!(!ran[..5].contains(&ran[5]), "the second point drops too");
        assert!(scheduler.change_points.is_empty());
    }

    #[test]
    fn pct_runs_an_actor_that_waited_too_long() {
        let mut choices = Choices::new(5);
        let mut scheduler = Scheduler::new(
            Policy::Pct {
                depth: 1,
                horizon: 10,
            },
            &mut choices,
        );
        let enabled = [Action::Deliver(0), Action::Poll(0)];
        let first = scheduler.choose(&enabled, 1, &mut choices);
        let other = if first == Action::Poll(0) {
            Action::Deliver(0)
        } else {
            Action::Poll(0)
        };

        let ran = (2..=2 * PATIENCE)
            .position(|step| scheduler.choose(&enabled, step, &mut choices) == other)
            .expect("the waiting actor runs");
        assert_eq!(u64::try_from(ran).expect("a step") + 2, PATIENCE + 1);
        assert_eq!(
            scheduler.choose(&enabled, PATIENCE + 2, &mut choices),
            first,
            "priorities hold again once it ran"
        );
    }
}
