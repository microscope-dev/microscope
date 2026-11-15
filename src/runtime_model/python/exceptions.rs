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

const MAX_MESSAGE: u64 = 1 << 13;

const _: () = assert!(
    MAX_MESSAGE <= usize::MAX as u64,
    "an exception's message fits a usize"
);
const _: () = assert!(
    matches!(EXCEPTION_FILTERS[0].kind, LanguageExceptionKind::Raised),
    "the agent's raised bit names the raised filter"
);
const _: () = assert!(
    matches!(EXCEPTION_FILTERS[1].kind, LanguageExceptionKind::Unhandled),
    "the agent's uncaught bit names the uncaught filter"
);

pub const fn filter(bit: u64) -> Option<&'static ExceptionFilter> {
    match bit {
        agent::RAISED => Some(&EXCEPTION_FILTERS[0]),
        agent::UNCAUGHT => Some(&EXCEPTION_FILTERS[1]),
        _ => None,
    }
}

pub fn bits(stops: ExceptionStops) -> u64 {
    [agent::RAISED, agent::UNCAUGHT]
        .into_iter()
        .filter(|&bit| filter(bit).is_some_and(|filter| stops.stops(filter.id)))
        .fold(0, |bits, bit| bits | bit)
}

pub fn read(stop: &dyn RuntimeStop, address: u64) -> Result<RuntimeException, Arc<str>> {
    let mut record = [0; 24];
    if !stop.read(VirtualAddress::new(address), &mut record) {
        return Err(
            format!("the Python agent's exception record at {address:#x} is unreadable").into(),
        );
    }
    let &[object, text, length] = record.as_chunks::<8>().0 else {
        return Err("the Python agent's exception record is malformed".into());
    };
    let (object, text, length) = (
        u64::from_le_bytes(object),
        u64::from_le_bytes(text),
        u64::from_le_bytes(length),
    );
    if object == 0 || text == 0 || length > MAX_MESSAGE {
        return Err("the Python agent's exception record is malformed".into());
    }

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
