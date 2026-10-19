//! A trace without end, kept as the profiler keeps a continuous recording:
//! a segment file every interval, holding that span's statistics.
//!
//! The last seconds of calls are held in memory, and a trigger writes them
//! to `windows/` beside the segments: a signal, a call slower than a
//! threshold, or a call that fails with a chosen errno. Windows are not
//! segments, so reading the directory counts no call twice.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::syscalls::stats::Folded;
use crate::syscalls::{Errno, Outcome, SyscallEvent, TraceEvent};

use super::capture::SyscallCapture;
use super::stacks::SharedStacks;

/// Where and how a continuous trace is kept.
#[derive(Debug, Clone)]
pub struct ContinuousOptions {
    pub directory: PathBuf,

    /// How long segments and windows are kept.
    pub keep: Duration,

    /// Nanoseconds each segment spans.
    pub segment: u64,

    /// Nanoseconds of calls a window holds; 0 keeps none.
    pub window: u64,

    /// A call that took at least this many nanoseconds writes a window.
    pub slower_than: Option<u64>,

    /// A call that fails with one of these writes a window.
    pub errnos: Vec<u16>,

    /// Settings each file records, such as the engine.
    pub settings: Vec<(String, String)>,
}

/// What a written window holds, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    pub calls: usize,
    pub reason: String,
}

/// A continuous trace's state between files.
#[derive(Debug)]
pub struct Continuous {
    options: ContinuousOptions,
    stacks: Option<SharedStacks>,

    /// When the segment began.
    began: u64,
    stats: Folded,

    /// What else happened over the segment: exits, losses, and the like.
    other: Vec<TraceEvent>,

    /// Every event of the last window's span.
    window: VecDeque<TraceEvent>,

    /// When a window was last written; triggers sooner than a window's
    /// span after it are let go.
    written: Option<u64>,
}

impl Continuous {
    /// A continuous trace whose first segment begins at `now`.
    #[must_use]
    pub fn new(options: ContinuousOptions, stacks: Option<SharedStacks>, now: u64) -> Self {
        Self {
            options,
            stacks,
            began: now,
            stats: Folded::default(),
            other: Vec::new(),
            window: VecDeque::new(),
            written: None,
        }
    }

    /// Records `key` in every file written from now on.
    pub fn setting(&mut self, key: &str, value: &str) {
        self.options
            .settings
            .push((key.to_owned(), value.to_owned()));
    }

    /// Whether the segment has run its span by `now`.
    #[must_use]
    pub const fn segment_due(&self, now: u64) -> bool {
        now.saturating_sub(self.began) >= self.options.segment
    }

    /// Adds one event; a call that meets a trigger writes the window.
    ///
    /// # Errors
    ///
    /// When writing a window fails.
    pub fn event(&mut self, event: &TraceEvent) -> std::io::Result<Option<Saved>> {
        let time = event.time();
        match event {
            TraceEvent::Call(call) => self.stats.add(call),
            other => self.other.push(other.clone()),
        }

        if self.options.window > 0 {
            self.window.push_back(event.clone());
            let since = time.saturating_sub(self.options.window);
            while self.window.front().is_some_and(|old| old.time() < since) {
                self.window.pop_front();
            }
        }

        let TraceEvent::Call(call) = event else {
            return Ok(None);
        };

        self.triggered_by(call)
            .map_or(Ok(None), |reason| self.trigger(&reason, time))
    }

    fn triggered_by(&self, call: &SyscallEvent) -> Option<String> {
        let name = call.call.display_name();
        if let (Some(threshold), Some(took)) = (self.options.slower_than, call.duration())
            && took >= threshold
        {
            return Some(format!("{name} took {took} ns"));
        }

        match call.outcome {
            Outcome::Failed(Errno(errno)) if self.options.errnos.contains(&errno) => {
                Some(format!("{name} failed with errno {errno}"))
            }
            _ => None,
        }
    }

    /// Adds statistics the kernel counted over the segment.
    pub fn counted(&mut self, stats: &Folded) {
        self.stats.merge(stats);
    }

    /// Writes the window as it is at `now`, for `reason`, unless one was
    /// written less than a window's span before.
    ///
    /// # Errors
    ///
    /// When writing it fails.
    pub fn trigger(&mut self, reason: &str, now: u64) -> std::io::Result<Option<Saved>> {
        if self.options.window == 0
            || self
                .written
                .is_some_and(|written| now.saturating_sub(written) < self.options.window)
        {
            return Ok(None);
        }

        self.written = Some(now);
        let mut capture = self.capture();
        capture.setting("trigger", reason);
        let mut calls = 0;
        for event in &self.window {
            match event {
                TraceEvent::Call(call) => {
                    calls += 1;
                    capture.call(call);
                }
                other => capture.event(other),
            }
        }

        let directory = self.options.directory.join("windows");
        std::fs::create_dir_all(&directory)?;
        let path = capture
            .finish()
            .write_segment(&directory, self.options.keep)?;

        Ok(Some(Saved {
            path,
            calls,
            reason: reason.to_owned(),
        }))
    }

    /// Writes the segment that ends at `now` and begins the next.
    ///
    /// # Errors
    ///
    /// When writing it fails; the segment's statistics are then lost.
    pub fn end_segment(&mut self, now: u64) -> std::io::Result<PathBuf> {
        let mut capture = self.capture();
        for event in self.other.drain(..) {
            capture.event(&event);
        }

        capture.span(self.began, now);
        capture.counted(&std::mem::take(&mut self.stats));
        self.began = now;
        capture
            .finish()
            .write_segment(&self.options.directory, self.options.keep)
    }

    fn capture(&self) -> SyscallCapture {
        let mut capture = SyscallCapture::new(self.stacks.clone());
        for (key, value) in &self.options.settings {
            capture.setting(key, value);
        }

        capture
    }

    /// The directory segments go to.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.options.directory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiler::Profile;
    use crate::syscalls::{Abi, EventFlags, Pid, SyscallId, Tid};

    fn call(name: &str, entered: u64, took: u64, outcome: Outcome) -> TraceEvent {
        TraceEvent::Call(SyscallEvent {
            thread: Tid::new(7),
            process: Pid::new(7),
            call: SyscallId::named(Abi::X86_64, name).expect(name),
            args: [0; 6],
            captured: Vec::new(),
            outcome,
            entered: Some(entered),
            exited: Some(entered + took),
            stack: None,
            flags: EventFlags::default(),
        })
    }

    #[test]
    fn segments_hold_their_span_s_counts_and_triggers_write_the_last_calls() {
        let directory =
            std::env::temp_dir().join(format!("microscope-continuous-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("a directory");
        let second = 1_000_000_000;
        let mut trace = Continuous::new(
            ContinuousOptions {
                directory: directory.clone(),
                keep: Duration::from_hours(1),
                segment: second,
                window: 2 * second,
                slower_than: Some(second / 10),
                errnos: vec![5],
                settings: vec![("engine".into(), "bpf".into())],
            },
            None,
            0,
        );
        let fast = |at| call("getppid", at, 10, Outcome::Returned(1));
        for at in 0..10 {
            assert_eq!(trace.event(&fast(at * 1000)).expect("added"), None);
        }

        let mut kernel = Folded::default();
        if let TraceEvent::Call(counted) = fast(0) {
            for _ in 0..5 {
                kernel.add(&counted);
            }
        }

        trace.counted(&kernel);
        assert!(!trace.segment_due(second - 1) && trace.segment_due(second));
        trace.end_segment(second).expect("a segment");

        // What the trace learns as it runs is recorded in the files after.
        trace.setting("budget 1", "cut at 16");

        // Three seconds on, the window holds only the last two.
        trace.event(&fast(3 * second)).expect("added");
        let saved = trace
            .event(&call("fsync", 3 * second, second, Outcome::Returned(0)))
            .expect("written")
            .expect("a window");
        assert_eq!(
            (saved.calls, saved.reason.as_str()),
            (2, "fsync took 1000000000 ns")
        );

        // Another trigger within the window's span is let go.
        let failed = call("read", 3 * second + 5, 1, Outcome::Failed(Errno(5)));
        assert_eq!(trace.event(&failed).expect("added"), None);
        trace.end_segment(4 * second).expect("a segment");

        let segments = Profile::read(&directory).expect("the segments").profile;
        let counts = segments.syscalls().stats().per_call();
        let named = |name| counts[&SyscallId::named(Abi::X86_64, name).expect(name)].calls;
        assert_eq!(
            (named("getppid"), named("fsync"), named("read")),
            (16, 1, 1)
        );
        assert_eq!(segments.setting("engine"), Some("bpf"));
        let window = Profile::read(&saved.path).expect("the window").profile;
        assert_eq!(window.setting("trigger"), Some("fsync took 1000000000 ns"));
        assert_eq!(window.setting("budget 1"), Some("cut at 16"));
        assert_eq!(window.syscalls().events.len(), 2);
        std::fs::remove_dir_all(&directory).expect("clean up");
    }
}
