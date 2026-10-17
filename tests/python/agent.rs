//! The agent is loaded by the fixture itself until P4.7 supplies injection.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use object::{Object as _, ObjectSymbol as _};

use crate::support::{ScratchDir, python};

/// Own a fixture before the first fallible wait, including panic paths.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[path = "../../src/runtime_model/python/hook.rs"]
mod hook;

#[path = "../../src/runtime_model/python/agent.rs"]
mod agent_source;

#[test]
fn the_hook_is_a_loadable_exported_function() {
    let bytes = hook::library();
    let file = object::File::parse(bytes.as_slice()).expect("parse the generated hook");
    assert_eq!(file.kind(), object::ObjectKind::Dynamic);
    let symbol = file
        .dynamic_symbols()
        .find(|symbol| symbol.name() == Ok(hook::SYMBOL))
        .expect("the exported hook");
    assert!(symbol.is_global());
    assert!(symbol.is_definition());
    assert_eq!(symbol.kind(), object::SymbolKind::Text);
}

#[test]
fn monitoring_reconfiguration_and_cleanup() {
    let scratch = ScratchDir::new("python-agent");
    let library = scratch.path().join("hook.so");
    std::fs::write(&library, hook::library()).expect("write the hook");
    let agent = scratch.path().join("agent.py");
    std::fs::write(&agent, agent_source::bootstrap().expect("agent bootstrap"))
        .expect("write the agent");
    let standalone = scratch.path().join("standalone.py");
    std::fs::write(
        &standalone,
        agent_source::script(library.to_str().expect("hook path"), 100, &[])
            .expect("whole-state script"),
    )
    .expect("write script");
    for interpreter in ["3.12", "3.13", "3.13t", "3.14", "3.14t", "3.15", "3.15t"] {
        let output = scratch.path().join(format!("{interpreter}.out"));
        let stdout = std::fs::File::create(&output).expect("fixture output");
        let mut child = Reaped(
            Command::new(python::interpreter(interpreter))
                .args(["-I", "-B"])
                .arg(python::fixture("agent.py"))
                .arg(&agent)
                .arg(&library)
                .arg(&standalone)
                .stdout(stdout.try_clone().expect("share output"))
                .stderr(stdout)
                .stdin(Stdio::null())
                .spawn()
                .expect("run the agent fixture"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("wait for the fixture") {
                break status;
            }
            if Instant::now() >= deadline {
                child.0.kill().expect("kill timed out fixture");
                child.0.wait().expect("reap timed out fixture");
                panic!("{interpreter}: agent fixture timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let output = std::fs::read_to_string(&output).expect("read fixture output");
        assert!(status.success(), "{interpreter}: {output}");
        assert!(output.contains("AGENT OK"), "{interpreter}: {output}");
    }
}

/// Ready reports are stepped over internally, a real callback delivers its
/// three C arguments, and a malformed report becomes a visible runtime stop.
#[tokio::test]
async fn native_reports_cross_the_public_debugger_boundary() {
    use crate::session::PythonSession;
    use microscope::{BreakpointOptions, BreakpointSpec, Condition, ExitStatus, StopReason};

    let scratch = ScratchDir::new("python-agent-native");
    let library = scratch.path().join("hook.so");
    let script = scratch.path().join("agent.py");
    std::fs::write(&library, hook::library()).expect("write hook");
    std::fs::write(&script, agent_source::bootstrap().expect("bootstrap")).expect("write agent");
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "agent.py",
            &[
                script.to_str().expect("script path"),
                library.to_str().expect("hook path"),
                "--native",
            ],
            vec![(
                BreakpointSpec::Function(hook::SYMBOL.to_owned()),
                BreakpointOptions {
                    condition: Some(Condition::parse("$rdi == 1").expect("hook condition")),
                    pending: true,
                    ..BreakpointOptions::default()
                },
            )],
        )
        .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{interpreter}: {reason:?}"
        );
        let trace = session
            .scenario
            .operation("hook backtrace", session.scenario.handle().backtrace())
            .await;
        assert!(
            trace.frames.iter().any(|frame| frame
                .recorded
                .as_ref()
                .is_some_and(|recorded| &*recorded.function == "target")),
            "{interpreter}: {trace:#?}"
        );
        assert_eq!(
            trace.termination,
            microscope::UnwindTermination::Complete,
            "{interpreter}"
        );
        assert!(trace.unfollowed.is_empty(), "{interpreter}: {trace:#?}");
        let registers = session
            .scenario
            .operation("hook arguments", session.scenario.handle().registers())
            .await;
        for (name, expected) in [("rdi", 1_u64), ("rsi", 42), ("rdx", 0)] {
            let value = registers
                .registers
                .iter()
                .find(|value| &*value.register.name == name)
                .and_then(|value| value.bytes.as_ref())
                .expect("hook register");
            assert_eq!(
                value.as_ref(),
                expected.to_le_bytes(),
                "{interpreter}: {name}"
            );
        }
        let reason = session.scenario.resume_to_stop().await;
        assert!(
            matches!(reason, StopReason::RuntimeEvent { ref description } if description.contains("invalid Python agent")),
            "{interpreter}: {reason:?}"
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        session.shutdown().await;
    }
}
