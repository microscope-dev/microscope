//! Minimal Linux `x86_64` syscall table for golden test programs.
//!
//! Every served call is recorded in `Kernel::served` so `tracing::TraceModel`
//! can cross-check BPF syscall event encoding and statistics folding.

use nix::errno::Errno;
use nix::libc;

use super::{CallPhase, ENOSYS_RESULT, ExitStatus, Happening, Kernel, Tgid, Thread, Tid};
use crate::sim::cpu::{R8, R9, R10, RAX, RDI, RDX, RSI, RSP};
use crate::sim::memory::MemoryError;

const SYS_WRITE: u64 = 1;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_GETPID: u64 = 39;
const SYS_CLONE: u64 = 56;

pub(super) const SYS_FORK: u64 = 57;

pub(super) const SYS_EXECVE: u64 = 59;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_GETPPID: u64 = 110;
const SYS_EXIT_GROUP: u64 = 231;

const THREAD_FLAGS: u64 = 0x50f00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Served {
    pub tid: Tid,
    pub tgid: Tgid,
    pub number: u32,
    pub args: [u64; 6],

    pub returned: Option<i64>,
}

pub(super) fn serve(kernel: &mut Kernel, tid: Tid) -> bool {
    let thread = &kernel.threads[&tid];
    let group = thread.tgid;
    let number = u32::try_from(thread.orig_rax).unwrap_or(u32::MAX);
    let general = &thread.registers.general;
    let args = [
        general[RDI],
        general[RSI],
        general[RDX],
        general[R10],
        general[R8],
        general[R9],
    ];

    let yielded = dispatch(kernel, tid);
    let returned = kernel
        .threads
        .get(&tid)
        .filter(|thread| !thread.state.exiting())
        .map(|thread| {
            match thread.call {
                CallPhase::Returning(result) => result,
                _ => thread.registers.general[RAX],
            }
            .cast_signed()
        });

    kernel.served.push(Served {
        tid,
        tgid: group,
        number,
        args,
        returned,
    });

    yielded
}

fn dispatch(kernel: &mut Kernel, tid: Tid) -> bool {
    let registers = kernel.threads[&tid].registers;
    let argument = |index: usize| registers.general[index];
    #[expect(
        clippy::cast_possible_truncation,
        reason = "exit statuses are ints, the low half of the register"
    )]
    let code = argument(RDI) as i32;

    let result = match kernel.threads[&tid].orig_rax {
        SYS_WRITE => write(kernel, tid, argument(RDI), argument(RSI), argument(RDX)),
        SYS_SCHED_YIELD => {
            set_result(kernel, tid, 0);
            return true;
        }
        SYS_CLONE => {
            clone(kernel, tid, argument(RDI), argument(RSI));
            return false;
        }
        SYS_FORK => {
            kernel.fork(tid);
            return false;
        }
        SYS_WAIT4 if argument(R10) == 0 => {
            kernel.wait4(tid, argument(RDI), argument(RSI), argument(RDX))
        }
        SYS_GETPID => {
            u64::try_from(kernel.threads[&tid].tgid.id()).expect("process ids are positive")
        }
        SYS_GETPPID => kernel.parent_of(tid),
        SYS_EXIT => {
            inside_call(kernel, tid);
            let group = kernel.threads[&tid].tgid;

            let last = kernel
                .threads_of(group)
                .all(|thread| thread.tid == tid || thread.state.exiting());
            if last {
                kernel
                    .processes
                    .get_mut(&group)
                    .expect("a thread's process exists")
                    .group_exit
                    .get_or_insert(ExitStatus::Code(code));
            }

            if tid == group.leader() {
                kernel.leader_exits.insert(tid, code);
            }

            kernel.reach_exit(tid, ExitStatus::Code(code));
            return false;
        }
        SYS_EXIT_GROUP => {
            inside_call(kernel, tid);
            let group = kernel.threads[&tid].tgid;
            kernel.happenings.push(Happening::GroupExit { tid });
            kernel.kill_process(group, ExitStatus::Code(code));

            let exit = kernel.processes[&group]
                .group_exit
                .expect("the group is exiting");
            kernel.reach_exit(tid, exit);
            return false;
        }
        number => {
            kernel.gap(format!("system call {number}"));
            return false;
        }
    };

    set_result(kernel, tid, result);
    false
}

fn set_result(kernel: &mut Kernel, tid: Tid, result: u64) {
    kernel
        .threads
        .get_mut(&tid)
        .expect("calling thread exists")
        .registers
        .general[RAX] = result;
}

fn inside_call(kernel: &mut Kernel, tid: Tid) {
    set_result(kernel, tid, ENOSYS_RESULT);
}

pub(super) const fn failure(errno: Errno) -> u64 {
    (-(errno as i64)).cast_unsigned()
}

fn write(kernel: &mut Kernel, tid: Tid, fd: u64, buffer: u64, count: u64) -> u64 {
    if fd != 1 && fd != 2 {
        kernel.gap(format!("write to descriptor {}", fd.cast_signed()));
        return ENOSYS_RESULT;
    }

    let group = kernel.threads[&tid].tgid;
    let process = &kernel.processes[&group];
    let bytes = match process.space.read_user(buffer, count) {
        Ok(bytes) => bytes,
        Err(MemoryError::Fault(_)) => return failure(Errno::EFAULT),
        Err(error) => {
            kernel.gap(format!("write from {error}"));
            return failure(Errno::EFAULT);
        }
    };

    let root = process.root;
    kernel
        .outputs
        .get_mut(&root)
        .expect("a launched process has an output")
        .extend_from_slice(&bytes);

    count
}

fn clone(kernel: &mut Kernel, parent: Tid, flags: u64, stack: u64) {
    if flags != THREAD_FLAGS {
        kernel.gap(format!("clone with flags {flags:#x}"));
        return;
    }

    if kernel.signal_pending(parent) {
        kernel.fail_to_restart(parent);
        return;
    }

    let creator = &kernel.threads[&parent];
    let traced = creator.traced() && creator.options.trace_clone;
    let child = kernel.allocate_tid();

    let shadow = kernel.new_shadow();
    let creator = &kernel.threads[&parent];
    let mut thread = Thread::created(creator, child, creator.tgid, SYS_CLONE, traced, shadow);
    if stack != 0 {
        thread.registers.general[RSP] = stack;
    }

    kernel.threads.insert(child, thread);
    kernel.happenings.push(Happening::Cloned { parent, child });

    let result = u64::try_from(child).expect("thread ids are positive");
    if traced {
        kernel.event_stop(parent, libc::PTRACE_EVENT_CLONE, result, result);
    } else {
        set_result(kernel, parent, result);
    }
}
