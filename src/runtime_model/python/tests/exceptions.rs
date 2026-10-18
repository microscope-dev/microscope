use super::super::{agent, exceptions};
use super::*;

/// An exception record at `HEAP`: the object's address, then the message's
/// address and length, with the message after it.
fn record(object: u64, message: &[u8], length: u64) -> Memory {
    let mut memory = Memory::default();
    let text = HEAP + 0x100;
    memory.write(HEAP, object, 8);
    memory.write(HEAP + 8, text, 8);
    memory.write(HEAP + 16, length, 8);
    memory.fill(text, message);
    memory
}

/// A report reads as the exception's message, as the agent formed it, and
/// a value that names its object; a record that cannot be what the agent
/// wrote is refused, never cut or guessed at.
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

/// Each filter has its own bit, which the agent's reports name it by, and
/// the bits a choice of exceptions sets are those of the Python filters it
/// stops; other runtimes' filters keep their places.
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

    let filters = crate::ExceptionStops::filters();
    assert_eq!(
        filters.iter().map(|filter| filter.id).collect::<Vec<_>>(),
        [
            "unhandled",
            "runtime-fatal",
            "raised",
            "rust-panic",
            "python-raised",
            "python-uncaught"
        ]
    );
    assert!(!raised.default && !uncaught.default);
    assert_eq!(exceptions::bits(crate::ExceptionStops::default()), 0);
    assert_eq!(exceptions::bits(crate::ExceptionStops::NONE), 0);
    let chosen = crate::ExceptionStops::NONE
        .with("python-uncaught", true)
        .and_then(|stops| stops.with("rust-panic", true))
        .expect("known filters");
    assert_eq!(exceptions::bits(chosen), agent::UNCAUGHT);
}
