//! Python's exceptions, as its agent reports them: raised, wherever Python
//! code raises one or one reaches it, and uncaught, as one leaves a thread's
//! outermost Python frame. Either reports the exception's object and a
//! message the agent formed without running the program's code.

use std::sync::Arc;

use super::agent;
use crate::runtime_model::{RuntimeException, RuntimeStop};
use crate::{ExceptionFilter, ExceptionStops, LanguageExceptionKind, VirtualAddress};

pub const EXCEPTION_FILTERS: [ExceptionFilter; 2] = [RAISED, UNCAUGHT];

const RAISED: ExceptionFilter = ExceptionFilter {
    id: "python-raised",
    language: "Python",
    kind: LanguageExceptionKind::Raised,
    label: "Every Python exception",
    description: "Stop wherever Python code raises an exception, or one reaches Python code, \
                  whether or not the program then catches it",
    default: false,
};

const UNCAUGHT: ExceptionFilter = ExceptionFilter {
    id: "python-uncaught",
    language: "Python",
    kind: LanguageExceptionKind::Unhandled,
    label: "Uncaught Python exceptions",
    description: "Stop where an exception leaves a thread's outermost Python frame, before Python \
                  prints its traceback",
    default: false,
};

/// The longest message read. The agent bounds what it forms well below
/// this, so a longer one is a malformed report, not a message to cut.
const MAX_MESSAGE: u64 = 1 << 13;

/// The filter the agent reports under `bit`.
pub const fn filter(bit: u64) -> Option<&'static ExceptionFilter> {
    match bit {
        agent::RAISED => Some(&EXCEPTION_FILTERS[0]),
        agent::UNCAUGHT => Some(&EXCEPTION_FILTERS[1]),
        _ => None,
    }
}

/// The agent's bits for the filters `stops` stops.
pub fn bits(stops: ExceptionStops) -> u64 {
    [agent::RAISED, agent::UNCAUGHT]
        .into_iter()
        .filter(|&bit| filter(bit).is_some_and(|filter| stops.stops(filter.id)))
        .fold(0, |bits, bit| bits | bit)
}

/// The exception the agent reports in the record at `address`: the
/// object's address, then its message's address and length in bytes,
/// which the reporting thread keeps alive while it is in the hook.
pub fn read(stop: &dyn RuntimeStop, address: u64) -> Result<RuntimeException, Arc<str>> {
    let mut record = [0; 24];
    if !stop.read(VirtualAddress::new(address), &mut record) {
        return Err(
            format!("the Python agent's exception record at {address:#x} is unreadable").into(),
        );
    }
    let word = |index: usize| {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&record[index * 8..index * 8 + 8]);
        u64::from_le_bytes(bytes)
    };
    let (object, text, length) = (word(0), word(1), word(2));
    if object == 0 || text == 0 || length > MAX_MESSAGE {
        return Err("the Python agent's exception record is malformed".into());
    }

    // At most `MAX_MESSAGE`, so it fits.
    let mut bytes = vec![0; usize::try_from(length).unwrap_or(usize::MAX)];
    if !stop.read(VirtualAddress::new(text), &mut bytes) {
        return Err(format!("the Python exception's message at {text:#x} is unreadable").into());
    }
    let message = String::from_utf8(bytes)
        .map_err(|_| Arc::<str>::from("the Python exception's message is not UTF-8"))?;
    Ok(RuntimeException {
        message: message.into(),
        value: Some(format!("(py){object:#x}").into()),
    })
}
