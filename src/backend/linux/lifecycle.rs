//! Starting, attaching to, and ending a traced process, and tracking its
//! threads, forks, and exec.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::signals::{Signal, WaitEvent};
use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExceptionDisposition, ExecutionId, ExitStatus, HeldChild, HeldProcess,
    LaunchOptions, ProcessId, Reply, ResumeScope, StopId, StopReason,
};

use crate::{Error, Result, VirtualAddress};

use super::breakpoints::{install_logical_breakpoint, lift_all_traps, reinstall_all_traps};
use super::native::{LinuxTraceOps, SignalQueue, is_vanished_tracee};
use super::{
    ActiveExecution, ActiveKind, BREAKPOINT_OPCODE, ClassifiedStop, Controller, ExpectedStop,
    HeldStop, Inferior, InferiorOrigin, LinuxError, MemoryAccessError, NativeThreadState, Orphans,
    Start, StopBarrier, Terminating, TraceThread, VforkWindow, Waiter, backend_error,
    debug_thread_id, exception_info, is_superseded, process_id,
};

/// How many times an attach may find threads it has not traced before it
/// gives up on a thread list that never settles.
const MAX_ATTACH_RESCANS: u32 = 128;

impl<P: LinuxTraceOps> Controller<P> {
    /// Begins a launch or attach, or holds it until the children the last
    /// program forked are released, since their waiter must finish before
    /// another waits.
    pub(super) fn start(&mut self, start: Start) {
        if self.orphans.is_some() && self.deferred_start.is_none() && !self.shutting_down {
            self.deferred_start = Some(start);
            return;
        }

        if self.inferior.is_some() || self.orphans.is_some() {
            start.fail(Error::AlreadyRunning);
            return;
        }

        match start {
            Start::Launch(options, reply) => self.launch(options, reply),
            Start::Attach {
                requested,
                held,
                reply,
            } => self.attach(requested, held, reply),
            Start::LaunchByExec {
                requested,
                stop_at_entry,
                release,
                reply,
            } => self.launch_by_exec(requested, stop_at_entry, release, reply),
        }
    }

    fn launch(&mut self, mut options: LaunchOptions, reply: Reply<ExecutionId>) {
        let stop_at_entry = options.stop_at_entry;
        // A bootstrap that cannot be made leaves only the agent unavailable.
        let python = self
            .ptrace
            .python_launch(&self.main.executable, &mut options.environment)
            .transpose();
        let started = self
            .ptrace
            .spawn(&self.launch_path, options)
            .and_then(|pid| {
                self.ptrace
                    .spawn_waiter(self.message_sender.clone())
                    .map(|waiter| (pid, waiter))
                    .inspect_err(|_| {
                        let _ = self.ptrace.kill(pid, Signal::SIGKILL);
                        let _ = self.ptrace.reap(pid);
                    })
            });

        match started {
            Ok((pid, waiter)) => {
                self.begin_launch(InferiorOrigin::Launched, pid, stop_at_entry, waiter, reply);
                let python = python
                    .map(|launch| launch.and_then(|launch| launch.launched(pid).map(|()| launch)));

                if let Some(inferior) = self.inferior.as_mut() {
                    inferior.python_launch = python;
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Seizes a process waiting to exec the executable and lets it, so the
    /// exec completes the launch as a spawned process's initial exec does.
    fn launch_by_exec(
        &mut self,
        requested: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    ) {
        match self.seize_exec_target(requested) {
            Ok((pid, waiter)) => {
                let origin = InferiorOrigin::LaunchedByExec;
                self.begin_launch(origin, pid, stop_at_entry, waiter, reply);
                release();
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn seize_exec_target(&self, requested: ProcessId) -> Result<(Pid, Waiter)> {
        let pid = requested_pid(requested)?;
        if self.ptrace.thread_group_id(pid)? != pid {
            return Err(Error::InvalidProcessId(requested.get()));
        }

        if !self.ptrace.seize(pid, true)? {
            return Err(Error::NotRunning);
        }

        // A thread the process started before it was seized would exec
        // untraced.
        if self.ptrace.process_threads(pid)? != [pid] {
            self.rollback_seized(&BTreeSet::from([pid]));
            return Err(Error::ProcessHasThreads(requested.get()));
        }

        self.ptrace
            .spawn_waiter(self.message_sender.clone())
            .inspect_err(|_| self.rollback_seized(&BTreeSet::from([pid])))
            .map(|waiter| (pid, waiter))
    }

    /// Makes `pid`, which has yet to exec the executable, the inferior of a
    /// launch that its exec completes.
    fn begin_launch(
        &mut self,
        origin: InferiorOrigin,
        pid: Pid,
        stop_at_entry: bool,
        waiter: Waiter,
        reply: Reply<ExecutionId>,
    ) {
        let expected = if origin == InferiorOrigin::LaunchedByExec {
            ExpectedStop::AdoptedExec
        } else {
            ExpectedStop::InitialExec
        };

        let process_id = process_id(pid);
        let execution_id = ExecutionId::new(1);
        self.reset_breakpoint_hit_counts();
        self.inferior = Some(Inferior {
            active: Some(ActiveExecution {
                id: execution_id,
                kind: ActiveKind::Launch,
                scope: ResumeScope::Process(process_id),
                resume_threads: BTreeSet::from([pid]),
            }),
            next_execution: 1,
            // Like a pause requested during launch, an entry stop completes
            // at the initial exec stop.
            barrier: stop_at_entry.then_some(StopBarrier::visible(pid, StopReason::Entry)),
            ..Inferior::new(
                origin,
                pid,
                self.main.loaded(crate::LoadBias::default()),
                BTreeMap::from([(pid, TraceThread::starting(expected))]),
                Some(waiter),
            )
        });
        self.launch_reply = Some(reply);
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorLaunched {
            revision: self.revision,
            process_id,
            execution_id,
        });
    }

    fn attach(&mut self, requested: ProcessId, held: bool, reply: Reply<StopId>) {
        let (tgid, seized, unseized, waiter) = match self.seize_process(requested) {
            Ok(seized) => seized,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        let threads = seized
            .iter()
            .map(|&pid| (pid, TraceThread::starting(ExpectedStop::InitialAttach)))
            .collect();

        self.reset_breakpoint_hit_counts();
        let mut inferior = Inferior::new(
            InferiorOrigin::Attached,
            tgid,
            self.main.loaded(crate::LoadBias::default()),
            threads,
            Some(waiter),
        );
        inferior.unseized_threads = unseized;
        inferior.held = held.then_some(HeldStop::Stopped);
        self.inferior = Some(inferior);
        self.attach_reply = Some(reply);
        self.attach_rescans = 0;

        for tid in seized {
            if let Err(error) = self.interrupt_or_retire(tid) {
                self.fail_inferior(error);
                return;
            }
        }

        // The attach stop is presented from the leader when it survives.
        let Some(inferior) = self.inferior.as_mut() else {
            self.fail_inferior(Error::NotRunning);
            return;
        };

        let Some(triggering_thread) = inferior
            .threads
            .contains_key(&tgid)
            .then_some(tgid)
            .or_else(|| inferior.threads.keys().next().copied())
        else {
            self.fail_inferior(Error::NotRunning);
            return;
        };

        inferior.barrier = Some(StopBarrier::visible(triggering_thread, StopReason::Attach));
    }

    /// Seizes every listed thread of the process `requested` names, and
    /// starts the waiter that reports them. Returns the process, the threads
    /// seized, those that had finished exiting, and the waiter.
    pub(super) fn seize_process(
        &self,
        requested: ProcessId,
    ) -> Result<(Pid, BTreeSet<Pid>, BTreeSet<Pid>, Waiter)> {
        let tgid = self.ptrace.thread_group_id(requested_pid(requested)?)?;
        let (seized, unseized) = self.seize_listed_threads(tgid)?;
        if self
            .expected_process_start_time
            .is_some_and(|expected| self.ptrace.process_start_time(tgid) != Some(expected))
        {
            self.rollback_seized(&seized);
            return Err(Error::TargetChangedDuringAttach);
        }

        if seized.is_empty() {
            return Err(Error::NotRunning);
        }

        let waiter = self
            .ptrace
            .spawn_waiter(self.message_sender.clone())
            .inspect_err(|_| self.rollback_seized(&seized))?;
        Ok((tgid, seized, unseized, waiter))
    }

    /// Interrupts a seized thread, or retires one that is exiting, whose exit
    /// status is due.
    fn interrupt_or_retire(&mut self, tid: Pid) -> Result<()> {
        if !self.ptrace.interrupt(tid)? {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            inferior.threads.remove(&tid);
            inferior.retired_threads.insert(tid);
        }

        Ok(())
    }

    /// Seizes every thread `tgid` lists, returning those seized and those
    /// that had finished exiting. A failure releases those seized.
    ///
    /// Threads created while these are seized are found once every seized
    /// thread has stopped; see `seize_untraced_threads`. Listing again now
    /// could not find them all, and would find threads already traced
    /// through their creator's clone event, which cannot be seized. Only
    /// while no thread is seized, so none is traced, does it list again.
    fn seize_listed_threads(&self, tgid: Pid) -> Result<(BTreeSet<Pid>, BTreeSet<Pid>)> {
        let mut seized = BTreeSet::new();
        let mut unseized = BTreeSet::new();
        let result = (|| -> Result<()> {
            // An unseized thread, such as a leader that exited alone, may
            // have created another the listing missed, in which the process
            // lives on. List again until a thread is seized or the listing
            // names none that was not tried.
            loop {
                let mut tried = false;
                for tid in self.ptrace.process_threads(tgid)? {
                    if seized.contains(&tid) || unseized.contains(&tid) {
                        continue;
                    }

                    tried = true;
                    if self.ptrace.seize(tid, false)? {
                        seized.insert(tid);
                    } else {
                        unseized.insert(tid);
                    }
                }

                if !seized.is_empty() || !tried {
                    return Ok(());
                }
            }
        })();
        if let Err(error) = result {
            self.rollback_seized(&seized);
            return Err(error);
        }

        Ok((seized, unseized))
    }

    pub(super) fn rollback_seized(&self, seized: &BTreeSet<Pid>) {
        for &pid in seized {
            let _ = self.ptrace.interrupt(pid);
        }

        for &pid in seized {
            loop {
                match self.ptrace.wait_status(pid) {
                    Ok(WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(..)) => {
                        let _ = self.ptrace.detach(pid, None);
                        break;
                    }
                    Ok(WaitEvent::Exited(..) | WaitEvent::Signaled(..)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }

    pub(super) fn handle_initial_stop(&mut self, pid: Pid) -> Result<()> {
        self.ptrace.set_options(pid, true)?;
        self.refresh_thread_names();
        let load_bias = self.executable_load_bias(pid)?;
        self.load_main_image(load_bias)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        let execution_id = inferior
            .active
            .as_ref()
            .ok_or_else(|| Error::internal("launch is active"))?
            .id;
        let process_id = process_id(inferior.tgid);
        let pause_requested = inferior.barrier.is_some();
        let generation = inferior.watch.generation;
        let thread = inferior.thread_mut(pid)?;
        thread.expected = ExpectedStop::None;

        // A freshly executed image starts with empty debug registers.
        thread.armed = Some(generation);
        if pause_requested {
            // A pause requested during launch completes at this stop instead
            // of letting the new image run first.
            thread.state = NativeThreadState::Stopped;
            if let Some(reply) = self.launch_reply.take() {
                let _ = reply.send(Ok(execution_id));
            }

            return self.finish_barrier_if_ready();
        }

        if self.catchpoints.is_empty() {
            self.ptrace.continue_execution(pid, None)?;
        } else {
            self.ptrace.continue_to_syscall(pid, None)?;
        }

        thread.state = NativeThreadState::Running;
        self.bump_revision();
        if let Some(reply) = self.launch_reply.take() {
            let _ = reply.send(Ok(execution_id));
        }

        let _ = self.events.send(DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id,
            execution_id,
            resumed: ResumeScope::Process(process_id),
        });

        Ok(())
    }

    /// The load bias of the executable in `pid`'s process, which fails
    /// unless the process runs the executable.
    fn executable_load_bias(&self, pid: Pid) -> Result<crate::LoadBias> {
        self.ptrace.load_bias(
            pid,
            &self.main.executable,
            self.main.image.address_range().start.get(),
            self.main.identity,
        )
    }

    /// Makes the program `pid` executed, which is not the main program, the
    /// main program under new ids, and returns where it is loaded. The old
    /// program's breakpoint locations go with it: one by function or line
    /// is placed again in the new program, and one by address stays
    /// pending, since its address means nothing there. Fails with why, and
    /// changes nothing, when the new program cannot be loaded.
    fn replace_main_program(&mut self, pid: Pid) -> Result<crate::LoadBias> {
        let program = self.ptrace.process_program(pid)?;
        let unloadable = |reason: String| {
            backend_error(LinuxError::ProgramUnreadable {
                path: program.path.clone(),
                reason,
            })
        };
        let image_id = crate::ModuleImageId::new(self.next_image_id);
        let debug = self
            .ptrace
            .load_modules(&[(program.path.clone(), image_id)], &self.debug_files)
            .into_iter()
            .next()
            .ok_or_else(|| Error::internal("a load for the program"))?
            .map_err(|error| unloadable(error.to_string()))?;
        crate::debug_info::check_architecture(&debug)
            .map_err(|error| unloadable(error.to_string()))?;
        let image_base = debug.image.address_range().start.get();
        let load_bias = self
            .ptrace
            .load_bias(pid, &program.path, image_base, program.identity)?;

        let module_id = crate::ModuleId::new(self.next_module_id);
        self.next_module_id = self
            .next_module_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::ModuleIdExhausted))?;
        self.next_image_id = self
            .next_image_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::ModuleImageIdExhausted))?;

        let previous = self.main.image.id();
        self.unregister_module(self.main.id);
        self.runtime_models.borrow_mut().remove(&previous);

        let mut cleared = false;
        for breakpoint in &mut self.breakpoints {
            if !breakpoint.locations.is_empty() {
                breakpoint.locations = Arc::from([]);
                cleared = true;
            }
        }
        if cleared {
            self.publish_breakpoints_changed();
        }

        self.main = super::MainProgram {
            id: module_id,
            executable: Arc::new(program.path.clone()),
            identity: program.identity,
            c_library: program.c_library,
            image: Arc::clone(&debug.image),
            unwind: Arc::clone(&debug.unwind),
        };
        let loaded = self.main.loaded(load_bias);
        self.modules.insert(
            module_id,
            super::RuntimeModule {
                loaded,
                image: debug.image,
                unwind: debug.unwind,
                variables: debug.variables,
                tls: None,
            },
        );
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ModuleLoaded {
            revision: self.revision,
            module: crate::LoadedModuleRecord {
                module: loaded,
                path: Arc::new(program.path),
            },
        });

        Ok(load_bias)
    }

    /// Places the executable's image at `load_bias` in a freshly executed
    /// process and starts following the dynamic loader, which the kernel
    /// mapped with the program, so breakpoints in libraries resolve as they
    /// load.
    fn load_main_image(&mut self, load_bias: crate::LoadBias) -> Result<()> {
        self.place_main_image(load_bias)?;
        self.refresh_libraries()
    }

    /// Places the executable's image at `load_bias` and plants every
    /// breakpoint in it.
    fn place_main_image(&mut self, load_bias: crate::LoadBias) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let main = self
            .modules
            .get_mut(&self.main.id)
            .ok_or_else(|| Error::internal("main module is registered"))?;
        inferior.loaded_module = self.main.loaded(load_bias);
        main.loaded = inferior.loaded_module;

        for breakpoint in self
            .breakpoints
            .iter()
            .filter(|breakpoint| breakpoint.enabled)
        {
            install_logical_breakpoint(&self.ptrace, inferior, breakpoint)?;
        }

        Ok(())
    }

    pub(super) fn handle_initial_attach_stop(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.expected = ExpectedStop::None;
        self.finish_barrier_if_ready()
    }

    /// Seizes the threads an attach has not traced yet, and returns whether
    /// the attach now waits for any of them to stop.
    ///
    /// The kernel decides whether a new thread is traced as its clone
    /// begins, so a thread seized inside clone creates an untraced thread,
    /// which can join the thread list after any listing taken meanwhile.
    /// Once every seized thread is stopped none is inside clone, and each
    /// thread they created since was announced by its clone event. Any
    /// other listed thread is untraced, and would die of the first
    /// breakpoint it reached.
    pub(super) fn seize_untraced_threads(&mut self) -> Result<bool> {
        loop {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let untraced = self
                .ptrace
                .process_threads(inferior.tgid)?
                .into_iter()
                .filter(|tid| {
                    !inferior.threads.contains_key(tid)
                        && !inferior.retired_threads.contains(tid)
                        && !inferior.vanished_threads.contains(tid)
                        && !inferior.unseized_threads.contains(tid)
                        && !inferior.unowned_stops.contains_key(tid)
                })
                .collect::<Vec<_>>();

            if untraced.is_empty() {
                return Ok(false);
            }

            // Untraced threads can keep creating untraced threads.
            self.attach_rescans += 1;
            if self.attach_rescans > MAX_ATTACH_RESCANS {
                return Err(backend_error(LinuxError::AttachThreadsUnstable));
            }

            let mut waiting = false;

            for tid in untraced {
                if !self.ptrace.seize(tid, false)? {
                    inferior.unseized_threads.insert(tid);
                    continue;
                }

                if self.ptrace.interrupt(tid)? {
                    inferior
                        .threads
                        .insert(tid, TraceThread::starting(ExpectedStop::InitialAttach));
                    waiting = true;
                } else {
                    // The thread is exiting; its exit status retires it.
                    inferior.retired_threads.insert(tid);
                }
            }

            if waiting {
                return Ok(true);
            }
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn handle_thread_start(&mut self, pid: Pid) -> Result<()> {
        let exit_kill = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin.owned());

        // A sibling's `exit_group` may already have killed the new thread
        // out of its first stop. Its exit status follows and retires it.
        let vanished = match self.ptrace.set_options(pid, exit_kill) {
            Ok(()) => false,
            Err(error) if is_vanished_tracee(&error) => true,
            Err(error) => return Err(error),
        };

        let arm_failure = if vanished {
            None
        } else {
            self.arm_new_thread(pid)
        };

        if let Some(inferior) = self.inferior.as_mut() {
            let name = self.ptrace.thread_name(inferior.tgid, pid);
            inferior.thread_mut(pid)?.name = name;
        }

        let (process_id, barrier_active, should_resume) = {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.state = if vanished {
                NativeThreadState::Exiting
            } else {
                NativeThreadState::Stopped
            };

            thread.expected = ExpectedStop::None;

            // A thread created while a breakpoint site is lifted for a repair
            // waits for the repair; resuming execution then resumes it.
            let should_resume = inferior.repairs.is_empty()
                && inferior
                    .active
                    .as_ref()
                    .is_some_and(|active| active.resume_threads.contains(&pid));
            (
                process_id(inferior.tgid),
                inferior.barrier.is_some(),
                should_resume,
            )
        };

        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadStarted {
            revision: self.revision,
            process_id,
            thread_id: debug_thread_id(pid),
        });

        if let Some(error) = arm_failure {
            // The new thread stays stopped: running it unarmed would silently
            // miss accesses the user asked to watch.
            return self.begin_visible_stop(
                pid,
                StopReason::WatchpointArmFailed {
                    thread_id: debug_thread_id(pid),
                    description: error.to_string().into(),
                },
            );
        }

        if barrier_active {
            self.finish_barrier_if_ready()
        } else if should_resume {
            self.continue_thread(pid)
        } else {
            Ok(())
        }
    }

    pub(super) fn initialize_attached_inferior(&mut self) -> Result<()> {
        let pid = self.inferior.as_ref().ok_or(Error::NotRunning)?.tgid;
        let load_bias = self.executable_load_bias(pid)?;
        self.place_main_image(load_bias)?;
        self.clear_attached_debug_registers()?;
        self.end_held_stop()
    }

    /// Ends the job-control stop a held process was released in, now that
    /// every thread is in a ptrace-stop: without it, the process would run
    /// while job control still counts it stopped, and stop again once
    /// detached. The `SIGCONT` is delivered when the process next runs,
    /// and suppressed then.
    fn end_held_stop(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.held != Some(HeldStop::Stopped) {
            return Ok(());
        }

        self.ptrace.kill(inferior.tgid, Signal::SIGCONT)?;
        inferior.held = Some(HeldStop::Continued);
        Ok(())
    }

    /// Whether `pending` is the `SIGCONT` that ended a held process's
    /// stop, which the program must never see. Recognizing it consumes it,
    /// and restores the mask a detach changed to take it.
    pub(super) fn take_held_continue(&mut self, signal: Signal, sender: Option<i32>) -> bool {
        let tracer = self.ptrace.tracer_process();
        let Some(inferior) = self.inferior.as_mut() else {
            return false;
        };

        let Some(held @ (HeldStop::Continued | HeldStop::Draining { .. })) = inferior.held else {
            return false;
        };

        if signal != Signal::SIGCONT || sender != Some(tracer) {
            return false;
        }

        record!("suppressed the SIGCONT that ended the held stop");
        inferior.held = None;
        if let HeldStop::Draining { thread, mask } = held {
            // A thread gone since has no mask left to restore.
            let _ = self.ptrace.set_signal_mask(thread, mask);
        }

        true
    }

    pub(super) fn handle_ptrace_event(&mut self, pid: Pid, event: i32) -> Result<()> {
        // SIGKILL may have taken the thread out of an event's stop since it
        // was reported, and the event message then describes the exit.
        if event != libc::PTRACE_EVENT_EXIT && is_superseded(&self.ptrace.signal_metadata(pid)) {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        }

        let thread = self
            .inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .thread_mut(pid)?;

        match event {
            libc::PTRACE_EVENT_CLONE => {
                thread.state = NativeThreadState::Stopped;
                thread.inside_call = true;
                self.handle_clone_event(pid)
            }
            libc::PTRACE_EVENT_FORK => {
                thread.state = NativeThreadState::Stopped;
                thread.inside_call = true;
                self.handle_fork_event(pid)
            }
            libc::PTRACE_EVENT_VFORK => {
                thread.state = NativeThreadState::Stopped;
                thread.inside_call = true;
                self.handle_vfork_event(pid)
            }
            libc::PTRACE_EVENT_VFORK_DONE => {
                thread.state = NativeThreadState::Stopped;
                thread.inside_call = true;
                self.handle_vfork_done(pid)
            }
            libc::PTRACE_EVENT_EXEC => {
                thread.state = NativeThreadState::Stopped;
                thread.inside_call = true;
                self.handle_exec_event(pid)
            }
            libc::PTRACE_EVENT_EXIT => {
                thread.state = NativeThreadState::Exiting;
                let alone = self.leader_exiting_alone(pid)?;
                self.adopt_unannounced_fork_children(pid);
                self.release_exiting_thread(pid)?;
                self.end_vfork_window_of(pid)?;
                if let Some(status) = alone {
                    let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
                    let awaited = inferior.thread(pid)?.awaiting_breakpoint.is_some();
                    return self.thread_exited(pid, status, awaited);
                }

                // A leader exiting alone gives a stop in progress nothing
                // more to wait for.
                self.finish_barrier_if_ready()
            }
            other => self.begin_visible_stop(
                pid,
                StopReason::Unclassifiable {
                    description: format!("unsupported ptrace event {other}").into(),
                },
            ),
        }
    }

    /// The status of a group leader stopped at its exit event that exits
    /// alone, by `exit` while other threads live, or `None` for any other
    /// thread or exit. Linux reports such a leader's exit status only once
    /// every other thread has exited, and the process's own status then
    /// replaces it, so this is the code the thread passed to `exit`, which
    /// its exit event carries.
    fn leader_exiting_alone(&self, pid: Pid) -> Result<Option<ExitStatus>> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;

        // With every other thread exiting too, the process is ending.
        let others_live = inferior.threads.iter().any(|(&other, thread)| {
            other != pid && !matches!(thread.state, NativeThreadState::Exiting)
        });

        if pid != inferior.tgid || !others_live {
            return Ok(None);
        }

        let (registers, message) = match self
            .ptrace
            .registers(pid)
            .and_then(|registers| Ok((registers, self.ptrace.event_message(pid)?)))
        {
            Err(error) if is_vanished_tracee(&error) => return Ok(None),
            result => result?,
        };

        // The exit stop is inside the system call that began the exit. A
        // status in wait's form with a signal in its low bits is SIGKILL's.
        let Ok(word) = i32::try_from(message) else {
            return Ok(None);
        };

        if registers.orig_rax.cast_signed() != libc::SYS_exit || word & 0x7f != 0 {
            return Ok(None);
        }

        Ok(Some(ExitStatus::Code(i64::from((word >> 8) & 0xff))))
    }

    /// Lets a thread stopped at its exit event finish exiting. A sibling's
    /// `exit_group` may already have released it, which is no failure: its
    /// exit status follows either way.
    pub(super) fn release_exiting_thread(&self, pid: Pid) -> Result<()> {
        match self.ptrace.continue_execution(pid, None) {
            Err(error) if is_vanished_tracee(&error) => Ok(()),
            result => result,
        }
    }

    /// The identifier the event stop of `pid` reports, or `None` once SIGKILL
    /// took the thread out of that stop, even while it was read. A thread
    /// leaves an event's stop only for its exit, whose message replaces the
    /// event's, so the stop is checked after the read.
    fn event_message_of(&self, pid: Pid) -> Result<Option<Pid>> {
        let message = match self.ptrace.event_message(pid) {
            Ok(message) => message,
            Err(error) if is_vanished_tracee(&error) => return Ok(None),
            Err(error) => return Err(error),
        };

        if is_superseded(&self.ptrace.signal_metadata(pid)) {
            return Ok(None);
        }

        let message = i32::try_from(message).map_err(|_| Error::AddressOverflow)?;
        Ok(Some(Pid::from_raw(message)))
    }

    pub(super) fn handle_clone_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        // A sibling's exit_group can end the new thread before this event
        // announces it.
        let ended = inferior.vanished_threads.remove(&child);
        let child_tgid = if ended {
            None
        } else {
            match self.ptrace.thread_group_id(child) {
                Ok(tgid) => Some(tgid),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    // The thread is already gone; its exit status follows.
                    inferior.retired_threads.insert(child);
                    None
                }
                Err(error) => return Err(error),
            }
        };

        let Some(child_tgid) = child_tgid else {
            inferior.unowned_stops.remove(&child);
            return self.restart_after_internal(parent);
        };

        if child_tgid != inferior.tgid {
            // The killed child's exit status is still delivered.
            inferior.unowned_stops.remove(&child);
            inferior.retired_threads.insert(child);
            let _ = self.ptrace.kill(child, Signal::SIGKILL);
            return self.begin_visible_stop(
                parent,
                StopReason::Unclassifiable {
                    description: backend_error(LinuxError::UnsupportedClone(child_tgid.as_raw()))
                        .to_string()
                        .into(),
                },
            );
        }

        // The thread a TID named exited, and its exit was handled, before
        // the kernel could give the TID to another.
        let previous = inferior
            .threads
            .insert(child, TraceThread::starting(ExpectedStop::None));
        debug_assert!(
            previous.is_none(),
            "a clone event of {parent} names thread {child}, which is already live"
        );

        // A new thread runs with its siblings, unless the execution resumed
        // one thread alone.
        if let Some(active) = inferior.active.as_mut()
            && matches!(active.scope, ResumeScope::Process(_))
        {
            active.resume_threads.insert(child);
        }

        let pending = inferior.unowned_stops.remove(&child);

        // The new thread, still starting, keeps a barrier from completing.
        self.restart_after_internal(parent)?;
        if let Some(status) = pending {
            self.process_wait(status)?;
        }

        Ok(())
    }

    /// Settles a forked child, which this session does not debug, and
    /// resumes the parent.
    pub(super) fn handle_fork_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };

        // The child's memory is the parent's as it forked; later edits, such
        // as lifting a site to step over it, never reach the child.
        let sites = self
            .inferior
            .as_ref()
            .ok_or(Error::NotRunning)?
            .inherited_sites();

        self.track_fork_child(child, sites);
        self.restart_after_internal(parent)
    }

    /// Settles a fork child whose initial stop already arrived, or records
    /// it to be settled at that stop.
    fn track_fork_child(&mut self, child: Pid, sites: Vec<(VirtualAddress, u8)>) {
        let Some(inferior) = self.inferior.as_mut() else {
            return;
        };

        if inferior.unowned_stops.remove(&child).is_some() {
            self.settle_fork_child(child, &sites);
        } else {
            inferior.fork_children.insert(child, sites);
        }
    }

    /// Settles a vfork child as [`Self::track_fork_child`] does a fork
    /// child, except that it is always released. It inherits no traps of
    /// its own: they are lifted from the memory it shares.
    fn track_vfork_child(&mut self, child: Pid) {
        if let Some(inferior) = self.inferior.as_mut() {
            inferior.vfork_children.insert(child);
        }

        self.track_fork_child(child, Vec::new());
    }

    /// Releases a stopped fork child of the inferior, or holds it for
    /// another session, after removing the breakpoints it inherited,
    /// `sites`, which would otherwise kill it with SIGTRAP once untraced.
    /// Debug registers are not inherited across fork. A child that cannot
    /// be cleaned is killed rather than released with traps in place.
    pub(super) fn settle_fork_child(&mut self, child: Pid, sites: &[(VirtualAddress, u8)]) {
        let Some(parent) = self.inferior.as_ref().map(|inferior| inferior.tgid) else {
            return;
        };

        let holdable = self
            .inferior
            .as_mut()
            .is_some_and(|inferior| !inferior.vfork_children.remove(&child));
        if !self.scrub_and_settle(parent, child, sites, holdable)
            && let Some(inferior) = self.inferior.as_mut()
        {
            inferior.killed_children.insert(child);
        }
    }

    /// Restores the bytes the traps in `sites` replaced in a stopped fork
    /// child of `parent`, then releases it, or holds it while a receiver of
    /// held children is open and the child is `holdable`: a vfork child is
    /// not. A shutdown releases every child: no client is left to take one. A child that cannot be scrubbed is killed rather than
    /// released with traps in place. Returns whether it was detached; one
    /// that was not stays traced until it exits.
    pub(super) fn scrub_and_settle(
        &self,
        parent: Pid,
        child: Pid,
        sites: &[(VirtualAddress, u8)],
        holdable: bool,
    ) -> bool {
        let hold = holdable
            && !self.shutting_down
            && self
                .held_children
                .as_ref()
                .is_some_and(|children| !children.is_closed());
        let settled = self.scrub(child, sites).and_then(|()| {
            if hold {
                self.hold_fork_child(parent, child)
            } else {
                self.ptrace.detach(child, None)
            }
        });

        let Ok(detached) = settled else {
            let _ = self.ptrace.kill(child, Signal::SIGKILL);
            return false;
        };

        // A held child is reported to its receiver instead.
        if detached && !hold {
            let _ = self.events.send(DebuggerEvent::ChildReleased {
                revision: self.revision,
                process_id: process_id(parent),
                child: process_id(child),
            });
        }

        detached
    }

    /// Detaches a scrubbed fork child of `parent` stopped, for another
    /// session to attach to, and sends it to the receiver of held children.
    /// The `SIGSTOP` queued first stops it before it runs an instruction
    /// untraced. Its identity is read while it is traced, so that no other
    /// process can have its identifier yet; one whose identity cannot be
    /// read is released to run instead, since no session could prove it
    /// attached to it. Returns whether it was detached.
    fn hold_fork_child(&self, parent: Pid, child: Pid) -> Result<bool> {
        let Some(start_time) = self.ptrace.process_start_time(child) else {
            record!("released {child}, whose start time is unreadable, rather than holding it");
            return self.ptrace.detach(child, None);
        };

        self.ptrace.request_stop(child, child)?;
        if !self.ptrace.detach(child, None)? {
            return Ok(false);
        }

        record!("held {child} for another session");
        let held = HeldChild::new(
            process_id(parent),
            HeldProcess {
                process_id: process_id(child),
                start_time,
            },
            self.ptrace.held_release(),
        );

        // A receiver gone since drops the child, which releases it.
        if let Some(children) = &self.held_children {
            let _ = children.send(held);
        }

        Ok(true)
    }

    /// Restores the bytes the traps in `sites` replaced in a stopped fork
    /// child.
    fn scrub(&self, child: Pid, sites: &[(VirtualAddress, u8)]) -> Result<()> {
        let mut cleaned = Ok(());

        for &(address, original_byte) in sites {
            let word = match self.ptrace.read_memory_word(child, address.get()) {
                Ok(word) => word,
                // Memory the child cannot read, such as a library unloaded
                // since, holds no trap it could execute.
                Err(MemoryAccessError::Inaccessible) => continue,
                Err(MemoryAccessError::Fatal(error)) => {
                    cleaned = Err(error);
                    break;
                }

                // Only a core dump reads part of a word.
                Err(MemoryAccessError::Partial { .. }) => {
                    cleaned = Err(backend_error(LinuxError::System(Errno::EIO)));
                    break;
                }
            };

            let mut bytes = word.to_ne_bytes();

            // Only a trap is ours to remove. A site removed before the fork,
            // or whose memory was remapped since, such as an unloaded
            // library's, holds code the child must keep.
            if bytes[0] != BREAKPOINT_OPCODE {
                continue;
            }

            bytes[0] = original_byte;
            cleaned = self
                .ptrace
                .write_word(child, address.get(), u64::from_ne_bytes(bytes));
            if cleaned.is_err() {
                break;
            }
        }

        cleaned
    }

    /// Finds the children `thread` forked whose fork events SIGKILL took
    /// away, while they are still its children, so they are scrubbed and
    /// settled like any other.
    fn adopt_unannounced_fork_children(&mut self, thread: Pid) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };

        let sites = inferior.inherited_sites();

        for child in self.ptrace.traced_children(inferior.tgid, thread) {
            let Some(inferior) = self.inferior.as_ref() else {
                return;
            };

            if !inferior.fork_children.contains_key(&child)
                && !inferior.killed_children.contains(&child)
                && !inferior.threads.contains_key(&child)
            {
                self.track_fork_child(child, sites.clone());
            }
        }
    }

    /// Handles a status of a fork child that outlived the inferior.
    pub(super) fn handle_orphan_wait(&mut self, status: WaitEvent) -> bool {
        let pid = status.pid();
        let Some(orphans) = self.orphans.as_mut() else {
            // The caller routes here only with orphans, so none is left to wait for.
            return self.inferior.is_some() || self.orphans.is_some();
        };

        match status {
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                let parent = orphans.parent;
                let sites = orphans.children.remove(&pid);
                if let Some(sites) = sites
                    && !self.scrub_and_settle(parent, pid, &sites, true)
                    && let Some(orphans) = self.orphans.as_mut()
                {
                    orphans.killed.insert(pid);
                }
            }
            WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT) => {
                let _ = self.release_exiting_thread(pid);
            }
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => {
                orphans.children.remove(&pid);
                orphans.killed.remove(&pid);
            }
            _ => {}
        }

        if self
            .orphans
            .as_ref()
            .is_some_and(|orphans| !orphans.children.is_empty() || !orphans.killed.is_empty())
        {
            return true;
        }

        let Some(orphans) = self.orphans.take() else {
            return self.inferior.is_some() || self.orphans.is_some();
        };

        // The program is gone, so a failure has no one to report to.
        if orphans
            .waiter
            .is_some_and(|waiter| self.join_waiter(waiter).is_err())
        {
            record!("the waiter of the orphaned children panicked");
        }

        if self.shutting_down {
            self.finish_shutdown(Ok(()));
            return false;
        }

        if let Some(start) = self.deferred_start.take() {
            self.start(start);
        }

        true
    }

    pub(super) fn handle_exec_event(&mut self, pid: Pid) -> Result<()> {
        let Some(old_tid) = self.event_message_of(pid)? else {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let mut survivor = inferior
            .threads
            .remove(&old_tid)
            .or_else(|| inferior.threads.remove(&pid))
            .unwrap_or_else(|| TraceThread::starting(ExpectedStop::None));

        if matches!(survivor.expected, ExpectedStop::AdoptedExec) {
            survivor.expected = ExpectedStop::InitialExec;
            inferior.threads.insert(pid, survivor);
            if self.executable_load_bias(pid).is_err() {
                return Err(backend_error(LinuxError::ExecutedAnotherProgram(
                    self.main.executable.to_path_buf(),
                )));
            }

            return self.handle_initial_stop(pid);
        }

        inferior
            .retired_threads
            .extend(inferior.threads.keys().copied());
        inferior.threads.clear();
        survivor.state = NativeThreadState::Stopped;
        survivor.expected = ExpectedStop::None;
        survivor.pending_signal = None;
        survivor.held_signal = None;
        survivor.stopped_at_breakpoint = None;
        survivor.trapped_at = None;
        survivor.awaiting_breakpoint = None;
        survivor.debugger_stop_pending = false;

        // The execing thread takes over the leader's TID, whose exit the
        // kernel never reports.
        inferior.retired_threads.remove(&pid);
        inferior.threads.insert(pid, survivor);
        inferior.breakpoints.clear();
        inferior.former_sites.clear();
        inferior.plan_sites.clear();
        inferior.repairs.clear();
        inferior.loader_site = None;
        inferior.runtime_hooks.clear();
        let agent_cleanup = inferior.retire_agents_after_exit();
        // The bootstrap ran in the image the exec replaced, and put the
        // variables that would lead a new interpreter to it back.
        inferior.python_launch = None;
        let had_arming_reasons = !inferior.managed.unarmed.is_empty();
        inferior.managed = super::managed::ManagedSites::default();
        inferior.stack_moves.clear();
        inferior.indirect = super::indirect::IndirectFunctions::default();

        // exec(2) flushes every debug register; the new image's addresses
        // have no relation to the old watchpoints.
        self.discard_watchpoints();

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        if let Some(thread) = inferior.threads.get_mut(&pid) {
            thread.armed = Some(generation);
            thread.watch_hits.clear();
        }

        self.reset_runtime_modules();
        self.refresh_thread_names();

        // The same program again keeps its image; another is loaded in
        // its place, unless it cannot be.
        let load_bias = self
            .executable_load_bias(pid)
            .or_else(|_| self.replace_main_program(pid));
        let followed = load_bias.is_ok();

        match load_bias {
            Ok(load_bias) => self.load_main_image(load_bias)?,
            Err(error) => {
                self.inferior
                    .as_mut()
                    .ok_or(Error::NotRunning)?
                    .unfollowed_exec = Some(error.to_string());
            }
        }
        // Arming the new image publishes reasons it reports, but not that
        // the old image's reasons are gone.
        if had_arming_reasons
            && self
                .inferior
                .as_ref()
                .is_some_and(|inferior| inferior.managed.unarmed.is_empty())
        {
            self.publish_breakpoints_changed();
        }

        let reason = agent_cleanup.map_or_else(
            |description| StopReason::RuntimeEvent { description },
            |()| StopReason::Exec { followed },
        );

        self.begin_visible_stop(pid, reason)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Collects SIGTRAPs seized threads queued before they were interrupted,
    /// and returns whether any thread was resumed for one.
    ///
    /// `PTRACE_INTERRUPT` stops a thread before it dequeues signals, so a trap
    /// raised just before it is still queued: publishing the stop would report
    /// the trap only after a later resume, and detaching would deliver it to
    /// an untraced process. Resumed without a signal, the thread dequeues the
    /// trap into a signal-delivery stop before running any instruction.
    pub(super) fn drain_queued_traps(&mut self) -> Result<bool> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if !inferior.origin.seized() {
            // Launched threads stop with SIGSTOP, which the kernel dequeues
            // only after synchronous signals such as SIGTRAP.
            return Ok(false);
        }

        let candidates = inferior
            .threads
            .iter()
            .filter(|(_, thread)| {
                matches!(thread.state, NativeThreadState::Stopped)
                    && thread.pending_signal.is_none()
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();

        // Validate every queue before resuming any thread. A later read
        // failure must leave the all-stop intact for shutdown cleanup.
        let mut queued = Vec::new();
        for pid in candidates {
            if self
                .ptrace
                .queued_signal(pid, Signal::SIGTRAP, SignalQueue::Thread)?
            {
                queued.push(pid);
            }
        }
        let resumed = !queued.is_empty();

        for pid in queued {
            self.resume_to_drain(pid)?;
        }

        Ok(resumed)
    }

    /// Resumes a stopped thread while the session detaches, to take a
    /// signal still queued for it, passing on a signal it stopped for as
    /// the detach would. A published stop ends with it. A thread SIGKILL
    /// has taken out of its stop goes on to its exit, which the detach
    /// awaits the same way.
    fn resume_to_drain(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        let signal = thread
            .pending_signal
            .take()
            .map(|pending| pending.signal)
            .filter(|signal| self.signals.get(*signal).pass);
        match self.ptrace.continue_execution(pid, signal) {
            Err(error) if !is_vanished_tracee(&error) => return Err(error),
            _ => {}
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(pid)?.state = NativeThreadState::Running;
        if inferior.public_stop.take().is_some() {
            self.bump_revision();
        }

        Ok(())
    }

    /// Resumes a stopped thread to take the `SIGCONT` that ended a held
    /// process's stop while it is still queued, since the detached program
    /// would receive it. Resumed, the thread dequeues it into a
    /// signal-delivery stop before running any instruction, and that stop
    /// is suppressed. A thread that blocks it, as a fork child still inside
    /// `fork` does, has it unblocked until then.
    fn drain_held_continue(&mut self) -> Result<bool> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let thread = match inferior.held {
            Some(HeldStop::Continued) => inferior
                .threads
                .iter()
                .find(|(_, thread)| {
                    matches!(thread.state, NativeThreadState::Stopped)
                        && thread.pending_signal.is_none()
                })
                .map(|(&pid, _)| pid),
            // The thread first reported the interrupt the attach left.
            Some(HeldStop::Draining { thread, .. }) => inferior
                .threads
                .get(&thread)
                .is_some_and(|state| matches!(state.state, NativeThreadState::Stopped))
                .then_some(thread),
            Some(HeldStop::Stopped) | None => None,
        };

        let Some(thread) = thread else {
            return Ok(false);
        };

        if inferior.held == Some(HeldStop::Continued) {
            let mask = match self.ptrace.signal_mask(thread) {
                Err(error) if is_vanished_tracee(&error) => {
                    inferior.thread_mut(thread)?.state = NativeThreadState::Running;
                    return Ok(true);
                }
                result => result?,
            };

            let continued = 1 << (Signal::SIGCONT.number() - 1);
            if mask & continued != 0 {
                match self.ptrace.set_signal_mask(thread, mask & !continued) {
                    // SIGKILL took the thread out of its stop, to its exit,
                    // which the detach awaits.
                    Err(error) if is_vanished_tracee(&error) => {
                        inferior.thread_mut(thread)?.state = NativeThreadState::Running;
                        return Ok(true);
                    }
                    result => result?,
                }

                inferior.held = Some(HeldStop::Draining { thread, mask });
            }
        }

        if !self
            .ptrace
            .queued_signal(thread, Signal::SIGCONT, SignalQueue::Process)?
        {
            // Another thread took it, or nothing sent it.
            if let Some(HeldStop::Draining { thread, mask }) = inferior.held {
                self.ptrace.set_signal_mask(thread, mask)?;
            }

            inferior.held = None;
            return Ok(false);
        }

        self.resume_to_drain(thread)?;
        Ok(true)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn handle_terminal(&mut self, pid: Pid, status: ExitStatus) -> Result<()> {
        let thread_id = debug_thread_id(pid);
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let exited = inferior
            .threads
            .remove(&pid)
            .ok_or(Error::UnknownThread(thread_id))?;
        if pid == inferior.tgid {
            // The kernel reports the leader's exit only once every other
            // thread is gone, even those whose own exits went unseen.
            let others = std::mem::take(&mut inferior.threads);
            inferior.retired_threads.extend(others.into_keys());
        }

        let process_id = process_id(inferior.tgid);
        let execution = inferior.active.as_ref().map(|active| active.id);
        let leader_exited = pid == inferior.tgid;

        // Until an attach stops every thread it seized, a thread created
        // after its listing may run untraced, so the last seized thread to
        // end need not be the process's last.
        if inferior.threads.is_empty()
            && !leader_exited
            && self.attach_reply.is_some()
            && self.seize_threads_left()?
        {
            return self.thread_exited(pid, status, exited.awaiting_breakpoint.is_some());
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        if inferior.threads.is_empty() {
            self.discard_watchpoints();
            let pending = self.settle_stopped_fork_children();
            let mut inferior = self.inferior.take().ok_or(Error::NotRunning)?;
            Self::abandon_assignment(&mut inferior);
            self.agent_cleanup_error = inferior.retire_agents_after_exit().err();
            let edits = inferior.take_pending_edits();
            let killed = std::mem::take(&mut inferior.killed_children);
            let waiter = inferior.waiter.take();

            if pending.is_empty() && killed.is_empty() {
                // Nothing is left to wait for, though the debugger's
                // process may have children from outside the session.
                if let Some(waiter) = waiter {
                    self.join_waiter(waiter)?;
                }
            } else {
                // The waiter reports the children's first stops, at which
                // they are settled, and the exits of those killed.
                self.orphans = Some(Orphans {
                    parent: inferior.tgid,
                    children: pending,
                    killed,
                    waiter,
                });
            }

            self.reset_runtime_modules();
            self.bump_revision();
            let _ = self.events.send(DebuggerEvent::InferiorExited {
                revision: self.revision,
                process_id,
                execution_id: execution,
                status: status.clone(),
            });

            // A process that exits before its first stop never completes a
            // pending launch or attach.
            let startup_failed = || backend_error(LinuxError::ExitedBeforeStop(status.clone()));
            if let Some(reply) = self.launch_reply.take() {
                let _ = reply.send(Err(startup_failed()));
            }

            if let Some(reply) = self.attach_reply.take() {
                let _ = reply.send(Err(startup_failed()));
            }

            if let Some(reply) = self.shutdown_reply.take() {
                let _ = reply.send(
                    self.agent_cleanup_error
                        .take()
                        .map_or(Ok(()), |reason| Err(Error::RuntimeAgent(reason))),
                );
            }

            if let Some(reply) = self.kill_reply.take() {
                let _ = reply.send(
                    self.agent_cleanup_error
                        .take()
                        .map_or(Ok(()), |reason| Err(Error::RuntimeAgent(reason))),
                );
            }

            self.settle_pending_edits(edits);
            return Ok(());
        }

        self.thread_exited(pid, status, exited.awaiting_breakpoint.is_some())
    }

    /// Seizes the threads an attach left untraced, and returns whether any
    /// now waits to stop. A process no longer listed has none.
    fn seize_threads_left(&mut self) -> Result<bool> {
        match self.seize_untraced_threads() {
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            seized => seized,
        }
    }

    /// Publishes the exit of a thread whose process lives on, and ends or
    /// advances the execution it affects: one of the thread alone ends in
    /// the exit. `awaited_breakpoint` says the thread was resumed alone to
    /// reach a breakpoint it awaited.
    fn thread_exited(
        &mut self,
        pid: Pid,
        status: ExitStatus,
        awaited_breakpoint: bool,
    ) -> Result<()> {
        let thread_id = debug_thread_id(pid);
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let process_id = process_id(inferior.tgid);
        let owned_execution = inferior.active.as_ref().is_some_and(|active| {
            matches!(active.scope, ResumeScope::Thread(thread) if thread == thread_id)
                || matches!(
                    active.kind,
                    ActiveKind::Step { owner, .. } | ActiveKind::RuntimeStep { owner, .. }
                        if owner.thread == pid
                )
        });

        // A thread resumed alone to reach its awaited breakpoint holds its
        // siblings back; they must run once it is gone.
        let ran_alone = awaited_breakpoint && inferior.active.is_some();
        let repair_interrupted = inferior.forget_thread(pid, &status);
        let barrier_active = inferior.barrier.is_some();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadExited {
            revision: self.revision,
            process_id,
            thread_id,
            status: status.clone(),
        });

        // A shutdown only stops and releases threads; it never runs them.
        if self.shutting_down {
            return Ok(());
        }

        if barrier_active {
            if owned_execution
                && let Some(barrier) = self
                    .inferior
                    .as_mut()
                    .and_then(|inferior| inferior.barrier.as_mut())
            {
                // The execution has no thread left to resume, so the stop
                // ends in this exit unless it publishes something that
                // matters more, and even if an edit drops that.
                let reason = StopReason::ThreadExited { thread_id, status };
                barrier.ended = Some(reason.clone());
                barrier.reason.get_or_insert(reason);
            }

            return self.finish_barrier_if_ready();
        }

        if owned_execution {
            let reason = StopReason::ThreadExited { thread_id, status };
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;

            // Every other thread stayed stopped during a single-thread
            // execution, and one of them presents the stop. During an
            // all-thread step they may all be running.
            if let Some(stopped) = inferior.threads.iter().find_map(|(&pid, thread)| {
                matches!(thread.state, NativeThreadState::Stopped).then_some(pid)
            }) {
                return self.begin_visible_stop(stopped, reason);
            }

            let running = inferior
                .threads
                .iter()
                .find(|&(&pid, thread)| !inferior.exited_leader(pid, thread))
                .map(|(&pid, _)| pid)
                .ok_or(Error::NotRunning)?;

            return self.raise_barrier(running, reason);
        }

        if repair_interrupted || ran_alone {
            return self.advance_execution();
        }

        Ok(())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        debug_assert!(
            !self.shutting_down,
            "shutdown began twice, which drops the first shutdown's reply"
        );
        debug_assert!(
            self.shutdown_reply.is_none(),
            "a shutdown reply waits with no shutdown under way"
        );
        self.shutting_down = true;
        self.shutdown_reply = reply;
        if let Some(start) = self.deferred_start.take() {
            start.fail(Error::RequestCancelled);
        }

        self.launch_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));
        self.attach_reply
            .take()
            .map(|reply| reply.send(Err(Error::RequestCancelled)));

        let Some(inferior) = self.inferior.as_ref() else {
            // Children the ended inferior forked are released first.
            if self.orphans.is_none() {
                let outcome = self
                    .agent_cleanup_error
                    .take()
                    .map_or(Ok(()), |reason| Err(Error::RuntimeAgent(reason)));
                self.finish_shutdown(outcome);
            }

            return;
        };

        if inferior.origin != InferiorOrigin::Attached {
            if let Err(error) = self.kill_inferior() {
                self.finish_shutdown(Err(error));
            }

            return;
        }

        // An attached process is detached once every thread is stopped. A
        // thread too far into its exit to be interrupted still reports its
        // exit status, which the detach awaits, and publishes as an exit.
        let running = inferior
            .threads
            .iter()
            .filter_map(|(&pid, thread)| {
                (!matches!(thread.state, NativeThreadState::Stopped)).then_some(pid)
            })
            .collect::<Vec<_>>();

        for tid in running {
            if let Err(error) = self.ptrace.interrupt(tid) {
                self.finish_shutdown(Err(error));
                return;
            }
        }

        if let Err(error) = self.detach_when_stopped() {
            self.finish_shutdown(Err(error));
        }
    }

    /// Sends the shutdown reply, if a client is waiting for one.
    pub(super) fn finish_shutdown(&mut self, result: Result<()>) {
        if let Some(reply) = self.shutdown_reply.take() {
            let _ = reply.send(result);
        }
    }

    /// Handles a wait status for a pid that is not a live thread of the
    /// inferior: a retired thread's late exit, a fork child, or a new
    /// thread's first stop arriving before its clone event. Returns whether
    /// the status needed nothing more.
    pub(super) fn absorb_untracked_wait(&mut self, status: &WaitEvent) -> bool {
        let pid = status.pid();
        let Some(inferior) = self.inferior.as_mut() else {
            return false;
        };

        if inferior.threads.contains_key(&pid) {
            return false;
        }

        match status {
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => {
                // A thread no event announced yet exited; one may still come.
                // Every record of the pid ends with it.
                let retired = inferior.retired_threads.remove(&pid);
                let killed = inferior.killed_children.remove(&pid);
                let forked = inferior.fork_children.remove(&pid).is_some();
                inferior.vfork_children.remove(&pid);

                if !(retired || killed || forked) {
                    inferior.vanished_threads.insert(pid);
                }

                inferior.unowned_stops.remove(&pid);
            }
            WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT) => {
                // A thread no event announced yet is exiting; let it go.
                inferior.unowned_stops.remove(&pid);
                inferior.vanished_threads.insert(pid);
                return self.release_exiting_thread(pid).is_ok();
            }
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                if let Some(sites) = inferior.fork_children.remove(&pid) {
                    self.settle_fork_child(pid, &sites);
                } else {
                    inferior.unowned_stops.insert(pid, *status);
                }
            }
            _ => return false,
        }

        true
    }

    /// Settles the fork children whose initial stop arrived, and returns
    /// those still on their way to it. Once the process has exited no
    /// event will announce them, and a traced child left stopped would keep
    /// the waiter from ever finishing.
    fn settle_stopped_fork_children(&mut self) -> BTreeMap<Pid, Vec<(VirtualAddress, u8)>> {
        let Some(inferior) = self.inferior.as_mut() else {
            return BTreeMap::new();
        };

        let stopped = std::mem::take(&mut inferior.unowned_stops);
        let pending = std::mem::take(&mut inferior.fork_children);
        let sites = inferior.inherited_sites();

        // Their parent is gone, so a vfork child no longer blocks it, and
        // an orphan is settled as any other.
        inferior.vfork_children.clear();

        for &pid in stopped.keys() {
            self.settle_fork_child(pid, &sites);
        }

        pending
    }

    pub(super) fn handle_shutdown_wait(&mut self, status: WaitEvent) -> bool {
        let attached = self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin == InferiorOrigin::Attached);
        if self.absorb_untracked_wait(&status) {
            if attached && let Err(error) = self.detach_when_stopped() {
                self.finish_shutdown(Err(error));
                return false;
            }

            return self.inferior.is_some() || self.orphans.is_some();
        }

        if attached {
            return self.handle_detach_wait(status);
        }

        let result = match status {
            WaitEvent::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitEvent::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_EXIT => {
                self.adopt_unannounced_fork_children(pid);
                self.ptrace.continue_during_shutdown(pid)
            }

            // SIGKILL may have moved the thread on to its exit stop since
            // this status, so continuing it may let it exit: the children it
            // forked are found while `/proc` still lists them.
            WaitEvent::Stopped(pid, _)
            | WaitEvent::PtraceEvent(pid, _, _)
            | WaitEvent::PtraceSyscall(pid) => {
                self.adopt_unannounced_fork_children(pid);
                self.kill_inferior()
                    .and_then(|()| self.ptrace.continue_during_shutdown(pid))
            }
            other @ WaitEvent::Continued(_) => Err(backend_error(LinuxError::UnexpectedWait(
                format!("{other:?}"),
            ))),
        };

        if let Err(error) = result {
            self.finish_shutdown(Err(error));
            return false;
        }

        self.inferior.is_some() || self.orphans.is_some()
    }

    pub(super) fn handle_detach_wait(&mut self, status: WaitEvent) -> bool {
        let result = match status {
            WaitEvent::Exited(pid, code) => {
                self.handle_terminal(pid, ExitStatus::Code(i64::from(code)))
            }
            WaitEvent::Signaled(pid, signal, _) => {
                self.handle_terminal(pid, ExitStatus::Terminated(exception_info(signal)))
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_EXIT => {
                self.adopt_unannounced_fork_children(pid);

                // An exiting leader settles the detach: Linux reports its
                // exit only after every other thread's.
                if let Some(thread) = self
                    .inferior
                    .as_mut()
                    .and_then(|inferior| inferior.threads.get_mut(&pid))
                {
                    thread.state = NativeThreadState::Exiting;
                }

                self.release_exiting_thread(pid)
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_CLONE => {
                self.handle_clone_during_detach(pid)
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_FORK => {
                self.handle_fork_during_detach(pid)
            }
            WaitEvent::PtraceEvent(pid, _, event) if event == libc::PTRACE_EVENT_VFORK => {
                self.handle_vfork_during_detach(pid)
            }
            WaitEvent::Stopped(pid, signal) => {
                let classified = self.classify_stop(pid, signal);

                // The program never sees the signal that ended its held stop.
                let pending = match classified {
                    ClassifiedStop::SignalDelivery(pending)
                        if !self.take_held_continue(pending.signal, pending.sender) =>
                    {
                        Some(pending)
                    }
                    _ => None,
                };

                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning);
                inferior.and_then(|inferior| {
                    let thread = inferior.thread_mut(pid)?;
                    thread.state = NativeThreadState::Stopped;
                    if pending.is_some() {
                        thread.pending_signal = pending;
                    }

                    Ok(())
                })
            }

            // A thread detached at a system call goes on with it.
            WaitEvent::PtraceEvent(pid, _, _) | WaitEvent::PtraceSyscall(pid) => {
                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning);
                inferior.and_then(|inferior| {
                    inferior.thread_mut(pid)?.state = NativeThreadState::Stopped;
                    Ok(())
                })
            }
            other @ WaitEvent::Continued(_) => Err(backend_error(LinuxError::UnexpectedWait(
                format!("{other:?}"),
            ))),
        };

        if let Err(error) = result.and_then(|()| self.detach_when_stopped()) {
            self.finish_shutdown(Err(error));
            return false;
        }

        self.inferior.is_some() || self.orphans.is_some()
    }

    /// Detaches once every live thread is stopped, every fork child has
    /// been released, and no thread holds a queued trap. A drained thread's
    /// next stop retries the detach.
    pub(super) fn detach_when_stopped(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        if inferior.threads.is_empty() {
            // Every thread exited while the session ended; once their exits
            // are collected nothing remains to release.
            if inferior.retired_threads.is_empty() && inferior.fork_children.is_empty() {
                self.forget_vanished_inferior();
            }

            return Ok(());
        }

        let ready = inferior.fork_children.is_empty()
            && inferior
                .threads
                .iter()
                .all(|(&pid, thread)| inferior.settled(pid, thread));
        if ready {
            let draining = self.drain_queued_traps().and_then(|resumed| {
                if resumed {
                    Ok(true)
                } else {
                    self.drain_held_continue()
                }
            });

            match draining {
                Ok(false) => self.detach_inferior(),
                Ok(true) => {}
                // Queue evidence can be unavailable even at an all-stop.
                // Report it, but still remove instrumentation and detach.
                Err(error) if self.sites_live() => self.detach_inferior_reporting(Some(error)),
                Err(error) => return Err(error),
            }
        }

        Ok(())
    }

    /// Ends the session of an attached process whose threads all exited.
    /// Its children still due are left to the orphans, as at its exit.
    fn forget_vanished_inferior(&mut self) {
        let pending = self.settle_stopped_fork_children();
        let Some(mut inferior) = self.inferior.take() else {
            return;
        };

        let killed = std::mem::take(&mut inferior.killed_children);
        let waiter = inferior.waiter.take();
        let outcome = if pending.is_empty() && killed.is_empty() {
            waiter.map_or(Ok(()), |waiter| self.join_waiter(waiter))
        } else {
            self.orphans = Some(Orphans {
                parent: inferior.tgid,
                children: pending,
                killed,
                waiter,
            });
            Ok(())
        };

        self.reset_runtime_modules();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorDetached {
            revision: self.revision,
            process_id: process_id(inferior.tgid),
        });
        self.settle_pending_edits(inferior.take_pending_edits());
        self.answer_kill();
        self.finish_shutdown(outcome);
    }

    /// Records a fork child announced while detaching; its initial stop
    /// releases it, and the detach waits for that.
    fn handle_fork_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        // The killed parent goes on to its exit, which the detach awaits.
        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };

        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        let sites = inferior.inherited_sites();
        self.track_fork_child(child, sites);
        Ok(())
    }

    /// Opens the vfork window of `parent`, whose child shares its memory
    /// until it execs or exits. The window's barrier stops the other threads,
    /// then lifts the traps and lets only `parent` run; see
    /// [`Self::settle_vfork_windows`].
    pub(super) fn handle_vfork_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        inferior.vfork_windows.insert(
            parent,
            VforkWindow {
                child,
                resumed: false,
            },
        );

        self.begin_internal_stop()?;
        self.restart_after_internal(parent)
    }

    /// Closes the vfork window of `parent`, whose child has exec'd or
    /// exited, and reinstalls the traps once no window remains open.
    pub(super) fn handle_vfork_done(&mut self, parent: Pid) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        // A parent that vforked before this session attached has no window.
        if inferior.vfork_windows.remove(&parent).is_some() {
            self.close_vfork_windows_if_empty()?;
        }

        if !self.barrier_active() {
            self.begin_internal_stop()?;
        }

        self.restart_after_internal(parent)
    }

    /// Ends the vfork window of a thread that exits, since no `VFORK_DONE`
    /// will come for it.
    pub(super) fn end_vfork_window_of(&mut self, thread: Pid) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };

        if inferior.vfork_windows.remove(&thread).is_some() {
            self.close_vfork_windows_if_empty()?;
        }

        Ok(())
    }

    /// Reinstalls the traps lifted for vfork windows, once none is open.
    fn close_vfork_windows_if_empty(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        if inferior.vfork_windows.is_empty() && inferior.traps_lifted {
            inferior.traps_lifted = false;
            reinstall_all_traps(&self.ptrace, inferior)?;
        }

        Ok(())
    }

    /// Runs once every thread is stopped. While a vfork window is open it
    /// lifts the traps, then resumes each vforking thread, and holds the stop
    /// until the window closes. Returns whether a window holds the stop.
    ///
    /// The traps are lifted before any child is released into the memory they
    /// share, so a child never runs a trap.
    pub(super) fn settle_vfork_windows(&mut self) -> Result<bool> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(false);
        };

        if inferior.vfork_windows.is_empty() {
            return Ok(false);
        }

        if inferior.vfork_windows.values().all(|window| window.resumed) {
            return Ok(true);
        }

        if !inferior.traps_lifted {
            // A repair's lifted site is restored first, so that the lift sees
            // every trap in place and the repair is not left to reinstall one.
            self.restore_active_breakpoints()?;
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            lift_all_traps(&self.ptrace, inferior)?;
            inferior.traps_lifted = true;
        }

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let unresumed: Vec<(Pid, VforkWindow)> = inferior
            .vfork_windows
            .iter()
            .filter(|(_, window)| !window.resumed)
            .map(|(&parent, &window)| (parent, window))
            .collect();
        for (parent, window) in unresumed {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            debug_assert!(
                inferior.traps_lifted,
                "resuming vforking thread {parent} while its child {} shares memory \
                 that still holds traps",
                window.child
            );
            inferior.vfork_windows.insert(
                parent,
                VforkWindow {
                    resumed: true,
                    ..window
                },
            );

            self.track_vfork_child(window.child);
            self.continue_thread(parent)?;
        }

        Ok(true)
    }

    /// Records a vfork child announced while detaching. The child shares the
    /// parent's traps, which detaching removes only once every thread has
    /// stopped, so they are lifted now.
    fn handle_vfork_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };

        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        lift_all_traps(&self.ptrace, inferior)?;
        self.track_vfork_child(child);
        Ok(())
    }

    pub(super) fn handle_clone_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;

        // The killed parent goes on to its exit, which the detach awaits.
        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };

        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;

        // The child's first stop may already have arrived.
        let stopped = inferior.unowned_stops.remove(&child).is_some();
        let thread = inferior
            .threads
            .entry(child)
            .or_insert_with(|| TraceThread::starting(ExpectedStop::None));
        if stopped {
            thread.state = NativeThreadState::Stopped;
        }

        Ok(())
    }

    /// Releases an attached process and publishes the detach.
    ///
    /// The tracer is exiting, so every step is attempted even if an earlier
    /// one fails: stopping early would leave the kernel to release threads
    /// with breakpoints or debug registers still armed. The first failure is
    /// reported in the shutdown reply.
    pub(super) fn detach_inferior(&mut self) {
        self.detach_inferior_reporting(None);
    }

    fn detach_inferior_reporting(&mut self, mut first_error: Option<Error>) {
        debug_assert!(
            self.inferior.as_ref().is_some_and(|inferior| {
                inferior
                    .threads
                    .iter()
                    .all(|(&pid, thread)| inferior.settled(pid, thread))
            }),
            "detaching an inferior with a thread that is not stopped"
        );

        // Every thread was stopped, and only SIGKILL takes one out of its
        // stop: a thread that refuses to be written belongs to a process
        // that is ending, where what is left can never run.
        // A queue read can fail after SIGCONT was temporarily unblocked.
        // Restore its mask even when draining could not finish.
        if let Some(HeldStop::Draining { thread, mask }) =
            self.inferior.as_ref().and_then(|inferior| inferior.held)
            && let Err(error) = self.ptrace.set_signal_mask(thread, mask)
            && !is_vanished_tracee(&error)
        {
            first_error.get_or_insert(error);
        }
        if let Err(reason) = self.prepare_agents_for_detach() {
            first_error.get_or_insert(Error::RuntimeAgent(reason));
        }
        if let Err(error) = self.disarm_for_detach()
            && !is_vanished_tracee(&error)
        {
            first_error.get_or_insert(error);
        }
        let Some(mut inferior) = self.inferior.take() else {
            self.answer_kill();
            self.finish_shutdown(first_error.map_or(Err(Error::NotRunning), Err));
            return;
        };

        let mut record = |result: Result<()>| {
            if let Err(error) = result
                && !is_vanished_tracee(&error)
            {
                first_error.get_or_insert(error);
            }
        };

        let installed = inferior
            .breakpoints
            .iter()
            .filter_map(|(&address, site)| site.installed.then_some(address))
            .collect::<Vec<_>>();
        let pid = inferior.memory_thread();
        for address in installed {
            record(
                self.ptrace
                    .remove_breakpoint(pid, &mut inferior.breakpoints, address),
            );
        }

        if let Some(waiter) = inferior.waiter.take() {
            record(self.join_waiter(waiter));
        }

        for (&pid, thread) in &inferior.threads {
            let signal = thread
                .pending_signal
                .or(thread.held_signal)
                .map(|pending| pending.signal)
                .filter(|signal| self.signals.get(*signal).pass);
            record(self.ptrace.detach(pid, signal).map(drop));
        }

        if !inferior.watch.watchpoints.is_empty() {
            self.publish_watchpoints_changed();
        }

        self.reset_runtime_modules();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorDetached {
            revision: self.revision,
            process_id: process_id(inferior.tgid),
        });
        self.settle_pending_edits(inferior.take_pending_edits());
        self.answer_kill();
        self.finish_shutdown(first_error.map_or(Ok(()), Err));
    }

    /// Answers a kill that a detach overtook: the session ended, and the
    /// signal was sent.
    fn answer_kill(&mut self) {
        if let Some(reply) = self.kill_reply.take() {
            let _ = reply.send(Ok(()));
        }
    }

    /// Kills the inferior and replies once its exit is processed.
    pub(super) fn kill(&mut self, reply: Reply<()>) {
        if self.inferior.is_none() {
            let _ = reply.send(Err(Error::NotRunning));
            return;
        }

        if self.kill_reply.is_some() {
            let _ = reply.send(Err(Error::RequestCancelled));
            return;
        }

        match self.kill_inferior() {
            Ok(()) => self.kill_reply = Some(reply),
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Sends the inferior `SIGTERM`, which is delivered without stopping,
    /// and resumes a stopped inferior so it receives it.
    pub(super) fn terminate(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let signal = Signal::SIGTERM;
        inferior.terminating = Some(Terminating {
            signal,
            delivered: false,
        });
        let (tgid, stop) = (
            inferior.tgid,
            inferior.public_stop.as_ref().map(|stop| stop.id),
        );
        self.ptrace.kill(tgid, signal)?;
        let Some(stop) = stop else {
            return Ok(());
        };

        let process = process_id(tgid);
        let scope = ResumeScope::Process(process);
        let execution = self.begin_execution(
            process,
            stop,
            scope,
            ActiveKind::Continue,
            ExceptionDisposition::Pass,
        )?;
        let _ = self.events.send(DebuggerEvent::InferiorContinued {
            revision: self.revision,
            process_id: process,
            execution_id: execution,
            resumed: scope,
        });

        Ok(())
    }

    pub(super) fn kill_inferior(&self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        self.ptrace.kill(inferior.tgid, Signal::SIGKILL)
    }

    pub(super) fn fail_inferior(&mut self, error: Error) {
        record!("inferior failed: {error}");
        if let Some(reply) = self.launch_reply.take() {
            let _ = reply.send(Err(error));
        } else if let Some(reply) = self.attach_reply.take() {
            let _ = reply.send(Err(error));
            self.begin_shutdown(None);
            return;
        }

        if self
            .inferior
            .as_ref()
            .is_some_and(|inferior| inferior.origin == InferiorOrigin::Attached)
        {
            // A shutdown under way already releases the process.
            if !self.shutting_down {
                self.begin_shutdown(None);
            }
        } else {
            let _ = self.kill_inferior();
        }
    }
}

impl Start {
    /// Answers the start's client with `error`.
    fn fail(self, error: Error) {
        match self {
            Self::Launch(_, reply) | Self::LaunchByExec { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Attach { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

/// The native identifier of a requested process.
pub(super) fn requested_pid(requested: ProcessId) -> Result<Pid> {
    i32::try_from(requested.get())
        .ok()
        .filter(|raw| *raw > 0)
        .map(Pid::from_raw)
        .ok_or(Error::InvalidProcessId(requested.get()))
}
