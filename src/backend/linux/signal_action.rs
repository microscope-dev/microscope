use std::time::Duration;

use nix::unistd::Pid;

use super::{InspectionOps, LinuxPtrace, LinuxTraceOps};
use crate::backend::linux::{Signal, WaitEvent};
use crate::{Error, Result};

pub(super) fn open_inspector(
    edge: &LinuxPtrace,
    pid: Pid,
    native_handler: u64,
    interrupted: &mut Option<crate::backend::linux::PendingSignal>,
) -> Result<bool> {
    let status = crate::backend::linux::proc_status::read_path(format!("/proc/{pid}/status"))?;
    if !unfiltered(&status) {
        return Err(Error::internal(
            "refusing rt_sigaction injection into a task with seccomp or unknown filter state",
        ));
    }
    if !usr1_unblocked(&status) {
        return Err(Error::internal("the stopped Node thread blocks SIGUSR1"));
    }
    let hub = edge
        .hub
        .as_ref()
        .ok_or_else(|| Error::internal("a signal disposition query needs the shared wait hub"))?;
    let wait = hub
        .edge()
        .intercept(pid)
        .ok_or_else(|| Error::internal("the thread already has a direct waiter"))?;
    if query_stopped(edge, pid, Signal::SIGUSR1, interrupted, &wait)? != native_handler {
        return Ok(false);
    }
    deliver_at_stop(edge, pid, native_handler, interrupted, &wait)?;
    Ok(true)
}

fn unfiltered(status: &[u8]) -> bool {
    crate::backend::linux::proc_status::field(status, b"Seccomp:").map(<[u8]>::trim_ascii)
        == Some(b"0")
}

fn usr1_unblocked(status: &[u8]) -> bool {
    crate::backend::linux::proc_status::field(status, b"SigBlk:")
        .and_then(|value| std::str::from_utf8(value.trim_ascii()).ok())
        .and_then(|value| u64::from_str_radix(value, 16).ok())
        .is_some_and(|mask| mask & (1 << (nix::libc::SIGUSR1 - 1)) == 0)
}

fn wait_stopped(
    edge: &LinuxPtrace,
    pid: Pid,
    wait: &crate::backend::linux::wait_hub::DirectWait,
) -> (WaitEvent, bool) {
    let mut timed_out = false;
    loop {
        match wait.receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(WaitEvent::Continued(_)) => {}
            Ok(status) => return (status, timed_out),
            Err(_) => {
                timed_out = true;
                let _ = edge.interrupt(pid);
            }
        }
    }
}

fn deliver_at_stop(
    edge: &LinuxPtrace,
    pid: Pid,
    native_handler: u64,
    interrupted: &mut Option<crate::backend::linux::PendingSignal>,
    wait: &crate::backend::linux::wait_hub::DirectWait,
) -> Result<()> {
    // The query leaves a real signal-delivery stop. Substitution here selects
    // the proven disposition before any sibling can replace it. A queued kill
    // followed by resuming all threads would reopen that race.
    edge.step(pid, Some(Signal::SIGUSR1))?;
    let (status, _) = wait_stopped(edge, pid, wait);
    if let WaitEvent::Stopped(_, signal) = status {
        let (registers, metadata) = evidence(edge, pid)?;
        let signal_entry =
            metadata.code == crate::backend::linux::TRAP_UNKNOWN && registers.rip == native_handler;
        if signal == Signal::SIGTRAP && signal_entry {
            return Ok(());
        }
        *interrupted = Some(crate::backend::linux::PendingSignal {
            signal,
            code: metadata.code,
            sender: metadata.sender,
            fault_address: metadata.fault_address,
        });
        return Err(Error::internal(format!(
            "native inspector activation was interrupted: {signal:?}, code {}, PC {:#x}, handler {native_handler:#x}",
            metadata.code, registers.rip,
        )));
    }
    if matches!(status, WaitEvent::PtraceEvent(_, _, event) if event == nix::libc::PTRACE_EVENT_STOP)
    {
        return Err(Error::internal(
            "native inspector activation did not reach its handler",
        ));
    }
    wait.forward(status);
    Err(Error::internal(
        "Node exited during native inspector activation",
    ))
}

fn has_exited(pid: Pid) -> bool {
    let status = crate::backend::linux::proc_status::read_path(format!("/proc/{pid}/stat"));
    status
        .as_ref()
        .is_ok_and(|status| crate::backend::linux::proc_status::stat_exited(status))
        || status
            .as_ref()
            .is_err_and(crate::backend::linux::proc_status::gone)
}

fn evidence(
    edge: &LinuxPtrace,
    pid: Pid,
) -> Result<(
    nix::libc::user_regs_struct,
    crate::backend::linux::SignalMetadata,
)> {
    loop {
        if let Ok(registers) = edge.registers(pid)
            && let Ok(metadata) = edge.signal_metadata(pid)
        {
            return Ok((registers, metadata));
        }
        if has_exited(pid) {
            return Err(Error::internal(
                "the task exited before its signal query stop could be read",
            ));
        }
        record!("holding {pid} until its signal query stop evidence can be preserved");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn query_stopped(
    edge: &LinuxPtrace,
    pid: Pid,
    signal: Signal,
    interrupted: &mut Option<crate::backend::linux::PendingSignal>,
    wait: &crate::backend::linux::wait_hub::DirectWait,
) -> Result<u64> {
    let saved = edge.registers(pid)?;
    let code = edge.read_word(pid, saved.rip)?;
    let scratch = saved
        .rsp
        .checked_sub(256)
        .ok_or_else(|| Error::internal("no scratch space below the thread's red zone"))?;
    let mut words = [0; 4];
    for (index, word) in words.iter_mut().enumerate() {
        *word = edge.read_word(pid, scratch + index as u64 * 8)?;
    }
    let mut query = saved;
    query.rax = 13;
    query.orig_rax = u64::MAX;
    query.rdi = u64::try_from(signal.number())
        .map_err(|_| Error::internal("a signal number is negative"))?;
    query.rsi = 0;
    query.rdx = scratch;
    query.r10 = 8;
    let mut exited = false;
    let outcome = (|| {
        edge.write_word(pid, saved.rip, (code & !0xffff) | 0x050f)?;
        edge.set_registers(pid, query)?;
        edge.step(pid, None)?;
        let (status, timed_out) = wait_stopped(edge, pid, wait);
        exited = matches!(status, WaitEvent::Exited(..) | WaitEvent::Signaled(..));
        if status != WaitEvent::Stopped(pid, Signal::SIGTRAP) {
            match status {
                WaitEvent::Stopped(_, signal) => {
                    let (_, metadata) = evidence(edge, pid)?;
                    if signal.number() != nix::libc::SIGSYS || metadata.code != 1 {
                        *interrupted = Some(crate::backend::linux::PendingSignal {
                            signal,
                            code: metadata.code,
                            sender: metadata.sender,
                            fault_address: metadata.fault_address,
                        });
                    }
                }
                WaitEvent::PtraceEvent(_, _, event) if event == nix::libc::PTRACE_EVENT_STOP => {}
                _ => wait.forward(status),
            }
            return Err(Error::internal("the signal query was interrupted"));
        }
        let (registers, metadata) = evidence(edge, pid)?;
        record!("signal query trap code {}", metadata.code);
        if !matches!(metadata.code, 1 | 2) || registers.rip != saved.rip.wrapping_add(2) {
            *interrupted = Some(crate::backend::linux::PendingSignal {
                signal: Signal::SIGTRAP,
                code: metadata.code,
                sender: metadata.sender,
                fault_address: metadata.fault_address,
            });
            return Err(Error::internal(format!(
                "the signal query stopped for another trap (code {}, PC {:#x}, expected {:#x})",
                metadata.code,
                registers.rip,
                saved.rip.wrapping_add(2)
            )));
        }
        if timed_out || registers.rax != 0 {
            return Err(Error::internal(format!(
                "rt_sigaction did not complete successfully (result {:#x})",
                registers.rax
            )));
        }
        edge.read_word(pid, scratch)
    })();
    if exited {
        return outcome;
    }
    loop {
        // GETREGS requires a ptrace stop. In particular, a failed resume
        // or a timeout is not permission to write into a running task.
        let restored = edge.registers(pid).and_then(|_| {
            for (index, word) in words.into_iter().enumerate() {
                edge.write_word(pid, scratch + index as u64 * 8, word)?;
            }
            edge.write_word(pid, saved.rip, code)?;
            edge.set_registers(pid, saved)
        });
        if restored.is_ok() {
            break;
        }
        if has_exited(pid) {
            return Err(Error::internal(
                "the task exited before its signal query could be restored",
            ));
        }
        record!("holding {pid} until the injected signal query is fully restored: {restored:?}");
        let _ = edge.interrupt(pid);
        std::thread::sleep(Duration::from_millis(1));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let pid = Pid::from_raw(i32::try_from(self.0.id()).unwrap());
            let _ = nix::sys::ptrace::detach(pid, None);
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn registers(registers: nix::libc::user_regs_struct) -> [u64; 27] {
        [
            registers.r15,
            registers.r14,
            registers.r13,
            registers.r12,
            registers.rbp,
            registers.rbx,
            registers.r11,
            registers.r10,
            registers.r9,
            registers.r8,
            registers.rax,
            registers.rcx,
            registers.rdx,
            registers.rsi,
            registers.rdi,
            registers.orig_rax,
            registers.rip,
            registers.cs,
            registers.eflags,
            registers.rsp,
            registers.ss,
            registers.fs_base,
            registers.gs_base,
            registers.ds,
            registers.es,
            registers.fs,
            registers.gs,
        ]
    }

    #[test]
    fn seccomp_must_be_proven_disabled_before_any_injection() {
        assert!(unfiltered(b"Name:\t\xff\nSeccomp:\t0\n"));
        for status in [
            b"".as_slice(),
            b"Seccomp: 1\n",
            b"Seccomp: 2\n",
            b"Seccomp: unknown\n",
            b"Seccomp: 0\nSeccomp: 0\n",
        ] {
            assert!(!unfiltered(status));
        }
    }

    #[test]
    fn signal_substitution_selects_the_proven_handler_before_other_threads_resume() {
        substitution_evidence(true);
    }

    #[test]
    fn signal_substitution_refuses_a_wrong_handler_and_preserves_its_trap() {
        substitution_evidence(false);
    }

    fn substitution_evidence(correct: bool) {
        use std::io::BufRead;
        use std::process::Stdio;

        let mut child = Child(
            std::process::Command::new("sh")
                .args(["-c", "trap ':' USR1; echo READY; read line"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "READY\n");
        let pid = Pid::from_raw(i32::try_from(child.0.id()).unwrap());
        let edge = LinuxPtrace::new();
        assert!(edge.seize(pid, false).unwrap());
        edge.interrupt(pid).unwrap();
        assert!(matches!(
            edge.wait_status(pid).unwrap(),
            WaitEvent::PtraceEvent(..)
        ));
        let hub = crate::backend::linux::wait_hub::WaitHub::new().unwrap();
        let (sender, _receiver) = tokio::sync::mpsc::channel(8);
        let route = hub.edge(hub.register(sender));
        route.start(pid);
        let wait = route.intercept(pid).unwrap();
        let mut pending = None;
        let handler = query_stopped(&edge, pid, Signal::SIGUSR1, &mut pending, &wait).unwrap();
        assert!(handler > 1);
        let delivered = deliver_at_stop(
            &edge,
            pid,
            handler + u64::from(!correct),
            &mut pending,
            &wait,
        );
        let pc = edge.registers(pid).unwrap().rip;
        assert_eq!(
            pc, handler,
            "the kernel stops before any handler instruction"
        );
        assert_eq!(
            edge.signal_metadata(pid).unwrap().code,
            crate::backend::linux::TRAP_UNKNOWN
        );
        if correct {
            delivered.unwrap();
            assert!(pending.is_none(), "{pending:?}");
        } else {
            assert!(delivered.is_err());
            let pending = pending.expect("the unexpected trap is preserved");
            assert_eq!(pending.signal, Signal::SIGTRAP);
            assert_eq!(pending.code, crate::backend::linux::TRAP_UNKNOWN);
        }
        drop(wait);
        drop(hub);
        drop(child);
    }

    #[test]
    fn a_blocked_or_unreadable_usr1_mask_refuses_signal_substitution() {
        assert!(usr1_unblocked(b"Name:\t\xff\nSigBlk:\t0000000000000000\n"));
        for status in [
            b"".as_slice(),
            b"SigBlk: 200\n",
            b"SigBlk: unknown\n",
            b"SigBlk: \xff\n",
        ] {
            assert!(!usr1_unblocked(status));
        }
    }

    #[test]
    fn an_active_kill_filter_refuses_the_query_without_mutating_or_killing_the_task() {
        use std::io::{BufRead, Write};
        use std::process::Stdio;

        let mut child = Child(
            std::process::Command::new(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("build/test-programs/signal-query-filter"),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
        );
        let mut ready = String::new();
        std::io::BufReader::new(child.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "READY\n");
        let pid = Pid::from_raw(i32::try_from(child.0.id()).unwrap());
        let edge = LinuxPtrace::new();
        assert!(edge.seize(pid, false).unwrap());
        edge.interrupt(pid).unwrap();
        assert!(matches!(
            edge.wait_status(pid).unwrap(),
            WaitEvent::PtraceEvent(..)
        ));
        let before = edge.registers(pid).unwrap();
        let code = edge.read_word(pid, before.rip).unwrap();
        let mut pending = None;
        let error = open_inspector(&edge, pid, 0, &mut pending).unwrap_err();
        assert!(error.to_string().contains("seccomp"), "{error}");
        assert!(pending.is_none());
        assert_eq!(registers(edge.registers(pid).unwrap()), registers(before));
        assert_eq!(edge.read_word(pid, before.rip).unwrap(), code);
        assert!(edge.detach(pid, None).unwrap());
        child.0.stdin.as_mut().unwrap().write_all(b"x").unwrap();
        assert!(child.0.wait().unwrap().success());
    }

    #[test]
    fn native_signal_query_restores_code_scratch_and_every_register_after_success_or_signal() {
        for (interrupt, bad_scratch) in [
            (None, false),
            (Some(Signal::SIGUSR1), false),
            (Some(Signal::SIGTRAP), false),
            (None, true),
        ] {
            let child = Child(
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap(),
            );
            let pid = Pid::from_raw(i32::try_from(child.0.id()).unwrap());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let stat =
                    crate::backend::linux::proc_status::read_path(format!("/proc/{pid}/stat"))
                        .unwrap();
                if crate::backend::linux::proc_status::stat_state(&stat) == Some(b'S') {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the native fixture did not enter its sleep"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            let edge = LinuxPtrace::new();
            assert!(edge.seize(pid, false).unwrap());
            edge.interrupt(pid).unwrap();
            assert!(matches!(
                edge.wait_status(pid).unwrap(),
                WaitEvent::PtraceEvent(..)
            ));
            let hub = crate::backend::linux::wait_hub::WaitHub::new().unwrap();
            let (sender, _receiver) = tokio::sync::mpsc::channel(8);
            let route = hub.edge(hub.register(sender));
            route.start(pid);
            let wait = route.intercept(pid).unwrap();
            let original = edge.registers(pid).unwrap();
            if bad_scratch {
                let mut registers = original;
                registers.rsp = original.rip + 256;
                edge.set_registers(pid, registers).unwrap();
            }
            let before = edge.registers(pid).unwrap();
            let code = edge.read_word(pid, before.rip).unwrap();
            let scratch = (0..4)
                .map(|at| edge.read_word(pid, before.rsp - 256 + at * 8).unwrap())
                .collect::<Vec<_>>();
            if let Some(signal) = interrupt {
                edge.kill(pid, signal).unwrap();
            }
            let mut pending = None;
            let outcome = query_stopped(&edge, pid, Signal::SIGUSR1, &mut pending, &wait);
            assert_eq!(registers(edge.registers(pid).unwrap()), registers(before));
            assert_eq!(edge.read_word(pid, before.rip).unwrap(), code);
            for (at, word) in scratch.into_iter().enumerate() {
                assert_eq!(
                    edge.read_word(pid, before.rsp - 256 + at as u64 * 8)
                        .unwrap(),
                    word
                );
            }
            if let Some(signal) = interrupt {
                assert!(outcome.is_err());
                let pending = pending.expect("the external signal stays attached to its task");
                assert_eq!(pending.signal, signal);
                assert_eq!(pending.code, nix::libc::SI_USER);
                assert_eq!(
                    pending.sender,
                    Some(i32::try_from(std::process::id()).unwrap())
                );
            } else if bad_scratch {
                assert!(
                    outcome
                        .unwrap_err()
                        .to_string()
                        .contains("rt_sigaction did not complete successfully")
                );
                assert!(pending.is_none());
            } else {
                assert_eq!(outcome.unwrap(), 0);
                assert!(pending.is_none());
            }
            edge.set_registers(pid, original).unwrap();
            drop(wait);
            drop(hub);
            drop(child);
        }
    }
}
