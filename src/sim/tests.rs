//! The simulation's gate: fixed seeds over every program and variant, and
//! a check that a seed always names the same run.

use std::num::NonZeroUsize;

use super::marks::Reach;
use super::report::{Failure, FailureKind};
use super::shrink;
use super::world::Sabotage;
use super::{Corpus, Settings, describe_failure, run};

/// How many fixed seeds the gate runs.
const GATE_SEEDS: u64 = 2000;

/// How many fixed seeds must reach each mark. Each mark is reached by
/// about eleven of every 2000 seeds or more, so a change that redraws the
/// seeds fails the floor by chance about once in 300 times, and a mark that
/// becomes a quarter as common fails it.
const MARK_FLOOR: u64 = 3;

/// How many seeds the determinism check runs twice.
const DETERMINISM_SEEDS: u64 = 32;

/// Every fixed seed runs a session the debugger handles correctly, and
/// enough of them reach each mark.
#[test]
fn fixed_seeds_pass_every_oracle() {
    let corpus = Corpus::load().expect("load the golden corpus");
    let settings = Settings::default();
    let threads = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let mut failures = Vec::new();
    let mut reach = Reach::default();

    // This is the suite's slowest test and its seeds are independent, so
    // they run on every core. The floor still counts all of them.
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

/// A seed names one run: running it again, after other runs in the same
/// process, records the same trace.
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

/// A planted bug's failing seed shrinks to a run that fails the same way
/// and is no larger, and the file the shrinker writes runs it again.
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

/// Runs fixed seeds with `sabotage` until one fails, returning its
/// failure's check.
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

/// A temporary breakpoint whose trap the CPU skips lets a hit go unseen,
/// which breakpoint accounting catches as it does for any breakpoint.
#[test]
fn a_skipped_temporary_trap_fails_breakpoint_accounting() {
    assert!(fails_with(
        Sabotage::SkipTemporaryTraps,
        &[("breakpoint accounting", "without a trap reporting it")]
    ));
}

/// A kernel that misreports a call's first argument makes the debugger
/// resume calls an `$arg0` condition stops without reporting them, which
/// catch accounting catches.
#[test]
fn a_misread_first_argument_fails_catch_accounting() {
    assert!(fails_with(
        Sabotage::MisreportFirstCallArgument,
        &[("catch accounting", "with no stop reporting it")]
    ));
}

/// A program released from a trap's stop with its SIGTRAP dies of it,
/// which transparency catches.
#[test]
fn traps_passed_on_detach_fail_transparency() {
    assert!(fails_with(
        Sabotage::PassTrapsOnDetach,
        &[("transparency", "died of signal 5")]
    ));
}

/// A kernel that misplaces the program's mappings makes the debugger
/// misplace the program. The client's check of where the debugger says it
/// loaded catches it, the frames oracle catches lines read from other code
/// than the frames are in, and a watch on an object by name watches other
/// memory than the symbol table says the object is in.
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

/// Whether fixed seeds run with `sabotage` fail in each of `ways`, a check
/// and part of its message, sweeping the seeds once for all of them.
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

/// A request the controller drops unanswered fails even while the program
/// is ending, when the client excuses most failures.
#[test]
fn a_reply_dropped_while_ending_fails() {
    assert!(fails_with(
        Sabotage::DropRequestsWhileEnding,
        &[("lost reply", "dropped the request without answering it")]
    ));
}

/// A kill the controller drops while a launch or attach waits for its
/// first stop fails. Only a client that sends a kill before its start is
/// answered reaches that state.
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

/// A kernel that loses the debugger's writes leaves traps the controller
/// believes installed missing from memory, which code integrity catches.
#[test]
fn lost_writes_fail_code_integrity() {
    assert_eq!(first_failure_with(Sabotage::LosePokes), "code integrity");
}

/// A waiter that never reaps leaves the client waiting with nothing left
/// to happen, which liveness catches.
#[test]
fn a_deaf_waiter_fails_liveness() {
    assert_eq!(first_failure_with(Sabotage::DeafWaiter), "liveness");
}

/// A thread that runs again while a stop is published, behind the
/// controller's back, breaks all-stop, which the oracle catches.
#[test]
fn a_thread_resumed_behind_the_controller_fails_all_stop() {
    assert_eq!(
        first_failure_with(Sabotage::ResumeBehindTheController),
        "all-stop"
    );
}

/// A thread that runs again while the controller holds it stopped, before
/// any stop is published, fails held stops.
#[test]
fn a_thread_resumed_while_gathering_fails_held_stops() {
    assert_eq!(
        first_failure_with(Sabotage::ResumeWhileGathering),
        "held stops"
    );
}

/// A kernel that loses the controller's requests to stop leaves it waiting
/// for stops that never come, which requested stops catches before the
/// run runs out of things to do.
#[test]
fn lost_stop_requests_fail_requested_stops() {
    assert_eq!(
        first_failure_with(Sabotage::LoseStopRequests),
        "requested stops"
    );
}

/// A CPU that executes the program's own instruction under a user's trap
/// lets a hit go unseen, which breakpoint accounting catches.
#[test]
fn a_skipped_trap_fails_breakpoint_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::SkipTraps),
        "breakpoint accounting"
    );
}

/// A kernel that misreports return addresses makes the debugger unwind to
/// callers that are not, which the backtrace oracle catches.
#[test]
fn skewed_return_addresses_fail_the_backtrace_oracle() {
    assert_eq!(
        first_failure_with(Sabotage::SkewReturnAddresses),
        "backtrace"
    );
}

/// Samples whose stack copies misreport return addresses make the
/// profiler unwind to callers that are not, which the samples oracle
/// catches.
#[test]
fn skewed_sampled_return_addresses_fail_the_samples_oracle() {
    assert_eq!(
        first_failure_with(Sabotage::SkewSampledReturnAddresses),
        "samples"
    );
}

/// A CPU whose single steps run on executes more than a step allows, which
/// the stepping oracle catches.
#[test]
fn late_single_steps_fail_the_stepping_oracle() {
    assert_eq!(first_failure_with(Sabotage::LateSingleSteps), "stepping");
}

/// A kernel that hides a call the stepping thread stands at makes a step
/// into another call of the line enter that one, which the stepping oracle
/// catches, if code integrity has not caught the lie first. A debugger
/// misled about where an instruction ends can also send a thread into the
/// middle of one, which the CPU cannot decode, a gap, within the action
/// before any oracle looks.
#[test]
fn hidden_calls_fail_the_stepping_oracle() {
    some_failure_past(
        Sabotage::HideSteppedCalls,
        "stepping",
        &["into the call returning to"],
        // Reading the code the thread stands at shows the lie too.
        &["stepping", "code integrity"],
        true,
    );
}

/// Runs fixed seeds with `sabotage` until one fails `check`, requiring
/// every failure on the way to be one of `allowed`.
fn some_failure_with(sabotage: Sabotage, check: &str, allowed: &[&str]) {
    some_failure_saying(sabotage, check, &[""], allowed);
}

/// Like [`some_failure_with`], for failures of `check` whose messages
/// contain each of `sayings`, sweeping the seeds once for all of them.
fn some_failure_saying(sabotage: Sabotage, check: &str, sayings: &[&str], allowed: &[&str]) {
    some_failure_past(sabotage, check, sayings, allowed, false);
}

/// Like [`some_failure_saying`], also passing over model gaps on the way
/// when `gaps` says so.
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

/// Like [`some_failure_past`], for programs of `corpus`.
fn some_failure_in(
    corpus: &Corpus,
    sabotage: Sabotage,
    check: &str,
    sayings: &[&str],
    allowed: &[&str],
    gaps: bool,
) {
    // The checks of what the client read judge the same values, and one
    // catches most lies first, so the others pass while one is tested.
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

/// A kernel that resumes a thread's stopped siblings with it makes them run
/// while the client continued one thread alone, which thread scope
/// catches. Siblings running behind the controller's back break much else,
/// which other checks may catch first in other runs.
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

/// How many seeds a sabotage test may run to find its failure. Most find it
/// in far fewer; a lie only a few seeds in a thousand show still makes it
/// fail by chance almost never.
const SABOTAGE_SEEDS: u64 = 4 * GATE_SEEDS;

/// The checks that read variables, any of which may catch a misreported
/// stack first.
const STACK_CHECKS: &[&str] = &[
    "variables",
    "expressions",
    "breakpoint conditions",
    "entry values",
    // What a stop at a call shows of a buffer on the stack.
    "catch stops",
];

/// The checks the world makes of what the client read, which a sabotage
/// test can mute: those of [`STACK_CHECKS`] it judges, and registers.
const JUDGED_STACK_CHECKS: &[&str] = &[
    "variables",
    "expressions",
    "entry values",
    "catch stops",
    "registers",
];

/// A kernel that misreports small numbers on the stack shows variables
/// with wrong values, which the variables oracle catches. The other
/// checks read the same values, and may catch it first.
#[test]
fn skewed_stack_words_fail_the_variables_oracle() {
    some_failure_with(Sabotage::SkewSmallStackWords, "variables", STACK_CHECKS);
}

/// The same misreported values make a marker's condition, or what it
/// expects, evaluated as an expression, false where it must hold, and show
/// bytes that memory does not hold, which the expressions oracle catches
/// every way.
#[test]
fn skewed_stack_words_fail_the_expressions_oracle() {
    some_failure_saying(
        Sabotage::SkewSmallStackWords,
        "expressions",
        &["not true", "which holds", "expected"],
        STACK_CHECKS,
    );
}

/// The same misreported values change the registers a caller saved there,
/// from which the debugger recovers what the caller passed, which the
/// entry values oracle catches. Only optimized code keeps a variable where
/// its function was entered with it, and unoptimized code keeps its
/// variables on the stack, where the other checks catch the lie first, so
/// only the optimized programs are run.
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

/// A kernel whose reads of an unchanged stack value disagree makes
/// conditions the client knows hold or fail decide hits wrongly, which the
/// breakpoint-conditions check catches.
#[test]
fn flickering_stack_words_fail_breakpoint_conditions() {
    some_failure_with(
        Sabotage::FlickeringStackWords,
        "breakpoint conditions",
        STACK_CHECKS,
    );
}

/// A kernel whose reads of an unchanged stack value disagree shows a
/// variable with one value and evaluates its name, arithmetic over it, and
/// casts of it with another, which the expressions oracle catches each way.
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

/// A kernel that misreports small numbers in registers shows values held
/// there that the registers do not hold, which the expressions oracle
/// catches.
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

/// The same misreported registers are what the frame a thread stopped in
/// shows, which the registers oracle catches.
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

/// Misreported small numbers on the stack change the registers a caller
/// saved there, which its frame shows, and the registers oracle catches.
#[test]
fn skewed_stack_words_fail_the_registers_oracle() {
    some_failure_with(
        Sabotage::SkewSmallStackWords,
        "registers",
        &["registers", "breakpoint conditions"],
    );
}

/// A kernel that keeps a thread's debug-register writes in a copy that
/// reads them back arms nothing, so the thread's accesses to watched
/// memory go unseen, which watch accounting catches.
#[test]
fn phantom_debug_registers_fail_watch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::PhantomArming),
        "watch accounting"
    );
}

/// A CPU that takes no debug exception for a watched access lets the
/// debugger report no hit, which watch accounting catches.
#[test]
fn missed_watch_traps_fail_watch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::MissWatchTraps),
        "watch accounting"
    );
}

/// A CPU that raises a watched access's debug exception again after the
/// next instruction makes the debugger count a hit no access made, which
/// watch accounting catches even when a condition declines both hits. A
/// repeated exception naming a slot whose watchpoint was removed meanwhile
/// is a stop the debugger cannot classify, which the client reports. One
/// raised again after a detach kills the released program with SIGTRAP,
/// which transparency reports.
#[test]
fn repeated_watch_traps_fail_watch_accounting() {
    some_failure_saying(
        Sabotage::RepeatWatchTraps,
        "watch accounting",
        &["accesses to it"],
        &["watch accounting", "protocol", "transparency"],
    );
}

/// A kernel whose reads skip a linked node makes the debugger present a
/// list without one of its elements, which the views oracle catches.
#[test]
fn skipped_linked_nodes_fail_the_views_oracle() {
    some_failure_with(
        Sabotage::SkipLinkedNodes,
        "views",
        &["views", "variables", "expressions", "breakpoint conditions"],
    );
}

/// A kernel that returns from a call before its event stop leaves the
/// controller holding a thread inside a call that is over, which the calls
/// oracle catches.
#[test]
fn a_call_returned_before_its_event_fails_calls() {
    assert_eq!(first_failure_with(Sabotage::ReturnBeforeEvents), "calls");
}

/// A kernel that lets a call a catchpoint stops run past its entry, though
/// the thread was resumed to stop there, hides it from the debugger, which
/// catch accounting catches.
#[test]
fn a_skipped_call_stop_fails_catch_accounting() {
    assert_eq!(
        first_failure_with(Sabotage::SkipCallStops),
        "catch accounting"
    );
}

/// A kernel that misreports a call's argument makes the debugger report a
/// call the thread did not make, which catch stops catch.
#[test]
fn a_misreported_call_argument_fails_catch_stops() {
    assert_eq!(
        first_failure_with(Sabotage::MisreportCallArguments),
        "catch stops"
    );
}
