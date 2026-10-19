//! Starting a program held between fork and exec.
//!
//! An engine then sees it from its first call: the BPF engine adds it to the
//! traced processes, and the ptrace engine seizes it, before it executes.

use std::os::fd::{AsRawFd as _, OwnedFd};
use std::process::{Command, ExitStatus};
use std::sync::mpsc;
use std::thread::JoinHandle;

use nix::libc;

use super::seccomp::Instruction;

/// A child released at its gate.
pub struct Gate {
    pub pid: u32,
    go: Option<OwnedFd>,
    spawner: Option<JoinHandle<std::io::Result<Option<ExitStatus>>>>,

    /// Dropping it lets a spawner that does not reap its child return.
    hold: Option<mpsc::Sender<()>>,
}

/// What the gate does once opened.
#[derive(Debug, Clone, Default)]
pub struct GateOptions {
    /// A seccomp filter the child installs before it executes.
    pub filter: Option<Vec<Instruction>>,

    /// Whether the spawning thread waits for the child and reaps it, as
    /// when nothing traces it. A ptrace engine reaps it itself.
    pub reap: bool,
}

impl Gate {
    /// Lets the child execute.
    pub fn open(&mut self) -> std::io::Result<()> {
        let go = self
            .go
            .take()
            .ok_or_else(|| std::io::Error::other("the gate is open"))?;
        let mut file = std::fs::File::from(go);
        std::io::Write::write_all(&mut file, &[1])?;
        record!("syscalls launch: {} opened its gate", self.pid);
        Ok(())
    }

    /// Waits for the spawning thread: the program's exit status when the
    /// thread reaped it, or why it never executed.
    pub fn finish(&mut self) -> std::io::Result<Option<ExitStatus>> {
        drop(self.hold.take());
        self.spawner.take().map_or(Ok(None), |spawner| {
            spawner
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("the spawning thread panicked")))
        })
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        // A gate dropped unopened ends the child before it executes.
        drop(self.go.take());
        let _ = self.finish();
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).map_err(std::io::Error::from)
}

/// Starts `command` released at a gate. A thread of its own spawns it, because
/// spawning returns only after the exec, and stays while the child lives:
/// the child dies with that thread.
pub fn launch(mut command: Command, options: GateOptions) -> std::io::Result<Gate> {
    let (pid_read, pid_write) = pipe()?;
    let (go_read, go_write) = pipe()?;
    gate(
        &mut command,
        [&pid_write, &go_read, &go_write],
        options.filter,
    );
    let (hold, released) = mpsc::channel::<()>();
    let reap = options.reap;
    let spawner = std::thread::Builder::new()
        .name("microscope-syscalls-child".into())
        .spawn(move || {
            let spawned = command.spawn();
            drop((pid_write, go_read));
            let mut child = spawned?;
            if reap {
                return child.wait().map(Some);
            }

            // The tracer reaps the child; this thread only outlives it.
            let _ = released.recv();
            drop(child);
            Ok(None)
        })?;
    let mut pid = [0_u8; 4];
    let mut file = std::fs::File::from(pid_read);
    if std::io::Read::read_exact(&mut file, &mut pid).is_err() {
        drop(hold);
        let error = match spawner.join() {
            Ok(Err(error)) => error,
            _ => std::io::Error::other("the program did not start"),
        };

        return Err(error);
    }

    let pid = u32::from_le_bytes(pid);
    record!("syscalls launch: {pid} waits at its gate");

    Ok(Gate {
        pid,
        go: Some(go_write),
        spawner: Some(spawner),
        hold: Some(hold),
    })
}

fn gate(
    command: &mut Command,
    [pid_write, go_read, go_write]: [&OwnedFd; 3],
    filter: Option<Vec<Instruction>>,
) {
    // Built before the fork: the child may not allocate.
    let filter = filter.map(|program| {
        program
            .into_iter()
            .map(|instruction| libc::sock_filter {
                code: instruction.code,
                jt: instruction.jt,
                jf: instruction.jf,
                k: instruction.k,
            })
            .collect()
    });
    super::super::sys::before_exec(
        command,
        super::super::sys::BeforeExec {
            dies_with_parent: true,
            gate: Some(super::super::sys::Gate {
                pid: pid_write.as_raw_fd(),
                go_read: go_read.as_raw_fd(),
                go_write: go_write.as_raw_fd(),
            }),
            filter,
            ..Default::default()
        },
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A gate closed unopened ends its child before it executes, which
    /// needs the child to have closed its own copy of the gate.
    #[test]
    fn a_gate_closed_unopened_ends_its_child_before_it_executes() {
        let options = GateOptions {
            filter: None,
            reap: true,
        };

        let mut gate = launch(Command::new("true"), options).expect("a gated child");
        let pid = gate.pid;
        drop(gate.go.take());
        let (finished, ended) = mpsc::channel();
        std::thread::spawn(move || {
            finished.send(gate.finish().map_err(|error| error.raw_os_error()))
        });
        let ended = ended.recv_timeout(Duration::from_secs(10));
        if ended.is_err() {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid.cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
        }

        assert_eq!(
            ended.expect("the child ends"),
            Err(Some(libc::ECANCELED)),
            "the child did not execute"
        );
    }

    /// A launched program dies with the thread that spawned it, as it
    /// would with the debugger, rather than running on unwatched.
    #[test]
    fn a_launched_program_dies_with_the_thread_that_spawned_it() {
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

        let options = GateOptions {
            filter: None,
            reap: false,
        };

        let mut command = Command::new("sleep");
        command.arg("100");
        let mut gate = launch(command, options).expect("a gated child");
        let pid = nix::unistd::Pid::from_raw(gate.pid.cast_signed());
        gate.open().expect("the gate opens");

        // The spawning thread returns once the program executes.
        gate.finish().expect("the program executes");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let status = loop {
            match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(WaitStatus::StillAlive) => {
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                    let _ = waitpid(pid, None);
                    panic!("the program outlived the thread that spawned it");
                }
                other => break other,
            }
        };

        assert_eq!(
            status,
            Ok(WaitStatus::Signaled(
                pid,
                nix::sys::signal::Signal::SIGKILL,
                false
            ))
        );
    }
}
