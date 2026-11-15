use std::collections::VecDeque;
use std::fmt;

use super::choices::{fnv1a, fnv1a_continue};

pub struct Trace {
    lines: VecDeque<String>,
    keep: Option<usize>,
    fingerprint: u64,
    dropped: u64,
}

impl Trace {
    #[must_use]
    pub const fn new(keep: Option<usize>) -> Self {
        Self {
            lines: VecDeque::new(),
            keep,
            fingerprint: fnv1a(b""),
            dropped: 0,
        }
    }

    pub fn line(&mut self, line: String) {
        self.fingerprint = fnv1a_continue(self.fingerprint, line.as_bytes());
        self.fingerprint = fnv1a_continue(self.fingerprint, b"\n");
        if self.keep.is_some_and(|keep| self.lines.len() == keep) {
            self.lines.pop_front();
            self.dropped += 1;
        }

        self.lines.push_back(line);
    }

    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    #[must_use]
    pub const fn lines(&self) -> (&VecDeque<String>, u64) {
        (&self.lines, self.dropped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Debugger,

    ModelGap,

    Simulator,
}

impl fmt::Display for FailureKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Debugger => "debugger",
            Self::ModelGap => "model gap",
            Self::Simulator => "simulator",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailureKind,

    pub check: &'static str,
    pub message: String,

    pub step: u64,
}

impl Failure {
    #[must_use]
    pub fn debugger(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Debugger,
            check,
            message: message.into(),
            step: 0,
        }
    }

    #[must_use]
    pub fn model_gap(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::ModelGap,
            check: "model",
            message: message.into(),
            step: 0,
        }
    }

    #[must_use]
    pub fn simulator(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Simulator,
            check,
            message: message.into(),
            step: 0,
        }
    }

    #[must_use]
    pub fn signature(&self) -> String {
        let words = self.message.split(['{', '[']).next().unwrap_or_default();
        format!(
            "{} {}: {}",
            self.kind,
            self.check,
            without_numbers(words).trim_end()
        )
    }
}

fn without_numbers(text: &str) -> String {
    let mut shape = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(next) = chars.next() {
        if next.is_ascii_digit() {
            while chars
                .next_if(|next| next.is_ascii_hexdigit() || *next == 'x')
                .is_some()
            {}
            shape.push('#');
        } else {
            shape.push(next);
        }
    }

    shape
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failure at #{}, {}: {}",
            self.kind, self.step, self.check, self.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_leave_out_what_differs_between_runs_of_one_bug() {
        let hit = |tid: u64, address: u64, count: u64| {
            Failure::debugger(
                "breakpoint accounting",
                format!(
                    "breakpoint 1's hit count went from {count} to {count}; thread {tid} \
                     trapped at {address:#x}, and the breakpoint owned that site"
                ),
            )
        };

        assert_eq!(
            hit(1004, 0x7fff_f7ff_7620, 0).signature(),
            hit(2013, 0x40_1334, 17).signature()
        );
        assert_eq!(
            hit(1004, 0x40_1334, 0).signature(),
            "debugger breakpoint accounting: breakpoint #'s hit count went from # to #; \
             thread # trapped at #, and the breakpoint owned that site"
        );
        let stop = |reason: &str| {
            Failure::debugger("protocol", format!("stop 7 reports {reason}")).signature()
        };

        assert_eq!(
            stop("Exception(ExceptionInfo { signal: 11 })"),
            stop("Exception(ExceptionInfo { signal: 5 })")
        );
        assert_ne!(
            stop("Exception(ExceptionInfo { signal: 11 })"),
            stop("Unclassifiable { address: 0x1000 }")
        );
        assert_ne!(
            hit(1004, 0x40_1334, 0).signature(),
            Failure::debugger(
                "breakpoint accounting",
                "breakpoint 1 owns no site at 0x401334"
            )
            .signature()
        );
    }
}
