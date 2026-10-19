mod support;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use microscope::{
    Breakpoint, BreakpointHit, BreakpointId, BreakpointLocation, BreakpointSpec,
    ExceptionDisposition, ExitStatus, HitCondition, InferiorState, ResumeScope, StepKind,
    StopReason, ThreadId, ThreadState, VirtualAddress,
};
use support::Scenario;

/// The compiler and linker variants of `hit-counts.c`.
const MATRIX: [&str; 4] = [
    "hit-counts-gcc-o0",
    "hit-counts-clang-o0",
    "hit-counts-clang-o2",
    "hit-counts-gcc-o2-nopie",
];

/// Unoptimized variants, whose line tables make source steps predictable.
const UNOPTIMIZED: [&str; 2] = ["hit-counts-gcc-o0", "hit-counts-clang-o0"];

/// How many times `hit-counts.c` calls `counted`.
const CALLS: u64 = 40;

/// What `hit-counts.c` adds to its second `shared` argument.
const SECOND_SITE_OFFSET: u64 = 1000;

/// How many times `hit-count-threads.c` reaches `contended`.
const CONTENDED_HITS: u64 = 4 * 100;

fn condition(text: &str) -> HitCondition {
    text.parse().expect("test hit condition")
}

/// Returns the one-based line of `hit-counts.c` containing `text`.
fn source_line(text: &str) -> u64 {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/c/hit-counts.c"
    ))
    .expect("read hit-counts.c");
    let mut lines = source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(text));
    let (index, _) = lines.next().expect("source text exists");
    assert!(lines.next().is_none(), "{text:?} is not unique");
    u64::try_from(index + 1).expect("line fits u64")
}

async fn add(scenario: &Scenario, function: &str, text: &str) -> Breakpoint {
    let spec = BreakpointSpec::Function(function.to_owned());
    let handle = scenario.handle().clone();
    scenario
        .operation(
            "add conditioned breakpoint",
            handle.add_breakpoint_with_hit_condition(spec, condition(text)),
        )
        .await
}

async fn breakpoint(scenario: &mut Scenario, id: BreakpointId) -> Breakpoint {
    scenario
        .snapshot()
        .await
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == id)
        .cloned()
        .expect("breakpoint exists")
}

/// Waits until a running program has reached breakpoint `id` `count` times.
async fn wait_for_hits(scenario: &Scenario, id: BreakpointId, count: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);

    loop {
        let snapshot = scenario
            .operation("snapshot", scenario.handle().snapshot())
            .await;
        let hit_count = snapshot
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == id)
            .expect("breakpoint exists")
            .hit_count;
        if hit_count >= count {
            return;
        }

        assert!(
            Instant::now() < deadline,
            "breakpoint {id} reached {hit_count} of {count} hits"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn global(scenario: &Scenario, name: &str) -> u64 {
    let handle = scenario.handle();
    let address = scenario.operation(name, handle.runtime_address(name)).await;
    scenario.operation(name, handle.read_word(address)).await
}

async fn program_counter(scenario: &Scenario) -> VirtualAddress {
    scenario
        .operation("location", scenario.handle().current_location())
        .await
        .address
}

async fn line(scenario: &Scenario) -> u64 {
    scenario
        .operation("location", scenario.handle().current_location())
        .await
        .image
        .source
        .expect("stop has a source line")
        .line
        .get()
}

/// The runtime addresses of a breakpoint's locations, in order.
async fn sites(scenario: &Scenario, breakpoint: &Breakpoint) -> Vec<VirtualAddress> {
    let mut sites = Vec::new();

    for resolved in breakpoint.locations.iter() {
        let BreakpointLocation::Image(image) = resolved.location else {
            panic!("function breakpoints resolve in the image");
        };

        let main = scenario
            .operation("modules", scenario.handle().loaded_modules())
            .await
            .modules[0]
            .module;

        sites.push(main.virtual_address(image).expect("relocate site"));
    }

    sites
}

fn hits(reason: &StopReason) -> &[BreakpointHit] {
    match reason {
        StopReason::Breakpoint { hits, .. } => hits,
        other => panic!("expected a breakpoint stop, got {other:?}"),
    }
}

#[tokio::test]
async fn colocated_hit_conditions_each_count_every_hit() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let mut conditions = Vec::new();
    for text in ["==3", "<3", ">=38", "%7"] {
        let added = add(&scenario, "counted", text).await;
        conditions.push((added.id, condition(text)));
    }

    // Readding a spec and condition returns the existing breakpoint.
    assert_eq!(add(&scenario, "counted", "%7").await.id, conditions[3].0);
    let first = breakpoint(&mut scenario, conditions[0].0).await;

    let mut stopped_at = Vec::new();
    let mut reason = scenario.run_to_stop().await;
    let site = sites(&scenario, &first).await;

    while let StopReason::Breakpoint { address, hits } = &reason {
        assert_eq!(
            [*address],
            site.iter().copied().map(Some).collect::<Vec<_>>()[..]
        );

        // The program's own call number names the hit.
        let hit = global(&scenario, "last_call").await + 1;
        let stopping = conditions
            .iter()
            .filter(|(_, condition)| condition.is_met(hit))
            .map(|&(breakpoint, _)| BreakpointHit {
                breakpoint,
                hit_count: hit,
            })
            .collect::<Vec<_>>();
        assert_eq!(hits[..], stopping[..], "at call {hit}");
        for &(id, _) in &conditions {
            assert_eq!(breakpoint(&mut scenario, id).await.hit_count, hit);
        }

        stopped_at.push(hit);
        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));
    assert_eq!(stopped_at, [1, 2, 3, 7, 14, 21, 28, 35, 38, 39, 40]);
    for (id, _) in conditions {
        assert_eq!(breakpoint(&mut scenario, id).await.hit_count, CALLS);
    }

    scenario.shutdown().await;
}

/// A thread that hit a breakpoint steps over the trap there when it next
/// runs, whichever breakpoint is there by then: its arrival was counted. A
/// thread a step left at an address arrives there as it resumes, for every
/// breakpoint there by then.
#[tokio::test]
async fn only_a_thread_that_hit_a_breakpoint_steps_over_one_added_where_it_stands() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let first = scenario.add_breakpoint("counted").await;
    scenario.run_to_stop().await;
    assert_eq!(global(&scenario, "last_call").await, 0);
    scenario.remove_breakpoint(first.id).await;
    let second = scenario.add_breakpoint("counted").await;
    assert_ne!(second.id, first.id);
    assert_eq!(
        hits(&scenario.resume_to_stop().await),
        [BreakpointHit {
            breakpoint: second.id,
            hit_count: 1,
        }]
    );
    assert_eq!(
        global(&scenario, "last_call").await,
        1,
        "the stop is the next call's"
    );

    scenario.remove_breakpoint(second.id).await;
    scenario.step_to_stop(StepKind::Instruction).await;
    let stepped = global(&scenario, "last_call").await;
    let here = program_counter(&scenario).await;
    let third = scenario
        .add_breakpoint_spec(BreakpointSpec::Address(here))
        .await;
    assert_eq!(
        hits(&scenario.resume_to_stop().await),
        [BreakpointHit {
            breakpoint: third.id,
            hit_count: 1,
        }]
    );
    assert_eq!(
        global(&scenario, "last_call").await,
        stepped,
        "the stop is the same call's"
    );

    let fourth = scenario.add_breakpoint("counted").await;
    assert_eq!(
        hits(&scenario.resume_to_stop().await),
        [BreakpointHit {
            breakpoint: fourth.id,
            hit_count: 1,
        }]
    );

    scenario.step_to_stop(StepKind::Instruction).await;
    assert_eq!(program_counter(&scenario).await, here);

    let handle = scenario.handle().clone();
    let fifth = scenario
        .operation(
            "add conditioned breakpoint",
            handle
                .add_breakpoint_with_hit_condition(BreakpointSpec::Address(here), condition(">=1")),
        )
        .await;

    let call = global(&scenario, "last_call").await;
    assert_eq!(
        hits(&scenario.resume_to_stop().await),
        [
            BreakpointHit {
                breakpoint: third.id,
                hit_count: 2,
            },
            BreakpointHit {
                breakpoint: fifth.id,
                hit_count: 1,
            },
        ]
    );
    assert_eq!(
        global(&scenario, "last_call").await,
        call,
        "the stop is the same call's"
    );
    scenario.shutdown().await;
}

/// A thread that hit a breakpoint and has not run since steps over the
/// trap there, whichever breakpoint it is by then, though another thread
/// ran while the breakpoint was replaced.
#[tokio::test]
async fn a_thread_kept_stopped_steps_over_a_breakpoint_replaced_where_it_hit_one() {
    let mut scenario = Scenario::launch("hit-count-threads");
    let first = scenario.add_breakpoint("contended").await;
    scenario.run_to_stop().await;
    let snapshot = scenario.snapshot().await;
    let InferiorState::Stopped {
        process_id,
        thread_id: worker,
        ..
    } = snapshot.inferior
    else {
        panic!("expected a stop, got {:?}", snapshot.inferior);
    };

    scenario.remove_breakpoint(first.id).await;

    // The main thread waits to join the workers, so it runs until paused.
    let main = ThreadId::new(process_id.get());
    let handle = scenario.handle().clone();
    let stop = snapshot.stop_id.expect("stopped");
    scenario
        .operation(
            "continue main alone",
            handle.continue_execution(stop, ResumeScope::Thread(main), ExceptionDisposition::Pass),
        )
        .await;

    let second = scenario.add_breakpoint("contended").await;
    assert_eq!(
        scenario.operation("pause", handle.pause()).await,
        StopReason::Pause
    );

    let total = global(&scenario, "total").await;
    assert_eq!(
        hits(&scenario.continue_alone_to_stop(worker).await),
        [BreakpointHit {
            breakpoint: second.id,
            hit_count: 1,
        }]
    );
    assert!(
        global(&scenario, "total").await > total,
        "the worker finished the call it stood in before the next one stopped"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn one_breakpoints_hits_are_numbered_across_all_its_locations() {
    for fixture in MATRIX {
        let mut scenario = Scenario::launch(fixture);
        let shared = add(&scenario, "shared", "%3").await;
        assert_eq!(
            shared.locations.len(),
            2,
            "{fixture}: one location per inline site"
        );

        // Hit n comes from call (n + 1) / 2, at the first site when n is odd.
        let value = |hit: u64| {
            hit.div_ceil(2)
                + if hit.is_multiple_of(2) {
                    SECOND_SITE_OFFSET
                } else {
                    0
                }
        };

        let mut reason = scenario.run_to_stop().await;
        let sites = sites(&scenario, &shared).await;
        let mut expected = 3;

        while let StopReason::Breakpoint {
            address: Some(address),
            ..
        } = &reason
        {
            assert_eq!(
                hits(&reason),
                [BreakpointHit {
                    breakpoint: shared.id,
                    hit_count: expected,
                }],
                "{fixture}"
            );

            let site = usize::from(expected.is_multiple_of(2));
            assert_eq!(*address, sites[site], "{fixture}: hit {expected}");
            assert_eq!(
                global(&scenario, "shared_total").await,
                (1..expected).map(value).sum::<u64>(),
                "{fixture}: the stop precedes exactly the earlier hits' additions"
            );
            expected += 3;
            reason = scenario.resume_to_stop().await;
        }

        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");
        assert_eq!(
            expected,
            2 * CALLS / 3 * 3 + 3,
            "{fixture}: every third hit stopped"
        );
        assert_eq!(
            breakpoint(&mut scenario, shared.id).await.hit_count,
            2 * CALLS
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn skipped_hits_are_transparent_to_next_and_finish() {
    for fixture in UNOPTIMIZED {
        let mut scenario = Scenario::launch(fixture);
        let caller = scenario.add_breakpoint("caller").await;
        assert!(matches!(
            scenario.run_to_stop().await,
            StopReason::Breakpoint { .. }
        ));

        let counted = add(&scenario, "counted", "==2").await;
        let shared = add(&scenario, "shared", "==1000").await;
        assert_eq!(line(&scenario).await, source_line("counted(call);"));

        // Stepping over each call skips the hit inside it.
        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );
        assert_eq!(line(&scenario).await, source_line("shared(call);"));
        assert_eq!(breakpoint(&mut scenario, counted.id).await.hit_count, 1);

        scenario.step_to_stop(StepKind::OverSource).await;
        assert_eq!(
            line(&scenario).await,
            source_line("shared(call + SECOND_SITE_OFFSET);")
        );
        assert_eq!(breakpoint(&mut scenario, shared.id).await.hit_count, 1);

        // Finishing the caller passes the second site's hit.
        assert_eq!(
            scenario.step_to_stop(StepKind::Out).await,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );
        assert_eq!(breakpoint(&mut scenario, shared.id).await.hit_count, 2);
        assert_eq!(global(&scenario, "last_call").await, 1);

        // A hit that meets its condition interrupts the step instead.
        assert_eq!(
            hits(&scenario.resume_to_stop().await),
            [BreakpointHit {
                breakpoint: caller.id,
                hit_count: 2,
            }]
        );
        assert_eq!(
            hits(&scenario.step_to_stop(StepKind::OverSource).await),
            [BreakpointHit {
                breakpoint: counted.id,
                hit_count: 2,
            }],
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

/// A hit skipped two calls below the line a next steps over leaves the
/// step running to the line after, not to the line after the hit's caller.
#[tokio::test]
async fn skipped_hits_deep_inside_a_call_are_transparent_to_next() {
    for fixture in UNOPTIMIZED {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main").await;
        scenario.run_to_stop().await;

        while line(&scenario).await != source_line("caller(call);") {
            scenario.step_to_stop(StepKind::OverSource).await;
        }

        let counted = add(&scenario, "counted", "==2").await;

        assert_eq!(
            scenario.step_to_stop(StepKind::OverSource).await,
            StopReason::Step {
                kind: StepKind::OverSource
            },
            "{fixture}"
        );

        let after = line(&scenario).await;
        assert!(
            after < source_line("caller(call);"),
            "{fixture}: the next ended on line {after}, not back at the loop"
        );
        assert_eq!(breakpoint(&mut scenario, counted.id).await.hit_count, 1);
        assert_eq!(global(&scenario, "last_call").await, 1);
        scenario.shutdown().await;
    }
}

/// A hit skipped just before a callee's marked epilogue, two calls below
/// the line a next steps over, is no epilogue of the stepping frame: the
/// step runs on, here to the program's exit. The simulator found nexts
/// over `main`'s call that ended inside `main`.
#[tokio::test]
async fn a_skipped_hit_before_a_deep_callees_epilogue_is_transparent_to_next() {
    let program = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("build/golden/stores/stores-clang-O2");
    let mut scenario = Scenario::new("skipped hit before an epilogue", program);
    scenario.add_breakpoint("rt_start").await;
    let options = microscope::LaunchOptions {
        arguments: vec!["1".into(), "0".into()],
        ..microscope::LaunchOptions::default()
    };

    assert!(matches!(
        scenario.run_with_to_stop(options).await,
        StopReason::Breakpoint { .. }
    ));
    scenario.remove_all_breakpoints().await;
    let calls_main = support::source_line("tests/golden/rt/rt.c", "rt_exit_group(main(");

    while line(&scenario).await != calls_main {
        scenario.step_to_stop(StepKind::OverSource).await;
    }

    // `bump` traps on the instruction its epilogue follows.
    let bump = add(&scenario, "bump", "==1000").await;

    assert_eq!(
        scenario.step_to_stop(StepKind::OverSource).await,
        StopReason::Exited(ExitStatus::Code(11))
    );
    assert_eq!(breakpoint(&mut scenario, bump.id).await.hit_count, 1);
    scenario.shutdown().await;
}

#[tokio::test]
async fn a_step_onto_a_skipped_site_stops_there_and_counts_the_hit_once() {
    for fixture in UNOPTIMIZED {
        let mut scenario = Scenario::launch(fixture);
        let caller = scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;
        let body = scenario
            .add_breakpoint_spec(BreakpointSpec::Source {
                path: "hit-counts.c".into(),
                line: microscope::LineNumber::new(source_line("shared_total += value;"))
                    .expect("one-based"),
            })
            .await;
        let body = scenario
            .operation(
                "condition",
                scenario
                    .handle()
                    .set_breakpoint_hit_condition(body.id, Some(condition("==1000"))),
            )
            .await;
        let sites = sites(&scenario, &body).await;

        for call in 1..=2 {
            scenario.step_to_stop(StepKind::OverSource).await;
            assert_eq!(line(&scenario).await, source_line("shared(call);"));

            // The source step reaches the skipped site's statement and ends
            // there, counting the hit the trap reported.
            assert_eq!(
                scenario.step_to_stop(StepKind::IntoSource).await,
                StopReason::Step {
                    kind: StepKind::IntoSource
                },
                "{fixture}"
            );
            assert_eq!(program_counter(&scenario).await, sites[0], "{fixture}");
            assert_eq!(
                breakpoint(&mut scenario, body.id).await.hit_count,
                2 * call - 1
            );
            if call == 1 {
                // The instruction under the trap runs; the hit is not
                // counted again.
                scenario.step_to_stop(StepKind::Instruction).await;
                assert!(program_counter(&scenario).await > sites[0], "{fixture}");
                assert_eq!(
                    breakpoint(&mut scenario, body.id).await.hit_count,
                    2 * call - 1
                );
            }

            // Continuing passes the second site once, then reaches the
            // next call.
            assert_eq!(
                hits(&scenario.resume_to_stop().await),
                [BreakpointHit {
                    breakpoint: caller.id,
                    hit_count: call + 1,
                }]
            );
            assert_eq!(breakpoint(&mut scenario, body.id).await.hit_count, 2 * call);
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_next_ending_on_a_skipped_site_steps_over_it_before_running_on() {
    for fixture in UNOPTIMIZED {
        let mut scenario = Scenario::launch(fixture);
        let caller = scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;
        let destination = source_line("shared(call + SECOND_SITE_OFFSET);");
        let skipped = scenario
            .add_breakpoint_spec(BreakpointSpec::Source {
                path: "hit-counts.c".into(),
                line: microscope::LineNumber::new(destination).expect("one-based"),
            })
            .await;

        scenario
            .operation(
                "condition",
                scenario
                    .handle()
                    .set_breakpoint_hit_condition(skipped.id, Some(condition("==1000"))),
            )
            .await;

        for call in 1..=2 {
            scenario.step_to_stop(StepKind::OverSource).await;
            assert_eq!(line(&scenario).await, source_line("shared(call);"));

            // The step's own trap and the skipped breakpoint share the site.
            assert_eq!(
                scenario.step_to_stop(StepKind::OverSource).await,
                StopReason::Step {
                    kind: StepKind::OverSource
                },
                "{fixture}"
            );
            assert_eq!(line(&scenario).await, destination, "{fixture}");
            assert_eq!(breakpoint(&mut scenario, skipped.id).await.hit_count, call);

            // Leaving the site runs its instruction instead of trapping again.
            let site = program_counter(&scenario).await;
            scenario.step_to_stop(StepKind::OverSource).await;
            assert!(program_counter(&scenario).await > site, "{fixture}");
            assert_eq!(breakpoint(&mut scenario, skipped.id).await.hit_count, call);
            assert_eq!(
                hits(&scenario.resume_to_stop().await),
                [BreakpointHit {
                    breakpoint: caller.id,
                    hit_count: call + 1,
                }]
            );
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn concurrent_hits_are_each_counted_exactly_once() {
    let mut scenario = Scenario::launch("hit-count-threads");
    let every = add(&scenario, "contended", "%25").await;
    let never = add(&scenario, "contended", "==1000000").await;

    let mut accepted = Vec::new();
    let mut reason = scenario.run_to_stop().await;

    while matches!(reason, StopReason::Breakpoint { .. }) {
        assert_eq!(hits(&reason).len(), 1, "{reason:?}");

        // A sibling may have stopped at the site with a hit of its own.
        for thread in scenario.snapshot().await.threads.iter() {
            if let ThreadState::Stopped {
                reason: Some(thread_reason),
            } = &thread.state
            {
                accepted.extend(hits(thread_reason).iter().map(|hit| hit.hit_count));
            }
        }

        reason = scenario.resume_to_stop().await;
    }

    assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)));

    let mut expected = (25..=CONTENDED_HITS).step_by(25).collect::<Vec<_>>();
    accepted.sort_unstable();
    expected.sort_unstable();
    assert_eq!(accepted, expected, "every 25th hit stopped exactly once");
    assert_eq!(
        breakpoint(&mut scenario, every.id).await.hit_count,
        CONTENDED_HITS
    );
    assert_eq!(
        breakpoint(&mut scenario, never.id).await.hit_count,
        CONTENDED_HITS
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn signals_during_internal_stops_are_reported_and_no_hit_is_lost() {
    let mut scenario = Scenario::launch("hit-count-signals");
    let contended = add(&scenario, "contended", "==1000000000").await;
    let finished = scenario.add_breakpoint("finished").await;

    let mut signals = 0;
    let mut reason = scenario.run_to_stop().await;

    while let StopReason::Exception(exception) = &reason {
        assert_eq!(exception.code, nix::sys::signal::Signal::SIGUSR1 as u64);
        signals += 1;
        reason = scenario
            .resume_with_exception(ExceptionDisposition::Pass)
            .await;
    }

    assert_eq!(
        hits(&reason),
        [BreakpointHit {
            breakpoint: finished.id,
            hit_count: 1,
        }]
    );
    assert_eq!(signals, 24);
    let calls = global(&scenario, "calls").await;

    // The program sends its last signal once this many calls were made.
    assert!(calls >= 24 * 20, "workers called while signals arrived");
    assert_eq!(
        breakpoint(&mut scenario, contended.id).await.hit_count,
        calls
    );
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn pause_amend_and_shutdown_interrupt_endless_skipped_hits() {
    let mut scenario = Scenario::launch("hit-count-spin");
    let spun = add(&scenario, "spun", "==1000000000").await;

    // A pause lands among internal stops and is reported as a pause.
    let running = scenario.start_running().await;
    wait_for_hits(&scenario, spun.id, 100).await;
    scenario.operation("pause", scenario.handle().pause()).await;
    let paused = running.await.expect("run task").expect("run");
    assert_eq!(paused, StopReason::Pause);
    let at_pause = breakpoint(&mut scenario, spun.id).await.hit_count;
    let spins = global(&scenario, "spins").await;

    // A thread stopped at the site has a counted hit it has not executed.
    assert!(
        spins <= at_pause && at_pause <= spins + 4,
        "{spins} spins, {at_pause} hits"
    );

    // An amended condition applies to the running program's next hit.
    let resumed = scenario.start_resuming().await;
    let amended = scenario
        .operation(
            "amend while running",
            scenario
                .handle()
                .set_breakpoint_hit_condition(spun.id, Some(condition(">=1"))),
        )
        .await;
    assert!(amended.hit_count >= at_pause);
    let stop = resumed.await.expect("resume task").expect("resume");
    let [hit] = hits(&stop) else {
        panic!("one breakpoint stopped: {stop:?}");
    };

    assert_eq!(hit.breakpoint, spun.id);
    assert!(hit.hit_count > amended.hit_count);
    assert_eq!(
        breakpoint(&mut scenario, spun.id).await.hit_condition,
        Some(condition(">=1"))
    );

    // Shutting down while hits are being skipped reaps the inferior.
    scenario
        .operation(
            "restore condition",
            scenario
                .handle()
                .set_breakpoint_hit_condition(spun.id, Some(condition("==1000000000"))),
        )
        .await;

    let _running = scenario.start_resuming().await;
    wait_for_hits(&scenario, spun.id, hit.hit_count + 100).await;
    scenario.shutdown().await;
}

#[tokio::test]
async fn attached_processes_skip_hits_and_detach_mid_run_unharmed() {
    let child = support::ExternalProcess::spawn_running(&Scenario::fixture("hit-count-spin"));

    // Every thread exists before the first attach, so all survive to the second.
    let tasks = format!("/proc/{}/task", child.process_id());
    support::wait_until("the fixture started its workers", || {
        std::fs::read_dir(&tasks).is_ok_and(|entries| entries.count() == 4)
    });
    let mut scenario = Scenario::attached("attached hit counts", child.attach().await);
    let spun = add(&scenario, "spun", "%50").await;

    for stop in 1..=3 {
        let reason = scenario.resume_to_stop().await;
        let [hit] = hits(&reason) else {
            panic!("one breakpoint stopped: {reason:?}");
        };

        assert_eq!(hit.breakpoint, spun.id);
        assert_eq!(hit.hit_count, 50 * stop);
    }

    let spins = global(&scenario, "spins").await;

    // Detaching while hits are being skipped removes every trap first.
    scenario
        .operation(
            "never stop",
            scenario
                .handle()
                .set_breakpoint_hit_condition(spun.id, Some(condition("==1000000000"))),
        )
        .await;

    let _running = scenario.start_resuming().await;
    wait_for_hits(&scenario, spun.id, 350).await;
    scenario.shutdown().await;

    // A process killed by a leftover trap could not be attached again.
    let mut reattached = Scenario::attached("reattached", child.attach().await);
    assert_eq!(
        reattached.snapshot().await.threads.len(),
        4,
        "every thread survived"
    );
    assert!(global(&reattached, "spins").await >= spins + 190);
    reattached.shutdown().await;
}

#[tokio::test]
async fn hit_counts_start_again_in_each_process() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let counted = add(&scenario, "counted", ">=39").await;

    for _ in 0..2 {
        assert_eq!(
            hits(&scenario.run_to_stop().await),
            [BreakpointHit {
                breakpoint: counted.id,
                hit_count: 39,
            }]
        );
        scenario.resume_to_stop().await;
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert_eq!(breakpoint(&mut scenario, counted.id).await.hit_count, CALLS);
    }

    scenario.shutdown().await;
}

#[tokio::test]
async fn hit_condition_requests_reject_unknown_breakpoints_and_keep_definitions() {
    let mut scenario = Scenario::launch("hit-counts-gcc-o0");
    let plain = scenario.add_breakpoint("counted").await;
    let conditioned = add(&scenario, "counted", "==2").await;
    assert_ne!(
        plain.id, conditioned.id,
        "conditions distinguish breakpoints"
    );
    assert_eq!(scenario.add_breakpoint("counted").await.id, plain.id);

    let missing = scenario
        .attempt(
            "amend missing",
            scenario
                .handle()
                .set_breakpoint_hit_condition(BreakpointId::new(99), None),
        )
        .await;
    assert!(matches!(
        missing,
        Err(microscope::Error::BreakpointNotFound(99))
    ));

    scenario.run_to_stop().await;
    let cleared = scenario
        .operation(
            "clear condition",
            scenario
                .handle()
                .set_breakpoint_hit_condition(conditioned.id, None),
        )
        .await;
    assert_eq!(
        cleared,
        Breakpoint {
            hit_condition: None,
            hit_count: 1,
            ..conditioned.clone()
        }
    );

    let both = scenario.resume_to_stop().await;
    let ids = hits(&both)
        .iter()
        .map(|hit| (hit.breakpoint, hit.hit_count))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        ids,
        BTreeSet::from([(plain.id, 2), (conditioned.id, 2)]),
        "both breakpoints at the site stop"
    );
    scenario.shutdown().await;
}

#[tokio::test]
async fn stepping_one_instruction_through_a_skipped_trap_counts_it_once_and_moves_on() {
    for fixture in UNOPTIMIZED {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("caller").await;
        scenario.run_to_stop().await;
        let entry = scenario
            .operation("symbol", scenario.handle().runtime_address("counted"))
            .await;
        scenario
            .add_breakpoint_spec(BreakpointSpec::Address(entry))
            .await;

        let counted = add(&scenario, "counted", "==1000").await;
        assert_eq!(
            support::breakpoint_address(&scenario.resume_to_stop().await),
            entry
        );
        let site = sites(&scenario, &counted).await[0];
        assert!(site > entry, "{fixture}: the prologue precedes the site");

        // Reaching the site's address by single steps executes no trap.
        while program_counter(&scenario).await != site {
            assert!(program_counter(&scenario).await < site, "{fixture}");
            scenario.step_to_stop(StepKind::Instruction).await;
        }

        assert_eq!(breakpoint(&mut scenario, counted.id).await.hit_count, 0);

        // The next single step executes the trap, which counts the hit, and
        // then the instruction under it.
        assert_eq!(
            scenario.step_to_stop(StepKind::Instruction).await,
            StopReason::Step {
                kind: StepKind::Instruction
            }
        );
        assert!(program_counter(&scenario).await > site, "{fixture}");
        assert_eq!(breakpoint(&mut scenario, counted.id).await.hit_count, 1);
        assert_eq!(global(&scenario, "last_call").await, 0);
        scenario.shutdown().await;
    }
}

/// How many lines of `hit-counts.c` resolve to code in every variant.
const MIN_LINE_BREAKPOINTS: usize = 16;

/// Runs a fixed script of steps from the first stop in `caller` and returns
/// where each stopped. With `skipping`, every source line and function first
/// gets a breakpoint that skips all of its hits.
async fn step_script(fixture: &str, skipping: bool) -> (Vec<(StopReason, VirtualAddress)>, u64) {
    use StepKind::{Instruction, IntoSource, Out, OverSource};
    let mut scenario = Scenario::launch(fixture);
    scenario.add_breakpoint("caller").await;
    scenario.run_to_stop().await;
    let mut skipped = Vec::new();
    if skipping {
        let handle = scenario.handle().clone();
        for function in ["counted", "caller", "main"] {
            skipped.push(add(&scenario, function, "==1000000").await.id);
        }

        for line in 1..=31 {
            let spec = BreakpointSpec::Source {
                path: "hit-counts.c".into(),
                line: microscope::LineNumber::new(line).expect("one-based"),
            };

            match scenario
                .attempt(
                    "skipping line breakpoint",
                    handle.add_breakpoint_with_hit_condition(spec, condition("==1000000")),
                )
                .await
            {
                Ok(added) => skipped.push(added.id),
                // Lines outside every function have no code.
                Err(microscope::Error::SourceLineUnavailable { .. }) => {}
                Err(error) => panic!("{fixture}: line {line}: {error}"),
            }
        }

        assert!(
            skipped.len() >= 3 + MIN_LINE_BREAKPOINTS,
            "{fixture}: only {} breakpoints skip hits",
            skipped.len()
        );
    }

    let script = [
        OverSource,
        OverSource,
        OverSource,
        Instruction,
        OverSource,
        OverSource,
        OverSource,
        IntoSource,
        IntoSource,
        IntoSource,
        Out,
        OverSource,
        OverSource,
        Instruction,
        Instruction,
        IntoSource,
        IntoSource,
        OverSource,
        OverSource,
        OverSource,
        OverSource,
        IntoSource,
        IntoSource,
        OverSource,
        OverSource,
        OverSource,
        OverSource,
        OverSource,
    ];

    let mut stops = Vec::new();
    for kind in script {
        let reason = scenario.step_to_stop(kind).await;
        if matches!(reason, StopReason::Exited(_)) {
            stops.push((reason, VirtualAddress::new(0)));
            break;
        }

        stops.push((reason, program_counter(&scenario).await));
    }

    let snapshot = scenario.snapshot().await;
    let skipped_hits = snapshot
        .breakpoints
        .iter()
        .filter(|breakpoint| skipped.contains(&breakpoint.id))
        .map(|breakpoint| breakpoint.hit_count)
        .sum();
    scenario.shutdown().await;
    (stops, skipped_hits)
}

#[tokio::test]
async fn breakpoints_that_skip_every_hit_never_change_where_steps_stop() {
    for fixture in MATRIX {
        let (plain, _) = step_script(fixture, false).await;
        let (skipping, skipped_hits) = step_script(fixture, true).await;
        assert_eq!(skipping, plain, "{fixture}");
        assert!(
            skipped_hits >= 10,
            "{fixture}: the steps passed skipped sites"
        );
    }
}
