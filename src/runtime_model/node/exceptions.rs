use crate::protocol::{ExceptionFilter, ExceptionStops, LanguageExceptionKind};

pub const EXCEPTION_FILTERS: [ExceptionFilter; 2] = [THROWN, UNCAUGHT];
const _: () = assert!(matches!(
    EXCEPTION_FILTERS[0].kind,
    LanguageExceptionKind::Raised
));
const _: () = assert!(matches!(
    EXCEPTION_FILTERS[1].kind,
    LanguageExceptionKind::Unhandled
));

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

pub fn pause_state(stops: ExceptionStops) -> &'static str {
    if stops.stops(THROWN.id) {
        "all"
    } else if stops.stops(UNCAUGHT.id) {
        "uncaught"
    } else {
        "none"
    }
}

pub fn filter(stops: ExceptionStops, uncaught: bool) -> Option<&'static ExceptionFilter> {
    [&EXCEPTION_FILTERS[1], &EXCEPTION_FILTERS[0]]
        .into_iter()
        .filter(|filter| uncaught || filter.kind == LanguageExceptionKind::Raised)
        .find(|filter| stops.stops(filter.id))
}
