//! A system call trace recorded from the page, by the ptrace engine, which
//! publishes what it has seen every few seconds, with the calls its threads
//! are blocked in.

use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use microscope::syscall_engines::capture::SyscallCapture;
use microscope::syscall_engines::stacks::SharedStacks;
use microscope::syscall_engines::{Finished, Sink, Target, TraceOptions, monotonic, ptrace};
use microscope::syscalls::captures::Limits;
use microscope::syscalls::fds::{self, FdTable};
use microscope::syscalls::filter::Filter;
use microscope::syscalls::{InFlight, TraceEvent};
use tokio::sync::oneshot;

use super::profiles::{LIVE_EVERY, Profiles};
use super::protocol::{ErrorKind, ProfileInfo, RecordSyscalls};
use super::session::Failure;
use crate::present::syscalls as present;

/// How long a call must have been running to be shown in flight.
const IN_FLIGHT_AFTER: u64 = 1_000_000;

/// What the trace records, from what the page asked for.
pub fn options(spec: &RecordSyscalls) -> Result<TraceOptions, Failure> {
    let expressions: Vec<String> = spec.filter.split_whitespace().map(str::to_owned).collect();
    let filter = Filter::parse(&expressions, &present::signal_named)
        .map_err(|error| Failure::new(ErrorKind::Invalid, error.to_string()))?;

    // The descriptor table must see every call that changes it, for the
    // paths the view shows to be true.
    let mut calls = filter.calls;
    for id in fds::followed() {
        calls.insert(id);
    }

    Ok(TraceOptions {
        calls,
        limits: Limits::default(),
        verbose: false,
        follow: true,
        stacks: spec.stacks.then(SharedStacks::default),
    })
}

/// Where one recording publishes.
pub struct Publisher<'a> {
    pub profiles: &'a Profiles,
    pub id: &'a str,
    pub name: &'a str,
    pub started: Option<oneshot::Sender<Result<(), String>>>,
}

impl Publisher<'_> {
    fn info(&self, revision: u32, ended: Option<String>) -> ProfileInfo {
        ProfileInfo {
            id: self.id.to_owned(),
            name: self.name.to_owned(),
            recording: ended.is_none(),
            revision,
            ended,
        }
    }
}

/// Traces `command` on this thread until it ends or `stop` is set.
pub fn record(
    mut publisher: Publisher<'_>,
    command: Command,
    program: &str,
    options: &TraceOptions,
    stop: &AtomicBool,
) {
    let mut capture = SyscallCapture::new(options.stacks.clone());
    capture.setting("program", program);
    capture.setting("syscalls", "ptrace");
    let mut live = Live {
        publisher: &mut publisher,
        capture,
        fds: FdTable::default(),
        in_flight: Vec::new(),
        seen_at: 0,
        revision: 0,
        published: None,
    };

    let traced = ptrace::trace(Target::Launch(command), options, stop, &mut live);
    let Live {
        capture, revision, ..
    } = live;

    match traced {
        Ok(Finished { end }) => {
            let ended = end.map_or_else(|| "ended".to_owned(), super::syscalls::end_text);
            let info = publisher.info(revision, Some(ended));
            publisher
                .profiles
                .publish(info, capture.finish(), (Vec::new(), 0));

            // Published before the page hears it began, so it can be read.
            if let Some(started) = publisher.started.take() {
                let _ = started.send(Ok(()));
            }
        }
        Err(error) => {
            if let Some(started) = publisher.started.take() {
                let _ = started.send(Err(error.to_string()));
            } else {
                let info = publisher.info(revision, Some(format!("failed: {error}")));
                publisher
                    .profiles
                    .publish(info, capture.finish(), (Vec::new(), 0));
            }
        }
    }
}

/// The trace's sink: the capture so far, and what is in flight.
struct Live<'a, 'b> {
    publisher: &'a mut Publisher<'b>,
    capture: SyscallCapture,

    /// Descriptors as the trace knows them, to seed a program's own.
    fds: FdTable,
    in_flight: Vec<InFlight>,

    /// When the in-flight calls were read.
    seen_at: u64,
    revision: u32,
    published: Option<Instant>,
}

impl Live<'_, '_> {
    /// Publishes the trace so far, at most every [`LIVE_EVERY`].
    fn publish(&mut self) {
        if self
            .published
            .is_some_and(|published| published.elapsed() < LIVE_EVERY)
        {
            return;
        }

        self.published = Some(Instant::now());
        let info = self.publisher.info(self.revision, None);
        self.publisher.profiles.publish(
            info,
            self.capture.snapshot(),
            (self.in_flight.clone(), self.seen_at),
        );
        self.revision += 1;

        if let Some(started) = self.publisher.started.take() {
            let _ = started.send(Ok(()));
        }
    }
}

/// Collects the descriptors a process starts with.
struct Seeds(Vec<TraceEvent>);

impl Sink for Seeds {
    fn event(&mut self, event: TraceEvent) {
        self.0.push(event);
    }
}

impl Sink for Live<'_, '_> {
    fn event(&mut self, event: TraceEvent) {
        self.capture.event(&event);
        self.fds.apply(&event);

        // A program's own descriptors, which it inherited rather than made.
        if let TraceEvent::Exec { process, .. } = event
            && self.fds.of(process).next().is_none()
        {
            let mut seeds = Seeds(Vec::new());
            ptrace::seed_descriptors(process.get(), &mut seeds);
            for seed in seeds.0 {
                self.capture.event(&seed);
                self.fds.apply(&seed);
            }
        }

        self.publish();
    }

    fn in_flight(&mut self, calls: &[InFlight]) {
        calls.clone_into(&mut self.in_flight);
        self.seen_at = monotonic();
        self.publish();
    }

    fn in_flight_after(&self) -> Option<u64> {
        Some(IN_FLIGHT_AFTER)
    }
}
