use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use nix::unistd::Pid;

use crate::protocol::{ExceptionStops, LanguageException, StopReason};
use crate::runtime_model::{HookEvent, HookPlace, RuntimeHook};
use crate::{Error, LanguageExceptionKind, Result, RuntimeId, VirtualAddress};

use super::breakpoints::remove_breakpoint_owner_from;
use super::frames::StackRoot;
use super::native::{InspectionOps, LinuxTraceOps};
use super::registers::x86_64_registers;
use super::{BreakpointOwner, Controller, debug_thread_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HookSite {
    runtime: RuntimeId,
    purpose: HookPurpose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookPurpose {
    Exception(RuntimeHook),

    Internal(HookPlace),
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn set_exception_stops(&mut self, stops: ExceptionStops) -> Result<ExceptionStops> {
        let previous = std::mem::replace(&mut self.exception_stops, stops);
        if self.sites_live() {
            self.sync_runtime_hooks()?;
            if previous != stops {
                self.rearm_reporting_runtimes();
            }
        }

        Ok(previous)
    }

    pub(super) fn sync_runtime_hooks(&mut self) -> Result<()> {
        let Some(inferior) = self.session.inferior() else {
            return Ok(());
        };

        let runtimes = self.runtimes(inferior);
        self.signals.set_runtime_handled(
            runtimes
                .iter()
                .flat_map(|runtime| runtime.model.signals().handled.iter().copied()),
        );
        if inferior.observer {
            return Ok(());
        }
        let stops = self.exception_stops;
        let wanted = runtimes
            .iter()
            .flat_map(|runtime| {
                let exceptions = runtime
                    .model
                    .hooks()
                    .iter()
                    .filter(|hook| stops.stops(hook.filter.id))
                    .filter_map(|hook| {
                        let address = runtime.module.virtual_address(hook.address).ok()?;
                        Some((address, HookPurpose::Exception(*hook)))
                    });
                let internal = runtime
                    .model
                    .internal_hooks()
                    .iter()
                    .copied()
                    .chain(self.agent_entry_hooks(runtime))
                    .flat_map(|hook| {
                        let addresses = match hook {
                            HookPlace::Own(address) => runtime
                                .module
                                .virtual_address(address)
                                .ok()
                                .into_iter()
                                .collect(),
                            HookPlace::Companion(name) => {
                                self.exported_functions(inferior, runtime.id, name)
                            }
                        };
                        addresses
                            .into_iter()
                            .map(move |address| (address, HookPurpose::Internal(hook)))
                    });

                exceptions.chain(internal).map(|(address, purpose)| {
                    (
                        address,
                        HookSite {
                            runtime: runtime.id,
                            purpose,
                        },
                    )
                })
            })
            .collect::<BTreeMap<_, _>>();

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let stale = inferior
            .runtime_hooks
            .iter()
            .filter(|(address, site)| wanted.get(address) != Some(site))
            .map(|(address, _)| *address)
            .collect::<Vec<_>>();

        for address in stale {
            remove_breakpoint_owner_from(
                &self.ptrace,
                inferior,
                address,
                BreakpointOwner::Runtime,
            )?;
            inferior.runtime_hooks.remove(&address);
        }

        let pid = inferior.memory_thread();

        for (address, site) in wanted {
            if inferior.runtime_hooks.contains_key(&address) {
                continue;
            }

            self.ptrace.install_breakpoint(
                pid,
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::Runtime,
            )?;
            inferior.runtime_hooks.insert(address, site);
        }

        Ok(())
    }

    fn exported_functions(
        &self,
        inferior: &super::Inferior,
        runtime: RuntimeId,
        name: &str,
    ) -> Vec<VirtualAddress> {
        self.loaded_images(inferior)
            .filter_map(|(module, image)| {
                if !Self::agent_companion_matches(inferior, runtime, image) {
                    return None;
                }
                let symbol = image.symbols_named(name).find(|symbol| {
                    symbol.exported() && symbol.kind() == crate::SymbolKind::Function
                })?;
                module.virtual_address(symbol.address()).ok()
            })
            .collect()
    }

    pub(super) fn runtime_hook_stop(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Option<StopReason> {
        let inferior = self.session.inferior()?;
        let site = *inferior.runtime_hooks.get(&address)?;

        match site.purpose {
            HookPurpose::Exception(hook) => {
                let kind = hook.filter.kind;
                if !self.exception_stops.stops(hook.filter.id) {
                    return None;
                }

                let (message, value) =
                    match self.read_hook(pid, site.runtime, |model, stop, registers| {
                        model.exception(stop, hook.address, registers)
                    }) {
                        Ok(report) => (report.message, report.value),
                        Err(reason) => (
                            format!("{}; its message is unreadable: {reason}", describe(kind))
                                .into(),
                            None,
                        ),
                    };

                Some(StopReason::LanguageException(LanguageException {
                    kind,
                    message,
                    value,
                }))
            }
            HookPurpose::Internal(hook) => {
                let event = self.read_hook(pid, site.runtime, |model, stop, registers| {
                    model.hook_event(stop, hook, registers)
                });

                match event {
                    Ok(HookEvent::Continue) => None,
                    Ok(HookEvent::AgentEntry) => self
                        .agent_entry(site.runtime)
                        .err()
                        .map(|description| StopReason::RuntimeEvent { description }),
                    Ok(HookEvent::Armed { generation }) => self
                        .queue_agent_report(site.runtime, generation, None)
                        .err()
                        .map(|description| StopReason::RuntimeEvent { description }),
                    Ok(HookEvent::AgentFailed {
                        generation,
                        description,
                    }) => match self.queue_agent_report(
                        site.runtime,
                        generation,
                        Some(Arc::clone(&description)),
                    ) {
                        Ok(true) => Some(StopReason::RuntimeEvent { description }),
                        Ok(false) => None,
                        Err(cleanup) => Some(StopReason::RuntimeEvent {
                            description: format!("{description}; source cleanup failed: {cleanup}")
                                .into(),
                        }),
                    },
                    Ok(HookEvent::Exception { filter, report }) => self
                        .exception_stops
                        .stops(filter.id)
                        .then_some(StopReason::LanguageException(LanguageException {
                            kind: filter.kind,
                            message: report.message,
                            value: report.value,
                        })),
                    Ok(HookEvent::Breakpoint { numbers }) => {
                        self.managed_hit(pid, site.runtime, &numbers)
                    }
                    Ok(HookEvent::StepEnded { number }) => {
                        self.runtime_step_end(pid, site.runtime, number, None)
                    }
                    Ok(HookEvent::StepIncomplete {
                        number,
                        description,
                    }) => self.runtime_step_end(pid, site.runtime, number, Some(description)),
                    Ok(HookEvent::Assigned { number, failure }) => {
                        self.runtime_assignment_end(pid, site.runtime, number, failure)
                    }
                    Ok(HookEvent::StepCalls { number, target }) => {
                        let _ = self.step_into_native(pid, site.runtime, number, target);
                        None
                    }
                    Err(description) => Some(StopReason::RuntimeEvent { description }),
                }
            }
        }
    }

    pub(super) fn reported_frame(
        &self,
        stop_id: crate::protocol::StopId,
        pid: Pid,
    ) -> Option<crate::StackFrameId> {
        let inferior = self.session.inferior()?;
        if !self.inspector_holds(debug_thread_id(pid)) {
            self.agent_hook(pid)?;
        }
        let trace = self.backtrace(stop_id, &StackRoot::of_thread(pid)).ok()?;

        self.blamed_frame(inferior, &trace)
    }

    pub(super) fn agent_hook(&self, pid: Pid) -> Option<&HookSite> {
        let registers = self.ptrace.registers(pid).ok()?;
        self.session
            .inferior()?
            .runtime_hooks
            .get(&VirtualAddress::new(registers.rip))
            .filter(|site| matches!(site.purpose, HookPurpose::Internal(HookPlace::Companion(_))))
    }

    pub(super) fn at_agent_hook(&self, pid: Pid, runtime: RuntimeId) -> bool {
        self.agent_hook(pid)
            .is_some_and(|site| site.runtime == runtime)
    }

    fn managed_hit(&mut self, pid: Pid, runtime: RuntimeId, numbers: &[u64]) -> Option<StopReason> {
        let managed = &self.session.inferior()?.managed;
        let owners: BTreeSet<_> = numbers
            .iter()
            .filter_map(|&number| managed.site(runtime, number))
            .flat_map(|(_, owners)| owners.iter().copied())
            .collect();
        if owners.is_empty() {
            return None;
        }
        let hits = self.judge_hits(pid, &owners, None);

        (!hits.is_empty()).then_some(StopReason::Breakpoint {
            address: None,
            hits,
        })
    }

    pub(super) fn read_hook<T>(
        &self,
        pid: Pid,
        runtime: RuntimeId,
        read: impl FnOnce(
            &dyn crate::runtime_model::RuntimeModel,
            &dyn crate::runtime_model::RuntimeStop,
            &crate::unwind::RegisterFile,
        ) -> std::result::Result<T, Arc<str>>,
    ) -> std::result::Result<T, Arc<str>> {
        let inferior = self
            .session
            .inferior()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let runtime = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or_else(|| Arc::<str>::from("the runtime is no longer loaded"))?;
        let registers = self
            .ptrace
            .registers(pid)
            .map_err(|error| Arc::<str>::from(error.to_string()))?;
        let registers = x86_64_registers(&registers);

        self.with_runtime_stop(inferior, &runtime, pid, |stop| {
            read(runtime.model.as_ref(), stop, &registers)
        })
    }

    pub(super) fn select_interpreted_frame(&mut self, pid: Pid, frame: crate::VirtualAddress) {
        let Some(stop) = self
            .session
            .inferior()
            .and_then(|inferior| inferior.run.stop())
        else {
            return;
        };
        let (stop_id, context) = (stop.id, stop.selected);
        let Ok(trace) = self.backtrace(stop_id, &StackRoot::of_thread(pid)) else {
            return;
        };

        let Some(id) = trace
            .frames
            .iter()
            .find(|candidate| candidate.kind == crate::FrameKind::Interpreted { frame })
            .map(|candidate| candidate.id)
        else {
            return;
        };

        if let Some(stop) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.stop_mut())
        {
            stop.selected_frames.insert(context, id);
        }
    }

    pub(super) fn select_blamed_frame(&mut self, pid: Pid) {
        let Some(inferior) = self.session.inferior() else {
            return;
        };

        let Some(stop) = inferior.run.stop() else {
            return;
        };

        let (stop_id, context) = (stop.id, stop.selected);
        let Ok(trace) = self.backtrace(stop_id, &StackRoot::of_thread(pid)) else {
            return;
        };

        let Some(frame) = self.blamed_frame(inferior, &trace) else {
            return;
        };

        debug_assert_eq!(
            context,
            crate::ExecutionContext::Thread(debug_thread_id(pid))
        );
        if let Some(stop) = self
            .session
            .inferior_mut()
            .and_then(|inferior| inferior.run.stop_mut())
        {
            stop.selected_frames.insert(context, frame);
        }
    }
}

impl<P: InspectionOps> Controller<P> {
    pub(super) fn blamed_frame(
        &self,
        inferior: &super::Inferior,
        trace: &crate::Backtrace,
    ) -> Option<crate::StackFrameId> {
        if let crate::ExecutionContext::Thread(thread) = trace.context
            && self.inspector_holds(thread)
        {
            return trace
                .frames
                .iter()
                .find(|frame| super::inspector::is_script_frame(frame))
                .map(|frame| frame.id);
        }

        let runtime = trace
            .frames
            .first()
            .and_then(|frame| inferior.runtime_hooks.get(&frame.instruction?))
            .and_then(|site| {
                self.runtimes(inferior)
                    .into_iter()
                    .find(|runtime| runtime.id == site.runtime)
            });
        let own = |frame: &crate::StackFrame| {
            let (Some(runtime), Some(module), Some(source)) =
                (&runtime, frame.module, &frame.source)
            else {
                return false;
            };

            self.modules
                .get(&module)
                .and_then(|module| module.image.source_file(source.file))
                .is_some_and(|file| runtime.model.own_source(&file.path))
        };

        trace
            .frames
            .iter()
            .find(|frame| frame.role == crate::CodeRole::Ordinary && !own(frame))
            .map(|frame| frame.id)
    }
}

const fn describe(kind: LanguageExceptionKind) -> &'static str {
    match kind {
        LanguageExceptionKind::Raised => "an exception was raised",
        LanguageExceptionKind::Unhandled => "an exception was not handled",
        LanguageExceptionKind::Fatal => "the runtime failed",
    }
}
