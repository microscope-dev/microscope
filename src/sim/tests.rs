use std::num::NonZeroUsize;

use super::marks::Reach;
use super::report::{Failure, FailureKind};
use super::shrink;
use super::world::Sabotage;
use super::{Corpus, Settings, describe_failure, run};

const GATE_SEEDS: u64 = 2000;

const MARK_FLOOR: u64 = 3;

const DETERMINISM_SEEDS: u64 = 32;

#[test]
fn fixed_seeds_pass_every_oracle() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings::default();
    let threads = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let mut failures = Vec::new();
    let mut reach = Reach::default();

    std::thread::scope(|scope| {
        let workers = (0..threads)
            .map(|first| {
                let (corpus, settings) = (&corpus, &settings);
                scope.spawn(move || {
                    let mut failures = Vec::new();
                    let mut reach = Reach::default();
                    for seed in (first as u64..GATE_SEEDS).step_by(threads) {
                        let outcome = run(seed, corpus, settings);
                        if outcome.failure.is_some() {
                            failures.push((seed, describe_failure(&outcome)));
                        }

                        reach.add(&outcome.marks);
                    }

                    (failures, reach)
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            let (theirs, reached) = worker.join().expect("a gate worker panicked");
            failures.extend(theirs);
            reach.merge(&reached);
        }
    });

    failures.sort_unstable_by_key(|(seed, _)| *seed);
    assert!(
        failures.is_empty(),
        "{} of {GATE_SEEDS} seeds failed; the first:\n{}",
        failures.len(),
        failures[0].1
    );

    let rare = reach.below(MARK_FLOOR);
    assert!(
        rare.is_empty(),
        "fewer than {MARK_FLOOR} fixed seeds reached {rare:?}; {reach}"
    );
}

#[test]
fn a_seed_always_names_the_same_run() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings::default();
    let first = (0..DETERMINISM_SEEDS)
        .map(|seed| run(seed, &corpus, &settings).fingerprint)
        .collect::<Vec<_>>();
    let again = (0..DETERMINISM_SEEDS)
        .rev()
        .map(|seed| run(seed, &corpus, &settings).fingerprint)
        .rev()
        .collect::<Vec<_>>();
    assert_eq!(first, again);
}

#[test]
fn shrinking_keeps_the_failure_and_makes_the_run_no_larger() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings {
        sabotage: Some(Sabotage::LosePokes),
        ..Settings::default()
    };
    let original = (0..GATE_SEEDS)
        .map(|seed| run(seed, &corpus, &settings))
        .find(|outcome| outcome.failure.is_some())
        .expect("a sabotaged run fails");
    let signature = original.failure.as_ref().map(Failure::signature);

    let shrunk = shrink::shrink(original.seed, &corpus, &settings, 120).expect("the seed fails");
    assert_eq!(
        shrunk.outcome.failure.as_ref().map(Failure::signature),
        signature
    );
    assert!(
        shrunk.outcome.steps <= original.steps,
        "{} actions, from {}",
        shrunk.outcome.steps,
        original.steps
    );
    assert!(shrunk.outcome.swarm.requests <= original.swarm.requests);
    assert!(
        shrunk.outcome.steps < original.steps || !shrunk.simpler.is_empty(),
        "shrinking {} actions found nothing simpler",
        original.steps
    );

    let (seed, simpler, tape) = shrink::read(&shrunk.to_text()).expect("a shrunk run reads back");
    let again = run(
        seed,
        &corpus,
        &Settings {
            simpler,
            tape: Some(tape),
            ..settings
        },
    );
    assert_eq!(again.fingerprint, shrunk.outcome.fingerprint);
}

fn first_failure_with(sabotage: Sabotage) -> &'static str {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings {
        sabotage: Some(sabotage),
        ..Settings::default()
    };

    (0..GATE_SEEDS)
        .find_map(|seed| run(seed, &corpus, &settings).failure)
        .map(|failure| {
            assert_eq!(failure.kind, FailureKind::Debugger, "{failure}");
            failure.check
        })
        .expect("a sabotaged run fails")
}

#[test]
fn a_skipped_temporary_trap_fails_breakpoint_accounting() {
    assert!(fails_with(
        Sabotage::SkipTemporaryTraps,
        &[("breakpoint accounting", "without a trap reporting it")]
    ));
}

#[test]
fn a_misread_first_argument_fails_catch_accounting() {
    assert!(fails_with(
        Sabotage::MisreportFirstCallArgument,
        &[("catch accounting", "with no stop reporting it")]
    ));
}

#[test]
fn traps_passed_on_detach_fail_transparency() {
    assert!(fails_with(
        Sabotage::PassTrapsOnDetach,
        &[("transparency", "died of signal 5")]
    ));
}

#[test]
fn shifted_mappings_fail_modules_frames_and_watch_accounting() {
    assert!(fails_with(
        Sabotage::ShiftMappings,
        &[
            ("modules", "the main module"),
            ("frames", "shows line"),
            ("watch accounting", "but watchpoint"),
        ]
    ));
}

fn fails_with(sabotage: Sabotage, ways: &[(&str, &str)]) -> bool {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings {
        sabotage: Some(sabotage),
        ..Settings::default()
    };

    let mut unseen = ways.to_vec();
    for seed in 0..GATE_SEEDS {
        let Some(failure) = run(seed, &corpus, &settings).failure else {
            continue;
        };

        if failure.kind == FailureKind::Debugger {
            unseen.retain(|(check, message)| {
                failure.check != *check || !failure.message.contains(message)
            });
        }

        if unseen.is_empty() {
            return true;
        }
    }

    false
}

#[test]
fn a_reply_dropped_while_ending_fails() {
    assert!(fails_with(
        Sabotage::DropRequestsWhileEnding,
        &[("lost reply", "dropped the request without answering it")]
    ));
}

#[test]
fn a_kill_dropped_while_a_start_waits_fails() {
    assert!(fails_with(
        Sabotage::DropKillWhileStarting,
        &[(
            "lost reply",
            "dropped a kill sent while a start waited for its first stop"
        )]
    ));
}

#[test]
fn lost_writes_fail_code_integrity() {
    assert_eq!(first_failure_with(Sabotage::LosePokes), "code integrity");
}

#[test]
fn a_deaf_waiter_fails_liveness() {
    assert_eq!(first_failure_with(Sabotage::DeafWaiter), "liveness");
}

#[test]
fn a_thread_resumed_behind_the_controller_fails_all_stop() {
    assert_eq!(
        first_failure_with(Sabotage::ResumeBehindTheController),
        "all-stop"
    );
}

#[test]
fn a_thread_resumed_while_gathering_fails_held_stops() {
    assert_eq!(
        first_failure_with(Sabotage::ResumeWhileGathering),
        "held stops"
    );
}

#[test]
fn lost_stop_requests_fail_requested_stops() {
    assert_eq!(
        first_failure_with(Sabotage::LoseStopRequests),
        "requested stops"
    );
}

#[test]
fn a_skipped_trap_fails_breakpoint_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::SkipTraps),
        "breakpoint accounting"
    );
}

#[test]
fn skewed_return_addresses_fail_the_backtrace_oracle() {
    assert_eq!(
        first_failure_with(Sabotage::SkewReturnAddresses),
        "backtrace"
    );
}

#[test]
fn skewed_sampled_return_addresses_fail_the_samples_oracle() {
    assert_eq!(
        first_failure_with(Sabotage::SkewSampledReturnAddresses),
        "samples"
    );
}

#[test]
fn late_single_steps_fail_the_stepping_oracle() {
    assert_eq!(first_failure_with(Sabotage::LateSingleSteps), "stepping");
}

#[test]
fn hidden_calls_fail_the_stepping_oracle() {
    some_failure_past(
        Sabotage::HideSteppedCalls,
        "stepping",
        &["into the call returning to"],
        &["stepping", "code integrity"],
        true,
    );
}

fn some_failure_with(sabotage: Sabotage, check: &str, allowed: &[&str]) {
    some_failure_saying(sabotage, check, &[""], allowed);
}

fn some_failure_saying(sabotage: Sabotage, check: &str, sayings: &[&str], allowed: &[&str]) {
    some_failure_past(sabotage, check, sayings, allowed, false);
}

fn some_failure_past(
    sabotage: Sabotage,
    check: &str,
    sayings: &[&str],
    allowed: &[&str],
    gaps: bool,
) {
    let corpus = Corpus::load().expect("load the golden corpus");
    some_failure_in(&corpus, sabotage, check, sayings, allowed, gaps);
}

fn some_failure_in(
    corpus: &Corpus,
    sabotage: Sabotage,
    check: &str,
    sayings: &[&str],
    allowed: &[&str],
    gaps: bool,
) {
    let muted = if STACK_CHECKS.contains(&check) || JUDGED_STACK_CHECKS.contains(&check) {
        JUDGED_STACK_CHECKS
            .iter()
            .copied()
            .filter(|other| *other != check)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let settings = Settings {
        sabotage: Some(sabotage),
        muted: muted.leak(),
        ..Settings::default()
    };

    let mut unseen = sayings.to_vec();
    for seed in 0..SABOTAGE_SEEDS {
        let Some(failure) = run(seed, corpus, &settings).failure else {
            continue;
        };

        if gaps && failure.kind == FailureKind::ModelGap {
            continue;
        }

        assert_eq!(failure.kind, FailureKind::Debugger, "{failure}");
        assert!(allowed.contains(&failure.check), "{failure}");
        if failure.check == check {
            unseen.retain(|saying| !failure.message.contains(saying));
            if unseen.is_empty() {
                return;
            }
        }
    }

    panic!("no sabotaged run failed {check} saying any of {unseen:?}");
}

#[test]
fn resumed_siblings_fail_thread_scope() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings {
        sabotage: Some(Sabotage::ResumeSiblings),
        ..Settings::default()
    };

    let caught = (0..GATE_SEEDS).any(|seed| {
        run(seed, &corpus, &settings)
            .failure
            .is_some_and(|failure| failure.check == "thread scope")
    });
    assert!(caught, "no sabotaged run failed thread scope");
}

const SABOTAGE_SEEDS: u64 = 4 * GATE_SEEDS;

const STACK_CHECKS: &[&str] = &[
    "variables",
    "expressions",
    "breakpoint conditions",
    "entry values",
    "catch stops",
];

const JUDGED_STACK_CHECKS: &[&str] = &[
    "variables",
    "expressions",
    "entry values",
    "catch stops",
    "registers",
];

#[test]
fn skewed_stack_words_fail_the_variables_oracle() {
    some_failure_with(Sabotage::SkewSmallStackWords, "variables", STACK_CHECKS);
}

#[test]
fn skewed_stack_words_fail_the_expressions_oracle() {
    some_failure_saying(
        Sabotage::SkewSmallStackWords,
        "expressions",
        &["not true", "which holds", "expected"],
        STACK_CHECKS,
    );
}

#[test]
fn skewed_stack_words_fail_the_entry_values_oracle() {
    let mut corpus = Corpus::load().expect("load the golden corpus");
    for program in &mut corpus.programs {
        program.variants.retain(|variant| variant.facts.optimized);
    }

    corpus
        .programs
        .retain(|program| !program.variants.is_empty());
    some_failure_in(
        &corpus,
        Sabotage::SkewSmallStackWords,
        "entry values",
        &[""],
        STACK_CHECKS,
        false,
    );
}

#[test]
fn flickering_stack_words_fail_breakpoint_conditions() {
    some_failure_with(
        Sabotage::FlickeringStackWords,
        "breakpoint conditions",
        STACK_CHECKS,
    );
}

#[test]
fn flickering_stack_words_fail_the_expressions_oracle() {
    some_failure_saying(
        Sabotage::FlickeringStackWords,
        "expressions",
        &[
            "as the variables view shows",
            "the exact result",
            "truncated",
        ],
        STACK_CHECKS,
    );
}

#[test]
fn skewed_registers_fail_the_expressions_oracle() {
    some_failure_saying(
        Sabotage::SkewSmallRegisters,
        "expressions",
        &["which holds"],
        &[
            "variables",
            "expressions",
            "breakpoint conditions",
            "entry values",
            "backtrace",
            "stepping",
        ],
    );
}

#[test]
fn skewed_registers_fail_the_registers_oracle() {
    some_failure_saying(
        Sabotage::SkewSmallRegisters,
        "registers",
        &["frame 0 shows"],
        &[
            "registers",
            "breakpoint conditions",
            "backtrace",
            "stepping",
        ],
    );
}

#[test]
fn skewed_stack_words_fail_the_registers_oracle() {
    some_failure_with(
        Sabotage::SkewSmallStackWords,
        "registers",
        &["registers", "breakpoint conditions"],
    );
}

#[test]
fn phantom_debug_registers_fail_watch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::PhantomArming),
        "watch accounting"
    );
}

#[test]
fn missed_watch_traps_fail_watch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::MissWatchTraps),
        "watch accounting"
    );
}

#[test]
fn repeated_watch_traps_fail_watch_accounting() {
    some_failure_saying(
        Sabotage::RepeatWatchTraps,
        "watch accounting",
        &["accesses to it"],
        &["watch accounting", "protocol", "transparency"],
    );
}

#[test]
fn skipped_linked_nodes_fail_the_views_oracle() {
    some_failure_with(
        Sabotage::SkipLinkedNodes,
        "views",
        &["views", "variables", "expressions", "breakpoint conditions"],
    );
}

#[test]
fn a_call_returned_before_its_event_fails_calls() {
    assert_eq!(first_failure_with(Sabotage::ReturnBeforeEvents), "calls");
}

#[test]
fn a_skipped_call_stop_fails_catch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::SkipCallStops),
        "catch accounting"
    );
}

#[test]
fn a_misreported_call_argument_fails_catch_stops() {
    assert_eq!(
        first_failure_with(Sabotage::MisreportCallArguments),
        "catch stops"
    );
}
