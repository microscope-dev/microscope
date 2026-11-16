use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::signals::{Signal, WaitEvent};
use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::protocol::{
    DebuggerEvent, ExceptionDisposition, ExecutionId, ExitStatus, LaunchOptions, ProcessId, Reply,
    ResumeScope, StopId, StopReason,
};

use crate::{Error, Result, VirtualAddress};

use super::RunPhase;
use super::breakpoints::{install_logical_breakpoint, lift_all_traps, reinstall_all_traps};
use super::memory::holding_word;
use super::native::{LinuxTraceOps, is_vanished_tracee};
use super::session::{Ended, Session, Shutdown, Startup, Traced};
use super::trap::{AtTrap, Lift, Trap};
use super::{
    ActiveExecution, ActiveKind, BREAKPOINT_OPCODE, ClassifiedStop, Controller, ExpectedStop,
    Inferior, InferiorOrigin, LinuxError, MemoryAccessError, NativeThreadState, Orphans, Start,
    StopBarrier, Terminating, TraceThread, Tracked, VforkWindow, Waiter, backend_error,
    debug_thread_id, exception_info, is_superseded, process_id,
};

const MAX_ATTACH_RESCANS: u32 = 128;

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn start(&mut self, start: Start) {
        match &mut self.session {
            Session::Idle { .. } => {}
            Session::Draining {
                deferred: deferred @ None,
                ..
            } if self.shutdown.is_none() => {
                *deferred = Some(start);
                return;
            }
            Session::Traced(_) | Session::Draining { .. } => {
                start.fail(Error::AlreadyRunning);
                return;
            }
        }

        match start {
            Start::Launch(options, reply) => self.launch(options, reply),
            Start::Attach {
                requested,
                observe,
                reply,
            } => self.attach(requested, observe, reply),
            Start::LaunchByExec {
                requested,
                stop_at_entry,
                release,
                reply,
            } => self.launch_by_exec(requested, stop_at_entry, release, reply),
            Start::Adopt {
                pid,
                launched,
                reply,
            } => self.adopt(pid, launched, reply),
            Start::AdoptExec {
                pid,
                launched,
                reply,
            } => self.adopt_exec(pid, launched, reply),
        }
    }

    fn trace(&mut self, inferior: Inferior, startup: Startup) {
        debug_assert!(
            self.session.is_idle(),
            "a process is traced while the session is not idle"
        );
        self.session = Session::Traced(Traced::starting(inferior, startup));
    }

    fn adopt_exec(&mut self, pid: Pid, launched: bool, reply: Reply<ExecutionId>) {
        match self.ptrace.spawn_waiter(pid, self.message_sender.clone()) {
            Ok(waiter) => {
                let origin = if launched {
                    InferiorOrigin::Launched
                } else {
                    InferiorOrigin::Attached
                };
                self.begin_launch(origin, ExpectedStop::AdoptedExec, pid, true, waiter, reply);
                self.join_python_bootstrap(pid);
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn join_python_bootstrap(&mut self, pid: Pid) {
        let python = self
            .ptrace
            .python_join(&self.main.executable, pid)
            .transpose();
        if let Some(inferior) = self.session.inferior_mut() {
            inferior.python_launch = python;
        }
    }

    fn adopt(&mut self, pid: Pid, launched: bool, reply: Reply<StopId>) {
        let waiter = match self.ptrace.spawn_waiter(pid, self.message_sender.clone()) {
            Ok(waiter) => waiter,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };

        let mut thread = TraceThread::starting(ExpectedStop::None);
        thread.state = NativeThreadState::Stopped;
        self.reset_breakpoint_hit_counts();
        let inferior = Inferior::new(
            if launched {
                InferiorOrigin::Launched
            } else {
                InferiorOrigin::Attached
            },
            pid,
            self.main.loaded(crate::LoadBias::default()),
            BTreeMap::from([(pid, thread)]),
            Some(waiter),
            RunPhase::gathering(None, StopBarrier::visible(pid, StopReason::Attach)),
        );
        self.trace(inferior, Startup::Attach { reply, rescans: 0 });
        self.join_python_bootstrap(pid);

        if let Err(error) = self.finish_barrier_if_ready() {
            self.fail_inferior(error);
        }
    }

    fn launch(&mut self, mut options: LaunchOptions, reply: Reply<ExecutionId>) {
        self.restore_launched_program();
        let stop_at_entry = options.stop_at_entry;
        let follows = self
            .group
            .as_ref()
            .is_some_and(|group| group.following.follows());
        let python = self
            .ptrace
            .python_launch(&self.main.executable, &mut options.environment, follows)
            .transpose();
        let node = self
            .ptrace
            .node_launch(
                &self.main.executable,
                &mut options.environment,
                follows,
                rustix::thread::gettid()
                    .as_raw_nonzero()
                    .get()
                    .unsigned_abs(),
            )
            .transpose();
        let started = self
            .ptrace
            .spawn(&self.launch_path, options)
            .and_then(|pid| {
                self.ptrace
                    .spawn_waiter(pid, self.message_sender.clone())
                    .map(|waiter| (pid, waiter))
                    .inspect_err(|_| {
                        let _ = self.ptrace.kill(pid, Signal::SIGKILL);
                        let _ = self.ptrace.reap(pid);
                    })
            });

        match started {
            Ok((pid, waiter)) => {
                let expected = ExpectedStop::InitialExec;
                self.begin_launch(
                    InferiorOrigin::Launched,
                    expected,
                    pid,
                    stop_at_entry,
                    waiter,
                    reply,
                );
                let python = python
                    .map(|launch| launch.and_then(|launch| launch.launched(pid).map(|()| launch)));

                if let Some(inferior) = self.session.inferior_mut() {
                    inferior.python_launch = python;
                    inferior.node_launch = node;
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn launch_by_exec(
        &mut self,
        requested: ProcessId,
        stop_at_entry: bool,
        release: Box<dyn FnOnce() + Send>,
        reply: Reply<ExecutionId>,
    ) {
        self.restore_launched_program();
        match self.seize_exec_target(requested) {
            Ok((pid, waiter)) => {
                let origin = InferiorOrigin::LaunchedByExec;
                let expected = ExpectedStop::AdoptedExec;
                self.begin_launch(origin, expected, pid, stop_at_entry, waiter, reply);
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

        if self.ptrace.process_threads(pid)? != [pid] {
            self.rollback_seized(&BTreeSet::from([pid]));
            return Err(Error::ProcessHasThreads(requested.get()));
        }

        self.ptrace
            .spawn_waiter(pid, self.message_sender.clone())
            .inspect_err(|_| self.rollback_seized(&BTreeSet::from([pid])))
            .map(|waiter| (pid, waiter))
    }

    fn begin_launch(
        &mut self,
        origin: InferiorOrigin,
        expected: ExpectedStop,
        pid: Pid,
        stop_at_entry: bool,
        waiter: Waiter,
        reply: Reply<ExecutionId>,
    ) {
        let process_id = process_id(pid);
        let execution_id = ExecutionId::new(1);
        self.reset_breakpoint_hit_counts();
        let launch = ActiveExecution {
            id: execution_id,
            kind: ActiveKind::Launch,
            scope: ResumeScope::Process(process_id),
            resume_threads: BTreeSet::from([pid]),
        };
        let run = if stop_at_entry {
            RunPhase::gathering(Some(launch), StopBarrier::visible(pid, StopReason::Entry))
        } else {
            RunPhase::running(launch)
        };
        let inferior = Inferior {
            next_execution: 1,
            ..Inferior::new(
                origin,
                pid,
                self.main.loaded(crate::LoadBias::default()),
                BTreeMap::from([(pid, TraceThread::starting(expected))]),
                Some(waiter),
                run,
            )
        };
        self.trace(inferior, Startup::Launch(reply));
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorLaunched {
            revision: self.revision,
            process_id,
            execution_id,
        });
    }

    fn attach(&mut self, requested: ProcessId, observe: bool, reply: Reply<StopId>) {
        self.restore_launched_program();
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
            RunPhase::idle(),
        );
        inferior
            .tracked
            .extend(unseized.into_iter().map(|tid| (tid, Tracked::Unseized)));
        inferior.observer = observe;
        self.trace(inferior, Startup::Attach { reply, rescans: 0 });

        for tid in seized {
            if let Err(error) = self.interrupt_or_retire(tid) {
                self.fail_inferior(error);
                return;
            }
        }

        let Some(inferior) = self.session.inferior_mut() else {
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

        if let Err(error) = inferior
            .run
            .gather(StopBarrier::visible(triggering_thread, StopReason::Attach))
        {
            self.fail_inferior(error);
        }
    }

    pub(super) fn seize_process(
        &self,
        requested: ProcessId,
    ) -> Result<(Pid, BTreeSet<Pid>, BTreeSet<Pid>, Waiter)> {
        let _span = crate::span!("attach seize");
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
            .spawn_waiter(tgid, self.message_sender.clone())
            .inspect_err(|_| self.rollback_seized(&seized))?;
        Ok((tgid, seized, unseized, waiter))
    }

    fn interrupt_or_retire(&mut self, tid: Pid) -> Result<()> {
        if !self.ptrace.interrupt(tid)? {
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            inferior.threads.remove(&tid);
            inferior.tracked.insert(tid, Tracked::Retired);
        }

        Ok(())
    }

    fn seize_listed_threads(&self, tgid: Pid) -> Result<(BTreeSet<Pid>, BTreeSet<Pid>)> {
        let mut seized = BTreeSet::new();
        let mut unseized = BTreeSet::new();
        let result = (|| -> Result<()> {
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
        let owned = self
            .session
            .inferior()
            .is_none_or(|inferior| inferior.origin.owned());
        self.ptrace.set_options(pid, owned)?;
        self.refresh_thread_names();
        let load_bias = self.executable_load_bias(pid)?;
        self.load_main_image(load_bias)?;
        let stack_start = self
            .ptrace
            .registers(pid)
            .ok()
            .map(|registers| registers.stack_pointer());
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        let execution_id = inferior
            .run
            .execution()
            .ok_or_else(|| Error::internal("launch is active"))?
            .id;
        let process_id = process_id(inferior.tgid);
        let pause_requested = inferior.run.barrier().is_some();
        let generation = inferior.watch.generation;
        let thread = inferior.thread_mut(pid)?;
        thread.expected = ExpectedStop::None;
        thread.stack_start = stack_start;

        thread.armed = Some(generation);
        if pause_requested {
            thread.state = NativeThreadState::Stopped;
            if let Some(reply) = self.session.take_launch() {
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
        if let Some(reply) = self.session.take_launch() {
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

    fn executable_load_bias(&self, pid: Pid) -> Result<crate::LoadBias> {
        self.ptrace.load_bias(
            pid,
            &self.main.executable,
            self.main.image.address_range().start.get(),
            self.main.identity,
        )
    }

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

        self.install_main_program(
            super::MainProgram {
                id: module_id,
                variables: debug.variables,
                executable: Arc::new(program.path),
                identity: program.identity,
                c_library: program.c_library,
                image: debug.image,
                unwind: debug.unwind,
            },
            load_bias,
            Some(process_id(pid)),
        );

        Ok(load_bias)
    }

    fn restore_launched_program(&mut self) {
        if self.main.id == self.launched.id {
            return;
        }

        self.install_main_program(self.launched.clone(), crate::LoadBias::default(), None);

        for breakpoint in &mut self.breakpoints {
            if let crate::BreakpointSpec::Address(address) = breakpoint.spec {
                breakpoint.locations = Arc::from([crate::ResolvedBreakpointLocation {
                    location: crate::BreakpointLocation::Virtual(address),
                    code_instances: Arc::from([]),
                    library: None,
                }]);
            }
        }
    }

    fn install_main_program(
        &mut self,
        main: super::MainProgram,
        load_bias: crate::LoadBias,
        process: Option<ProcessId>,
    ) {
        let previous = crate::LoadedModuleRecord {
            module: self.main.loaded(
                self.modules
                    .get(&self.main.id)
                    .map_or_else(crate::LoadBias::default, |module| module.loaded.load_bias),
            ),
            path: Arc::clone(&self.main.executable),
        };
        self.unregister_module(self.main.id);
        self.runtime_models
            .borrow_mut()
            .remove(&previous.module.image);

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

        self.main = main;
        let loaded = self.main.loaded(load_bias);
        self.modules.insert(
            self.main.id,
            super::RuntimeModule {
                loaded,
                image: Arc::clone(&self.main.image),
                unwind: Arc::clone(&self.main.unwind),
                variables: Arc::clone(&self.main.variables),
                tls: None,
            },
        );
        let record = crate::LoadedModuleRecord {
            module: loaded,
            path: Arc::clone(&self.main.executable),
        };
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ModuleLoaded {
            revision: self.revision,
            module: record.clone(),
        });
        self.program.send_replace(Arc::clone(&self.main.image));
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ProgramChanged {
            revision: self.revision,
            process_id: process,
            previous,
            program: record,
        });
    }

    fn load_main_image(&mut self, load_bias: crate::LoadBias) -> Result<()> {
        self.place_main_image(load_bias)?;
        self.refresh_libraries()
    }

    fn place_main_image(&mut self, load_bias: crate::LoadBias) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
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
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let thread = inferior.thread_mut(pid)?;
        thread.state = NativeThreadState::Stopped;
        thread.expected = ExpectedStop::None;
        self.finish_barrier_if_ready()
    }

    pub(super) fn seize_untraced_threads(&mut self) -> Result<bool> {
        loop {
            let Some(Traced {
                inferior,
                startup: Some(Startup::Attach { rescans, .. }),
                ..
            }) = self.session.traced_mut()
            else {
                return Err(Error::internal("an attach is starting"));
            };
            let untraced = self
                .ptrace
                .process_threads(inferior.tgid)?
                .into_iter()
                .filter(|tid| {
                    !inferior.threads.contains_key(tid) && !inferior.tracked.contains_key(tid)
                })
                .collect::<Vec<_>>();

            if untraced.is_empty() {
                return Ok(false);
            }

            *rescans += 1;
            if *rescans > MAX_ATTACH_RESCANS {
                return Err(backend_error(LinuxError::AttachThreadsUnstable));
            }

            let mut waiting = false;

            for tid in untraced {
                if !self.ptrace.seize(tid, false)? {
                    inferior.tracked.insert(tid, Tracked::Unseized);
                    continue;
                }

                if self.ptrace.interrupt(tid)? {
                    inferior
                        .threads
                        .insert(tid, TraceThread::starting(ExpectedStop::InitialAttach));
                    waiting = true;
                } else {
                    inferior.tracked.insert(tid, Tracked::Retired);
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
            .session
            .inferior()
            .is_some_and(|inferior| inferior.origin.owned());

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

        let stack_start = if vanished {
            None
        } else {
            self.ptrace
                .registers(pid)
                .ok()
                .map(|registers| registers.stack_pointer())
        };

        if let Some(inferior) = self.session.inferior_mut() {
            let name = self.ptrace.thread_name(inferior.tgid, pid);
            let thread = inferior.thread_mut(pid)?;
            thread.name = name;
            thread.stack_start = stack_start;
        }

        let (process_id, barrier_active, should_resume) = {
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            let thread = inferior.thread_mut(pid)?;
            thread.state = if vanished {
                NativeThreadState::Exiting
            } else {
                NativeThreadState::Stopped
            };

            thread.expected = ExpectedStop::None;

            let should_resume = !inferior.repair_under_way()
                && inferior
                    .run
                    .execution()
                    .is_some_and(|active| active.resume_threads.contains(&pid));
            (
                process_id(inferior.tgid),
                inferior.run.barrier().is_some(),
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
        let pid = self.session.inferior().ok_or(Error::NotRunning)?.tgid;
        let load_bias = self.executable_load_bias(pid)?;
        self.place_main_image(load_bias)?;
        self.clear_attached_debug_registers()
    }

    pub(super) fn handle_ptrace_event(&mut self, pid: Pid, event: i32) -> Result<()> {
        if event != libc::PTRACE_EVENT_EXIT && is_superseded(&self.ptrace.signal_metadata(pid)) {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        }

        let thread = self
            .session
            .inferior_mut()
            .ok_or(Error::NotRunning)?
            .thread_mut(pid)?;

        match event {
            libc::PTRACE_EVENT_CLONE => {
                thread.state = NativeThreadState::Stopped;
                thread.call.at_event();
                self.handle_clone_event(pid)
            }
            libc::PTRACE_EVENT_FORK => {
                thread.state = NativeThreadState::Stopped;
                thread.call.at_event();
                self.handle_fork_event(pid)
            }
            libc::PTRACE_EVENT_VFORK => {
                thread.state = NativeThreadState::Stopped;
                thread.call.at_event();
                self.handle_vfork_event(pid)
            }
            libc::PTRACE_EVENT_VFORK_DONE => {
                thread.state = NativeThreadState::Stopped;
                thread.call.at_event();
                self.handle_vfork_done(pid)
            }
            libc::PTRACE_EVENT_EXEC => {
                thread.state = NativeThreadState::Stopped;
                thread.call.at_event();
                self.handle_exec_event(pid)
            }
            libc::PTRACE_EVENT_EXIT => {
                thread.state = NativeThreadState::Exiting;
                let alone = self.leader_exiting_alone(pid)?;
                self.adopt_unannounced_fork_children(pid);
                self.release_exiting_thread(pid)?;
                self.end_vfork_window_of(pid)?;
                if let Some(status) = alone {
                    let inferior = self.session.inferior().ok_or(Error::NotRunning)?;
                    let held = inferior.thread(pid)?.holds_siblings();
                    return self.thread_exited(pid, status, held);
                }

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

    fn leader_exiting_alone(&self, pid: Pid) -> Result<Option<ExitStatus>> {
        let inferior = self.session.inferior().ok_or(Error::NotRunning)?;

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

        let Ok(word) = i32::try_from(message) else {
            return Ok(None);
        };

        if registers.syscall_number().cast_signed() != libc::SYS_exit || word & 0x7f != 0 {
            return Ok(None);
        }

        Ok(Some(ExitStatus::Code(i64::from((word >> 8) & 0xff))))
    }

    pub(super) fn release_exiting_thread(&self, pid: Pid) -> Result<()> {
        match self.ptrace.continue_execution(pid, None) {
            Err(error) if is_vanished_tracee(&error) => Ok(()),
            result => result,
        }
    }

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

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        let record = inferior.tracked.remove(&child);
        let child_tgid = if record == Some(Tracked::Vanished) {
            None
        } else {
            match self.ptrace.thread_group_id(child) {
                Ok(tgid) => Some(tgid),
                Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                    inferior.tracked.insert(child, Tracked::Retired);
                    None
                }
                Err(error) => return Err(error),
            }
        };

        let Some(child_tgid) = child_tgid else {
            return self.restart_after_internal(parent);
        };

        if child_tgid != inferior.tgid {
            inferior.tracked.insert(child, Tracked::Retired);
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

        let previous = inferior
            .threads
            .insert(child, TraceThread::starting(ExpectedStop::None));
        debug_assert!(
            previous.is_none(),
            "a clone event of {parent} names thread {child}, which is already live"
        );

        if let Some(active) = inferior.run.execution_mut()
            && matches!(active.scope, ResumeScope::Process(_))
        {
            active.resume_threads.insert(child);
        }

        let pending = match record {
            Some(Tracked::EarlyStop(status)) => Some(status),
            _ => None,
        };

        self.restart_after_internal(parent)?;
        if let Some(status) = pending {
            self.process_wait(status)?;
        }

        Ok(())
    }

    pub(super) fn handle_fork_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };

        let sites = self
            .session
            .inferior()
            .ok_or(Error::NotRunning)?
            .inherited_sites();

        self.track_fork_child(child, sites, false);
        self.restart_after_internal(parent)
    }

    fn track_fork_child(&mut self, child: Pid, sites: Vec<(VirtualAddress, u8)>, vfork: bool) {
        let Some(inferior) = self.session.inferior_mut() else {
            return;
        };

        match inferior.tracked.remove(&child) {
            Some(Tracked::EarlyStop(_)) => self.settle_fork_child(child, &sites, vfork),
            _ => {
                inferior
                    .tracked
                    .insert(child, Tracked::ForkChild { sites, vfork });
            }
        }
    }

    pub(super) fn settle_fork_child(
        &mut self,
        child: Pid,
        sites: &[(VirtualAddress, u8)],
        vfork: bool,
    ) {
        let Some(parent) = self.session.inferior().map(|inferior| inferior.tgid) else {
            return;
        };

        if !self.scrub_and_settle(parent, child, sites, vfork)
            && let Some(inferior) = self.session.inferior_mut()
        {
            inferior.tracked.insert(child, Tracked::Killed);
        }
    }

    pub(super) fn scrub_and_settle(
        &mut self,
        parent: Pid,
        child: Pid,
        sites: &[(VirtualAddress, u8)],
        vfork: bool,
    ) -> bool {
        if self.shutdown.is_none()
            && let Some(following) = self.following()
        {
            let following = Arc::clone(following);
            if self.scrub(child, sites).is_err() {
                let _ = self.ptrace.kill(child, Signal::SIGKILL);
                return false;
            }

            if let Some(inferior) = self.session.inferior_mut() {
                inferior.tracked.insert(child, Tracked::HandedOver);
            }
            following.transfer(super::follow::Transfer {
                child,
                parent,
                vfork,
                program: self.forked_program(child),
                launched: self
                    .session
                    .inferior()
                    .is_some_and(|inferior| inferior.origin != InferiorOrigin::Attached),
            });
            return true;
        }

        let Ok(detached) = self
            .scrub(child, sites)
            .and_then(|()| self.ptrace.detach(child, None))
        else {
            let _ = self.ptrace.kill(child, Signal::SIGKILL);
            return false;
        };

        if detached {
            let _ = self.events.send(DebuggerEvent::ChildReleased {
                revision: self.revision,
                process_id: process_id(parent),
                child: process_id(child),
            });
        }

        detached
    }

    fn scrub(&self, child: Pid, sites: &[(VirtualAddress, u8)]) -> Result<()> {
        let mut cleaned = Ok(());

        for &(address, original_byte) in sites {
            let (word_address, offset) = holding_word(address.get());
            let word = match self.ptrace.read_memory_word(child, word_address) {
                Ok(word) => word,
                Err(MemoryAccessError::Inaccessible) => continue,
                Err(MemoryAccessError::Fatal(error)) => {
                    cleaned = Err(error);
                    break;
                }

                Err(MemoryAccessError::Partial { .. }) => {
                    cleaned = Err(backend_error(LinuxError::System(Errno::EIO)));
                    break;
                }
            };

            let mut bytes = word.to_ne_bytes();

            let byte = offset.of(&mut bytes);
            if *byte != BREAKPOINT_OPCODE {
                continue;
            }

            *byte = original_byte;
            cleaned = self
                .ptrace
                .write_word(child, word_address, u64::from_ne_bytes(bytes));
            if cleaned.is_err() {
                break;
            }
        }

        cleaned
    }

    fn adopt_unannounced_fork_children(&mut self, thread: Pid) {
        let Some(inferior) = self.session.inferior() else {
            return;
        };

        let sites = inferior.inherited_sites();

        for child in self.ptrace.traced_children(inferior.tgid, thread) {
            let Some(inferior) = self.session.inferior() else {
                return;
            };

            let settled = matches!(
                inferior.tracked.get(&child),
                Some(Tracked::ForkChild { .. } | Tracked::Killed | Tracked::HandedOver)
            );
            if !settled && !inferior.threads.contains_key(&child) {
                self.track_fork_child(child, sites.clone(), false);
            }
        }
    }

    pub(super) fn handle_orphan_wait(&mut self, status: WaitEvent) -> bool {
        let pid = status.pid();
        let Some(orphans) = self.session.orphans_mut() else {
            return !self.session.is_idle();
        };

        match status {
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                let parent = orphans.parent;
                let sites = orphans.children.remove(&pid);
                if let Some(sites) = sites
                    && !self.scrub_and_settle(parent, pid, &sites, false)
                    && let Some(orphans) = self.session.orphans_mut()
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
            .session
            .orphans_mut()
            .is_some_and(|orphans| !orphans.children.is_empty() || !orphans.killed.is_empty())
        {
            return true;
        }

        let Session::Draining {
            orphans,
            deferred,
            ended,
            cleanup_error,
        } = std::mem::replace(&mut self.session, Session::idle())
        else {
            return true;
        };

        if orphans
            .waiter
            .is_some_and(|waiter| self.join_waiter(waiter).is_err())
        {
            record!("the waiter of the orphaned children panicked");
        }

        if self.shutdown.is_some() {
            self.session = Session::Idle {
                ended: Some(ended),
                cleanup_error: None,
            };
            self.finish_shutdown(cleanup_result(cleanup_error));
            return false;
        }

        self.session = Session::Idle {
            ended: Some(ended),
            cleanup_error,
        };
        if let Some(start) = deferred {
            self.start(start);
        }

        true
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one reset per piece of state an exec replaces"
    )]
    pub(super) fn handle_exec_event(&mut self, pid: Pid) -> Result<()> {
        let Some(old_tid) = self.event_message_of(pid)? else {
            return self.handle_classified_stop(pid, ClassifiedStop::Superseded);
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
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

        for pid in std::mem::take(&mut inferior.threads).into_keys() {
            inferior.tracked.insert(pid, Tracked::Retired);
        }
        survivor.state = NativeThreadState::Stopped;
        survivor.expected = ExpectedStop::None;
        survivor.pending_signal = None;
        survivor.held_signal = None;
        survivor.trap = AtTrap::Clear;
        survivor.retrap = None;
        survivor.stack_start = self
            .ptrace
            .registers(pid)
            .ok()
            .map(|registers| registers.stack_pointer());

        inferior.tracked.remove(&pid);
        inferior.threads.insert(pid, survivor);
        inferior.breakpoints.clear();
        inferior.former_sites.clear();
        inferior.plan_sites.clear();
        inferior.loader_site = None;
        inferior.runtime_hooks.clear();
        let agent_cleanup = inferior.retire_agents_after_exit();
        inferior.python_launch = None;
        let had_arming_reasons = !inferior.managed.unarmed.is_empty();
        inferior.managed = super::managed::ManagedSites::default();
        inferior.stack_moves.clear();
        inferior.interpreted_moves = super::interpreted_watches::InterpretedMoves::default();
        inferior.indirect = super::indirect::IndirectFunctions::default();
        inferior.inspectors = super::inspector::Inspectors::default();

        self.discard_watchpoints();

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let generation = inferior.watch.generation;
        if let Some(thread) = inferior.threads.get_mut(&pid) {
            thread.armed = Some(generation);
            thread.watch_hits.clear();
        }

        if self.group.is_some()
            && let Ok(program) = std::fs::read_link(format!("/proc/{pid}/exe"))
            && self.demotes_at_exec(&program)
        {
            self.leave_for_watching(pid, program);
            return Ok(());
        }

        self.reset_runtime_modules();
        self.refresh_thread_names();

        let load_bias = self
            .executable_load_bias(pid)
            .or_else(|_| self.replace_main_program(pid));
        let followed = load_bias.is_ok();

        match load_bias {
            Ok(load_bias) => self.load_main_image(load_bias)?,
            Err(error) => {
                self.session
                    .inferior_mut()
                    .ok_or(Error::NotRunning)?
                    .unfollowed_exec = Some(error.to_string());
            }
        }
        if had_arming_reasons
            && self
                .session
                .inferior()
                .is_some_and(|inferior| inferior.managed.unarmed.is_empty())
        {
            self.publish_breakpoints_changed();
        }

        let continued = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.execution())
            .is_some_and(|active| {
                matches!(
                    active.kind,
                    super::ActiveKind::Continue | super::ActiveKind::Launch
                )
            });
        if followed && agent_cleanup.is_ok() && continued && !self.exec_stops {
            return self.restart_after_internal(pid);
        }

        let reason = agent_cleanup.map_or_else(
            |description| StopReason::RuntimeEvent { description },
            |()| StopReason::Exec { followed },
        );

        self.begin_visible_stop(pid, reason)
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn drain_queued_traps(&mut self) -> Result<bool> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if !inferior.origin.seized() {
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

        let mut queued = Vec::new();
        for pid in candidates {
            if self.ptrace.queued_signal(pid, Signal::SIGTRAP)? {
                queued.push(pid);
            }
        }
        let resumed = !queued.is_empty();

        for pid in queued {
            self.resume_to_drain(pid)?;
        }

        Ok(resumed)
    }

    pub(super) fn drain_requested_stops(&mut self) -> Result<bool> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if inferior.origin.seized() {
            return Ok(false);
        }

        let candidates = inferior
            .threads
            .iter()
            .filter(|(_, thread)| {
                matches!(thread.state, NativeThreadState::Stopped) && thread.debugger_stop_pending
            })
            .map(|(&pid, _)| pid)
            .collect::<Vec<_>>();

        let mut queued = Vec::new();
        for pid in candidates {
            if self.ptrace.queued_signal(pid, Signal::SIGSTOP)? {
                queued.push(pid);
            } else {
                inferior.thread_mut(pid)?.debugger_stop_pending = false;
            }
        }
        let resumed = !queued.is_empty();

        for pid in queued {
            self.resume_to_drain(pid)?;
        }

        Ok(resumed)
    }

    fn resume_to_drain(&mut self, pid: Pid) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
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

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        inferior.thread_mut(pid)?.state = NativeThreadState::Running;
        if inferior.run.release() {
            self.bump_revision();
        }

        Ok(())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn handle_terminal(&mut self, pid: Pid, status: ExitStatus) -> Result<()> {
        let thread_id = debug_thread_id(pid);
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let exited = inferior
            .threads
            .remove(&pid)
            .ok_or(Error::UnknownThread(thread_id))?;
        if pid == inferior.tgid {
            for other in std::mem::take(&mut inferior.threads).into_keys() {
                inferior.tracked.insert(other, Tracked::Retired);
            }
        }

        let process_id = process_id(inferior.tgid);
        let execution = inferior.run.execution().map(|active| active.id);
        let leader_exited = pid == inferior.tgid;

        if inferior.threads.is_empty()
            && !leader_exited
            && self.session.attaching()
            && self.seize_threads_left()?
        {
            return self.thread_exited(pid, status, exited.holds_siblings());
        }

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        if inferior.threads.is_empty() {
            self.discard_watchpoints();
            let orphans = self
                .settle_stopped_fork_children()
                .ok_or(Error::NotRunning)?;
            let ended = Ended::Exited(process_id, status.clone());
            let Traced {
                mut inferior,
                startup,
                kill,
            } = self
                .session
                .take_traced(|_| ended.clone())
                .ok_or(Error::NotRunning)?;
            Self::abandon_assignment(&mut inferior);
            let mut cleanup_error = inferior.retire_agents_after_exit().err();
            let edits = inferior.take_pending_edits();

            let (joined, orphans) = if orphans.children.is_empty() && orphans.killed.is_empty() {
                let joined = (orphans.waiter).map_or(Ok(()), |waiter| self.join_waiter(waiter));
                (joined, None)
            } else {
                (Ok(()), Some(orphans))
            };

            self.reset_runtime_modules();
            self.bump_revision();
            let _ = self.events.send(DebuggerEvent::InferiorExited {
                revision: self.revision,
                process_id,
                execution_id: execution,
                status: status.clone(),
            });

            if let Some(startup) = startup {
                startup.fail(backend_error(LinuxError::ExitedBeforeStop(status)));
            }

            if self
                .shutdown
                .as_ref()
                .is_some_and(|shutdown| shutdown.reply.is_some())
            {
                self.finish_shutdown(cleanup_result(cleanup_error.take()));
            }

            if let Some(reply) = kill {
                let _ = reply.send(cleanup_result(cleanup_error.take()));
            }

            self.session = match orphans {
                Some(orphans) => Session::Draining {
                    orphans,
                    deferred: None,
                    ended,
                    cleanup_error,
                },
                None => Session::Idle {
                    ended: Some(ended),
                    cleanup_error,
                },
            };
            self.settle_pending_edits(edits);
            return joined;
        }

        self.thread_exited(pid, status, exited.holds_siblings())
    }

    fn seize_threads_left(&mut self) -> Result<bool> {
        match self.seize_untraced_threads() {
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            seized => seized,
        }
    }

    fn thread_exited(&mut self, pid: Pid, status: ExitStatus, held_siblings: bool) -> Result<()> {
        let thread_id = debug_thread_id(pid);
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let process_id = process_id(inferior.tgid);
        let owned_execution = inferior.run.execution().is_some_and(|active| {
            matches!(active.scope, ResumeScope::Thread(thread) if thread == thread_id)
                || matches!(
                    active.kind,
                    ActiveKind::Step { owner, .. } | ActiveKind::RuntimeStep { owner, .. }
                        if owner.thread == pid
                )
        });

        let held_siblings = held_siblings && inferior.run.execution().is_some();
        inferior.forget_thread(pid, &status);
        let barrier_active = inferior.run.barrier().is_some();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::ThreadExited {
            revision: self.revision,
            process_id,
            thread_id,
            status: status.clone(),
        });

        if self.shutdown.is_some() {
            if barrier_active && self.inspector_cleanup_running() {
                return self.finish_barrier_if_ready();
            }
            return Ok(());
        }

        if barrier_active {
            if owned_execution
                && let Some(barrier) = self
                    .session
                    .inferior_mut()
                    .and_then(|inferior| inferior.run.barrier_mut())
            {
                barrier.end_execution(StopReason::ThreadExited { thread_id, status });
            }

            return self.finish_barrier_if_ready();
        }

        if owned_execution {
            let reason = StopReason::ThreadExited { thread_id, status };
            let inferior = self.session.inferior().ok_or(Error::NotRunning)?;

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

        if held_siblings {
            return self.advance_execution();
        }

        Ok(())
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn begin_shutdown(&mut self, reply: Option<Reply<()>>) {
        debug_assert!(
            self.shutdown.is_none(),
            "shutdown began twice, which drops the first shutdown's reply"
        );
        self.shutdown = Some(Shutdown { reply });

        match &mut self.session {
            Session::Idle { cleanup_error, .. } => {
                let outcome = cleanup_result(cleanup_error.take());
                self.finish_shutdown(outcome);
                return;
            }

            Session::Draining { deferred, .. } => {
                if let Some(start) = deferred.take() {
                    start.fail(Error::RequestCancelled);
                }
                return;
            }
            Session::Traced(traced) => {
                if let Some(startup) = traced.startup.take() {
                    startup.fail(Error::RequestCancelled);
                }
            }
        }

        let Some(inferior) = self.session.inferior() else {
            return;
        };

        if inferior.origin != InferiorOrigin::Attached && self.ending == super::Ending::Kill {
            if let Err(error) = self.kill_inferior() {
                self.finish_shutdown(Err(error));
            }

            return;
        }

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

    pub(super) fn finish_shutdown(&mut self, result: Result<()>) {
        if let Some(reply) = self
            .shutdown
            .as_mut()
            .and_then(|shutdown| shutdown.reply.take())
        {
            let _ = reply.send(result);
        }
    }

    pub(super) fn absorb_untracked_wait(&mut self, status: &WaitEvent) -> bool {
        let pid = status.pid();
        let Some(inferior) = self.session.inferior_mut() else {
            return false;
        };

        if inferior.threads.contains_key(&pid) {
            return false;
        }

        match status {
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => {
                match inferior.tracked.remove(&pid) {
                    Some(Tracked::Retired | Tracked::Killed | Tracked::ForkChild { .. }) => {}
                    None
                    | Some(
                        Tracked::Vanished
                        | Tracked::Unseized
                        | Tracked::EarlyStop(_)
                        | Tracked::HandedOver,
                    ) => {
                        inferior.tracked.insert(pid, Tracked::Vanished);
                    }
                }
            }
            WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_EXIT) => {
                let record = inferior.tracked.entry(pid).or_insert(Tracked::Vanished);
                if matches!(record, Tracked::EarlyStop(_)) {
                    *record = Tracked::Vanished;
                }
                return self.release_exiting_thread(pid).is_ok();
            }
            WaitEvent::Stopped(..) | WaitEvent::PtraceEvent(_, _, libc::PTRACE_EVENT_STOP) => {
                match inferior.tracked.remove(&pid) {
                    Some(Tracked::ForkChild { sites, vfork }) => {
                        self.settle_fork_child(pid, &sites, vfork);
                    }
                    None
                    | Some(
                        Tracked::Retired
                        | Tracked::Vanished
                        | Tracked::Unseized
                        | Tracked::EarlyStop(_)
                        | Tracked::Killed
                        | Tracked::HandedOver,
                    ) => {
                        inferior.tracked.insert(pid, Tracked::EarlyStop(*status));
                    }
                }
            }
            _ => return false,
        }

        true
    }

    fn settle_stopped_fork_children(&mut self) -> Option<Orphans> {
        let inferior = self.session.inferior_mut()?;

        let sites = inferior.inherited_sites();
        let mut stopped = Vec::new();
        let mut pending = BTreeMap::new();
        inferior.tracked.retain(|&pid, record| match record {
            Tracked::EarlyStop(_) => {
                stopped.push(pid);
                false
            }
            Tracked::ForkChild { sites, .. } => {
                pending.insert(pid, std::mem::take(sites));
                false
            }
            Tracked::Retired
            | Tracked::Vanished
            | Tracked::Unseized
            | Tracked::Killed
            | Tracked::HandedOver => true,
        });

        for pid in stopped {
            self.settle_fork_child(pid, &sites, false);
        }

        let inferior = self.session.inferior_mut()?;
        let killed = (inferior.tracked.iter())
            .filter(|(_, record)| **record == Tracked::Killed)
            .map(|(&pid, _)| pid)
            .collect();
        Some(Orphans {
            parent: inferior.tgid,
            children: pending,
            killed,
            waiter: inferior.waiter.take(),
        })
    }

    pub(super) fn handle_shutdown_wait(&mut self, status: WaitEvent) -> bool {
        let attached = self.session.inferior().is_some_and(|inferior| {
            inferior.origin == InferiorOrigin::Attached || self.ending == super::Ending::Release
        });
        if self.absorb_untracked_wait(&status) {
            if attached && let Err(error) = self.detach_when_stopped() {
                self.finish_shutdown(Err(error));
                return false;
            }

            return !self.session.is_idle();
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

        !self.session.is_idle()
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

                if let Some(thread) = self
                    .session
                    .inferior_mut()
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
                let stop = self.classify_stop(pid, signal);
                let requested = matches!(stop, ClassifiedStop::DebuggerRequested);
                let pending = match stop {
                    ClassifiedStop::SignalDelivery(pending) => Some(pending),
                    _ => None,
                };

                let inferior = self.session.inferior_mut().ok_or(Error::NotRunning);
                inferior.and_then(|inferior| {
                    let thread = inferior.thread_mut(pid)?;
                    thread.state = NativeThreadState::Stopped;
                    if requested {
                        thread.debugger_stop_pending = false;
                    }
                    if pending.is_some() {
                        thread.pending_signal = pending;
                    }

                    Ok(())
                })
            }

            WaitEvent::PtraceEvent(pid, _, _) | WaitEvent::PtraceSyscall(pid) => {
                let inferior = self.session.inferior_mut().ok_or(Error::NotRunning);
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

        !self.session.is_idle()
    }

    pub(super) fn detach_when_stopped(&mut self) -> Result<()> {
        let Some(inferior) = self.session.inferior() else {
            return Ok(());
        };

        if inferior.threads.is_empty() {
            let due = (inferior.tracked.values())
                .any(|record| matches!(record, Tracked::Retired | Tracked::ForkChild { .. }));
            if !due {
                self.forget_vanished_inferior();
            }

            return Ok(());
        }

        let ready = !(inferior.tracked.values())
            .any(|record| matches!(record, Tracked::ForkChild { .. }))
            && inferior
                .threads
                .iter()
                .all(|(&pid, thread)| inferior.settled(pid, thread));
        if ready {
            let drained = self
                .drain_requested_stops()
                .and_then(|stops| Ok(self.drain_queued_traps()? || stops));
            match drained {
                Ok(false) => match self.begin_inspector_cleanup() {
                    Ok(true) => {}
                    Ok(false) => self.detach_inferior(),
                    Err(error) => self.detach_inferior_reporting(Some(error)),
                },
                Ok(true) => {}
                Err(error) if self.sites_live() => self.detach_inferior_reporting(Some(error)),
                Err(error) => return Err(error),
            }
        }

        Ok(())
    }

    fn leave_for_watching(&mut self, pid: Pid, program: std::path::PathBuf) {
        let Some(Traced {
            mut inferior,
            startup,
            kill,
        }) = self.session.take_traced(Ended::Detached)
        else {
            return;
        };

        if let Some(startup) = startup {
            startup.fail(Error::RequestCancelled);
        }
        if let Some(reply) = kill {
            let _ = reply.send(Ok(()));
        }

        if let Some(waiter) = inferior.waiter.take() {
            match self.join_waiter(waiter) {
                Ok(()) => {}
                #[cfg_attr(
                    not(debug_assertions),
                    expect(unused_variables, reason = "only recorded")
                )]
                Err(error) => record!("the waiter did not stop: {error}"),
            }
        }

        let parent = self.group.as_ref().and_then(|group| group.parent);
        if let Some(following) = self.following() {
            following.demote(pid, parent, inferior.origin.owned(), program);
        }

        self.reset_runtime_modules();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorDetached {
            revision: self.revision,
            process_id: process_id(inferior.tgid),
        });
        self.settle_pending_edits(inferior.take_pending_edits());
    }

    fn forget_vanished_inferior(&mut self) {
        let Some(orphans) = self.settle_stopped_fork_children() else {
            return;
        };
        let Some(Traced {
            mut inferior,
            startup,
            kill,
        }) = self.session.take_traced(Ended::Detached)
        else {
            return;
        };
        debug_assert!(startup.is_none(), "a shutdown cancels the startup");

        let outcome = if orphans.children.is_empty() && orphans.killed.is_empty() {
            (orphans.waiter).map_or(Ok(()), |waiter| self.join_waiter(waiter))
        } else {
            self.session = Session::Draining {
                orphans,
                deferred: None,
                ended: Ended::Detached(process_id(inferior.tgid)),
                cleanup_error: None,
            };
            Ok(())
        };

        self.reset_runtime_modules();
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::InferiorDetached {
            revision: self.revision,
            process_id: process_id(inferior.tgid),
        });
        self.settle_pending_edits(inferior.take_pending_edits());
        answer_overtaken_kill(kill);
        self.finish_shutdown(outcome);
    }

    fn handle_fork_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };

        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        let sites = inferior.inherited_sites();
        self.track_fork_child(child, sites, false);
        Ok(())
    }

    pub(super) fn handle_vfork_event(&mut self, parent: Pid) -> Result<()> {
        let Some(child) = self.event_message_of(parent)? else {
            return self.handle_classified_stop(parent, ClassifiedStop::Superseded);
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
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

    pub(super) fn handle_vfork_done(&mut self, parent: Pid) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        if inferior.vfork_windows.remove(&parent).is_some() {
            self.close_vfork_windows_if_empty()?;
        }

        if !self.barrier_active() {
            self.begin_internal_stop()?;
        }

        self.restart_after_internal(parent)
    }

    pub(super) fn end_vfork_window_of(&mut self, thread: Pid) -> Result<()> {
        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(());
        };

        if inferior.vfork_windows.remove(&thread).is_some() {
            self.close_vfork_windows_if_empty()?;
        }

        Ok(())
    }

    fn close_vfork_windows_if_empty(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        if inferior.vfork_windows.is_empty() {
            reinstall_all_traps(&self.ptrace, inferior)?;
        }

        Ok(())
    }

    pub(super) fn settle_vfork_windows(&mut self) -> Result<bool> {
        let Some(inferior) = self.session.inferior_mut() else {
            return Ok(false);
        };

        if inferior.vfork_windows.is_empty() {
            return Ok(false);
        }

        if inferior.vfork_windows.values().all(|window| window.resumed) {
            return Ok(true);
        }

        self.restore_active_breakpoints()?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        lift_all_traps(&self.ptrace, inferior)?;

        let unresumed: Vec<(Pid, VforkWindow)> = inferior
            .vfork_windows
            .iter()
            .filter(|(_, window)| !window.resumed)
            .map(|(&parent, &window)| (parent, window))
            .collect();
        for (parent, window) in unresumed {
            let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
            debug_assert!(
                inferior
                    .breakpoints
                    .values()
                    .all(|site| site.trap == Trap::Lifted(Lift::Vfork)),
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

            self.track_fork_child(window.child, Vec::new(), true);
            self.continue_thread(parent)?;
        }

        Ok(true)
    }

    fn handle_vfork_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };

        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;
        lift_all_traps(&self.ptrace, inferior)?;
        self.track_fork_child(child, Vec::new(), true);
        Ok(())
    }

    pub(super) fn handle_clone_during_detach(&mut self, parent: Pid) -> Result<()> {
        let event = self.event_message_of(parent)?;
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;

        let Some(child) = event else {
            inferior.thread_mut(parent)?.state = NativeThreadState::Running;
            return Ok(());
        };

        inferior.thread_mut(parent)?.state = NativeThreadState::Stopped;

        let stopped = matches!(inferior.tracked.remove(&child), Some(Tracked::EarlyStop(_)));
        let thread = inferior
            .threads
            .entry(child)
            .or_insert_with(|| TraceThread::starting(ExpectedStop::None));
        if stopped {
            thread.state = NativeThreadState::Stopped;
        }

        Ok(())
    }

    pub(super) fn detach_inferior(&mut self) {
        self.detach_inferior_reporting(None);
    }

    pub(super) fn detach_inferior_reporting(&mut self, mut first_error: Option<Error>) {
        debug_assert!(
            self.session.inferior().is_some_and(|inferior| {
                inferior
                    .threads
                    .iter()
                    .all(|(&pid, thread)| inferior.settled(pid, thread))
            }),
            "detaching an inferior with a thread that is not stopped"
        );

        if let Err(reason) = self.prepare_agents_for_detach() {
            first_error.get_or_insert(Error::RuntimeAgent(reason));
        }
        if let Err(error) = self.disarm_for_detach()
            && !is_vanished_tracee(&error)
        {
            first_error.get_or_insert(error);
        }
        let Some(Traced {
            mut inferior,
            startup,
            kill,
        }) = self.session.take_traced(Ended::Detached)
        else {
            self.finish_shutdown(first_error.map_or(Err(Error::NotRunning), Err));
            return;
        };
        debug_assert!(startup.is_none(), "a shutdown cancels the startup");

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
            .filter_map(|(&address, site)| (site.trap == Trap::Installed).then_some(address))
            .collect::<Vec<_>>();
        let pid = inferior.memory_thread();
        for address in installed {
            record(
                self.ptrace
                    .remove_breakpoint(pid, &mut inferior.breakpoints, address)
                    .map(drop),
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
        answer_overtaken_kill(kill);
        self.finish_shutdown(first_error.map_or(Ok(()), Err));
    }

    pub(super) fn kill(&mut self, reply: Reply<()>) {
        match self.session.traced() {
            None => {
                let _ = reply.send(Err(Error::NotRunning));
                return;
            }
            Some(Traced { kill: Some(_), .. }) => {
                let _ = reply.send(Err(Error::RequestCancelled));
                return;
            }
            Some(_) => {}
        }

        match self.kill_inferior() {
            Ok(()) => {
                if let Some(traced) = self.session.traced_mut() {
                    traced.kill = Some(reply);
                }
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    pub(super) fn terminate(&mut self) -> Result<()> {
        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let signal = Signal::SIGTERM;
        inferior.terminating = Some(Terminating {
            signal,
            delivered: false,
        });
        let (tgid, stop) = (inferior.tgid, inferior.run.stop().map(|stop| stop.id));
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
        let Some(inferior) = self.session.inferior() else {
            return Ok(());
        };

        self.ptrace.kill(inferior.tgid, Signal::SIGKILL)
    }

    pub(super) fn fail_inferior(&mut self, error: Error) {
        record!("inferior failed: {error}");
        if let Some(reply) = self.session.take_launch() {
            let _ = reply.send(Err(error));
        } else if let Some(reply) = self.session.take_attach() {
            let _ = reply.send(Err(error));
            self.begin_shutdown(None);
            return;
        }

        if self
            .session
            .inferior()
            .is_some_and(|inferior| inferior.origin == InferiorOrigin::Attached)
        {
            if self.shutdown.is_none() {
                self.begin_shutdown(None);
            }
        } else {
            let _ = self.kill_inferior();
        }
    }
}

impl Start {
    fn fail(self, error: Error) {
        match self {
            Self::Launch(_, reply)
            | Self::LaunchByExec { reply, .. }
            | Self::AdoptExec { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Attach { reply, .. } | Self::Adopt { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

fn cleanup_result(cleanup_error: Option<Arc<str>>) -> Result<()> {
    cleanup_error.map_or(Ok(()), |reason| Err(Error::RuntimeAgent(reason)))
}

fn answer_overtaken_kill(kill: Option<Reply<()>>) {
    if let Some(reply) = kill {
        let _ = reply.send(Ok(()));
    }
}

pub(super) fn requested_pid(requested: ProcessId) -> Result<Pid> {
    i32::try_from(requested.get())
        .ok()
        .filter(|raw| *raw > 0)
        .map(Pid::from_raw)
        .ok_or(Error::InvalidProcessId(requested.get()))
}
