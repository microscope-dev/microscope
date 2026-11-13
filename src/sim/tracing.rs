//! A model of the BPF engine's tracepoint stream. Each call the simulated
//! kernel served is written as the stream program writes it and counted
//! into per-CPU statistics as the count program counts it. The real
//! decoder must read each record back as the call it was made of, and the
//! real folder must count every call served.
//!
//! The ring the records pass through, and the counter of those a full one
//! loses, are the kernel's; the engine reads them through BPF maps the
//! simulation does not reach, so they are not modeled.

use std::collections::BTreeMap;

use super::kernel::syscalls::Served;
use super::marks::{Mark, Marks};
use crate::backend::syscall_engines::bpf::{self, RawErrnoKey, RawStatValue, Record};
use crate::syscalls::stats::BUCKETS;
use crate::syscalls::{Abi, Outcome, Pid, SyscallId, Tid};

/// The CPUs calls are counted on.
const CPUS: u32 = 4;

/// The threads' statistics the kernel has room for: few, so that runs
/// spill into the per-CPU statistics.
const THREAD_ROWS: usize = 6;

/// The statistics, and what the oracle compares them with.
#[derive(Default)]
pub struct TraceModel {
    served: u64,

    /// Each thread's, by tgid, tid, and call, and those of threads with no
    /// room, by call and CPU.
    threads: BTreeMap<(u32, u32, SyscallId), RawStatValue>,
    spilled: BTreeMap<(SyscallId, u32), RawStatValue>,

    /// Failures by tgid, tid, slot, and errno.
    errnos: BTreeMap<(u32, u32, u32, u32), u64>,

    /// Each call's calls and errors, by errno, as served.
    expected: BTreeMap<SyscallId, (u64, u64, BTreeMap<u16, u64>)>,

    /// Calls served by each thread that had room, and by every other
    /// thread as thread 0 of process 0.
    by_thread: BTreeMap<(Pid, Tid, SyscallId), u64>,
}

const BLANK: RawStatValue = RawStatValue {
    calls: 0,
    errors: 0,
    timed: 0,
    total_ns: 0,
    min_ns: 0,
    max_ns: 0,
    bytes: 0,
    log2_ns: [0; BUCKETS],
};

impl TraceModel {
    /// Writes, reads back, and counts each call served.
    pub fn feed(&mut self, served: Vec<Served>, marks: &mut Marks) -> Result<(), String> {
        for call in served {
            if self.count(&call) {
                marks.hit(Mark::TraceStatsSpilled);
            }
            self.served += 1;
            decode(&call, &record(&call, self.served))?;
            marks.hit(Mark::TraceDecoded);
        }

        Ok(())
    }

    /// Counts a call as the count program does: into its thread's
    /// statistics, or once there is no room, a CPU's of its thread's;
    /// returns whether it spilled.
    fn count(&mut self, call: &Served) -> bool {
        let id = SyscallId::new(Abi::X86_64, call.number);
        let thread = (call.tgid.id().cast_unsigned(), call.tid.cast_unsigned());
        let key = (thread.0, thread.1, id);
        let (stat, (process, thread)) =
            if self.threads.contains_key(&key) || self.threads.len() < THREAD_ROWS {
                (self.threads.entry(key).or_insert(BLANK), thread)
            } else {
                let cpu = thread.1 % CPUS;
                (self.spilled.entry((id, cpu)).or_insert(BLANK), (0, 0))
            };
        stat.calls += 1;
        *self
            .by_thread
            .entry((Pid::new(process), Tid::new(thread), id))
            .or_default() += 1;

        let expected = self.expected.entry(id).or_default();
        expected.0 += 1;
        if let Some(errno) = call.returned.and_then(errno_of) {
            stat.errors += 1;
            expected.1 += 1;
            *expected.2.entry(errno).or_default() += 1;
            *self
                .errnos
                .entry((process, thread, bpf::errno_slot(id), u32::from(errno)))
                .or_default() += 1;
        }

        process == 0
    }

    /// Judges the run's statistics: the folded statistics count each call
    /// served, each under the thread that made it while there was room.
    pub fn finish(&self) -> Result<(), String> {
        let threads = self
            .threads
            .iter()
            .map(|(&(tgid, tid, call), &value)| (tgid, tid, call, value));
        let spilled = self
            .spilled
            .iter()
            .map(|(&(call, _), &value)| (0, 0, call, value));
        let folded = bpf::fold(
            Pid::new(0),
            threads.chain(spilled),
            self.errnos.iter().map(|(&(tgid, tid, slot, err), &count)| {
                (
                    RawErrnoKey {
                        tid,
                        tgid,
                        slot,
                        err,
                    },
                    count,
                )
            }),
        );

        let per_call = folded.per_call();
        for (call, (calls, errors, errnos)) in &self.expected {
            let stats = per_call.get(call);
            let counted = stats.map(|stats| (stats.calls, stats.errors, &stats.errors_by_errno));
            if counted != Some((*calls, *errors, errnos)) {
                return Err(format!(
                    "{} was served {calls} times, {errors} failing as {errnos:?}, but the \
                     statistics count {counted:?}",
                    call.display_name()
                ));
            }
        }

        let by_thread: BTreeMap<_, _> = folded
            .by_call
            .iter()
            .map(|(&key, stats)| (key, stats.calls))
            .collect();
        if by_thread != self.by_thread {
            return Err(format!(
                "the threads served {:?}, but the statistics count {by_thread:?}",
                self.by_thread
            ));
        }

        Ok(())
    }
}

/// Reads `bytes` with the real decoder, which must give back `call`, the
/// call they were made of.
fn decode(call: &Served, bytes: &[u8]) -> Result<(), String> {
    let event = match bpf::decode(bytes) {
        Ok(Record::Call(event)) => event,
        other => return Err(format!("the record of {call:?} decoded as {other:?}")),
    };

    let outcome = call
        .returned
        .map_or(Outcome::ProcessExited, Outcome::of_return);
    let read = (
        event.thread,
        event.process,
        event.call,
        event.args,
        &event.outcome,
    );
    let made = (
        Tid::new(call.tid.cast_unsigned()),
        Pid::new(call.tgid.id().cast_unsigned()),
        SyscallId::new(Abi::X86_64, call.number),
        call.args,
        &outcome,
    );
    if read != made {
        return Err(format!(
            "the trace shows {read:?}, but the kernel served {made:?}"
        ));
    }

    Ok(())
}

/// A failed call's errno.
fn errno_of(returned: i64) -> Option<u16> {
    (-4095..0)
        .contains(&returned)
        .then(|| u16::try_from(-returned).ok())
        .flatten()
}

/// The stream program's record of `call`, the `sequence`th served, whose
/// times the sequence stands for.
fn record(call: &Served, sequence: u64) -> Vec<u8> {
    bpf::encode_call(
        call.tid.cast_unsigned(),
        call.tgid.id().cast_unsigned(),
        SyscallId::new(Abi::X86_64, call.number),
        call.args,
        call.returned,
        (2 * sequence, 2 * sequence + 1),
    )
}
