use std::process::Stdio;

use microscope::{Expression, LaunchOptions, StepKind, StopReason, ThreadActivity, ThreadState};

use crate::stops::place;
use crate::support::{self, Scenario};

const SOURCE: &str = "tests/fixtures/go/callers/main.go";

#[tokio::test]
async fn a_stripped_program_says_its_goroutines_are_unavailable() {
    for fixture in ["callers-go-stripped", "callers-go-external-stripped"] {
        let mut scenario = Scenario::launch(fixture);
        scenario.add_breakpoint("main.reached").await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        let page = scenario
            .operation("tasks", scenario.handle().tasks(None, 16))
            .await;
        assert!(page.tasks.is_empty(), "{fixture}: {page:?}");
        let [gap] = page.gaps.as_ref() else {
            panic!("{fixture}: {page:?}");
        };

        assert!(gap.contains("debug information"), "{fixture}: {gap}");

        for thread in scenario.snapshot().await.threads.iter() {
            assert!(
                matches!(thread.state, ThreadState::Stopped { .. }),
                "{fixture}"
            );
            assert_eq!(
                thread.activity,
                Some(ThreadActivity::Unknown(gap.clone())),
                "{fixture}"
            );
        }

        let task = Expression::parse("$task").expect("parses");
        let refused = scenario
            .handle()
            .evaluate(&task)
            .await
            .expect_err("the goroutine is unknown");
        assert!(
            refused.to_string().contains(gap.as_ref()),
            "{fixture}: {refused}"
        );

        scenario.step_to_stop(StepKind::OverSource).await;
        assert_eq!(
            place(&scenario).await,
            (
                "main.reached".to_owned(),
                support::source_line(SOURCE, "sink += len(name)")
            ),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}
