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

/// The statistics, and what the oracle compares them with.
#[derive(Default)]
pub struct TraceModel {
    served: u64,
    stats: BTreeMap<SyscallId, BTreeMap<u32, RawStatValue>>,

    /// Failures by slot and errno.
    errnos: BTreeMap<(u32, u32), u64>,

    /// Each call's calls and errors, by errno, as served.
    expected: BTreeMap<SyscallId, (u64, u64, BTreeMap<u16, u64>)>,
}

impl TraceModel {
    /// Writes, reads back, and counts each call served.
    pub fn feed(&mut self, served: Vec<Served>, marks: &mut Marks) -> Result<(), String> {
        for call in served {
            self.count(&call);
            self.served += 1;
            decode(&call, &record(&call, self.served))?;
            marks.hit(Mark::TraceDecoded);
        }

        Ok(())
    }

    /// Counts a call as the count program does, on a CPU of its thread's.
    fn count(&mut self, call: &Served) {
        let id = SyscallId::new(Abi::X86_64, call.number);
        let stat = self
            .stats
            .entry(id)
            .or_default()
            .entry(call.tid.cast_unsigned() % CPUS)
            .or_insert(RawStatValue {
                calls: 0,
                errors: 0,
                timed: 0,
                total_ns: 0,
                min_ns: 0,
                max_ns: 0,
                bytes: 0,
                log2_ns: [0; BUCKETS],
            });
        stat.calls += 1;

        let expected = self.expected.entry(id).or_default();
        expected.0 += 1;
        if let Some(errno) = call.returned.and_then(errno_of) {
            stat.errors += 1;
            expected.1 += 1;
            *expected.2.entry(errno).or_default() += 1;
            *self
                .errnos
                .entry((bpf::errno_slot(id), u32::from(errno)))
                .or_default() += 1;
        }
    }

    /// Judges the run's statistics: the folded statistics count each call
    /// served.
    pub fn finish(&self) -> Result<(), String> {
        let folded = bpf::fold(
            Pid::new(0),
            self.stats
                .iter()
                .map(|(&call, cpus)| (call, cpus.values().copied().collect())),
            self.errnos
                .iter()
                .map(|(&(slot, err), &count)| (RawErrnoKey { slot, err }, count)),
        );

        for (call, (calls, errors, errnos)) in &self.expected {
            let stats = folded.by_call.get(&(Pid::new(0), *call));
            let counted = stats.map(|stats| (stats.calls, stats.errors, &stats.errors_by_errno));
            if counted != Some((*calls, *errors, errnos)) {
                return Err(format!(
                    "{} was served {calls} times, {errors} failing as {errnos:?}, but the \
                     statistics count {counted:?}",
                    call.display_name()
                ));
            }
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
