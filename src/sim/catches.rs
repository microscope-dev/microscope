use std::collections::BTreeMap;

use super::cpu::{R8, R9, R10, RAX, RDI, RDX, RSI};
use super::kernel::catching::UserCatch;
use super::kernel::{Kernel, State, StopKind, Tgid, Tid};
use super::marks::Mark;
use super::oracles::ending;
use crate::StopReason;
use crate::backend::sim_edge::Truth;
use crate::syscalls::{Abi, CaptureState, SyscallId};

pub fn unseen(kernel: &Kernel) -> Result<(), String> {
    let end = |exit| if exit { "exit" } else { "entry" };

    if let Some(unseen) = kernel.catching.unseen.first() {
        return Err(format!(
            "thread {} made system call {} with a catchpoint stopping its {}, but no \
             system-call-stop reported it",
            unseen.tid,
            unseen.call,
            end(unseen.exit)
        ));
    }

    kernel.catching.skipped.first().map_or(Ok(()), |skipped| {
        Err(format!(
            "thread {} stopped at the {} of system call {}, which a catchpoint stops, and \
             was resumed with no stop reporting it",
            skipped.tid,
            end(skipped.exit),
            skipped.call
        ))
    })
}

pub type Known = BTreeMap<u64, UserCatch>;

pub type Reasons = BTreeMap<Tid, (StopReason, Known)>;

pub fn known(reasons: &mut Reasons, truth: &Truth, current: &Known) -> BTreeMap<Tid, Known> {
    reasons.retain(|tid, (reason, _)| truth.reasons.get(tid) == Some(reason));

    let mut known = BTreeMap::new();

    for (&tid, reason) in &truth.reasons {
        if !matches!(reason, StopReason::Syscall(_)) {
            continue;
        }

        let (_, then) = reasons
            .entry(tid)
            .or_insert_with(|| (reason.clone(), current.clone()));
        let then = then
            .iter()
            .filter(|(id, _)| current.contains_key(id))
            .map(|(&id, rule)| (id, rule.clone()))
            .collect();
        known.insert(tid, then);
    }

    known
}

pub fn judge(
    kernel: &Kernel,
    truth: &Truth,
    known: &BTreeMap<Tid, Known>,
) -> Result<Vec<Mark>, String> {
    let (Some(_), Some(tgid)) = (truth.public_stop, truth.inferior) else {
        return Ok(Vec::new());
    };

    if ending(kernel, tgid) {
        return Ok(Vec::new());
    }

    let mut marks = Vec::new();

    for (&tid, reason) in &truth.reasons {
        let StopReason::Syscall(stop) = reason else {
            continue;
        };

        let empty = Known::new();
        let catches = known.get(&tid).unwrap_or(&empty);
        let Some(thread) = kernel.threads.get(&tid) else {
            return Err(format!("thread {tid} reports a call, but it is gone"));
        };

        let State::Stopped {
            kind: StopKind::Syscall { exit },
            ..
        } = thread.state
        else {
            return Err(format!(
                "thread {tid} reports {} at a call, but the kernel holds it {:?}",
                stop.call.display_name(),
                thread.state
            ));
        };

        if exit != stop.returned.is_some() {
            return Err(format!(
                "thread {tid} reports a call's {}, but it is at its {}",
                if stop.returned.is_some() {
                    "exit"
                } else {
                    "entry"
                },
                if exit { "exit" } else { "entry" }
            ));
        }

        let general = &thread.registers.general;
        let call = u32::try_from(thread.orig_rax)
            .ok()
            .map(|number| SyscallId::new(Abi::X86_64, number));
        let args = [
            general[RDI],
            general[RSI],
            general[RDX],
            general[R10],
            general[R8],
            general[R9],
        ];
        if call != Some(stop.call) || args != stop.args {
            return Err(format!(
                "thread {tid} reports {}{:x?}, but made call {} with {args:x?}",
                stop.call.display_name(),
                stop.args,
                thread.orig_rax
            ));
        }

        let returned = general[RAX].cast_signed();
        if exit && stop.returned != Some(returned) {
            return Err(format!(
                "thread {tid} reports {} returned {:?}, but it returned {returned}",
                stop.call.display_name(),
                stop.returned
            ));
        }

        judge_memory(kernel, tgid, tid, stop)?;
        judge_listed(catches, thread, tid, stop, exit)?;
        marks.push(if exit {
            Mark::CatchExitStop
        } else {
            Mark::CatchEntryStop
        });
    }

    if marks.len() > 1 {
        marks.push(Mark::CatchCoHit);
    }

    Ok(marks)
}

fn judge_memory(
    kernel: &Kernel,
    process: Tgid,
    tid: Tid,
    stop: &crate::SyscallStop,
) -> Result<(), String> {
    let space = &kernel.processes[&process].space;

    for capture in stop.captured.iter() {
        if !matches!(capture.state, CaptureState::Complete) {
            continue;
        }

        let address = stop.args[usize::from(capture.arg)];
        let bytes = space.read_user(address, capture.bytes.len() as u64).ok();
        if bytes.as_deref() != Some(&capture.bytes[..]) {
            return Err(format!(
                "thread {tid} reports {:?} behind argument {} of {}, but memory holds \
                 {bytes:?}",
                capture.bytes,
                capture.arg,
                stop.call.display_name()
            ));
        }
    }

    Ok(())
}

fn judge_listed(
    catches: &Known,
    thread: &super::kernel::Thread,
    tid: Tid,
    stop: &crate::SyscallStop,
    exit: bool,
) -> Result<(), String> {
    let stopping = catches
        .iter()
        .filter(|(_, rule)| rule.stops(thread.orig_rax, &thread.registers, exit))
        .map(|(&id, _)| id);

    for id in stopping {
        if !stop.catchpoints.iter().any(|listed| listed.get() == id) {
            return Err(format!(
                "thread {tid}'s stop at {} lists catchpoints {:?}, but catchpoint {id} \
                 stops it too",
                stop.call.display_name(),
                stop.catchpoints
            ));
        }
    }

    for listed in stop.catchpoints.iter() {
        let stops = catches
            .get(&listed.get())
            .is_none_or(|rule| rule.stops(thread.orig_rax, &thread.registers, exit));
        if !stops {
            return Err(format!(
                "thread {tid}'s stop at {} lists catchpoint {listed}, which does not \
                 stop it",
                stop.call.display_name()
            ));
        }
    }

    Ok(())
}
