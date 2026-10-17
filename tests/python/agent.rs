//! The agent is loaded by the fixture itself until P4.7 supplies injection.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use object::{Object as _, ObjectSection as _, ObjectSegment as _, ObjectSymbol as _};

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
fn the_command_slot_and_atomic_reader_have_the_declared_abi() {
    let bytes = hook::library();
    let file = object::File::parse(bytes.as_slice()).expect("parse companion");
    let control = file
        .dynamic_symbols()
        .find(|symbol| symbol.name() == Ok(hook::CONTROL_SYMBOL))
        .expect("control object");
    assert_eq!(control.kind(), object::SymbolKind::Data);
    assert_eq!(control.address(), hook::CONTROL_OFFSET as u64);
    assert_eq!(control.address() % 8, 0);
    assert_eq!(control.size(), hook::CONTROL_SIZE as u64);
    let data = file
        .section_by_name(".data")
        .expect("writable section")
        .data()
        .expect("data bytes");
    assert_eq!(&data[..8], hook::CONTROL_MAGIC);
    assert_eq!(&data[20..24], hook::CONTROL_CAPACITY.to_le_bytes());
    assert_eq!(&data[8..20], &[0; 12]);
    assert_eq!(&data[24..hook::CONTROL_HEADER], &[0; 8]);
    let segment = file
        .segments()
        .find(|segment| segment.address() == control.address())
        .expect("control mapping");
    assert_eq!(
        segment.flags(),
        object::SegmentFlags::Elf {
            p_flags: object::elf::PF_R | object::elf::PF_W
        }
    );
    let sequence = file
        .dynamic_symbols()
        .find(|symbol| symbol.name() == Ok(hook::SEQUENCE_SYMBOL))
        .expect("sequence reader");
    assert_eq!(sequence.kind(), object::SymbolKind::Text);
    let text = file.section_by_name(".text").expect("text");
    let offset = usize::try_from(sequence.address() - text.address()).expect("code offset");
    let code = &text.data().expect("code bytes")
        [offset..offset + usize::try_from(sequence.size()).expect("code length")];
    let mut decoder =
        iced_x86::Decoder::with_ip(64, code, sequence.address(), iced_x86::DecoderOptions::NONE);
    assert_eq!(decoder.decode().mnemonic(), iced_x86::Mnemonic::Endbr64);
    let load = decoder.decode();
    assert_eq!(load.mnemonic(), iced_x86::Mnemonic::Mov);
    assert_eq!(load.op0_register(), iced_x86::Register::RAX);
    assert_eq!(load.op1_kind(), iced_x86::OpKind::Memory);
    assert_eq!(load.memory_size(), iced_x86::MemorySize::UInt64);
    assert_eq!(load.ip_rel_memory_address(), control.address() + 8);
    assert_eq!(decoder.decode().mnemonic(), iced_x86::Mnemonic::Ret);
    assert!(!decoder.can_decode(), "exactly one native data load");
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
        let output = fixture_output(
            interpreter,
            "agent.py",
            &[
                agent.as_os_str(),
                library.as_os_str(),
                standalone.as_os_str(),
            ],
            &output,
        );
        assert!(output.contains("AGENT OK"), "{interpreter}: {output}");
    }
}

fn fixture_output(
    interpreter: &str,
    fixture: &str,
    arguments: &[&std::ffi::OsStr],
    output: &std::path::Path,
) -> String {
    let stdout = std::fs::File::create(output).expect("fixture output");
    let mut child = Reaped(
        Command::new(python::interpreter(interpreter))
            .args(["-I", "-B"])
            .arg(python::fixture(fixture))
            .args(arguments)
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
        assert!(
            Instant::now() < deadline,
            "{interpreter}: {fixture} timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = std::fs::read_to_string(output).expect("read fixture output");
    assert!(status.success(), "{interpreter} {fixture}: {output}");
    output
}

#[test]
fn monitoring_callbacks_survive_reconfiguration_and_code_reuse() {
    let scratch = ScratchDir::new("python-agent-races");
    let library = scratch.path().join("hook.so");
    std::fs::write(&library, hook::library()).expect("write control companion");
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runtime_model/python/agent.py");
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        for (fixture, modes) in [
            ("agent_races.py", &["capture", "disable"][..]),
            ("agent_control.py", &["chain"][..]),
            (
                "agent_discovery.py",
                &[
                    "repeat",
                    "fault",
                    "resumed",
                    "delegated",
                    "identity",
                    "gc",
                    "reused",
                    "coexist",
                    "coexist_matching",
                ][..],
            ),
        ] {
            for mode in modes {
                let output = scratch
                    .path()
                    .join(format!("{interpreter}-{fixture}-{mode}.out"));
                let output = fixture_output(
                    interpreter,
                    fixture,
                    &[
                        source.as_os_str(),
                        std::ffi::OsStr::new(mode),
                        library.as_os_str(),
                    ],
                    &output,
                );
                let prefix = match fixture {
                    "agent_races.py" => "RACE",
                    "agent_control.py" => "CONTROL AGENT",
                    _ => "DISCOVERY",
                };
                assert!(
                    output
                        .lines()
                        .any(|line| line == format!("{prefix} OK {mode}")),
                    "{interpreter}: {output}"
                );
            }
        }
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

#[tokio::test]
async fn bootstrap_failure_reports_its_generation_through_the_owned_library() {
    use crate::session::PythonSession;
    use microscope::{ExitStatus, StopReason};
    let scratch = ScratchDir::new("python-agent-failed-bootstrap");
    let library = scratch.path().join("hook.so");
    let bootstrap = scratch.path().join("bootstrap.py");
    std::fs::write(&library, hook::library()).expect("write hook");
    std::fs::write(&bootstrap, agent_source::bootstrap().expect("bootstrap"))
        .expect("write script");
    for interpreter in ["3.14", "3.14t", "3.15", "3.15t"] {
        let (mut session, reason, _) = PythonSession::launch_to_stop(
            interpreter,
            "agent.py",
            &[
                bootstrap.to_str().expect("script"),
                library.to_str().expect("library"),
                "--failed-bootstrap",
            ],
            Vec::new(),
        )
        .await;
        assert!(
            matches!(reason, StopReason::RuntimeEvent { ref description } if description.contains("generation 1") && description.contains("initialization failed")),
            "{interpreter}: {reason:?}: {}",
            session.output()
        );
        assert_eq!(
            session.scenario.resume_to_stop().await,
            StopReason::Exited(ExitStatus::Code(0))
        );
        assert!(session.output().contains("FAILED BOOTSTRAP OK"));
        session.shutdown().await;
    }
}
