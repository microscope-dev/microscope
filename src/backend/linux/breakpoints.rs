//! Logical breakpoints and the software trap sites that implement them.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use nix::unistd::Pid;

use super::signals::Signal;

use crate::protocol::{
    Breakpoint, BreakpointHit, BreakpointId, BreakpointSpec, ConditionOwner, DebuggerEvent,
    ExecutionId, HitCondition, ResolvedBreakpointLocation, StopReason,
};

use crate::{
    BreakpointLocation, Error, LineNumber, ManagedLocation, Result, TrapAddress, VirtualAddress,
};

use super::memory::MemoryAccessError;
use super::native::{InspectionOps, LinuxTraceOps, is_vanished_tracee};
use super::{BREAKPOINT_OPCODE, BreakpointOwner, Controller, Inferior, LinuxError, backend_error};

/// The logical breakpoints clients requested, in creation order, counted by
/// spec so that adding one need not compare it with every other. A spec
/// never changes once added; everything else is edited in place.
#[derive(Debug, Default)]
pub(super) struct UserBreakpoints {
    list: Vec<Breakpoint>,
    specs: HashMap<BreakpointSpec, usize>,
}

impl UserBreakpoints {
    pub(super) fn push(&mut self, breakpoint: Breakpoint) {
        *self.specs.entry(breakpoint.spec.clone()).or_default() += 1;
        self.list.push(breakpoint);
    }

    pub(super) fn remove(&mut self, index: usize) -> Breakpoint {
        let breakpoint = self.list.remove(index);
        if let Some(count) = self.specs.get_mut(&breakpoint.spec) {
            *count -= 1;
            if *count == 0 {
                self.specs.remove(&breakpoint.spec);
            }
        }

        breakpoint
    }

    pub(super) fn take(&mut self) -> Vec<Breakpoint> {
        self.specs.clear();
        std::mem::take(&mut self.list)
    }

    /// Panics if the spec counts disagree with the list. The message names
    /// no spec, which holds the user's text.
    #[expect(
        clippy::disallowed_methods,
        reason = "a sum does not depend on the order of its terms"
    )]
    pub(super) fn check_invariants(&self) {
        debug_assert_eq!(
            self.specs.values().sum::<usize>(),
            self.list.len(),
            "the spec counts cover a different number of breakpoints than are listed"
        );
        for breakpoint in &self.list {
            debug_assert!(
                self.specs
                    .get(&breakpoint.spec)
                    .is_some_and(|&count| count > 0),
                "breakpoint {} has a spec the counts do not include",
                breakpoint.id
            );
        }
    }

    /// The breakpoint that a request with this spec and these options would
    /// duplicate.
    fn identical(
        &self,
        spec: &BreakpointSpec,
        options: &crate::BreakpointOptions,
    ) -> Option<&Breakpoint> {
        if !self.specs.contains_key(spec) {
            return None;
        }

        self.list.iter().find(|breakpoint| {
            breakpoint.spec == *spec
                && breakpoint.hit_condition == options.hit_condition
                && breakpoint.condition == options.condition
                && breakpoint.log_message == options.log_message
                && breakpoint.enabled == options.enabled
                && breakpoint.temporary == options.temporary
        })
    }
}

impl Deref for UserBreakpoints {
    type Target = [Breakpoint];

    fn deref(&self) -> &[Breakpoint] {
        &self.list
    }
}

impl DerefMut for UserBreakpoints {
    fn deref_mut(&mut self) -> &mut [Breakpoint] {
        &mut self.list
    }
}

impl<'a> IntoIterator for &'a UserBreakpoints {
    type Item = &'a Breakpoint;
    type IntoIter = std::slice::Iter<'a, Breakpoint>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}

impl<'a> IntoIterator for &'a mut UserBreakpoints {
    type Item = &'a mut Breakpoint;
    type IntoIter = std::slice::IterMut<'a, Breakpoint>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter_mut()
    }
}

impl<P: InspectionOps> Controller<P> {
    /// Public snapshots retain resolution intent while reporting why a
    /// runtime could not accept it. No target read is needed here.
    pub(super) fn describe_breakpoint(&self, breakpoint: &Breakpoint) -> Breakpoint {
        let mut shown = breakpoint.clone();
        let pending = self
            .inferior
            .as_ref()
            .filter(|_| breakpoint.enabled)
            .map_or_else(Vec::new, |inferior| {
                breakpoint
                    .locations
                    .iter()
                    .filter_map(|resolved| {
                        let BreakpointLocation::Managed(location) = &resolved.location else {
                            return None;
                        };
                        // A site the runtime's agent acknowledged stays
                        // armed while it confirms a change that keeps it.
                        let confirmed = inferior.managed.number(location).is_some_and(|number| {
                            inferior
                                .agents
                                .get(&location.runtime)
                                .is_some_and(|agent| agent.confirms(number))
                        });
                        if confirmed {
                            return None;
                        }
                        // An inspector says of each location whether V8 set it.
                        if let Some(readiness) = inferior
                            .managed
                            .number(location)
                            .and_then(|number| self.inspector_readiness(location.runtime, number))
                        {
                            return readiness.err().map(|reason| {
                                crate::PendingBreakpointLocation {
                                    location: resolved.location.clone(),
                                    reason,
                                }
                            });
                        }
                        // A change not yet armed, as after an edit that
                        // failed between claiming and arming, is unconfirmed.
                        let reason = match inferior.managed.unarmed.get(&location.runtime) {
                            Some(reason) => Arc::clone(reason),
                            None if inferior.managed.is_stale(location.runtime) => {
                                "the runtime has not yet been armed with this location".into()
                            }
                            None => return None,
                        };
                        Some(crate::PendingBreakpointLocation {
                            location: resolved.location.clone(),
                            reason,
                        })
                    })
                    .collect::<Vec<_>>()
            });

        if shown.pending_locations.as_ref() != pending.as_slice() {
            shown.pending_locations = pending.into();
        }
        shown
    }

    pub(super) fn breakpoint_snapshot(&self) -> Arc<[Breakpoint]> {
        self.breakpoints
            .iter()
            .map(|breakpoint| self.describe_breakpoint(breakpoint))
            .collect()
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Adds a logical breakpoint. Its traps are installed at once when the
    /// inferior's sites are live, which requires every thread to be stopped;
    /// a launching or attaching inferior installs them at its first stop.
    pub(super) fn add_breakpoint(
        &mut self,
        spec: BreakpointSpec,
        options: crate::BreakpointOptions,
    ) -> Result<Breakpoint> {
        let spec = self.in_stopped_scope(spec);
        if let Some(existing) = self.breakpoints.identical(&spec, &options) {
            return Ok(self.describe_breakpoint(existing));
        }

        // Only a function breakpoint can wait on an indirect function's
        // resolver.
        let follows_resolvers = self.follows_resolvers(&spec);
        if follows_resolvers {
            self.learn_from_got(Some(&spec));
        }

        let id = BreakpointId::new(self.next_breakpoint_id);
        let next_id = self
            .next_breakpoint_id
            .checked_add(1)
            .ok_or_else(|| backend_error(LinuxError::BreakpointIdExhausted))?;
        let breakpoint = Breakpoint {
            hit_condition: options.hit_condition,
            condition: options.condition,
            log_message: options.log_message,
            enabled: options.enabled,
            temporary: options.temporary,
            ..self.resolve_breakpoint(id, spec, options.pending)?
        };

        if breakpoint.enabled && self.sites_live() {
            let inferior = self
                .inferior
                .as_mut()
                .ok_or_else(|| Error::internal("live sites have an inferior"))?;
            install_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
            self.step_over_where_threads_trapped(&breakpoint)?;
        }

        self.next_breakpoint_id = next_id;
        self.breakpoints.push(breakpoint.clone());
        if follows_resolvers {
            self.sync_resolvers()?;
        }

        let _ = self.sync_runtime_arming();
        self.publish_breakpoints_changed();
        Ok(self.describe_breakpoint(&breakpoint))
    }

    /// Reads a function location at a stop as the source of the selected
    /// frame's package reads it: qualified by that package, when the
    /// package has a function of the name and nothing else in its image has
    /// the whole name or a qualified one it spells. The breakpoint keeps
    /// the qualified name, so it resolves the same way whatever stops later.
    fn in_stopped_scope(&self, spec: BreakpointSpec) -> BreakpointSpec {
        match spec {
            BreakpointSpec::Function(name) => {
                BreakpointSpec::Function(self.location_in_stopped_package(&name).unwrap_or(name))
            }
            BreakpointSpec::FileFunction { path, function } => BreakpointSpec::FileFunction {
                function: self
                    .location_in_stopped_package(&function)
                    .unwrap_or(function),
                path,
            },
            spec @ (BreakpointSpec::Source { .. } | BreakpointSpec::Address(_)) => spec,
        }
    }

    fn location_in_stopped_package(&self, location: &str) -> Option<String> {
        let inferior = self.inferior.as_ref()?;
        let stop = inferior.public_stop.as_ref()?;
        let root = self.stack_root(stop.id, stop.selected).ok()?;
        let frame = self
            .resolve_frame(inferior, &root, super::selected_frame(stop))
            .ok()?
            .frame?;
        let (function, module) = (frame.function?, frame.module?);
        let modules = self.unwind_modules(inferior);
        let image = modules
            .iter()
            .find(|candidate| candidate.loaded.id == module)?
            .image;
        image.location_in_package(location, image.function_package(function.id)?)
    }

    pub(super) fn resolve_breakpoint(
        &self,
        id: BreakpointId,
        spec: BreakpointSpec,
        pending: bool,
    ) -> Result<Breakpoint> {
        let locations = if let BreakpointSpec::Address(address) = &spec {
            Arc::from([ResolvedBreakpointLocation {
                location: BreakpointLocation::Virtual(*address),
                code_instances: Arc::from([]),
                library: None,
            }])
        } else {
            self.resolve_by_location(&spec, pending)?
        };

        Ok(Breakpoint {
            id,
            spec,
            locations,
            pending_locations: Arc::from([]),
            hit_condition: None,
            condition: None,
            log_message: None,
            hit_count: 0,
            enabled: true,
            temporary: false,
        })
    }

    /// Resolves a function or source spec by every resolver: in the
    /// program and every loaded library, and by each runtime that runs code
    /// it names itself. A spec that resolves nowhere is pending when
    /// `pending` asks, as is one naming a function the program imports.
    fn resolve_by_location(
        &self,
        spec: &BreakpointSpec,
        pending: bool,
    ) -> Result<Arc<[ResolvedBreakpointLocation]>> {
        let (managed, refusal) = self.resolve_managed(spec);
        match self.resolve_in_modules(spec) {
            Ok(locations) => Ok(locations.iter().cloned().chain(managed).collect()),

            // Code a runtime runs itself has no native location.
            Err(_) if !managed.is_empty() => Ok(managed.into()),
            Err(Error::FunctionNotFound(_) | Error::SourceFileNotFound(_)) if pending => {
                Ok(Arc::from([]))
            }
            Err(Error::FunctionNotFound(_) | Error::SourceFileNotFound(_))
                if let Some(reason) = refusal =>
            {
                Err(Error::RuntimeBreakpoint(reason))
            }

            // A function the program imports is in a library yet to load.
            Err(Error::FunctionNotFound(name)) if self.imports_function(&name) => Ok(Arc::from([])),
            Err(error) => Err(error),
        }
    }

    /// Resolves a function or source spec in the program and every loaded
    /// shared library. The spec fails only when no module has code for it;
    /// a module that does not know the name or file contributes nothing.
    pub(super) fn resolve_in_modules(
        &self,
        spec: &BreakpointSpec,
    ) -> Result<Arc<[ResolvedBreakpointLocation]>> {
        let mut locations = Vec::new();
        let mut failure = None;

        // Whether a module knows the spec but has no code for it yet, as
        // one whose indirect function's resolver has yet to choose.
        let mut known = false;
        let main = (self.main.id, &*self.main.image, None);
        let libraries = self
            .modules
            .values()
            .filter(|module| module.loaded.id != self.main.id)
            .map(|module| (module.loaded.id, &*module.image, Some(module.loaded)));
        for (module, image, loaded) in std::iter::once(main).chain(libraries) {
            let chosen = |resolver| self.chosen_implementation(module, resolver);
            match resolve_in_image(image, spec, &chosen) {
                Ok(addresses) => {
                    known = true;
                    for (address, code_instances) in addresses {
                        let location = match loaded {
                            None => BreakpointLocation::Image(address),
                            // A copy of part of an image holds only the
                            // locations in what it copied.
                            Some(loaded) => match loaded.virtual_address(address) {
                                Ok(address) => BreakpointLocation::Virtual(address),
                                Err(Error::AddressOutsideModule) => continue,
                                Err(error) => return Err(error),
                            },
                        };
                        locations.push(ResolvedBreakpointLocation {
                            location,
                            code_instances,
                            library: loaded.map(|_| module),
                        });
                    }
                }

                // A module without the name or file is not a failure, but
                // one with the file and no code at the line is, unless
                // another module has code for it.
                Err(error @ (Error::FunctionNotFound(_) | Error::SourceFileNotFound(_))) => {
                    failure.get_or_insert(error);
                }
                Err(error) => {
                    if failure.as_ref().is_none_or(|failure| {
                        matches!(
                            failure,
                            Error::FunctionNotFound(_) | Error::SourceFileNotFound(_)
                        )
                    }) {
                        failure = Some(error);
                    }
                }
            }
        }

        if locations.is_empty() && !known {
            return Err(
                failure.unwrap_or_else(|| Error::internal("the program itself was searched"))
            );
        }

        Ok(locations.into())
    }

    /// The locations that each loaded runtime that runs code `spec` names
    /// manages, and why a runtime that cannot stop there cannot, if one
    /// cannot.
    fn resolve_managed(
        &self,
        spec: &BreakpointSpec,
    ) -> (Vec<ResolvedBreakpointLocation>, Option<Arc<str>>) {
        let mut locations = Vec::new();
        let mut refusal = None;
        let Some(inferior) = self.inferior.as_ref() else {
            return (locations, refusal);
        };

        for runtime in self.runtimes(inferior) {
            let resolved =
                self.with_runtime_stop(inferior, &runtime, inferior.memory_thread(), |stop| {
                    runtime.model.resolve_breakpoint(stop, spec)
                });
            match resolved {
                None => {}
                Some(Ok(places)) => {
                    locations.extend(places.into_iter().map(|place| {
                        ResolvedBreakpointLocation {
                            location: BreakpointLocation::Managed(ManagedLocation {
                                runtime: runtime.id,
                                place,
                            }),
                            code_instances: Arc::from([]),
                            library: (runtime.module.id != inferior.loaded_module.id)
                                .then_some(runtime.module.id),
                        }
                    }));
                }
                Some(Err(reason)) => {
                    refusal.get_or_insert(reason);
                }
            }
        }

        (locations, refusal)
    }

    /// Whether a breakpoint's spec can name an indirect function, whose
    /// resolver's entries and returns follow the breakpoint once sites are
    /// live.
    fn follows_resolvers(&self, spec: &BreakpointSpec) -> bool {
        self.sites_live() && matches!(spec, BreakpointSpec::Function(_))
    }

    /// Whether the program or a loaded module imports a function of the
    /// name.
    fn imports_function(&self, name: &str) -> bool {
        self.main.image.imports_function(name)
            || self
                .modules
                .values()
                .any(|module| module.image.imports_function(name))
    }

    /// A thread that reported a trap at a site steps over the trap there
    /// when it moves on, whichever breakpoint owns the site by then: its
    /// arrival there was counted. Removing the breakpoint it hit forgets the
    /// step, so one added there again before the thread moves restores it.
    /// Any other thread standing at the new breakpoint arrives there when it
    /// resumes, as one does after a step that ends at a breakpoint.
    fn step_over_where_threads_trapped(&mut self, breakpoint: &Breakpoint) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        let sites = trap_addresses(inferior, breakpoint)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        for thread in inferior.threads.values_mut() {
            if thread.stopped_at_breakpoint.is_none()
                && let Some(address) = thread.trapped_at.filter(|address| sites.contains(address))
            {
                thread.stopped_at_breakpoint = Some(address);
            }
        }

        Ok(())
    }

    pub(super) fn remove_breakpoint(&mut self, id: BreakpointId) -> Result<Breakpoint> {
        let index = self
            .breakpoints
            .iter()
            .position(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        let breakpoint = self.breakpoints[index].clone();
        if breakpoint.enabled && self.sites_live() {
            let inferior = self
                .inferior
                .as_mut()
                .ok_or_else(|| Error::internal("live sites have an inferior"))?;
            remove_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
        }

        self.breakpoints.remove(index);
        if self.follows_resolvers(&breakpoint.spec) {
            self.sync_resolvers()?;
        }

        let _ = self.sync_runtime_arming();
        self.publish_breakpoints_changed();
        // This definition is retired, rather than waiting for arming.
        let mut breakpoint = breakpoint;
        breakpoint.pending_locations = Arc::from([]);
        Ok(breakpoint)
    }

    /// Enables or disables a breakpoint. Disabling releases its sites as
    /// removing it does, keeping the locations it last resolved to.
    /// Enabling resolves its spec again, as adding does: code moved or
    /// unloaded while it owned no sites never updated those locations, and
    /// a trap written there could land in memory that now holds something
    /// else.
    pub(super) fn set_breakpoint_enabled(
        &mut self,
        id: BreakpointId,
        enabled: bool,
    ) -> Result<Breakpoint> {
        let index = self
            .breakpoints
            .iter()
            .position(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        let current = &self.breakpoints[index];
        if current.enabled == enabled {
            return Ok(self.describe_breakpoint(current));
        }

        let mut breakpoint = current.clone();
        breakpoint.enabled = enabled;
        if enabled {
            if self.follows_resolvers(&breakpoint.spec) {
                self.learn_from_got(Some(&breakpoint.spec));
            }

            // A spec that resolves nowhere now is kept pending, as one
            // whose library unloaded is.
            breakpoint.locations = self
                .resolve_breakpoint(id, breakpoint.spec.clone(), true)?
                .locations;
        }

        if self.sites_live() {
            let inferior = self
                .inferior
                .as_mut()
                .ok_or_else(|| Error::internal("live sites have an inferior"))?;
            if enabled {
                install_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
                self.step_over_where_threads_trapped(&breakpoint)?;
            } else {
                remove_logical_breakpoint(&self.ptrace, inferior, &breakpoint)?;
            }
        }

        self.breakpoints[index] = breakpoint.clone();
        if self.follows_resolvers(&breakpoint.spec) {
            self.sync_resolvers()?;
        }

        let _ = self.sync_runtime_arming();
        self.publish_breakpoints_changed();
        Ok(self.describe_breakpoint(&breakpoint))
    }

    /// Deletes the temporary breakpoints a stop's hits name, as part of
    /// publishing that stop, and returns whether any was. The hits stay in
    /// the threads' stop reasons.
    pub(super) fn remove_stopped_temporaries(&mut self) -> Result<bool> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(false);
        };

        let hit = inferior
            .threads
            .values()
            .filter_map(|thread| match &thread.reason {
                Some(StopReason::Breakpoint { hits, .. }) => Some(hits.iter()),
                _ => None,
            })
            .flatten()
            .map(|hit| hit.breakpoint)
            .collect::<BTreeSet<_>>();
        let stopped =
            |breakpoint: &Breakpoint| breakpoint.temporary && hit.contains(&breakpoint.id);

        // Every trap is lifted before any breakpoint is forgotten, so one
        // that fails to lift leaves them all.
        for breakpoint in self
            .breakpoints
            .iter()
            .filter(|breakpoint| breakpoint.enabled && stopped(breakpoint))
        {
            remove_logical_breakpoint(&self.ptrace, inferior, breakpoint)?;
        }

        let mut removed = false;
        while let Some(index) = self.breakpoints.iter().position(stopped) {
            self.breakpoints.remove(index);
            removed = true;
        }

        if removed {
            let _ = self.sync_runtime_arming();
        }

        Ok(removed)
    }

    pub(super) fn remove_all_breakpoints(&mut self) -> Result<Arc<[Breakpoint]>> {
        if self.breakpoints.is_empty() {
            return Ok(Arc::from([]));
        }

        if self.sites_live() {
            let inferior = self
                .inferior
                .as_mut()
                .ok_or_else(|| Error::internal("live sites have an inferior"))?;
            let stopped_at = inferior
                .threads
                .iter()
                .map(|(&pid, thread)| (pid, thread.stopped_at_breakpoint))
                .collect::<Vec<_>>();
            let mut removed = Vec::new();
            for breakpoint in self
                .breakpoints
                .iter()
                .filter(|breakpoint| breakpoint.enabled)
            {
                if let Err(cause) = remove_logical_breakpoint(&self.ptrace, inferior, breakpoint) {
                    // As in `remove_logical_breakpoint`: a dying process
                    // gets nothing back.
                    if is_vanished_tracee(&cause) {
                        return Err(cause);
                    }
                    for prior in removed.iter().rev() {
                        if let Err(recovery) =
                            install_logical_breakpoint(&self.ptrace, inferior, prior)
                        {
                            return Err(backend_error(LinuxError::BreakpointRemoveRecovery {
                                cause: cause.to_string(),
                                recovery: recovery.to_string(),
                            }));
                        }
                    }

                    for (pid, address) in stopped_at {
                        inferior.thread_mut(pid)?.stopped_at_breakpoint = address;
                    }

                    return Err(cause);
                }

                removed.push(breakpoint.clone());
            }
        }

        let removed = self.breakpoints.take();
        let removed: Arc<[Breakpoint]> = removed
            .into_iter()
            .map(|mut breakpoint| {
                breakpoint.pending_locations = Arc::from([]);
                breakpoint
            })
            .collect();
        if self.sites_live() {
            self.sync_resolvers()?;
        }

        let _ = self.sync_runtime_arming();
        self.publish_breakpoints_changed();
        Ok(removed)
    }

    /// Replaces a breakpoint's hit condition, keeping the hits it counted.
    pub(super) fn set_breakpoint_hit_condition(
        &mut self,
        id: BreakpointId,
        hit_condition: Option<HitCondition>,
    ) -> Result<Breakpoint> {
        self.edit_breakpoint(id, |breakpoint| breakpoint.hit_condition = hit_condition)
    }

    pub(super) fn set_breakpoint_condition(
        &mut self,
        id: BreakpointId,
        condition: Option<crate::Condition>,
    ) -> Result<Breakpoint> {
        self.edit_breakpoint(id, |breakpoint| breakpoint.condition = condition)
    }

    pub(super) fn set_breakpoint_log_message(
        &mut self,
        id: BreakpointId,
        log_message: Option<crate::LogMessage>,
    ) -> Result<Breakpoint> {
        self.edit_breakpoint(id, |breakpoint| breakpoint.log_message = log_message)
    }

    /// Changes controller state only, so no stop is required.
    fn edit_breakpoint(
        &mut self,
        id: BreakpointId,
        edit: impl FnOnce(&mut Breakpoint),
    ) -> Result<Breakpoint> {
        let breakpoint = self
            .breakpoints
            .iter_mut()
            .find(|breakpoint| breakpoint.id == id)
            .ok_or(Error::BreakpointNotFound(id.get()))?;
        edit(breakpoint);
        let breakpoint = breakpoint.clone();
        self.publish_breakpoints_changed();
        Ok(self.describe_breakpoint(&breakpoint))
    }

    /// Counts one hit for every logical breakpoint owning the site at
    /// `address` that thread `pid` reached, and returns those the hit stops
    /// at: its hit condition and condition are met, and it logs no message
    /// instead. A condition that cannot be evaluated stops, as gdb does,
    /// since skipping the hit could hide what the user asked to see.
    pub(super) fn record_breakpoint_hits(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Arc<[BreakpointHit]> {
        let Some(site) = self
            .inferior
            .as_ref()
            .and_then(|inferior| inferior.breakpoints.get(&address))
        else {
            return Arc::from([]);
        };

        let owners = site
            .owners
            .iter()
            .filter_map(|owner| match owner {
                BreakpointOwner::User(id) => Some(*id),
                BreakpointOwner::Plan(_)
                | BreakpointOwner::Loader
                | BreakpointOwner::Runtime
                | BreakpointOwner::StackMove
                | BreakpointOwner::InterpretedMove
                | BreakpointOwner::Resolver
                | BreakpointOwner::InspectorHook => None,
            })
            .collect::<BTreeSet<_>>();
        self.judge_hits(pid, &owners, Some(address))
    }

    /// Counts a hit of each enabled breakpoint among `owners` by thread
    /// `pid` at the location at `address`, or at a managed location when
    /// `None`, and returns those whose hit condition and condition this
    /// hit meets, in identifier order. A breakpoint that logs publishes its
    /// message instead.
    pub(super) fn judge_hits(
        &mut self,
        pid: Pid,
        owners: &BTreeSet<BreakpointId>,
        address: Option<VirtualAddress>,
    ) -> Arc<[BreakpointHit]> {
        let mut candidates = Vec::new();
        for breakpoint in &mut self.breakpoints {
            if !owners.contains(&breakpoint.id) {
                continue;
            }

            breakpoint.hit_count = breakpoint.hit_count.saturating_add(1);
            if breakpoint
                .hit_condition
                .is_none_or(|condition| condition.is_met(breakpoint.hit_count))
            {
                candidates.push((
                    BreakpointHit {
                        breakpoint: breakpoint.id,
                        hit_count: breakpoint.hit_count,
                    },
                    breakpoint.condition.clone(),
                    breakpoint.log_message.clone(),
                ));
            }
        }

        let reason = StopReason::Breakpoint {
            address,
            hits: Arc::from([]),
        };

        let mut stopping = Vec::new();
        for (hit, condition, log_message) in candidates {
            if let Some(condition) = condition {
                let owner = ConditionOwner::Breakpoint(hit.breakpoint);
                match self.judge_condition(pid, &condition, &reason, owner) {
                    Some(true) => {}
                    Some(false) => continue,
                    // A breakpoint that logs stops too.
                    None => {
                        stopping.push(hit);
                        continue;
                    }
                }
            }

            if let Some(message) = log_message {
                let parts = self.log_parts(pid, &message, &reason);
                self.publish_hit_event(pid, |revision, process_id, thread_id| {
                    DebuggerEvent::LogMessage {
                        revision,
                        process_id,
                        thread_id,
                        breakpoint: hit.breakpoint,
                        parts,
                    }
                });
                continue;
            }

            stopping.push(hit);
        }

        stopping.sort_unstable_by_key(|hit| hit.breakpoint);
        stopping.into()
    }

    /// Whether `condition` holds in the innermost frame of thread `pid`, as
    /// a hit's stop `reason` presents it, or `None` when it cannot be
    /// evaluated, which is published. Such a hit stops, as gdb does, since
    /// skipping it could hide what the user asked to see. A thread SIGKILL
    /// took out of its stop meanwhile stops too, and its exit, not the
    /// condition, is what clients hear of.
    pub(super) fn judge_condition(
        &mut self,
        pid: Pid,
        condition: &crate::Condition,
        reason: &StopReason,
        owner: ConditionOwner,
    ) -> Option<bool> {
        match self.condition_met(pid, condition, reason) {
            Ok(holds) => Some(holds),
            Err(None) => None,
            Err(Some(error)) => {
                self.publish_hit_event(pid, |revision, process_id, thread_id| {
                    DebuggerEvent::ConditionFailed {
                        revision,
                        process_id,
                        thread_id,
                        owner,
                        error: error.into(),
                    }
                });
                None
            }
        }
    }

    /// Evaluates a condition in the innermost frame of a thread stopped at a
    /// hit: its truth, or why it has none, or `Err(None)` when the thread
    /// left its stop.
    fn condition_met(
        &self,
        pid: Pid,
        condition: &crate::Condition,
        reason: &StopReason,
    ) -> std::result::Result<bool, Option<String>> {
        let expression = condition.expression();
        match self.evaluate_at_hit(pid, expression, true, reason) {
            Ok(crate::Evaluation::Value { value, cause }) => match value.state {
                crate::VariableState::Available {
                    value: crate::VariableValue::Scalar(crate::ScalarValue::Boolean(holds)),
                    ..
                } => Ok(holds),
                crate::VariableState::Unavailable(reason) => Err(Some(cause.map_or_else(
                    || format!("the condition is unavailable: {reason}"),
                    |cause| {
                        format!(
                            "`{}` is unavailable: {reason}",
                            cause.text(expression.text())
                        )
                    },
                ))),
                state => Err(Some(format!("the condition has no truth value: {state:?}"))),
            },
            Ok(_) => Err(Some("the condition has no truth value".to_owned())),
            Err(error) if is_vanished_tracee(&error) => Err(None),
            Err(error) => Err(Some(error.to_string())),
        }
    }

    /// Reads the values a log message shows, as the hitting thread sees them.
    fn log_parts(
        &self,
        pid: Pid,
        message: &crate::LogMessage,
        reason: &StopReason,
    ) -> Arc<[crate::LogPart]> {
        message
            .segments()
            .iter()
            .map(|segment| match segment {
                crate::LogSegment::Text(text) => crate::LogPart::Text(Arc::clone(text)),
                crate::LogSegment::Value(expression) => {
                    match self.evaluate_at_hit(pid, expression, false, reason) {
                        Ok(crate::Evaluation::Value { value, .. }) => crate::LogPart::Value {
                            expression: expression.clone(),
                            type_info: value.type_info,
                            state: value.state,
                        },
                        Ok(_) => crate::LogPart::Error {
                            expression: expression.clone(),
                            error: "a range cannot be logged".into(),
                        },
                        Err(error) => crate::LogPart::Error {
                            expression: expression.clone(),
                            error: error.to_string().into(),
                        },
                    }
                }
            })
            .collect()
    }

    fn publish_hit_event(
        &mut self,
        pid: Pid,
        event: impl FnOnce(u64, crate::ProcessId, crate::ThreadId) -> DebuggerEvent,
    ) {
        let Some(process_id) = self
            .inferior
            .as_ref()
            .map(|inferior| super::process_id(inferior.tgid))
        else {
            return;
        };

        self.bump_revision();
        let _ = self.events.send(event(
            self.revision,
            process_id,
            super::debug_thread_id(pid),
        ));
    }

    /// Starts every breakpoint's count again for a new process.
    pub(super) fn reset_breakpoint_hit_counts(&mut self) {
        for breakpoint in &mut self.breakpoints {
            breakpoint.hit_count = 0;
        }
    }

    pub(super) fn publish_breakpoints_changed(&mut self) {
        self.bump_revision();
        let _ = self.events.send(DebuggerEvent::BreakpointsChanged {
            revision: self.revision,
        });
    }

    pub(super) fn remove_breakpoint_owner(
        &mut self,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
    }

    pub(super) fn cleanup_plan_breakpoints(&mut self, execution: ExecutionId) -> Result<()> {
        let owner = BreakpointOwner::Plan(execution);
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(recorded) = inferior.plan_sites.get(&execution) else {
            return Ok(());
        };

        let addresses = recorded
            .iter()
            .copied()
            .filter(|address| {
                inferior
                    .breakpoints
                    .get(address)
                    .is_some_and(|site| site.owners.contains(&owner))
            })
            .collect::<Vec<_>>();

        for address in addresses {
            self.remove_breakpoint_owner(address, owner)?;
        }

        self.inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .plan_sites
            .remove(&execution);
        Ok(())
    }

    pub(super) fn install_additional_plan_breakpoints(
        &mut self,
        execution: ExecutionId,
        addresses: &BTreeSet<VirtualAddress>,
    ) -> Result<()> {
        let owner = BreakpointOwner::Plan(execution);
        let new_addresses = {
            let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
            addresses
                .iter()
                .copied()
                .filter(|address| {
                    inferior
                        .breakpoints
                        .get(address)
                        .is_none_or(|site| !site.owners.contains(&owner))
                })
                .collect::<Vec<_>>()
        };

        let mut installed = Vec::new();
        for address in new_addresses {
            let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
            if let Err(error) = install_plan_breakpoint(&self.ptrace, inferior, address, execution)
            {
                if self.lost_to_sigkill(&error) {
                    return Err(error);
                }

                let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
                for address in installed.into_iter().rev() {
                    if let Err(recovery) =
                        remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)
                    {
                        let _ = self.ptrace.kill(inferior.tgid, Signal::SIGKILL);
                        return Err(backend_error(LinuxError::ResumeRecovery {
                            cause: error.to_string(),
                            recovery: recovery.to_string(),
                        }));
                    }
                }

                return Err(error);
            }

            installed.push(address);
        }

        Ok(())
    }

    pub(super) fn restore_active_breakpoints(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };

        let removed: Vec<_> = inferior
            .repairs
            .iter()
            .filter(|group| group.site_removed)
            .map(|group| group.address)
            .collect();
        let pid = inferior.memory_thread();

        for address in removed {
            self.ptrace
                .reinstall_breakpoint(pid, &mut inferior.breakpoints, address)?;
        }

        inferior.repairs.clear();
        Ok(())
    }

    /// Forgets every site whose memory no longer holds its trap, after the
    /// process may have changed its mappings. Code that moved, as the vDSO
    /// does under mremap(2), carries its traps along, so each is taken out
    /// where it went; a function breakpoint is installed there again when
    /// it is resolved. A site whose memory went away, or now holds
    /// something else, is never written. Each owner loses the site: a
    /// breakpoint its location, a plan its site, and the loader its hook.
    /// Returns whether any breakpoint lost a location.
    pub(super) fn reconcile_sites(&mut self, moved: &[MovedCode]) -> Result<bool> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(false);
        };

        let pid = inferior.memory_thread();
        let mut lost = Vec::new();

        // Traps come out of moved code first, since one may have landed on
        // another site's address.
        for (&address, site) in &inferior.breakpoints {
            let Some(destination) = moved
                .iter()
                .filter(|_| site.installed)
                .find_map(|code| code.destination(address))
            else {
                continue;
            };

            let destination = destination.get();
            if site.original_byte != BREAKPOINT_OPCODE
                && memory_byte(&self.ptrace, pid, destination)? == Some(BREAKPOINT_OPCODE)
            {
                let word = self.ptrace.read_word(pid, destination)?;
                self.ptrace.write_word(
                    pid,
                    destination,
                    (word & !0xff) | u64::from(site.original_byte),
                )?;
            }

            lost.push(address);
        }

        let installed = inferior
            .breakpoints
            .iter()
            .filter(|&(address, site)| site.installed && !lost.contains(address))
            .map(|(&address, site)| (address, site.original_byte))
            .collect::<Vec<_>>();

        // A process may have thousands of sites, most sharing a page with
        // others, which one read covers.
        let page = |address: VirtualAddress| address.get() / PAGE_BYTES;

        for sites in installed.chunk_by(|(one, _), (other, _)| page(*one) == page(*other)) {
            let (first, end) = (sites[0].0.get(), sites[sites.len() - 1].0.get() + 1);
            let block = usize::try_from(end - first)
                .ok()
                .and_then(|size| self.ptrace.read_block(pid, first, size));
            for &(address, original_byte) in sites {
                let byte = match &block {
                    Some(bytes) => Some(
                        usize::try_from(address.get() - first)
                            .ok()
                            .and_then(|offset| bytes.get(offset).copied())
                            .ok_or_else(|| {
                                Error::internal("a site lies within the block read over it")
                            })?,
                    ),
                    None => memory_byte(&self.ptrace, pid, address.get())?,
                };

                // A trap over a trap cannot tell whether the memory changed.
                if byte != Some(BREAKPOINT_OPCODE)
                    && (byte.is_none() || original_byte != BREAKPOINT_OPCODE)
                {
                    lost.push(address);
                }
            }
        }

        let mut lost_locations = Vec::new();

        for address in lost {
            // Each address is lost once, so a site that is gone has nothing left to forget.
            let Some(site) = inferior.breakpoints.remove(&address) else {
                continue;
            };

            forget_removed_site(inferior, address, site.original_byte);

            for owner in site.owners {
                match owner {
                    BreakpointOwner::User(id) => lost_locations.push((id, address)),
                    BreakpointOwner::Plan(execution) => {
                        if let Some(sites) = inferior.plan_sites.get_mut(&execution) {
                            sites.remove(&address);
                        }
                    }
                    BreakpointOwner::Loader => inferior.loader_site = None,
                    BreakpointOwner::Runtime => {
                        inferior.runtime_hooks.remove(&address);
                    }
                    BreakpointOwner::StackMove => inferior.stack_moves.forget(address),
                    BreakpointOwner::InterpretedMove => {
                        inferior.interpreted_moves.forget(address);
                    }
                    BreakpointOwner::Resolver => inferior.indirect.forget(address),
                    BreakpointOwner::InspectorHook => inferior.inspectors.forget(address),
                }
            }
        }

        for &(id, address) in &lost_locations {
            let Some(breakpoint) = self
                .breakpoints
                .iter_mut()
                .find(|breakpoint| breakpoint.id == id)
            else {
                continue;
            };

            breakpoint.locations = breakpoint
                .locations
                .iter()
                .filter(|location| !traps_at(inferior, location, address))
                .cloned()
                .collect();
        }

        Ok(!lost_locations.is_empty())
    }
}

/// Module code that moved since the last stop: the bytes at `from` are now
/// at `to` onwards, the debugger's traps among them.
#[derive(Debug)]
pub(super) struct MovedCode {
    pub(super) from: std::ops::Range<VirtualAddress>,
    pub(super) to: VirtualAddress,
}

impl MovedCode {
    /// Where the byte now at `address` came from, if this code holds it.
    pub(super) fn source(&self, address: VirtualAddress) -> Option<VirtualAddress> {
        let offset = address.get().checked_sub(self.to.get())?;
        let source = VirtualAddress::new(self.from.start.get().checked_add(offset)?);
        self.from.contains(&source).then_some(source)
    }

    /// Where the byte at `address` went, if this code held it.
    fn destination(&self, address: VirtualAddress) -> Option<VirtualAddress> {
        if !self.from.contains(&address) {
            return None;
        }

        self.to
            .get()
            .checked_add(address.get() - self.from.start.get())
            .map(VirtualAddress::new)
    }
}

/// The size of the pages [`Controller::reconcile_sites`] reads at once.
const PAGE_BYTES: u64 = 4096;

/// The byte at `address`, or `None` where no memory is mapped.
fn memory_byte(ptrace: &impl LinuxTraceOps, pid: Pid, address: u64) -> Result<Option<u8>> {
    match ptrace.read_memory_word(pid, address) {
        Ok(word)
        | Err(MemoryAccessError::Partial {
            word,
            readable: 1..,
        }) => Ok(Some(word.to_ne_bytes()[0])),
        Err(MemoryAccessError::Inaccessible | MemoryAccessError::Partial { .. }) => Ok(None),
        Err(MemoryAccessError::Fatal(error)) => Err(error),
    }
}

/// Resolves a function or source spec in one module image to image
/// addresses and the code instances at each.
fn resolve_in_image(
    image: &crate::ModuleImage,
    spec: &BreakpointSpec,
    chosen: &dyn Fn(crate::ImageAddress) -> Option<crate::ImageAddress>,
) -> Result<Vec<(crate::ImageAddress, Arc<[crate::CodeInstanceId]>)>> {
    match spec {
        BreakpointSpec::Address(_) => Ok(Vec::new()),
        // Searching every symbol for a runtime's function would find none.
        BreakpointSpec::Function(name) if crate::names_runtime_function(name) => {
            Err(Error::FunctionNotFound(name.clone()))
        }
        BreakpointSpec::Function(name) => {
            // Like gdb, break at every function with the name that has
            // code: overloads and same-named static functions alike.
            // A function the image only declares, such as one another
            // module defines, has no code here.
            match image.functions_located(name, None) {
                Ok(functions) => function_locations(image, functions),
                Err(Error::FunctionNotFound(_)) => symbol_locations(image, name, chosen),
                Err(error) => Err(error),
            }
        }
        BreakpointSpec::FileFunction { path, function } => {
            let source = image.source_file_matching(path)?;
            function_locations(image, image.functions_located(function, Some(source.id))?)
        }
        BreakpointSpec::Source { path, line } => {
            let source = image.source_file_matching(path)?;
            let unavailable = || Error::SourceLineUnavailable {
                path: path.clone(),
                line: line.get(),
            };

            let Some(line) = image.breakpoint_line(source.id, *line) else {
                if image.statement_addresses(source.id, *line).next().is_none()
                    && image.keeps_line_breakpoints(source.id)
                {
                    let (before, after) = image.nearest_statement_lines(source.id, *line);
                    return Err(Error::SourceLineWithoutStatement {
                        path: path.clone(),
                        line: line.get(),
                        before: before.map(LineNumber::get),
                        after: after.map(LineNumber::get),
                    });
                }

                return Err(unavailable());
            };

            let mut addresses = image
                .statement_addresses(source.id, line)
                .collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(unavailable());
            }

            // A coroutine's dispatch and the code leading from it into a
            // state run as it resumes, not as execution arrives at the
            // line; code a compiler generated, such as the glue dropping a
            // future, never shows as the program's line, though the
            // program's code may be inlined into a wrapper's.
            let arrivals = addresses
                .iter()
                .copied()
                .filter(|address| {
                    !image.is_resume_code(*address)
                        && image
                            .locate(*address)
                            .function
                            .is_none_or(|function| function.role != crate::CodeRole::Wrapper)
                })
                .collect::<Vec<_>>();
            if !arrivals.is_empty() {
                addresses = arrivals;
            }

            addresses.sort_unstable();

            // Like gdb, stop where the line begins in each instance of its
            // code, not again at its later statements, such as the use of
            // a call's result or a loop's condition.
            let mut seen = BTreeSet::new();
            let mut locations = Vec::new();
            for address in addresses {
                let mut code_instances = image.instances_containing(address).collect::<Vec<_>>();
                code_instances.sort_unstable_by_key(|instance| instance.id());
                code_instances.dedup_by_key(|instance| instance.id());
                let innermost = code_instances
                    .iter()
                    .find(|instance| {
                        !code_instances
                            .iter()
                            .any(|other| other.parent() == Some(instance.id()))
                    })
                    .map(|instance| instance.id());

                // The line is the innermost instance's code, so a stop there
                // presents that instance's frame.
                if innermost.is_none_or(|instance| seen.insert(instance)) {
                    locations.push((address, innermost.into_iter().collect()));
                }
            }

            Ok(locations)
        }
    }
}

/// The entries of the code symbols with a name, for an image whose debug
/// information does not describe the function, such as a system library's.
/// An indirect function's symbol names its resolver, not the function: it
/// enters the implementation its resolver `chosen`, and nowhere until the
/// resolver has chosen one.
fn symbol_locations(
    image: &crate::ModuleImage,
    name: &str,
    chosen: &dyn Fn(crate::ImageAddress) -> Option<crate::ImageAddress>,
) -> Result<Vec<(crate::ImageAddress, Arc<[crate::CodeInstanceId]>)>> {
    let mut entries = BTreeSet::new();
    let mut indirect = false;

    for symbol in image.symbols_answering(name) {
        if symbol.extent().is_none() {
            continue;
        }

        match symbol.kind() {
            crate::SymbolKind::Function => {
                entries.insert(symbol.address());
            }
            crate::SymbolKind::IndirectFunction => {
                indirect = true;
                entries.extend(chosen(symbol.address()));
            }
            crate::SymbolKind::Data | crate::SymbolKind::Unknown => {}
        }
    }

    if entries.is_empty() && !indirect {
        return Err(Error::FunctionNotFound(name.to_owned()));
    }

    Ok(entries
        .into_iter()
        .map(|address| (address, Arc::from([])))
        .collect())
}

/// The locations of every instance of some functions: one per address
/// where an instance's code begins after its prologue.
fn function_locations<'a>(
    image: &crate::ModuleImage,
    functions: impl IntoIterator<Item = crate::Function<'a>>,
) -> Result<Vec<(crate::ImageAddress, Arc<[crate::CodeInstanceId]>)>> {
    let mut instances = Vec::new();
    for function in functions {
        instances.extend(function.instances());
    }

    if instances.is_empty()
        || instances.iter().any(|instance| {
            image
                .recommended_entries_for_instance(instance.id())
                .next()
                .is_none()
        })
    {
        return Err(Error::LocationUnavailable);
    }

    let mut by_address = BTreeMap::<_, Vec<_>>::new();

    for instance in instances {
        for entry in image.recommended_entries_for_instance(instance.id()) {
            by_address
                .entry(entry.address)
                .or_default()
                .push(instance.id());
        }
    }

    Ok(by_address
        .into_iter()
        .map(|(address, code_instances)| (address, code_instances.into()))
        .collect())
}

/// Where a location's trap is in the process; `None` for a location a
/// runtime manages, which has no trap.
pub(super) fn runtime_breakpoint_address(
    inferior: &Inferior,
    location: &BreakpointLocation,
) -> Result<Option<VirtualAddress>> {
    match location.address() {
        Some(TrapAddress::Image(address)) => {
            inferior.loaded_module.virtual_address(address).map(Some)
        }
        Some(TrapAddress::Virtual(address)) => Ok(Some(address)),
        None => Ok(None),
    }
}

/// Whether the location's trap is at `address`.
fn traps_at(
    inferior: &Inferior,
    location: &ResolvedBreakpointLocation,
    address: VirtualAddress,
) -> bool {
    runtime_breakpoint_address(inferior, &location.location).is_ok_and(|trap| trap == Some(address))
}

/// Where the traps of a breakpoint's locations are, leaving out the
/// locations runtimes manage.
pub(super) fn trap_addresses(
    inferior: &Inferior,
    breakpoint: &Breakpoint,
) -> Result<Vec<VirtualAddress>> {
    breakpoint
        .locations
        .iter()
        .filter_map(|resolved| runtime_breakpoint_address(inferior, &resolved.location).transpose())
        .collect()
}

/// The managed locations among a breakpoint's.
pub(super) fn managed_locations(breakpoint: &Breakpoint) -> impl Iterator<Item = &ManagedLocation> {
    breakpoint
        .locations
        .iter()
        .filter_map(|resolved| match &resolved.location {
            BreakpointLocation::Managed(location) => Some(location),
            BreakpointLocation::Image(_) | BreakpointLocation::Virtual(_) => None,
        })
}

/// Installs a site that `execution`'s plan owns, and records it for the
/// plan's cleanup. The record comes first, so even a failed install is
/// visited.
pub(super) fn install_plan_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    address: VirtualAddress,
    execution: ExecutionId,
) -> Result<()> {
    debug_assert!(
        !inferior.traps_lifted,
        "installing the trap at {address} for execution {execution} while the traps \
         are lifted for a vfork child"
    );
    inferior
        .plan_sites
        .entry(execution)
        .or_default()
        .insert(address);
    let pid = inferior.memory_thread();
    ptrace.install_breakpoint(
        pid,
        &mut inferior.breakpoints,
        address,
        BreakpointOwner::Plan(execution),
    )
}

pub(super) fn install_logical_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    breakpoint: &Breakpoint,
) -> Result<()> {
    debug_assert!(
        !inferior.traps_lifted,
        "installing breakpoint {} while the traps are lifted for a vfork child",
        breakpoint.id
    );
    let owner = BreakpointOwner::User(breakpoint.id);
    let addresses = trap_addresses(inferior, breakpoint)?;
    let mut installed = Vec::with_capacity(addresses.len());

    for address in addresses {
        let pid = inferior.memory_thread();
        if let Err(cause) =
            ptrace.install_breakpoint(pid, &mut inferior.breakpoints, address, owner)
        {
            for installed_address in installed.into_iter().rev() {
                if let Err(recovery) =
                    remove_breakpoint_owner_from(ptrace, inferior, installed_address, owner)
                {
                    return Err(backend_error(LinuxError::BreakpointInstallRecovery {
                        cause: cause.to_string(),
                        recovery: recovery.to_string(),
                    }));
                }
            }

            return Err(cause);
        }

        installed.push(address);
    }

    for location in managed_locations(breakpoint) {
        inferior.managed.own(location, breakpoint.id);
    }

    Ok(())
}

pub(super) fn remove_logical_breakpoint(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    breakpoint: &Breakpoint,
) -> Result<()> {
    let owner = BreakpointOwner::User(breakpoint.id);
    let addresses = trap_addresses(inferior, breakpoint)?;

    for &address in &addresses {
        let site = inferior
            .breakpoints
            .get(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        if !site.owners.contains(&owner) {
            return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
        }
    }

    let mut removed = Vec::new();
    for address in addresses {
        if let Err(cause) = remove_breakpoint_owner_from(ptrace, inferior, address, owner) {
            // SIGKILL took the thread: the address space is dying, so
            // nothing is put back, and the caller learns of the death
            // itself, which the process's exit status then confirms. Each
            // site record still matches memory: the traps lifted are gone.
            if is_vanished_tracee(&cause) {
                return Err(cause);
            }
            for prior in removed.into_iter().rev() {
                if let Err(recovery) = ptrace.install_breakpoint(
                    inferior.memory_thread(),
                    &mut inferior.breakpoints,
                    prior,
                    owner,
                ) {
                    return Err(backend_error(LinuxError::BreakpointRemoveRecovery {
                        cause: cause.to_string(),
                        recovery: recovery.to_string(),
                    }));
                }
            }

            return Err(cause);
        }

        removed.push(address);
    }

    for location in managed_locations(breakpoint) {
        inferior.managed.release(location, breakpoint.id);
    }

    Ok(())
}

/// Restores the original byte of every installed trap, keeping each site and
/// its owners. A vfork child shares this memory and must not run a trap
/// while it execs, so the traps stay lifted until [`reinstall_all_traps`].
pub(super) fn lift_all_traps(ptrace: &dyn LinuxTraceOps, inferior: &mut Inferior) -> Result<()> {
    let pid = inferior.memory_thread();
    let installed: Vec<_> = inferior
        .breakpoints
        .iter()
        .filter_map(|(&address, site)| site.installed.then_some(address))
        .collect();
    for address in installed {
        ptrace.remove_breakpoint(pid, &mut inferior.breakpoints, address)?;
    }

    debug_assert!(
        inferior.breakpoints.values().all(|site| !site.installed),
        "lifting every trap left one in place"
    );
    Ok(())
}

/// Writes back the trap of every site [`lift_all_traps`] lifted.
pub(super) fn reinstall_all_traps(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
) -> Result<()> {
    let pid = inferior.memory_thread();
    let lifted: Vec<_> = inferior
        .breakpoints
        .iter()
        .filter_map(|(&address, site)| (!site.installed).then_some(address))
        .collect();
    for address in lifted {
        ptrace.reinstall_breakpoint(pid, &mut inferior.breakpoints, address)?;
    }

    debug_assert!(
        inferior.breakpoints.values().all(|site| site.installed),
        "reinstalling every trap left one lifted"
    );
    Ok(())
}

/// Forgets every repair of a site whose original instruction is restored,
/// remembering the site for processes forked while it was installed.
pub(super) fn forget_removed_site(
    inferior: &mut Inferior,
    address: VirtualAddress,
    original_byte: u8,
) {
    inferior
        .former_sites
        .entry(address)
        .or_insert(original_byte);

    // With the original instruction restored, no thread needs a repair step.
    for thread in inferior.threads.values_mut() {
        if thread.stopped_at_breakpoint == Some(address) {
            thread.stopped_at_breakpoint = None;
        }
    }

    inferior.repairs.retain(|group| group.address != address);
}

pub(super) fn remove_breakpoint_owner_from(
    ptrace: &dyn LinuxTraceOps,
    inferior: &mut Inferior,
    address: VirtualAddress,
    owner: BreakpointOwner,
) -> Result<()> {
    let remove_site = {
        let site = inferior
            .breakpoints
            .get_mut(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        if !site.owners.contains(&owner) {
            return Err(backend_error(LinuxError::BreakpointOwnerMissing(address)));
        }

        site.owners.len() == 1
    };

    if remove_site {
        let pid = inferior.memory_thread();
        ptrace.remove_breakpoint(pid, &mut inferior.breakpoints, address)?;
        let removed = inferior
            .breakpoints
            .remove(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        forget_removed_site(inferior, address, removed.original_byte);
    } else {
        let site = inferior
            .breakpoints
            .get_mut(&address)
            .ok_or_else(|| backend_error(LinuxError::BreakpointSiteMissing(address)))?;
        let removed = site.owners.remove(&owner);
        debug_assert!(
            removed,
            "{owner:?} owned the site at {address}, then did not"
        );
    }

    Ok(())
}
