use super::super::agent as control_step;
use super::super::{control, hook};
use super::*;
use proptest::prelude::*;

fn slot(sequence: u64) -> (Memory, VirtualAddress) {
    let mut memory = Memory::default();
    let address = VirtualAddress::new(HEAP);
    memory.fill(HEAP, &vec![0; hook::CONTROL_SIZE]);
    memory.fill(HEAP, hook::CONTROL_MAGIC);
    memory.write(HEAP + 8, sequence, 8);
    memory.write(HEAP + 20, u64::from(hook::CONTROL_CAPACITY), 4);
    (memory, address)
}

#[test]
fn command_publication_is_bounded_expected_and_last() {
    let (memory, address) = slot(7);
    let probes = [("LINE", 1, 42, "quote\"\\\n.py", 13, "")];
    let writes =
        control::command(&memory, address, (7, 8), &probes, None, 0, None).expect("command");
    assert_eq!(writes.len(), 3);
    assert_eq!(writes[0].address, address.checked_add(32).unwrap());
    assert_eq!(writes[1].address, address.checked_add(16).unwrap());
    assert_eq!(writes[2].address, address.checked_add(8).unwrap());
    assert_eq!(writes[2].expected, 7_u64.to_le_bytes());
    assert_eq!(writes[2].bytes, 8_u64.to_le_bytes());
    assert_eq!(
        writes[1].bytes,
        u32::try_from(writes[0].bytes.len())
            .expect("bounded payload")
            .to_le_bytes()
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&writes[0].bytes).unwrap(),
        serde_json::json!([8, probes, null, 0])
    );

    // A step travels with the probes it is armed beside.
    let step = (5, control_step::OVER, 7, 2, 0x1000);
    let writes =
        control::command(&memory, address, (7, 8), &probes, Some(step), 3, None).expect("command");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&writes[0].bytes).unwrap(),
        serde_json::json!([8, probes, [5, 1, 7, 2, 4096], 3])
    );

    for write in writes {
        let mut held = vec![0; write.bytes.len()];
        assert!(memory.read(write.address, &mut held));
        assert_eq!(held, write.expected);
    }
}

#[test]
fn corrupt_changed_unreadable_and_overflowing_command_slots_are_refused() {
    for (offset, byte) in [(0, b'!'), (8, 8), (20, 1), (24, 1)] {
        let (mut memory, address) = slot(7);
        memory.fill(HEAP + offset, &[byte]);
        assert!(
            control::command(&memory, address, (7, 8), &[], None, 0, None).is_err(),
            "{offset}"
        );
    }
    let (mut memory, address) = slot(7);
    for next in [0, 6, 7] {
        assert!(control::command(&memory, address, (7, next), &[], None, 0, None).is_err());
    }
    assert!(
        control::command(
            &memory,
            address.checked_add(1).unwrap(),
            (7, 8),
            &[],
            None,
            0,
            None
        )
        .is_err()
    );
    assert!(
        control::command(
            &memory,
            VirtualAddress::new(u64::MAX - 7),
            (7, 8),
            &[],
            None,
            0,
            None
        )
        .is_err()
    );
    memory.bytes.remove(&(HEAP + 32));
    assert!(control::command(&memory, address, (7, 8), &[], None, 0, None).is_err());
    memory.bytes.remove(&(HEAP + 20));
    assert!(control::command(&memory, address, (7, 8), &[], None, 0, None).is_err());
}

#[test]
fn oversized_configuration_is_refused_without_truncation() {
    let (memory, address) = slot(0);
    let path = "x".repeat(hook::CONTROL_CAPACITY as usize);
    assert!(
        control::command(
            &memory,
            address,
            (0, 1),
            &[("LINE", 1, 1, &path, 1, "")],
            None,
            0,
            None
        )
        .unwrap_err()
        .contains("bounded")
    );
}

proptest! {
    #[test]
    fn arbitrary_generations_preserve_sequence_bits(previous in any::<u64>(), next in any::<u64>()) {
        let (memory, address) = slot(previous);
        match control::command(&memory, address, (previous, next), &[], None, 0, None) {
            Ok(writes) => {
                prop_assert!(next > previous);
                prop_assert_eq!(&writes[2].expected, &previous.to_le_bytes());
                prop_assert_eq!(&writes[2].bytes, &next.to_le_bytes());
            }
            Err(_) => prop_assert!(next <= previous),
        }
    }
}

fn prepared() -> (
    Process,
    u64,
    u64,
    Arc<dyn crate::runtime_model::AgentRequestTicket>,
) {
    let (mut process, runtime, state, _) = super::injection::target();
    let plan = super::super::injection::prepare(
        &process.reader(),
        VirtualAddress::new(runtime),
        "/tmp/owned.py",
        &[super::super::injection::Candidate {
            thread: ThreadId::new(100),
            native: crate::runtime_model::TargetThreadId::new(100).unwrap(),
        }],
    )
    .unwrap();
    let ticket = control::ticket(&process.layout, VirtualAddress::new(runtime), &plan);

    for write in plan.writes {
        process.memory.fill(write.address.get(), &write.bytes);
    }
    (process, runtime, state, ticket)
}

#[test]
fn retirement_cancels_an_unconsumed_request_without_clearing_shared_breaker_bits() {
    use crate::runtime_model::{AgentRetirement, AgentRetirementProof};
    let (mut process, _, state, ticket) = prepared();
    let AgentRetirement::Retired(writes) = ticket
        .retire(&process.memory, AgentRetirementProof::Cancel)
        .unwrap()
    else {
        panic!("the owned pending request is cancellable")
    };
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].address.get(), state + 72);
    assert_eq!(writes[1].address.get(), state + 76);
    for write in writes {
        process.memory.fill(write.address.get(), &write.bytes);
    }
    assert_eq!(
        process
            .reader()
            .word(VirtualAddress::new(state + 64))
            .unwrap(),
        0x4000_0000_0000_0124
    );
    assert_eq!(
        process
            .reader()
            .bytes(VirtualAddress::new(state + 72), 5)
            .unwrap(),
        [0; 5]
    );
}

#[test]
fn consumption_without_acknowledgement_keeps_the_source_owned() {
    use crate::runtime_model::{AgentRetirement, AgentRetirementProof};
    let (mut process, _, state, ticket) = prepared();
    process.memory.write(state + 72, 0, 4);
    assert!(matches!(
        ticket.retire(&process.memory, AgentRetirementProof::Cancel),
        Ok(AgentRetirement::Unconfirmed)
    ));
    let AgentRetirement::Retired(writes) = ticket
        .retire(&process.memory, AgentRetirementProof::Acknowledged)
        .unwrap()
    else {
        panic!("acknowledgement proves consumption")
    };
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].address.get(), state + 76);
}

#[test]
fn partial_unresumed_publication_is_retired_but_foreign_and_incomplete_state_is_refused() {
    use crate::runtime_model::{AgentRetirement, AgentRetirementProof};
    let (mut process, runtime, state, ticket) = prepared();
    process.memory.fill(state + 76, b"/other.py\0");
    assert!(
        ticket
            .retire(&process.memory, AgentRetirementProof::Cancel)
            .is_err()
    );
    assert!(
        ticket
            .retire(&process.memory, AgentRetirementProof::Unresumed)
            .is_err(),
        "even an unresumed request must retain ownership evidence"
    );
    process.memory.fill(state + 76, b"/tmp/\0\0\0\0\0\0\0\0\0");
    assert!(matches!(
        ticket.retire(&process.memory, AgentRetirementProof::Unresumed),
        Ok(AgentRetirement::Retired(_))
    ));
    process.memory.write(state + 72, 2, 4);
    assert!(
        ticket
            .retire(&process.memory, AgentRetirementProof::Acknowledged)
            .is_err()
    );
    process.memory.write(state + 72, 0, 4);
    process.memory.bytes.remove(&(state + 16));
    assert!(
        ticket
            .retire(&process.memory, AgentRetirementProof::Acknowledged)
            .is_err()
    );
    process.word(runtime, Field::RuntimeInterpretersHead, 0);
    assert!(
        matches!(ticket.retire(&process.memory, AgentRetirementProof::Cancel), Ok(AgentRetirement::Retired(writes)) if writes.is_empty())
    );
}

/// Empty selectors match any code in the agent, so a function breakpoint
/// without a name would stop at every Python call.
#[test]
fn a_python_function_breakpoint_needs_a_name() {
    use crate::runtime_model::RuntimeModel as _;
    let model = super::super::PythonRuntime {
        version: crate::image::python::PyVersion(0x030e_00f0),
        reading: Err("not needed to resolve".into()),
        free_threaded: false,
        entries: Vec::new(),
        evaluation: Vec::new(),
    };
    let memory = Memory::default();
    let resolve = |name: &str| {
        model.resolve_breakpoint(&memory, &crate::BreakpointSpec::Function(name.into()))
    };

    assert!(matches!(resolve("py:"), Some(Err(_))));
    assert!(matches!(
        resolve("py:main").expect("a Python function").as_deref(),
        Ok([crate::ManagedPlace::Function { name }]) if &**name == "main"
    ));
    assert!(matches!(
        resolve("py:outer.<locals>.area").expect("a Python function").as_deref(),
        Ok([crate::ManagedPlace::Function { name }]) if &**name == "outer.<locals>.area"
    ));
    // A native function in a `py` namespace is not Python's.
    assert!(resolve("py::run").is_none());
    assert!(resolve("main").is_none());
}

/// A Python line breakpoint's path matches as a native one's does, so `.`
/// components and repeated separators change nothing it names.
#[test]
fn a_python_line_breakpoint_path_ignores_its_dot_components() {
    use crate::runtime_model::RuntimeModel as _;
    let model = super::super::PythonRuntime {
        version: crate::image::python::PyVersion(0x030e_00f0),
        reading: Err("not needed to resolve".into()),
        free_threaded: false,
        entries: Vec::new(),
        evaluation: Vec::new(),
    };
    let memory = Memory::default();
    let resolve = |path: &str| {
        let line = crate::LineNumber::new(5).expect("a line");
        model.resolve_breakpoint(
            &memory,
            &crate::BreakpointSpec::Source {
                path: path.into(),
                line,
            },
        )
    };

    for (requested, armed) in [
        ("./pkg/./later.py", "pkg/later.py"),
        ("later.py", "later.py"),
        ("/srv//app/./later.py", "/srv/app/later.py"),
        ("../later.py", "../later.py"),
    ] {
        assert!(
            matches!(
                resolve(requested).expect("a Python line").as_deref(),
                Ok([crate::ManagedPlace::Line { path, line: 5 }]) if &**path == armed
            ),
            "{requested}"
        );
    }
    assert!(resolve("later.pyc").is_none());
    assert!(resolve("later").is_none());
}
