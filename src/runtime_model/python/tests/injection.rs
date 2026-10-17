use super::super::injection::{AgentFailure, Candidate};
use super::*;
use crate::runtime_model::TargetThreadId;

fn candidate(controller: u64, native: u64) -> Candidate {
    Candidate {
        thread: ThreadId::new(controller),
        native: TargetThreadId::new(native).expect("a native thread id"),
    }
}

pub(super) fn target() -> (Process, u64, u64, u64) {
    shaped(192, 64)
}

fn shaped(thread_size: u64, capacity: u64) -> (Process, u64, u64, u64) {
    let mut layout = layout();
    for (field, offset) in [
        (Field::ThreadSize, thread_size),
        (Field::DebugEvalBreaker, 64),
        (Field::DebugSupport, 72),
        (Field::DebugPending, 0),
        (Field::DebugScript, 4),
        (Field::DebugScriptSize, capacity),
        (Field::DebugEnabled, 24),
        (Field::RuntimeInitialized, 24),
        (Field::RuntimeFinalizing, 16),
    ] {
        layout.set_field(field, offset);
    }
    layout.set_constant(Constant::DebugStopBit, 32);
    let mut process = Process::new(layout);
    let second = process.thread_state(0, 0x5000, 200);
    let first = process.thread_state(second, 0x4000, 100);
    let interpreter = process.interpreter(0, 0, first);
    process.memory.write(interpreter + 24, 1, 4);
    process.memory.write(first + 64, 0x4000_0000_0000_0104, 8);
    let runtime = process.runtime(interpreter);
    process.memory.write(runtime + 24, 1, 4);
    process.memory.threads = vec![ThreadId::new(100), ThreadId::new(200)];
    (process, runtime, first, second)
}

fn request(
    process: &Process,
    runtime: u64,
    script: &str,
    candidates: &[u64],
) -> Result<Vec<crate::runtime_model::RuntimeWrite>, Arc<str>> {
    super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        script,
        &candidates
            .iter()
            .copied()
            .map(|id| candidate(id, id))
            .collect::<Vec<_>>(),
    )
    .map_err(|reason| reason.to_string().into())
}

#[test]
fn injection_prefers_the_selected_eligible_thread_and_preserves_the_eval_breaker() {
    let (process, runtime, first, second) = target();
    let writes = request(&process, runtime, "/tmp/agent.py", &[200, 100]).expect("injection");
    assert_eq!(writes.len(), 3);
    assert_eq!(writes[0].address.get(), second + 76);
    assert_eq!(writes[0].bytes, b"/tmp/agent.py\0");
    assert_eq!(writes[1].address.get(), second + 72);
    assert_eq!(writes[1].bytes, 1_u32.to_le_bytes());
    assert_eq!(writes[2].address.get(), second + 64);
    assert_eq!(writes[2].bytes, 32_u64.to_le_bytes());
    let writes = request(&process, runtime, "/a.py", &[100]).expect("injection");
    assert_eq!(writes[2].address.get(), first + 64);
    assert_eq!(writes[2].bytes, 0x4000_0000_0000_0124_u64.to_le_bytes());
    for write in writes {
        let mut expected = vec![0; write.expected.len()];
        assert!(process.memory.read(write.address, &mut expected));
        assert_eq!(write.expected, expected);
        assert_eq!(write.bytes.len(), expected.len());
    }
}

#[test]
fn injection_refuses_disabled_busy_missing_and_malformed_state() {
    let (mut process, runtime, first, _) = target();
    for candidates in [&[][..], &[999][..]] {
        assert!(
            request(&process, runtime, "/a.py", candidates)
                .unwrap_err()
                .contains("no thread")
        );
    }
    process.memory.write(first + 72, 1, 4);
    assert_eq!(
        super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &[candidate(100, 100)]
        ),
        Err(AgentFailure::PendingRequest),
    );
    process.memory.write(first + 72, 0, 4);
    let interpreter = threads::thread_states(&process.reader(), VirtualAddress::new(runtime))
        .unwrap()
        .0[0]
        .interpreter_state
        .get();
    process.memory.write(interpreter + 24, 0, 4);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("disabled")
    );
    process.memory.write(interpreter + 24, 1, 4);
    for path in [
        "",
        "relative.py",
        "/nul\0.py",
        &format!("/{}", "a".repeat(63)),
    ] {
        assert!(
            request(&process, runtime, path, &[100]).is_err(),
            "{path:?}"
        );
    }
    process.layout.set_field(Field::DebugScriptSize, 1 << 20);
    assert!(request(&process, runtime, "/a.py", &[100]).is_err());
    process.layout.set_field(Field::DebugScriptSize, 64);
    process.unmap(first + 76, 1);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("unreadable")
    );
}

#[test]
fn incomplete_lists_overlapping_fields_and_uninitialized_runtimes_are_refused() {
    let (mut process, runtime, first, _) = target();
    process.memory.write(runtime + 24, 0, 4);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("initialization")
    );
    process.memory.write(runtime + 24, 1, 4);
    process.word(first, Field::ThreadNext, first);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("loops")
    );
    process.word(first, Field::ThreadNext, 0);
    for (field, offset) in [
        (Field::DebugScript, 0),
        (Field::DebugScript, u64::from(u32::MAX) - 1),
        (Field::ThreadSize, 80),
        (Field::ThreadSize, u64::from(u32::MAX)),
    ] {
        let old = process.offset(field);
        process.layout.set_field(field, offset);
        let result = super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &[candidate(100, 100)],
        );
        assert!(
            matches!(result, Err(AgentFailure::Refused(_))),
            "{field:?}: {result:?}"
        );
        process.layout.set_field(field, old);
    }
    for bit in [0, 3, 255] {
        process.layout.set_constant(Constant::DebugStopBit, bit);
        assert!(
            request(&process, runtime, "/a.py", &[100])
                .unwrap_err()
                .contains("eval breaker bit")
        );
    }
}

#[test]
fn another_interpreter_or_an_ambiguous_native_thread_is_refused() {
    let (mut process, runtime, first, _) = target();
    let state = process.thread_state(0, 0x6000, 300);
    let second_interpreter = process.interpreter(1, 0, state);
    let first_interpreter = threads::thread_states(&process.reader(), VirtualAddress::new(runtime))
        .unwrap()
        .0[0]
        .interpreter_state
        .get();
    process.word(
        first_interpreter,
        Field::InterpreterNext,
        second_interpreter,
    );
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("multiple interpreters")
    );
    process.word(first_interpreter, Field::InterpreterNext, 0);
    process.word(first, Field::ThreadNext, state);
    process.word(state, Field::ThreadNativeId, 100);
    for candidates in [&[100][..], &[100, 200][..]] {
        let result = super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &candidates
                .iter()
                .map(|&id| candidate(id, id))
                .collect::<Vec<_>>(),
        );
        assert!(
            matches!(result, Err(AgentFailure::Refused(ref reason)) if reason.contains("more than one CPython thread state")),
            "{result:?}"
        );
    }
}

#[test]
fn reserved_control_flags_are_malformed_rather_than_waiting_or_busy() {
    let (mut process, runtime, first, _) = target();
    let interpreter = threads::thread_states(&process.reader(), VirtualAddress::new(runtime))
        .unwrap()
        .0[0]
        .interpreter_state
        .get();
    for (address, reset) in [(runtime + 24, 1), (interpreter + 24, 1), (first + 72, 0)] {
        process.memory.write(address, 2, 4);
        let reason = request(&process, runtime, "/a.py", &[100]).unwrap_err();
        assert!(reason.contains("invalid"), "{reason}");
        process.memory.write(address, reset, 4);
    }
}

#[test]
fn an_interpreter_without_threads_still_counts_as_another_interpreter() {
    let (mut process, runtime, _, _) = target();
    let empty = process.interpreter(1, 0, 0);
    let first = threads::thread_states(&process.reader(), VirtualAddress::new(runtime))
        .unwrap()
        .0[0]
        .interpreter_state
        .get();
    process.word(first, Field::InterpreterNext, empty);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("multiple interpreters")
    );
}

#[test]
fn finalization_is_refused_even_while_the_initialized_flag_is_still_set() {
    let (mut process, runtime, _, _) = target();
    process.word(runtime, Field::RuntimeFinalizing, 0x4000);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("finalizing")
    );
}

#[test]
fn the_callers_read_budget_is_preserved_and_a_default_budget_bounds_large_lists() {
    let (mut process, runtime, first, _) = target();
    let mut budget = InspectionBudget::new(InspectionLimits {
        memory_reads: 0,
        ..InspectionLimits::default()
    });
    let charged = super::super::values::Budget::new(&mut budget);
    let reader = Reader {
        charge: Some(&charged),
        ..process.reader()
    };
    let result = super::super::injection::plan(
        &reader,
        VirtualAddress::new(runtime),
        "/a.py",
        &[candidate(100, 100)],
    );
    assert_eq!(
        result,
        Err(AgentFailure::ReadLimit(
            "MemoryReads limit 0 exhausted after 0 while requesting 1".into()
        ))
    );

    let mut next = 0;
    for number in 300..1800 {
        next = process.thread_state(next, 0x6000, number);
    }
    process.word(first, Field::ThreadNext, next);
    let result = super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        "/a.py",
        &[candidate(100, 100)],
    );
    assert!(
        matches!(result, Err(AgentFailure::ReadLimit(_))),
        "{result:?}"
    );
}

#[test]
fn candidate_ids_use_the_targets_namespace_and_require_a_live_controller_thread() {
    let (mut process, runtime, first, second) = target();
    process.word(first, Field::ThreadNativeId, 1);
    process.word(second, Field::ThreadNativeId, 2);
    let proven = candidate(200, 1);
    assert_eq!(proven.native.get(), 1);
    let writes = super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        "/a.py",
        &[proven],
    )
    .expect("namespaced candidate");
    assert_eq!(writes[0].address.get(), first + 76);
    let result = super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        "/a.py",
        &[candidate(999, 1)],
    );
    assert_eq!(result, Err(AgentFailure::NoThread));
}

#[test]
fn initialization_and_no_candidate_are_typed_waits() {
    let (mut process, runtime, _, _) = target();
    assert_eq!(
        super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &[]
        ),
        Err(AgentFailure::NoThread)
    );
    process.word(runtime, Field::RuntimeInitialized, 0);
    assert_eq!(
        super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &[candidate(100, 100)]
        ),
        Err(AgentFailure::Initializing)
    );
}

#[test]
fn a_path_buffer_larger_than_the_read_limit_is_refused_even_if_it_fits_the_thread() {
    let (process, runtime, _, _) = shaped(8192, 5000);
    assert!(
        request(&process, runtime, "/a.py", &[100])
            .unwrap_err()
            .contains("buffer")
    );
}

/// Reject precisely the second read of the script bytes, after the whole
/// thread-state read succeeded. This pins the expected-bytes precondition.
struct RejectExpected<'a> {
    memory: &'a Memory,
    at: VirtualAddress,
    reached: std::cell::Cell<bool>,
}

impl RuntimeStop for RejectExpected<'_> {
    fn read(&self, at: VirtualAddress, bytes: &mut [u8]) -> bool {
        if at == self.at && bytes.len() == 6 {
            self.reached.set(true);
            return false;
        }
        self.memory.read(at, bytes)
    }
    fn thread_pointer(&self, thread: ThreadId) -> Option<u64> {
        self.memory.thread_pointer(thread)
    }
    fn instruction(&self, thread: ThreadId) -> Option<VirtualAddress> {
        self.memory.instruction(thread)
    }
    fn load_bias(&self) -> LoadBias {
        self.memory.load_bias()
    }
    fn threads(&self) -> Vec<ThreadId> {
        self.memory.threads()
    }
}

#[test]
fn an_unreadable_expected_bytes_read_refuses_the_whole_request() {
    let (process, runtime, first, _) = target();
    let stop = RejectExpected {
        memory: &process.memory,
        at: VirtualAddress::new(first + 76),
        reached: std::cell::Cell::new(false),
    };
    let reader = Reader {
        stop: &stop,
        ..process.reader()
    };
    let result = super::super::injection::plan(
        &reader,
        VirtualAddress::new(runtime),
        "/a.py",
        &[candidate(100, 100)],
    );
    assert!(
        matches!(result, Err(AgentFailure::Unavailable(ref reason)) if reason.contains("unreadable")),
        "{result:?}"
    );
    assert!(
        stop.reached.get(),
        "the fault must occur after the thread-state read"
    );
}

#[test]
fn a_consumed_but_unacknowledged_slot_is_never_overwritten() {
    let (mut process, runtime, first, _) = target();
    process.memory.fill(first + 76, b"/old.py\0");
    assert_eq!(
        super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &[candidate(100, 100)]
        ),
        Err(AgentFailure::UnclearedSlot {
            address: VirtualAddress::new(first + 76)
        }),
    );
}

proptest::proptest! {
    #[test]
    fn injection_preserves_every_other_eval_breaker_bit(breaker: u64) {
        let (mut process, runtime, first, _) = target();
        process.memory.write(first + 64, breaker, 8);
        let writes = request(&process, runtime, "/a.py", &[100]).expect("request");
        proptest::prop_assert_eq!(&writes[2].expected, &breaker.to_le_bytes());
        proptest::prop_assert_eq!(&writes[2].bytes, &(breaker | 32).to_le_bytes());
    }
}

#[test]
fn static_request_errors_precede_waits_and_memory_reads() {
    let (mut process, runtime, _, _) = target();
    process.word(runtime, Field::RuntimeInitialized, 0);
    let result = super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        "relative.py",
        &[],
    );
    assert!(
        matches!(result, Err(AgentFailure::Refused(_))),
        "{result:?}"
    );
    process.layout.set_constant(Constant::DebugStopBit, 3);
    let result = super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        "/a.py",
        &[],
    );
    assert!(
        matches!(result, Err(AgentFailure::Refused(_))),
        "{result:?}"
    );
    process.unmap(runtime, 32);
    let result = super::super::injection::plan(
        &process.reader(),
        VirtualAddress::new(runtime),
        "/a.py",
        &[],
    );
    assert!(
        matches!(result, Err(AgentFailure::Refused(_))),
        "{result:?}"
    );
}

#[test]
fn general_thread_walking_metadata_is_checked_before_initialization() {
    for field in [
        Field::RuntimeInterpretersHead,
        Field::InterpreterNext,
        Field::InterpreterThreadsHead,
        Field::InterpreterId,
        Field::ThreadNext,
        Field::ThreadCurrentFrame,
        Field::ThreadNativeId,
    ] {
        let (mut process, runtime, _, _) = target();
        process.word(runtime, Field::RuntimeInitialized, 0);
        process.layout.set_field(field, u64::from(u32::MAX));
        let result = super::super::injection::plan(
            &process.reader(),
            VirtualAddress::new(runtime),
            "/a.py",
            &[],
        );
        assert!(
            matches!(result, Err(AgentFailure::Refused(_))),
            "{field:?}: {result:?}"
        );
    }
}

#[test]
fn an_ambiguous_preferred_thread_does_not_hide_an_eligible_alternative() {
    let (mut process, runtime, _, second) = target();
    let duplicate = process.thread_state(0, 0x7000, 100);
    process.word(second, Field::ThreadNext, duplicate);
    let writes = request(&process, runtime, "/a.py", &[100, 200]).expect("unambiguous alternative");
    assert_eq!(writes[0].address.get(), second + 76);
}
