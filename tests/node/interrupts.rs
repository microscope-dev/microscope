//! Interrupts of a Node program: one that runs JavaScript pauses in V8's
//! inspector, at a statement with its locals, and one that runs none
//! stops natively instead. A pause V8 takes after that native stop, at
//! the next statement the program runs, never stops it.

use microscope::VariableState;

use crate::oracle;
use crate::session::{MATRIX, NodeSession};

const FIXTURE: &str = "interrupts.js";

/// The lines of `spin()`'s loop, in the fixture.
const SPIN: std::ops::RangeInclusive<u32> = 9..=11;

/// What the selected frame shows for the variable `name`: its summary,
/// `unavailable` for one V8 keeps where microscope cannot read it, or
/// `absent`.
fn shown(variables: &[microscope::Variable], name: &str) -> String {
    let Some(variable) = variables.iter().find(|variable| &*variable.name == name) else {
        return "absent".to_owned();
    };
    match &variable.state {
        VariableState::Available {
            presentation: Some(presentation),
            ..
        } => presentation.summary.to_string(),
        VariableState::Unavailable(_) => "unavailable".to_owned(),
        other => format!("{other:?}"),
    }
}

/// On every build, an interrupt stops a program waiting in native code,
/// and one spinning in JavaScript pauses inside `spin()`, whose locals are
/// the ones V8 lists there. The loop runs optimized by then, so a local
/// is unavailable rather than read from where it is not, and the
/// argument passed is still read from its slot.
#[tokio::test]
async fn interrupts_pause_running_javascript_where_it_runs() {
    let mut lies = Vec::new();
    for entry in MATRIX {
        let mut session = NodeSession::launch(entry, FIXTURE, &[], &[]).await;
        let context = session.context();
        session.wrote("IDLE").await;
        // V8 cannot pause in `fs.readSync`, so this stop is native.
        session.pause().await;

        // The line wakes the program, whose next statement is where V8
        // takes the pause it was asked for; the program runs on.
        session.proceed().await;
        session.wrote("SPINNING").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // The second interrupt follows a resume at once, while V8 may not
        // yet have left its pause.
        for round in 0..2 {
            session.pause().await;
            let pause = oracle::selected(&session.scenario).await;
            if pause.function != "spin" || !SPIN.contains(&pause.line) {
                lies.push(format!("{context}: interrupt {round} paused at {pause}"));
            }
            let variables = session
                .scenario
                .operation("variables", session.scenario.handle().variables())
                .await;
            let seen =
                ["limit", "arguments[0]", "total"].map(|name| shown(&variables.variables, name));
            let interpreted = seen[0] == "100000" && seen[1] == "absent";
            let optimized = seen[0] == "unavailable" && seen[1] == "100000";
            let total = seen[2] == "unavailable" || seen[2].parse::<u32>().is_ok();
            if !(interpreted || optimized) || !total {
                lies.push(format!(
                    "{context}: interrupt {round}: limit, arguments[0], total are {seen:?}"
                ));
            }
            session.resume().await;
        }
        session.pause().await;
        session.shutdown().await;
    }
    assert!(lies.is_empty(), "{}", lies.join("\n"));
}
