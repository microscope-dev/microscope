use super::super::{agent, exceptions};
use super::*;

fn record(object: u64, message: &[u8], length: u64) -> Memory {
    let mut memory = Memory::default();
    let text = HEAP + 0x100;
    memory.write(HEAP, object, 8);
    memory.write(HEAP + 8, text, 8);
    memory.write(HEAP + 16, length, 8);
    memory.fill(text, message);
    memory
}

#[test]
fn an_exception_report_reads_its_object_and_message_or_is_refused() {
    let message = "ValueError: naïve";
    let memory = record(0x7000, message.as_bytes(), message.len() as u64);
    let read = exceptions::read(&memory, HEAP).expect("a record");
    assert_eq!(&*read.message, message);
    assert_eq!(read.value.as_deref(), Some("(py)0x7000"));

    let refused = |memory: &Memory| exceptions::read(memory, HEAP).unwrap_err();
    assert!(refused(&record(0, b"x", 1)).contains("malformed"));
    assert!(refused(&record(0x7000, b"x", (1 << 13) + 1)).contains("malformed"));
    assert!(refused(&record(0x7000, b"x", 2)).contains("unreadable"));
    assert!(refused(&record(0x7000, b"\xff", 1)).contains("UTF-8"));
    assert!(refused(&Memory::default()).contains("unreadable"));
}

#[test]
fn python_exception_filters_are_bits_the_agent_shares() {
    let raised = exceptions::filter(agent::RAISED).expect("raised");
    let uncaught = exceptions::filter(agent::UNCAUGHT).expect("uncaught");
    assert_eq!(
        (raised.id, uncaught.id),
        ("python-raised", "python-uncaught")
    );
    assert_eq!(exceptions::filter(0), None);
    assert_eq!(exceptions::filter(agent::RAISED | agent::UNCAUGHT), None);

    assert!(!raised.default && !uncaught.default);
    assert_eq!(exceptions::bits(crate::ExceptionStops::default()), 0);
    assert_eq!(exceptions::bits(crate::ExceptionStops::NONE), 0);
    let chosen = crate::ExceptionStops::NONE
        .with("python-uncaught", true)
        .and_then(|stops| stops.with("rust-panic", true))
        .expect("known filters");
    assert_eq!(exceptions::bits(chosen), agent::UNCAUGHT);
    let raised = crate::ExceptionStops::NONE
        .with("python-raised", true)
        .expect("a known filter");
    assert_eq!(exceptions::bits(raised), agent::RAISED);
}
