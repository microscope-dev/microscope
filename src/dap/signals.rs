use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use microscope::ExceptionStops;
use serde_json::{Value, json};

use super::protocol::SetExceptionBreakpointsArguments;

struct Filter {
    id: &'static str,
    label: &'static str,
    description: &'static str,
    default: bool,

    signals: Option<&'static [&'static str]>,
}

const FILTERS: [Filter; 4] = [
    Filter {
        id: "fatal",
        label: "Fatal signals",
        description: "Stop on SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT, SIGSYS, and SIGTRAP, \
                      except those a language runtime handles itself, such as faults it turns \
                      into exceptions",
        default: true,
        signals: Some(&[
            "SIGSEGV", "SIGBUS", "SIGILL", "SIGFPE", "SIGABRT", "SIGSYS", "SIGTRAP",
        ]),
    },
    Filter {
        id: "interrupt",
        label: "Interrupt (SIGINT)",
        description: "Stop on SIGINT without delivering it",
        default: true,
        signals: Some(&["SIGINT"]),
    },
    Filter {
        id: "routine",
        label: "Routine signals",
        description: "Stop on signals programs use for routine work: SIGALRM, SIGURG, SIGCHLD, \
                      SIGWINCH, SIGPROF, SIGVTALRM, SIGIO, and SIGPWR",
        default: false,
        signals: Some(&[
            "SIGALRM",
            "SIGURG",
            "SIGCHLD",
            "SIGWINCH",
            "SIGPROF",
            "SIGVTALRM",
            "SIGIO",
            "SIGPWR",
        ]),
    },
    Filter {
        id: "other",
        label: "Other signals",
        description: "Stop on every other signal, such as SIGUSR1, SIGTERM, SIGPIPE, and \
                      real-time signals",
        default: true,
        signals: None,
    },
];

pub fn filters() -> Value {
    let signals = FILTERS.iter().map(|filter| {
        json!({
            "filter": filter.id,
            "label": filter.label,
            "description": filter.description,
            "default": filter.default,
            "supportsCondition": true,
            "conditionDescription": "Comma-separated signals to stop on instead, such as SIGUSR1,SIGUSR2",
        })
    });
    let exceptions = ExceptionStops::filters().iter().map(|filter| {
        json!({
            "filter": filter.id,
            "label": filter.label,
            "description": filter.description,
            "default": filter.default,
        })
    });
    let syscalls = json!({
        "filter": SYSCALLS,
        "label": "System calls",
        "description": "Stop as system calls begin and return. While this is on, every call \
                        the program makes stops its thread briefly.",
        "default": false,
        "supportsCondition": true,
        "conditionDescription": "Comma-separated calls to stop at, such as openat,connect; every call when empty",
    });
    signals.chain(exceptions).chain([syscalls]).collect()
}

const SYSCALLS: &str = "syscalls";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    stopping: BTreeSet<u64>,
    exceptions: ExceptionStops,

    syscalls: Option<Arc<[microscope::syscalls::SyscallId]>>,
}

impl Default for Selection {
    fn default() -> Self {
        let enabled = FILTERS
            .iter()
            .filter(|filter| filter.default)
            .map(|filter| (filter.id, None))
            .collect();
        Self {
            exceptions: ExceptionStops::default(),
            ..Self::from_filters(&enabled)
        }
    }
}

fn syscall_list(text: &str) -> Result<Arc<[microscope::syscalls::SyscallId]>, String> {
    let mut calls = Vec::new();

    for name in text
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let named: Vec<_> = microscope::syscalls::Abi::ALL
            .into_iter()
            .filter_map(|abi| microscope::syscalls::SyscallId::named(abi, name))
            .collect();
        if named.is_empty() {
            return Err(format!("unknown system call '{name}'"));
        }

        calls.extend(named);
    }

    Ok(calls.into())
}

impl Selection {
    pub fn stops(&self, code: u64) -> bool {
        self.stopping.contains(&code)
    }

    pub const fn exceptions(&self) -> ExceptionStops {
        self.exceptions
    }

    pub const fn syscalls(&self) -> Option<&Arc<[microscope::syscalls::SyscallId]>> {
        self.syscalls.as_ref()
    }

    pub fn parse(arguments: &SetExceptionBreakpointsArguments) -> (Self, Vec<Value>) {
        let mut enabled = BTreeMap::new();
        let mut exceptions = ExceptionStops::NONE;
        let mut syscalls = None;
        let mut breakpoints = Vec::new();
        let options = arguments.filter_options.iter().flatten().map(|option| {
            (
                option.filter_id.as_str(),
                option
                    .condition
                    .as_deref()
                    .filter(|text| !text.trim().is_empty()),
            )
        });

        for (id, condition) in arguments
            .filters
            .iter()
            .map(|id| (id.as_str(), None))
            .chain(options)
        {
            if id == SYSCALLS {
                match syscall_list(condition.unwrap_or_default()) {
                    Ok(calls) => {
                        syscalls = Some(calls);
                        breakpoints.push(json!({"verified": true}));
                    }
                    Err(message) => breakpoints.push(unverified(&message)),
                }

                continue;
            }

            if let Some(chosen) = exceptions.with(id, true) {
                if condition.is_some() {
                    breakpoints.push(unverified(&format!(
                        "exception filter '{id}' takes no condition"
                    )));
                    continue;
                }

                exceptions = chosen;
                breakpoints.push(json!({"verified": true}));
                continue;
            }

            let Some(filter) = FILTERS.iter().find(|filter| filter.id == id) else {
                breakpoints.push(unverified(&format!("unknown exception filter '{id}'")));
                continue;
            };

            let signals = match condition.map(signal_list).transpose() {
                Ok(signals) => signals,
                Err(message) => {
                    breakpoints.push(unverified(&message));
                    continue;
                }
            };

            enabled.insert(filter.id, signals);
            breakpoints.push(json!({"verified": true}));
        }

        (
            Self {
                exceptions,
                syscalls,
                ..Self::from_filters(&enabled)
            },
            breakpoints,
        )
    }

    fn from_filters(enabled: &BTreeMap<&str, Option<BTreeSet<u64>>>) -> Self {
        let grouped = FILTERS
            .iter()
            .filter_map(|filter| filter.signals)
            .flatten()
            .filter_map(|name| microscope::signal_named(name))
            .collect::<BTreeSet<_>>();
        let mut stopping = BTreeSet::new();

        for filter in &FILTERS {
            match enabled.get(filter.id) {
                None => {}
                Some(Some(listed)) => stopping.extend(listed),
                Some(None) => match filter.signals {
                    Some(names) => {
                        stopping.extend(
                            names
                                .iter()
                                .filter_map(|name| microscope::signal_named(name)),
                        );
                    }
                    None => stopping
                        .extend(microscope::signal_codes().filter(|code| !grouped.contains(code))),
                },
            }
        }

        Self {
            stopping,
            exceptions: ExceptionStops::default(),
            syscalls: None,
        }
    }
}

fn unverified(message: &str) -> Value {
    json!({"verified": false, "message": message, "reason": "failed"})
}

fn signal_list(text: &str) -> Result<BTreeSet<u64>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            microscope::signal_named(name).ok_or_else(|| format!("unknown signal '{name}'"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dap::protocol::ExceptionFilterOptions;

    fn code(name: &str) -> u64 {
        microscope::signal_named(name).expect("known signal")
    }

    #[test]
    fn default_filters_stop_exactly_where_the_default_policy_does() {
        let selection = Selection::default();
        for name in [
            "SIGSEGV", "SIGABRT", "SIGINT", "SIGUSR1", "SIGTERM", "SIGPIPE", "SIG34",
        ] {
            assert!(selection.stops(code(name)), "{name}");
        }

        for name in [
            "SIGALRM", "SIGURG", "SIGCHLD", "SIGWINCH", "SIGPROF", "SIGPWR",
        ] {
            assert!(!selection.stops(code(name)), "{name}");
        }

        let advertised = filters();
        let defaults = advertised
            .as_array()
            .expect("filters")
            .iter()
            .filter(|filter| filter["default"] == json!(true))
            .map(|filter| filter["filter"].as_str().expect("id").to_owned())
            .collect::<Vec<_>>();
        let (parsed, _) = Selection::parse(&SetExceptionBreakpointsArguments {
            filters: defaults,
            filter_options: None,
        });
        assert_eq!(parsed, selection);
    }

    #[test]
    fn conditions_replace_a_filters_signals_and_bad_entries_are_unverified() {
        let (selection, breakpoints) = Selection::parse(&SetExceptionBreakpointsArguments {
            filters: vec!["fatal".to_owned(), "bogus".to_owned()],
            filter_options: Some(vec![
                ExceptionFilterOptions {
                    filter_id: "other".to_owned(),
                    condition: Some(" SIGUSR1, usr2 ".to_owned()),
                },
                ExceptionFilterOptions {
                    filter_id: "routine".to_owned(),
                    condition: Some("SIGNOPE".to_owned()),
                },
            ]),
        });
        assert!(selection.stops(code("SIGSEGV")));
        assert_eq!(selection.exceptions(), ExceptionStops::NONE);
        assert!(selection.stops(code("SIGUSR1")) && selection.stops(code("SIGUSR2")));
        assert!(!selection.stops(code("SIGTERM")) && !selection.stops(code("SIGINT")));
        assert!(!selection.stops(code("SIGALRM")));
        assert_eq!(
            breakpoints,
            [
                json!({"verified": true}),
                unverified("unknown exception filter 'bogus'"),
                json!({"verified": true}),
                unverified("unknown signal 'SIGNOPE'"),
            ]
        );
    }
}
