//! strace's selection language: which calls to trace and which of their
//! results to show.
//!
//! The set of calls is applied where the engine sees
//! calls, in the kernel when it can; the rest applies to finished events.

use std::collections::BTreeSet;

use regex::Regex;

use super::model::{Abi, Outcome, SyscallEvent, SyscallId};
use super::specs::{self, Class};
use super::table;

/// A set of system calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSet {
    calls: BTreeSet<SyscallId>,

    /// The same calls as a bitmap per ABI, by number, which a tracer asks
    /// about every call it sees.
    bits: [Vec<u64>; 3],
}

impl CallSet {
    #[must_use]
    pub fn all() -> Self {
        Self::of(table::all().collect())
    }

    #[must_use]
    pub const fn none() -> Self {
        Self {
            calls: BTreeSet::new(),
            bits: [Vec::new(), Vec::new(), Vec::new()],
        }
    }

    fn of(calls: BTreeSet<SyscallId>) -> Self {
        let mut set = Self {
            calls,
            bits: Default::default(),
        };

        set.index();
        set
    }

    fn index(&mut self) {
        let mut bits: [Vec<u64>; 3] = Default::default();

        for id in &self.calls {
            let words = &mut bits[id.abi as usize];
            let word = (id.number / 64) as usize;
            if words.len() <= word {
                words.resize(word + 1, 0);
            }

            words[word] |= 1 << (id.number % 64);
        }

        self.bits = bits;
    }

    #[must_use]
    pub fn contains(&self, id: SyscallId) -> bool {
        self.bits[id.abi as usize]
            .get((id.number / 64) as usize)
            .is_some_and(|word| word & (1 << (id.number % 64)) != 0)
    }

    #[must_use]
    pub fn is_all(&self) -> bool {
        table::all().all(|id| self.calls.contains(&id))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.calls.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = SyscallId> + '_ {
        self.calls.iter().copied()
    }

    /// The numbers it holds under one ABI.
    pub fn numbers(&self, abi: Abi) -> impl Iterator<Item = u32> + '_ {
        self.calls
            .iter()
            .filter(move |id| id.abi == abi)
            .map(|id| id.number)
    }

    pub fn insert(&mut self, id: SyscallId) {
        self.calls.insert(id);
        self.index();
    }

    fn extend(&mut self, other: &BTreeSet<SyscallId>) {
        self.calls.extend(other);
        self.index();
    }

    fn remove(&mut self, other: &BTreeSet<SyscallId>) {
        self.calls.retain(|id| !other.contains(id));
        self.index();
    }
}

/// Which outcomes to show, as `-e status=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(clippy::struct_excessive_bools, reason = "strace's four statuses")]
pub struct StatusSet {
    pub successful: bool,
    pub failed: bool,
    pub unfinished: bool,
    pub unavailable: bool,
}

impl StatusSet {
    pub const ALL: Self = Self {
        successful: true,
        failed: true,
        unfinished: true,
        unavailable: true,
    };

    #[must_use]
    pub const fn admits(self, outcome: Outcome) -> bool {
        match outcome {
            Outcome::Returned(_) => self.successful,
            Outcome::Failed(_) => self.failed,
            Outcome::Unfinished => self.unfinished,
            Outcome::ProcessExited | Outcome::Unavailable => self.unavailable,
        }
    }
}

/// Why an expression did not parse, worded for the command line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FilterError(pub String);

fn error<T>(message: impl Into<String>) -> Result<T, FilterError> {
    Err(FilterError(message.into()))
}

/// Everything the selection flags say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// Calls to trace.
    pub calls: CallSet,

    /// Outcomes to show.
    pub status: StatusSet,

    /// Show only calls that name one of these paths, by argument or
    /// descriptor (`-P`).
    pub paths: Vec<String>,

    /// Show only calls on these descriptors (`--trace-fds`).
    pub fds: Option<BTreeSet<i64>>,

    /// Signals to show (`-e signal=`), by number.
    pub signals: BTreeSet<i32>,

    /// Descriptors whose reads and writes are dumped in hex.
    pub read_dumps: BTreeSet<i64>,
    pub write_dumps: BTreeSet<i64>,

    /// Show only calls that took at least this long.
    pub slower_than: Option<u64>,

    /// Calls whose arguments are left raw (`-e raw=`).
    pub raw: CallSet,

    /// Calls whose structures are printed in full (`-e verbose=`), or
    /// abbreviated when not (`-e abbrev=`).
    pub verbose: CallSet,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            calls: CallSet::all(),
            status: StatusSet::ALL,
            paths: Vec::new(),
            fds: None,
            signals: (1..=64).collect(),
            read_dumps: BTreeSet::new(),
            write_dumps: BTreeSet::new(),
            slower_than: None,
            raw: CallSet::none(),
            verbose: CallSet::all(),
        }
    }
}

/// The calls one element names: a name, `/regex`, `%class`, `all`, or
/// `none`, each optionally `@64`, `@32`, or `@x32`; `?` before it ignores a
/// name that matches nothing.
fn element(text: &str) -> Result<BTreeSet<SyscallId>, FilterError> {
    let (optional, text) = text
        .strip_prefix('?')
        .map_or((false, text), |rest| (true, rest));
    let (body, abis): (&str, Vec<Abi>) = match text.rsplit_once('@') {
        Some((body, suffix)) if !body.is_empty() => match Abi::from_suffix(suffix) {
            Some(abi) => (body, vec![abi]),
            None => return error(format!("unknown personality @{suffix} in {text:?}")),
        },
        _ => (text, Abi::ALL.to_vec()),
    };

    let calls = |predicate: &dyn Fn(SyscallId, &str) -> bool| -> BTreeSet<SyscallId> {
        abis.iter()
            .flat_map(|&abi| {
                table::calls(abi)
                    .iter()
                    .map(move |&(number, name)| (SyscallId::new(abi, number), name))
            })
            .filter(|&(id, name)| predicate(id, name))
            .map(|(id, _)| id)
            .collect()
    };

    let found = match body {
        "all" => calls(&|_, _| true),
        "none" => BTreeSet::new(),
        _ => {
            if let Some(pattern) = body.strip_prefix('/') {
                let regex = Regex::new(pattern).map_err(|source| {
                    FilterError(format!("bad regular expression {pattern:?}: {source}"))
                })?;
                calls(&|_, name| regex.is_match(name))
            } else if let Some(name) = body.strip_prefix('%').or_else(|| {
                // strace still accepts the classes' old names without `%`,
                // where no call has the name.
                Class::named(body)
                    .filter(|_| table::number(Abi::X86_64, body).is_none())
                    .map(|_| body)
            }) {
                // `%%stat` and `%%statfs` take every variant.
                let classes = match name {
                    "%stat" => vec![Class::Stat, Class::Lstat, Class::Fstat],
                    "%statfs" => vec![Class::Statfs, Class::Fstatfs],
                    _ => match Class::named(name) {
                        Some(class) => vec![class],
                        None => return error(format!("unknown class %{name}")),
                    },
                };

                // strace's `%%statfs` takes ustat too, which no class does.
                let ustat = name == "%statfs";

                calls(&|id, call| {
                    (ustat && call == "ustat")
                        || specs::spec(id)
                            .or_else(|| specs::spec_named(call))
                            .is_some_and(|spec| {
                                classes.iter().any(|&class| spec.classes.contains(class))
                            })
                })
            } else {
                calls(&|_, name| name == body)
            }
        }
    };

    if found.is_empty() && body != "none" && !optional {
        return error(format!("invalid system call {text:?}"));
    }

    Ok(found)
}

/// A set expression: elements separated by commas, all negated by a
/// leading `!`.
fn set(text: &str) -> Result<(bool, BTreeSet<SyscallId>), FilterError> {
    let (negated, text) = text
        .strip_prefix('!')
        .map_or((false, text), |rest| (true, rest));
    let mut calls = BTreeSet::new();
    for part in text.split(',').filter(|part| !part.is_empty()) {
        calls.extend(element(part)?);
    }

    Ok((negated, calls))
}

fn numbers(text: &str, what: &str) -> Result<BTreeSet<i64>, FilterError> {
    let (negated, text) = text
        .strip_prefix('!')
        .map_or((false, text), |rest| (true, rest));
    if negated {
        return error(format!("{what} cannot be negated"));
    }

    text.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse()
                .map_err(|_| FilterError(format!("invalid {what} {part:?}")))
        })
        .collect()
}

/// Signals by name (`SIGINT` or `INT`), number, or `all`.
fn signal_set(
    text: &str,
    named: &dyn Fn(&str) -> Option<i32>,
) -> Result<BTreeSet<i32>, FilterError> {
    let (negated, text) = text
        .strip_prefix('!')
        .map_or((false, text), |rest| (true, rest));
    let mut signals = BTreeSet::new();

    for part in text.split(',').filter(|part| !part.is_empty()) {
        match part {
            "all" => signals.extend(1..=64),
            "none" => {}
            _ => {
                let number = part
                    .parse()
                    .ok()
                    .filter(|n| (1..=64).contains(n))
                    .or_else(|| named(part).or_else(|| named(&format!("SIG{part}"))));
                match number {
                    Some(number) => {
                        signals.insert(number);
                    }
                    None => return error(format!("invalid signal {part:?}")),
                }
            }
        }
    }

    Ok(if negated {
        (1..=64)
            .filter(|signal| !signals.contains(signal))
            .collect()
    } else {
        signals
    })
}

impl Filter {
    /// Applies `-e` expressions in order. `trace=` sets accumulate: the
    /// first plain set replaces "all", later ones add, and negated ones
    /// remove.
    pub fn parse(
        expressions: &[String],
        signal_named: &dyn Fn(&str) -> Option<i32>,
    ) -> Result<Self, FilterError> {
        let mut filter = Self::default();
        let mut traced_by_name = false;

        for expression in expressions {
            let (qualifier, value) = expression.split_once('=').unwrap_or(("trace", expression));

            match qualifier {
                "trace" | "t" => {
                    let (negated, calls) = set(value)?;
                    if negated {
                        filter.calls.remove(&calls);
                    } else {
                        if !traced_by_name {
                            filter.calls = CallSet::none();
                        }

                        filter.calls.extend(&calls);
                    }

                    traced_by_name = true;
                }
                "status" => filter.status = Self::status(value)?,
                "signal" | "signals" | "s" => filter.signals = signal_set(value, signal_named)?,
                "read" | "reads" | "r" => filter.read_dumps = numbers(value, "descriptor")?,
                "write" | "writes" | "w" => filter.write_dumps = numbers(value, "descriptor")?,
                "trace-fds" | "trace-fd" | "fd" | "fds" => {
                    filter.fds = Some(numbers(value, "descriptor")?);
                }
                "raw" | "x" => {
                    let (negated, calls) = set(value)?;
                    filter.raw = if negated {
                        complement(&calls)
                    } else {
                        from(calls)
                    };
                }
                "verbose" | "v" => {
                    let (negated, calls) = set(value)?;
                    filter.verbose = if negated {
                        complement(&calls)
                    } else {
                        from(calls)
                    };
                }
                "abbrev" | "a" => {
                    let (negated, calls) = set(value)?;
                    filter.verbose = if negated {
                        from(calls)
                    } else {
                        complement(&calls)
                    };
                }
                "inject" | "fault" => {
                    return error(format!(
                        "-e {qualifier}= is not supported: microscope never changes what a system call does"
                    ));
                }
                other => return error(format!("unknown qualifier {other:?}")),
            }
        }

        Ok(filter)
    }

    fn status(text: &str) -> Result<StatusSet, FilterError> {
        let (negated, text) = text
            .strip_prefix('!')
            .map_or((false, text), |rest| (true, rest));
        let mut set = StatusSet {
            successful: false,
            failed: false,
            unfinished: false,
            unavailable: false,
        };

        for part in text.split(',').filter(|part| !part.is_empty()) {
            match part {
                "all" => set = StatusSet::ALL,
                "none" => {}
                "successful" => set.successful = true,
                "failed" => set.failed = true,
                "unfinished" => set.unfinished = true,
                "unavailable" | "detached" => set.unavailable = true,
                other => return error(format!("invalid status {other:?}")),
            }
        }

        Ok(if negated {
            StatusSet {
                successful: !set.successful,
                failed: !set.failed,
                unfinished: !set.unfinished,
                unavailable: !set.unavailable,
            }
        } else {
            set
        })
    }

    /// Whether a finished call is shown. `path_of` names what a descriptor
    /// of the call's process refers to, when the trace knows; `paths`
    /// gives the call's own path arguments.
    #[must_use]
    pub fn shows(
        &self,
        event: &SyscallEvent,
        paths: &[&str],
        path_of: &dyn Fn(i64) -> Option<String>,
    ) -> bool {
        if !self.calls.contains(event.call) || !self.status.admits(event.outcome) {
            return false;
        }

        if let Some(slower) = self.slower_than
            && event.duration().is_none_or(|duration| duration < slower)
        {
            return false;
        }

        let fds = || descriptors(event);

        if let Some(wanted) = &self.fds
            && !fds().any(|fd| wanted.contains(&fd))
        {
            return false;
        }

        if !self.paths.is_empty() {
            let named = paths
                .iter()
                .any(|path| self.paths.iter().any(|wanted| wanted == path));
            let by_fd =
                || fds().any(|fd| path_of(fd).is_some_and(|path| self.paths.contains(&path)));

            if !named && !by_fd() {
                return false;
            }
        }

        true
    }
}

fn from(calls: BTreeSet<SyscallId>) -> CallSet {
    CallSet::of(calls)
}

fn complement(calls: &BTreeSet<SyscallId>) -> CallSet {
    let mut all = CallSet::all();
    all.remove(calls);
    all
}

/// The descriptors a call names, by argument or result.
pub fn descriptors(event: &SyscallEvent) -> impl Iterator<Item = i64> + '_ {
    let spec = specs::spec(event.call);
    let arguments = spec
        .into_iter()
        .flat_map(|spec| spec.args.iter().enumerate())
        .filter(|(_, arg)| matches!(arg, specs::Arg::Fd | specs::Arg::DirFd))
        .map(|(index, _)| fd_value(event.args[index]))
        .filter(|&fd| fd >= 0);

    let result = spec
        .filter(|spec| spec.ret == specs::Ret::Fd)
        .and_then(|_| event.outcome.raw())
        .filter(|&fd| fd >= 0);
    arguments.chain(result)
}

/// The paths a call names by argument, as captured.
#[must_use]
pub fn paths(event: &SyscallEvent) -> Vec<String> {
    let Some(spec) = specs::spec(event.call) else {
        return Vec::new();
    };

    spec.args
        .iter()
        .enumerate()
        .filter(|(_, arg)| matches!(arg, specs::Arg::Path))
        .filter_map(|(index, _)| {
            let capture = event.capture(u8::try_from(index).ok()?)?;
            let bytes = capture.bytes.split(|&b| b == 0).next().unwrap_or_default();
            Some(String::from_utf8_lossy(bytes).into_owned())
        })
        .collect()
}

/// A descriptor argument as the kernel reads it: an `int`.
#[must_use]
pub fn fd_value(word: u64) -> i64 {
    #[expect(clippy::cast_possible_truncation, reason = "the kernel reads an int")]
    i64::from(word as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscalls::model::{Pid, Tid};

    fn signal(name: &str) -> Option<i32> {
        match name {
            "SIGINT" => Some(2),
            "SIGTERM" => Some(15),
            _ => None,
        }
    }

    fn parse(expressions: &[&str]) -> Result<Filter, FilterError> {
        Filter::parse(
            &expressions
                .iter()
                .map(|&text| text.to_owned())
                .collect::<Vec<_>>(),
            &signal,
        )
    }

    fn named(name: &str) -> SyscallId {
        SyscallId::named(Abi::X86_64, name).expect(name)
    }

    #[test]
    fn trace_sets_take_names_classes_regexes_and_negation() {
        let filter = parse(&["trace=%file,connect", "trace=!stat"]).expect("parses");
        assert!(filter.calls.contains(named("openat")));
        assert!(filter.calls.contains(named("connect")));
        assert!(!filter.calls.contains(named("stat")));
        assert!(!filter.calls.contains(named("read")));
        assert!(
            filter
                .calls
                .contains(SyscallId::named(Abi::X86_32, "openat").expect("i386"))
        );

        let only_64 = parse(&["openat@64"]).expect("parses");
        assert_eq!(only_64.calls.len(), 1);

        let regex = parse(&["trace=/^epoll_"]).expect("parses");
        assert!(regex.calls.contains(named("epoll_wait")));
        assert!(!regex.calls.contains(named("poll")));

        let all_but = parse(&["trace=!%net"]).expect("parses");
        assert!(all_but.calls.contains(named("read")) && !all_but.calls.contains(named("sendto")));

        assert!(parse(&["trace=nosuchcall"]).is_err());
        assert!(parse(&["trace=?nosuchcall,read"]).is_ok());
        assert!(parse(&["trace=%nosuchclass"]).is_err());
        assert!(parse(&["inject=openat:error=ENOENT"]).is_err());
    }

    #[test]
    fn statuses_signals_and_descriptors_filter_events() {
        let filter = parse(&["status=failed", "signal=!INT", "trace-fds=3"]).expect("parses");
        assert!(!filter.signals.contains(&2) && filter.signals.contains(&15));
        let mut event = SyscallEvent {
            thread: Tid::new(1),
            process: Pid::new(1),
            call: named("read"),
            args: [3, 0, 10, 0, 0, 0],
            captured: Vec::new(),
            outcome: Outcome::of_return(-2),
            entered: Some(10),
            exited: Some(20),
            stack: None,
            flags: super::super::model::EventFlags::default(),
        };

        assert!(filter.shows(&event, &[], &|_| None));
        event.args[0] = 4;
        assert!(!filter.shows(&event, &[], &|_| None));
        event.args[0] = 3;
        event.outcome = Outcome::Returned(1);
        assert!(!filter.shows(&event, &[], &|_| None));

        let by_path = Filter {
            paths: vec!["/etc/hosts".to_owned()],
            ..Filter::default()
        };

        assert!(by_path.shows(&event, &[], &|fd| {
            (fd == 3).then(|| "/etc/hosts".to_owned())
        }));
        assert!(!by_path.shows(&event, &[], &|_| None));
        assert!(by_path.shows(&event, &["/etc/hosts"], &|_| None));
    }
}
