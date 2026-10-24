//! The exceptions JavaScript reports through Node's inspector, which
//! `Debugger.setPauseOnExceptions` chooses.

use crate::protocol::{ExceptionFilter, ExceptionStops, LanguageExceptionKind};

pub const EXCEPTION_FILTERS: [ExceptionFilter; 2] = [THROWN, UNCAUGHT];

const THROWN: ExceptionFilter = ExceptionFilter {
    id: "javascript-thrown",
    language: "JavaScript",
    kind: LanguageExceptionKind::Raised,
    label: "JavaScript: thrown",
    description: "Stop wherever JavaScript throws an exception or rejects a promise, whether or \
                  not the program then catches it",
    default: false,
};

const UNCAUGHT: ExceptionFilter = ExceptionFilter {
    id: "javascript-uncaught",
    language: "JavaScript",
    kind: LanguageExceptionKind::Unhandled,
    label: "JavaScript: uncaught",
    description: "Stop where JavaScript throws an exception or rejects a promise that nothing \
                  catches, before Node reports it",
    default: false,
};

/// The state `Debugger.setPauseOnExceptions` takes for `stops`.
pub fn pause_state(stops: ExceptionStops) -> &'static str {
    if stops.stops(THROWN.id) {
        "all"
    } else if stops.stops(UNCAUGHT.id) {
        "uncaught"
    } else {
        "none"
    }
}

/// The filter a pause on an exception stops for: `uncaught` says whether
/// V8 found nothing that catches it.
pub fn filter(stops: ExceptionStops, uncaught: bool) -> Option<&'static ExceptionFilter> {
    [&EXCEPTION_FILTERS[1], &EXCEPTION_FILTERS[0]]
        .into_iter()
        .filter(|filter| uncaught || filter.kind == LanguageExceptionKind::Raised)
        .find(|filter| stops.stops(filter.id))
}
