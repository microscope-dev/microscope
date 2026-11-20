use std::time::Duration;

use proptest::prelude::*;

use super::{
    Action, BLOCKED_RETRY, Effect, FIRST_RETRY, Instant, LAST_RETRY, Machine, Outcome, State,
};

const fn at(secs: u64) -> Instant {
    Instant(Duration::from_secs(secs))
}

fn token(agent_id: i64, secs: u64) -> Outcome {
    Outcome::Token {
        agent_id,
        refresh_after: Duration::from_secs(secs),
    }
}

const RETRY: Outcome = Outcome::Retry { retry_after: None };

#[test]
fn an_agent_enrolls_then_refreshes_when_told() {
    let mut machine = Machine::new(at(0), None, false);
    assert_eq!(machine.next(at(0)), Action::Enroll { previous: None });
    assert_eq!(machine.on(token(7, 600), at(0), 0.0), Effect::None);
    assert_eq!(machine.name(), "enrolled");
    assert_eq!(machine.next(at(1)), Action::Wait { until: at(600) });
    assert_eq!(machine.next(at(600)), Action::Refresh { agent_id: 7 });

    machine.on(token(7, 600), at(600), 1.0);
    assert_eq!(machine.next(at(601)), Action::Wait { until: at(1140) });
}

#[test]
fn a_known_agent_refreshes_at_once_on_start() {
    let machine = Machine::new(at(5), Some(9), false);
    assert_eq!(machine.next(at(5)), Action::Refresh { agent_id: 9 });
    assert_eq!(machine.agent_id(), Some(9));
}

#[test]
fn retries_back_off_from_ten_seconds_to_ten_minutes() {
    let mut machine = Machine::new(at(0), Some(1), false);
    let mut now = at(0);
    let mut ceilings = Vec::new();
    for _ in 0..9 {
        machine.on(RETRY, now, 1.0);
        let Action::Wait { until } = machine.next(now) else {
            panic!("a retry did not wait");
        };
        ceilings.push(now.until(until).as_secs());
        now = until;
    }
    assert_eq!(ceilings, [10, 20, 40, 80, 160, 320, 600, 600, 600]);
    assert_eq!(machine.name(), "backoff");

    machine.on(RETRY, now, 0.0);
    assert_eq!(
        machine.next(now),
        Action::Wait {
            until: now.after(LAST_RETRY / 2)
        }
    );

    machine.on(token(1, 600), now, 0.0);
    assert_eq!(machine.name(), "enrolled");
    machine.on(RETRY, now, 1.0);
    assert_eq!(
        machine.next(now),
        Action::Wait {
            until: now.after(FIRST_RETRY)
        }
    );
}

#[test]
fn retry_after_is_honored_but_not_past_an_hour() {
    let mut machine = Machine::new(at(0), None, false);
    machine.on(
        Outcome::Retry {
            retry_after: Some(Duration::from_secs(90)),
        },
        at(0),
        0.0,
    );
    assert_eq!(machine.next(at(0)), Action::Wait { until: at(90) });
    machine.on(
        Outcome::Retry {
            retry_after: Some(Duration::from_hours(30)),
        },
        at(90),
        0.0,
    );
    assert_eq!(
        machine.next(at(90)),
        Action::Wait {
            until: at(90 + 3600)
        }
    );
}

#[test]
fn a_bad_enrollment_key_waits_an_hour_or_for_the_config() {
    let mut machine = Machine::new(at(0), None, false);
    machine.on(Outcome::EnrollKey, at(0), 0.5);
    assert_eq!(machine.name(), "blocked");
    assert_eq!(
        machine.next(at(1)),
        Action::Wait {
            until: at(0).after(BLOCKED_RETRY)
        }
    );
    assert_eq!(machine.next(at(3600)), Action::Enroll { previous: None });

    machine.config_changed(at(10));
    assert_eq!(machine.next(at(10)), Action::Enroll { previous: None });
}

#[test]
fn a_revoked_agent_forgets_its_key_and_waits_for_a_person() {
    let mut machine = Machine::new(at(0), Some(3), false);
    assert_eq!(machine.on(Outcome::Revoked, at(0), 0.0), Effect::ForgetKey);
    assert_eq!(machine.next(at(1_000_000)), Action::WaitForConfig);
    assert_eq!(machine.on(token(3, 600), at(5), 0.0), Effect::None);
    assert_eq!(machine.state(), &State::Revoked);

    machine.config_changed(at(7));
    assert_eq!(machine.next(at(7)), Action::Enroll { previous: None });
}

#[test]
fn reenroll_starts_over_at_once_and_backs_off_if_it_repeats() {
    let mut machine = Machine::new(at(0), Some(41), false);
    assert_eq!(machine.on(Outcome::Reenroll, at(0), 0.0), Effect::ForgetKey);
    assert_eq!(machine.next(at(0)), Action::Enroll { previous: Some(41) });

    machine.on(token(42, 600), at(0), 0.0);
    machine.on(Outcome::Reenroll, at(600), 0.0);
    assert_eq!(machine.next(at(600)), Action::Wait { until: at(605) });
    assert_eq!(machine.next(at(605)), Action::Enroll { previous: Some(42) });

    machine.on(token(43, 600), at(605), 0.0);
    machine.on(token(43, 600), at(1200), 0.0);
    machine.on(Outcome::Reenroll, at(1800), 0.0);
    assert_eq!(
        machine.next(at(1800)),
        Action::Enroll { previous: Some(43) }
    );
}

#[test]
fn a_server_cannot_make_an_agent_refresh_in_a_tight_loop() {
    let mut machine = Machine::new(at(0), None, false);
    machine.on(token(1, 0), at(0), 1.0);
    assert_eq!(machine.next(at(0)), Action::Wait { until: at(10) });
    machine.on(token(1, 86_400), at(10), 0.0);
    assert_eq!(machine.next(at(10)), Action::Wait { until: at(3610) });

    let mut local = Machine::new(at(0), None, true);
    local.on(token(1, 1), at(0), 1.0);
    assert_eq!(local.next(at(0)), Action::Wait { until: at(1) });
}

#[test]
fn request_refresh_schedules_early_refresh_but_respects_backoff_and_min_refresh() {
    let mut machine = Machine::new(at(0), None, false);
    machine.on(token(7, 600), at(0), 0.0);
    assert_eq!(machine.next(at(5)), Action::Wait { until: at(600) });

    machine.request_refresh(at(5));
    assert_eq!(machine.next(at(5)), Action::Wait { until: at(10) });
    assert_eq!(machine.next(at(10)), Action::Refresh { agent_id: 7 });

    machine.on(token(7, 600), at(10), 0.0);
    machine.request_refresh(at(30));
    assert_eq!(machine.next(at(30)), Action::Refresh { agent_id: 7 });

    machine.on(
        Outcome::Retry {
            retry_after: Some(Duration::from_secs(90)),
        },
        at(30),
        0.0,
    );
    assert_eq!(machine.next(at(30)), Action::Wait { until: at(120) });
    machine.request_refresh(at(35));
    assert_eq!(machine.next(at(35)), Action::Wait { until: at(120) });
}

#[derive(Clone, Debug)]
enum Step {
    Pass(Duration),
    Answer(Outcome, f64),
    ConfigChanged,
}

fn outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        (1_i64..5, 0_u64..7200).prop_map(|(id, secs)| token(id, secs)),
        proptest::option::of(0_u64..200_000).prop_map(|secs| Outcome::Retry {
            retry_after: secs.map(Duration::from_secs)
        }),
        Just(Outcome::EnrollKey),
        Just(Outcome::Revoked),
        Just(Outcome::Reenroll),
    ]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0_u64..10_000).prop_map(|secs| Step::Pass(Duration::from_secs(secs))),
        6 => (outcome(), 0.0..=1.0_f64).prop_map(|(outcome, jitter)| Step::Answer(outcome, jitter)),
        1 => Just(Step::ConfigChanged),
    ]
}

proptest! {
    #[test]
    fn no_failure_is_retried_sooner_than_five_seconds_or_later_than_an_hour(
        enrolled in proptest::option::of(1_i64..5),
        loopback in any::<bool>(),
        steps in proptest::collection::vec(step(), 1..80),
    ) {
        let mut machine = Machine::new(at(0), enrolled, loopback);
        let mut now = at(0);
        let mut reenrolled_since_refresh = false;

        for step in steps {
            match step {
                Step::Pass(time) => now = now.after(time),
                Step::ConfigChanged => {
                    let before = machine.state().clone();
                    machine.config_changed(now);
                    if matches!(before, State::Blocked { .. } | State::Revoked) {
                        let enrolls = matches!(machine.next(now), Action::Enroll { .. });
                        prop_assert!(enrolls);
                        reenrolled_since_refresh = false;
                    } else {
                        prop_assert_eq!(machine.state(), &before);
                    }
                }
                Step::Answer(outcome, jitter) => {
                    if !matches!(machine.next(now), Action::Enroll { .. } | Action::Refresh { .. }) {
                        continue;
                    }
                    let was_revoked = matches!(machine.state(), State::Revoked);
                    let refreshing = matches!(machine.state(), State::Enrolled { .. });
                    let effect = machine.on(outcome, now, jitter);

                    prop_assert_eq!(
                        effect == Effect::ForgetKey,
                        !was_revoked && matches!(outcome, Outcome::Revoked | Outcome::Reenroll)
                    );
                    let wait = match machine.next(now) {
                        Action::Wait { until } => Some(now.until(until)),
                        Action::WaitForConfig => None,
                        Action::Enroll { .. } | Action::Refresh { .. } => Some(Duration::ZERO),
                    };

                    if refreshing && matches!(outcome, Outcome::Token { .. }) {
                        reenrolled_since_refresh = false;
                    }
                    if was_revoked {
                        prop_assert_eq!(wait, None);
                        continue;
                    }
                    match outcome {
                        Outcome::Retry { retry_after } => {
                            let wait = wait.unwrap();
                            prop_assert!(wait >= Duration::from_secs(5));
                            let cap = LAST_RETRY.max(retry_after.unwrap_or_default().min(Duration::from_hours(1)));
                            prop_assert!(wait <= cap, "{wait:?} > {cap:?}");
                        }
                        Outcome::Token { .. } => {
                            let wait = wait.unwrap();
                            let low = if loopback { Duration::from_secs(1) } else { Duration::from_secs(9) };
                            prop_assert!(wait >= low && wait <= Duration::from_hours(1), "{wait:?}");
                        }
                        Outcome::EnrollKey if matches!(machine.state(), State::Blocked { .. }) => {
                            prop_assert_eq!(wait, Some(BLOCKED_RETRY));
                        }
                        Outcome::EnrollKey => prop_assert!(wait.unwrap() >= Duration::from_secs(5)),
                        Outcome::Revoked => prop_assert_eq!(wait, None),
                        Outcome::Reenroll if was_revoked => {}
                        Outcome::Reenroll => {
                            if reenrolled_since_refresh {
                                prop_assert!(wait.unwrap() >= Duration::from_secs(5));
                            }
                            reenrolled_since_refresh = true;
                        }
                    }
                }
            }
            if let Action::Refresh { agent_id } = machine.next(now) {
                prop_assert_eq!(machine.agent_id(), Some(agent_id));
            }
        }
    }
}

proptest! {
    #[test]
    fn whatever_happened_before_an_agent_is_enrolled_soon_after_the_server_comes_back(
        enrolled in proptest::option::of(1_i64..5),
        history in proptest::collection::vec((outcome(), 0.0..=1.0_f64, 0_u64..5000), 0..40),
    ) {
        let mut machine = Machine::new(at(0), enrolled, false);
        let mut now = at(0);
        for (outcome, jitter, pass) in history {
            if matches!(machine.next(now), Action::Enroll { .. } | Action::Refresh { .. }) {
                machine.on(outcome, now, jitter);
            }
            now = now.after(Duration::from_secs(pass));
        }
        if machine.state() == &State::Revoked {
            machine.config_changed(now);
        }

        let back = now;
        let mut answered = 0;
        while !matches!(machine.state(), State::Enrolled { .. }) || answered == 0 {
            match machine.next(now) {
                Action::Wait { until } => now = until,
                Action::WaitForConfig => prop_assert!(false, "waits for a person while the server is fine"),
                Action::Enroll { .. } | Action::Refresh { .. } => {
                    machine.on(token(1, 600), now, 0.5);
                    answered += 1;
                }
            }
            prop_assert!(answered <= 1);
        }
        prop_assert!(back.until(now) <= BLOCKED_RETRY, "{:?}", back.until(now));
    }
}
