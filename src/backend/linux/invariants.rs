//! The facts the controller's state keeps between messages, checked after
//! each one in development builds. A fact here must hold even when a ptrace
//! request failed partway through an operation, as a tracee killed from
//! outside makes one do. See `dev/assertions.md`.

use std::collections::BTreeSet;

use super::{BreakpointOwner, Controller, Inferior, NativeThreadState, debug_thread_id};
use crate::ModuleId;
use crate::backend::linux::native::InspectionOps;

/// The state size up to which every message is followed by a check.
/// Beyond it, checks are spaced out so that their cost per message stays
/// about that of checking this much state, and a session with thousands of
/// breakpoints runs at its usual speed.
const CHECKED_EVERY_MESSAGE: usize = 256;

impl<P: InspectionOps> Controller<P> {
    /// How many messages pass between checks of the current state.
    pub(super) fn invariant_check_stride(&self) -> u64 {
        let size = self.breakpoints.len()
            + self.modules.len()
            + self.inferior.as_ref().map_or(0, |inferior| {
                inferior.breakpoints.len()
                    + inferior.threads.len()
                    + inferior.watch.watchpoints.len()
            });
        u64::try_from(size / CHECKED_EVERY_MESSAGE).map_or(u64::MAX, |stride| stride.max(1))
    }

    /// Panics if the controller's state contradicts itself.
    pub(super) fn check_invariants(&self) {
        self.check_modules();
        self.check_user_breakpoints();
        self.check_lifecycle();
        if let Some(inferior) = &self.inferior {
            inferior.check_invariants(self.shutting_down);
            self.check_watchpoint_ids(inferior);
            self.check_managed(inferior);
        }
    }

    /// Each managed location is owned by enabled breakpoints that list it,
    /// and while sites are live, each location an enabled breakpoint lists
    /// is owned by it.
    fn check_managed(&self, inferior: &Inferior) {
        let lists = |breakpoint: &crate::Breakpoint, location: &crate::ManagedLocation| {
            super::breakpoints::managed_locations(breakpoint).any(|listed| listed == location)
        };

        for (location, owners) in inferior.managed.owned() {
            debug_assert!(!owners.is_empty(), "a managed location has no owners");
            for &owner in owners {
                debug_assert!(
                    self.breakpoints
                        .iter()
                        .any(|breakpoint| breakpoint.id == owner
                            && breakpoint.enabled
                            && lists(breakpoint, location)),
                    "breakpoint {owner} owns a managed location it does not list while enabled"
                );
            }
        }

        let sites_live = self.launch_reply.is_none()
            && self.attach_reply.is_none()
            && inferior
                .threads
                .iter()
                .all(|(&pid, thread)| inferior.settled(pid, thread));

        if sites_live {
            for breakpoint in self
                .breakpoints
                .iter()
                .filter(|breakpoint| breakpoint.enabled)
            {
                for location in super::breakpoints::managed_locations(breakpoint) {
                    debug_assert!(
                        inferior.managed.owns(location, breakpoint.id),
                        "breakpoint {} lists a managed location it does not own",
                        breakpoint.id
                    );
                }
            }
        }
    }

    fn check_modules(&self) {
        let mut images = BTreeSet::new();

        for (&id, module) in &self.modules {
            debug_assert_eq!(
                module.loaded.id, id,
                "module table key {id:?} holds module {:?}",
                module.loaded.id
            );
            debug_assert!(
                id.get() < self.next_module_id,
                "module {id:?} is not below the next module id {}",
                self.next_module_id
            );
            debug_assert!(
                module.loaded.image.get() < self.next_image_id,
                "module {id:?} has image {:?}, not below the next image id {}",
                module.loaded.image,
                self.next_image_id
            );
            let first = images.insert(module.loaded.image);
            debug_assert!(
                first,
                "image {:?} is bound to more than one loaded module",
                module.loaded.image
            );
        }

        debug_assert!(
            self.modules
                .get(&ModuleId::new(0))
                .is_some_and(|main| main.loaded.image == self.module_image.id()),
            "module 0 is not the executable's image {:?}",
            self.module_image.id()
        );
    }

    fn check_user_breakpoints(&self) {
        self.breakpoints.check_invariants();
        let mut ids = BTreeSet::new();

        for breakpoint in &self.breakpoints {
            debug_assert!(
                breakpoint.id.get() < self.next_breakpoint_id,
                "breakpoint {} is not below the next breakpoint id {}",
                breakpoint.id,
                self.next_breakpoint_id
            );
            let first = ids.insert(breakpoint.id);
            debug_assert!(first, "breakpoint {} is listed twice", breakpoint.id);
        }

        for &id in self.catchpoints.keys() {
            debug_assert!(
                id.get() < self.next_catchpoint_id,
                "catchpoint {id} is not below the next catchpoint id {}",
                self.next_catchpoint_id
            );
        }
    }

    fn check_watchpoint_ids(&self, inferior: &Inferior) {
        for &id in inferior.watch.watchpoints.keys() {
            debug_assert!(
                id.get() < self.next_watchpoint_id,
                "watchpoint {id} is not below the next watchpoint id {}",
                self.next_watchpoint_id
            );
        }
    }

    fn check_lifecycle(&self) {
        if let Some(orphans) = &self.orphans {
            debug_assert!(
                !orphans.children.is_empty() || !orphans.killed.is_empty(),
                "orphans of {} are kept with no child left to wait for",
                orphans.parent
            );
            for pid in &orphans.killed {
                debug_assert!(
                    !orphans.children.contains_key(pid),
                    "orphan {pid} is both awaiting its first stop and killed"
                );
            }
        }

        debug_assert!(
            self.inferior.is_some()
                || (self.launch_reply.is_none()
                    && self.attach_reply.is_none()
                    && self.kill_reply.is_none()),
            "a launch, attach, or kill reply is pending with no inferior to answer it"
        );
        debug_assert!(
            self.deferred_start.is_none() || (self.orphans.is_some() && !self.shutting_down),
            "a launch or attach is deferred with no orphans to wait for, or during shutdown"
        );
    }
}

impl Inferior {
    /// Panics if the inferior's breakpoint, thread, stop, and watch state
    /// contradict each other.
    fn check_invariants(&self, shutting_down: bool) {
        self.check_sites();
        self.check_threads(shutting_down);
        if !shutting_down {
            self.check_stop();
        }

        self.check_watches();
    }

    fn check_sites(&self) {
        let stack_moves = self.stack_moves.planted();
        let resolvers = self.indirect.planted();

        for (owner, planted) in [
            (BreakpointOwner::StackMove, &stack_moves),
            (BreakpointOwner::Resolver, &resolvers),
        ] {
            for address in planted {
                debug_assert!(
                    self.breakpoints
                        .get(address)
                        .is_some_and(|site| site.owners.contains(&owner)),
                    "the site at {address} is recorded for {owner:?}, which does not own it"
                );
            }
        }

        for (&address, site) in &self.breakpoints {
            debug_assert!(
                !site.owners.is_empty(),
                "the site at {address} has no owners"
            );
            debug_assert!(
                !(self.traps_lifted && site.installed),
                "the site at {address} is installed while the traps are lifted"
            );
            for owner in &site.owners {
                match owner {
                    BreakpointOwner::Plan(execution) => {
                        debug_assert!(
                            self.plan_sites
                                .get(execution)
                                .is_some_and(|sites| sites.contains(&address)),
                            "execution {execution} owns the site at {address} \
                             but does not record it"
                        );
                    }
                    BreakpointOwner::Loader => debug_assert_eq!(
                        self.loader_site,
                        Some(address),
                        "the loader owns the site at {address}, not its recorded site"
                    ),
                    BreakpointOwner::Runtime => debug_assert!(
                        self.runtime_hooks.contains_key(&address),
                        "a runtime hook owns the site at {address}, but none is recorded"
                    ),
                    BreakpointOwner::StackMove => debug_assert!(
                        stack_moves.contains(&address),
                        "a stack mover owns the site at {address}, but none is recorded"
                    ),
                    BreakpointOwner::Resolver => debug_assert!(
                        resolvers.contains(&address),
                        "a resolver owns the site at {address}, but none is recorded"
                    ),
                    BreakpointOwner::User(_) => {}
                }
            }
        }

        if let Some(address) = self.loader_site {
            debug_assert!(
                self.breakpoints
                    .get(&address)
                    .is_some_and(|site| site.owners.contains(&BreakpointOwner::Loader)),
                "the loader's site at {address} has no loader owner"
            );
        }

        for &address in self.runtime_hooks.keys() {
            debug_assert!(
                self.breakpoints
                    .get(&address)
                    .is_some_and(|site| site.owners.contains(&BreakpointOwner::Runtime)),
                "the runtime hook at {address} has no runtime owner"
            );
        }

        debug_assert!(
            !self.traps_lifted || !self.vfork_windows.is_empty(),
            "the traps are lifted with no vfork window open to reinstall them"
        );

        for group in &self.repairs {
            let site = self.breakpoints.get(&group.address);
            debug_assert!(
                site.is_some(),
                "a repair at {} has no breakpoint site",
                group.address
            );
            debug_assert!(
                !group.site_removed || site.is_some_and(|site| !site.installed),
                "the repair at {} removed its trap, but the site is installed",
                group.address
            );
        }
    }

    fn check_threads(&self, shutting_down: bool) {
        for (&pid, thread) in &self.threads {
            let id = debug_thread_id(pid);
            debug_assert!(
                !self.retired_threads.contains(&pid),
                "thread {id} is both live and retired"
            );
            debug_assert!(
                !self.vanished_threads.contains(&pid),
                "thread {id} is both live and vanished"
            );
            debug_assert!(
                !self.unseized_threads.contains(&pid),
                "thread {id} is both live and unseized"
            );
            debug_assert!(
                !self.unowned_stops.contains_key(&pid),
                "thread {id} is live and its stop is recorded as unowned"
            );
            if let Some(address) = thread.stopped_at_breakpoint {
                debug_assert!(
                    self.breakpoints.contains_key(&address),
                    "thread {id} is stopped at a breakpoint at {address}, which has no site"
                );
            }

            debug_assert!(
                thread
                    .armed
                    .is_none_or(|armed| armed <= self.watch.generation),
                "thread {id} is armed with watch plan {:?}, past the current plan {}",
                thread.armed,
                self.watch.generation
            );
            if !shutting_down {
                debug_assert!(
                    !matches!(thread.state, NativeThreadState::StopRequested)
                        || self.barrier.is_some(),
                    "thread {id} was asked to stop with no stop in progress"
                );
            }
        }

        for pid in &self.vfork_children {
            debug_assert!(
                self.fork_children.contains_key(pid),
                "vfork child {pid} is marked but no longer awaits its first stop"
            );
        }

        for pid in self.fork_children.keys() {
            debug_assert!(
                !self.killed_children.contains(pid),
                "fork child {pid} is both awaiting its first stop and killed"
            );
        }

        if !shutting_down && let Some(active) = &self.active {
            for pid in &active.resume_threads {
                debug_assert!(
                    self.threads.contains_key(pid),
                    "execution {} resumes thread {}, which is gone",
                    active.id,
                    debug_thread_id(*pid)
                );
            }
        }

        if let Some(barrier) = &self.barrier {
            debug_assert!(
                !barrier.paused || barrier.reason.is_some(),
                "a paused stop has no reason to publish"
            );
            if !shutting_down {
                debug_assert!(
                    self.threads.contains_key(&barrier.triggering_thread),
                    "the stop in progress was triggered by thread {}, which is gone",
                    debug_thread_id(barrier.triggering_thread)
                );
            }
        }

        if !shutting_down {
            debug_assert!(
                !self.threads.is_empty(),
                "inferior {} has no threads",
                self.tgid
            );
        }
    }

    fn check_stop(&self) {
        let Some(stop) = &self.public_stop else {
            return;
        };

        debug_assert!(
            self.barrier.is_none(),
            "stop {} is published while another stop is in progress",
            stop.id
        );
        debug_assert!(
            self.active.as_ref().is_none(),
            "stop {} is published while execution {:?} is active",
            stop.id,
            self.active.as_ref().map(|active| active.id)
        );
        debug_assert!(
            self.repairs.is_empty(),
            "stop {} is published with {} breakpoint repairs outstanding",
            stop.id,
            self.repairs.len()
        );
        for &pid in stop.presentations.keys() {
            debug_assert!(
                self.threads.contains_key(&pid),
                "stop {} presents thread {}, which is gone",
                stop.id,
                debug_thread_id(pid)
            );
        }

        if let Some(pid) = stop.selected_thread {
            debug_assert!(
                self.threads.contains_key(&pid),
                "stop {} selects thread {}, which is gone",
                stop.id,
                debug_thread_id(pid)
            );
        }
    }

    /// A watchpoint holds debug-register slots exactly while it is enabled
    /// and its task's stack is not moving. A stop in progress may still be
    /// about to follow a move.
    fn check_watches(&self) {
        let plan = &self.watch.plan;

        for slot in plan.slots().iter().flatten() {
            for id in &slot.owners {
                debug_assert!(
                    self.watch.watchpoints.contains_key(id),
                    "a debug-register slot is owned by watchpoint {id}, which is gone"
                );
            }
        }

        if self.barrier.is_some() {
            return;
        }

        for (&id, record) in &self.watch.watchpoints {
            let planned = plan
                .slots()
                .iter()
                .flatten()
                .any(|slot| slot.owners.contains(&id));
            let set_aside = self.stack_moves.sets_aside(record.task);
            debug_assert_eq!(
                planned,
                record.watchpoint.enabled && !set_aside,
                "watchpoint {id} holds slots: {planned}, but enabled: {}, set aside \
                 while its stack moves: {set_aside}",
                record.watchpoint.enabled
            );
        }
    }
}
