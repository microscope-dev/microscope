use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::process::Stdio;

use microscope::{ExecutionContext, LaunchOptions, StackFrameId, StopContext, StopReason};

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};
use crate::workers::tasks;

const FIXTURE: &str = "tokio-corrupt-o0";

#[derive(Debug, Default)]
struct Truth {
    shards: BTreeMap<u64, u64>,
    damaged: Vec<(String, u64, u64)>,
}

impl Truth {
    fn parse(text: &str) -> Self {
        let mut truth = Self::default();
        for fields in Report::parse(text).lines() {
            match fields.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["shard", id, shard] => {
                    truth
                        .shards
                        .insert(id.parse().expect("an id"), shard.parse().expect("a shard"));
                }
                ["damaged", kind, id, shard] => truth.damaged.push((
                    kind.to_owned(),
                    id.parse().expect("an id"),
                    shard.parse().expect("a shard"),
                )),
                _ => {}
            }
        }

        truth
    }
}

#[tokio::test]
async fn a_damaged_list_is_reported_and_the_rest_listed() {
    let scratch = ScratchDir::new("corrupt");
    let output = scratch.path().join("stdout");

    let mut scenario = Scenario::launch(FIXTURE);
    scenario.add_breakpoint("truth_reached").await;

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
            ..LaunchOptions::default()
        })
        .await;

    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );

    let (intact, gaps) = tasks(&scenario, 7).await;
    assert!(gaps.is_empty(), "{gaps:?}");

    let truth = Truth::parse(&std::fs::read_to_string(&output).expect("the output"));

    assert_eq!(
        intact
            .iter()
            .map(|task| task.id.number)
            .collect::<BTreeSet<_>>(),
        truth.shards.keys().copied().collect(),
    );

    assert!(matches!(
        scenario.resume_to_stop().await,
        StopReason::Breakpoint { .. }
    ));

    let truth = Truth::parse(&std::fs::read_to_string(&output).expect("the output"));
    assert_eq!(truth.damaged.len(), 4, "{truth:?}");

    let (listed, gaps) = tasks(&scenario, 7).await;
    let numbers = listed.iter().map(|task| task.id.number).collect::<Vec<_>>();
    let unique = numbers.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), numbers.len(), "listed twice: {numbers:?}");

    assert!(
        unique.is_subset(&truth.shards.keys().copied().collect()),
        "{numbers:?}"
    );

    let damaged_shards = truth
        .damaged
        .iter()
        .map(|(_, _, shard)| *shard)
        .collect::<BTreeSet<_>>();

    for (id, shard) in &truth.shards {
        if !damaged_shards.contains(shard) {
            assert!(unique.contains(id), "task {id} of shard {shard}: {gaps:?}");
        }
    }

    for (kind, id, _) in &truth.damaged {
        let reason = match kind.as_str() {
            "owner" => "belongs to task list",
            "vtable" => "polls no task",
            "broken" => "the task at 0x10 is unreadable",
            _ => "links back to",
        };

        assert!(
            gaps.iter().any(|gap| gap.contains(reason)),
            "{kind}: {gaps:?}"
        );

        let refused = matches!(kind.as_str(), "owner" | "vtable");
        assert_eq!(unique.contains(id), !refused, "{kind} {id}: {numbers:?}");
    }

    let stop = scenario.snapshot().await.stop_id.expect("stopped");
    let survivor = listed
        .iter()
        .find(|task| !damaged_shards.contains(&truth.shards[&task.id.number]))
        .expect("a task of an undamaged shard");

    let trace = scenario
        .operation(
            "backtrace",
            scenario
                .handle()
                .at(StopContext {
                    stop,
                    execution: ExecutionContext::Task(survivor.id),
                    frame: StackFrameId::INNERMOST,
                })
                .backtrace(),
        )
        .await;

    assert!(
        trace.frames.iter().any(|frame| frame
            .function
            .as_ref()
            .is_some_and(|f| &*f.name == "parked")),
        "{trace:#?}"
    );

    scenario.shutdown().await;
}
