//! Breakpoint locations language runtimes manage. The debugger keeps no
//! trap at one: the runtime arms each and reports a hit through one of its
//! internal hooks. Ownership follows the native split, as for a trap site:
//! the site record says which user breakpoints own each location. A
//! runtime is armed whole, with its locations, its step, and which
//! exceptions stop, whenever any of them changes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::protocol::BreakpointId;
use crate::runtime_model::{Arming, ManagedArm};
use crate::{ManagedLocation, RuntimeId};

use super::Controller;
use super::native::LinuxTraceOps;

/// The managed locations user breakpoints own, and how each runtime's
/// arming stands.
#[derive(Debug, Default)]
pub(super) struct ManagedSites {
    sites: BTreeMap<ManagedLocation, ManagedSite>,

    /// The number the next new site takes. A site's number is never given
    /// to another in the same process, so a hit a runtime reports for a
    /// site since removed names none.
    next: u64,

    /// The runtimes whose arming changed since they were last armed.
    stale: BTreeSet<RuntimeId>,

    /// Why each runtime that could not be armed was not.
    pub(super) unarmed: BTreeMap<RuntimeId, Arc<str>>,
}

/// One managed location and the breakpoints that own it.
#[derive(Debug)]
struct ManagedSite {
    /// The number the runtime reports a hit of the site by.
    number: u64,
    owners: BTreeSet<BreakpointId>,
}

impl ManagedSites {
    /// Gives `owner` the location, numbering it if it is new.
    pub(super) fn own(&mut self, location: &ManagedLocation, owner: BreakpointId) {
        if let Some(site) = self.sites.get_mut(location) {
            site.owners.insert(owner);
            return;
        }

        self.sites.insert(
            location.clone(),
            ManagedSite {
                number: self.next,
                owners: BTreeSet::from([owner]),
            },
        );
        self.next = self.next.saturating_add(1);
        self.stale.insert(location.runtime);
    }

    /// Takes the location from `owner`, and forgets it once no breakpoint
    /// owns it.
    pub(super) fn release(&mut self, location: &ManagedLocation, owner: BreakpointId) {
        let Some(site) = self.sites.get_mut(location) else {
            return;
        };

        site.owners.remove(&owner);
        if site.owners.is_empty() {
            self.sites.remove(location);
            self.stale.insert(location.runtime);
        }
    }

    /// The location the runtime `runtime` numbers `number`, and the
    /// breakpoints that own it.
    pub(super) fn site(
        &self,
        runtime: RuntimeId,
        number: u64,
    ) -> Option<(&ManagedLocation, &BTreeSet<BreakpointId>)> {
        self.sites
            .iter()
            .find(|(location, site)| location.runtime == runtime && site.number == number)
            .map(|(location, site)| (location, &site.owners))
    }

    /// What the runtime `runtime` is to be armed with.
    pub(super) fn arms(&self, runtime: RuntimeId) -> Vec<ManagedArm> {
        self.sites
            .iter()
            .filter(|(location, _)| location.runtime == runtime)
            .map(|(location, site)| ManagedArm {
                number: site.number,
                place: location.place.clone(),
            })
            .collect()
    }

    /// The runtimes whose arming changed since they were last armed,
    /// which are then counted armed.
    pub(super) fn take_stale(&mut self) -> BTreeSet<RuntimeId> {
        std::mem::take(&mut self.stale)
    }

    /// Marks the runtime's arming changed, so that it is armed again.
    pub(super) fn mark_stale(&mut self, runtime: RuntimeId) {
        self.stale.insert(runtime);
    }

    /// Whether `owner` owns the location.
    pub(super) fn owns(&self, location: &ManagedLocation, owner: BreakpointId) -> bool {
        self.sites
            .get(location)
            .is_some_and(|site| site.owners.contains(&owner))
    }

    /// Every location and its owners, for the invariant check.
    pub(super) fn owned(
        &self,
    ) -> impl Iterator<Item = (&ManagedLocation, &BTreeSet<BreakpointId>)> {
        self.sites
            .iter()
            .map(|(location, site)| (location, &site.owners))
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Arms each runtime whose arming changed with all of it, while every
    /// thread is stopped. A runtime that cannot be armed keeps why, and is
    /// armed again at the next change.
    /// Returns whether the public arming reasons changed. A configuration
    /// edit publishes its whole change; standalone rearming must notify it.
    #[must_use]
    pub(super) fn sync_runtime_arming(&mut self) -> bool {
        let Some(inferior) = self.inferior.as_mut() else {
            return false;
        };
        let mut changed = false;
        for runtime in inferior.managed.take_stale() {
            let previous = self.arming_problem(runtime);
            let _ = self.arm_runtime(runtime);
            changed |= previous != self.arming_problem(runtime);
        }
        changed
    }

    /// Arms the runtime `runtime` with everything its internal hooks are to
    /// report: its managed locations, its step, and which exceptions stop.
    pub(super) fn arm_runtime(&mut self, runtime: RuntimeId) -> Result<(), Arc<str>> {
        let result = self.prepare_runtime_arming(runtime);
        if let Some(inferior) = self.inferior.as_mut() {
            match &result {
                Ok(()) => {
                    inferior.managed.unarmed.remove(&runtime);
                }
                Err(reason) => {
                    record!("runtime {runtime} is not armed: {reason}");
                    inferior.managed.unarmed.insert(runtime, Arc::clone(reason));
                }
            }
        }
        result
    }

    fn arming_problem(&self, runtime: RuntimeId) -> Option<Arc<str>> {
        self.inferior
            .as_ref()?
            .managed
            .unarmed
            .get(&runtime)
            .cloned()
    }

    /// Publishes only the final readiness change of a standalone operation,
    /// including any rollback of an unsuccessful step request.
    pub(super) fn with_arming_notification<T>(
        &mut self,
        runtime: RuntimeId,
        operation: impl FnOnce(&mut Self) -> T,
    ) -> T {
        let previous = self.arming_problem(runtime);
        let result = operation(self);
        if previous != self.arming_problem(runtime) {
            self.publish_breakpoints_changed();
        }
        result
    }

    fn prepare_runtime_arming(&mut self, runtime: RuntimeId) -> Result<(), Arc<str>> {
        let inferior = self
            .inferior
            .as_ref()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let breakpoints = inferior.managed.arms(runtime);
        let step = inferior
            .runtime_step
            .as_ref()
            .filter(|(stepping, _)| *stepping == runtime)
            .map(|(_, step)| step);
        let Some(bound) = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
        else {
            // A runtime that is gone took what it was armed with.
            return if breakpoints.is_empty() && step.is_none() {
                Ok(())
            } else {
                Err("the runtime is no longer loaded".into())
            };
        };

        let arming = Arming {
            breakpoints: &breakpoints,
            step,
            exceptions: self.exception_stops,
        };
        let writes =
            self.with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                bound.model.arm(stop, &arming)
            })?;
        self.write_for_runtime(&writes)
    }

    /// Marks every loaded runtime that reports events through internal
    /// hooks to be armed again, as when which exceptions stop changed.
    pub(super) fn rearm_reporting_runtimes(&mut self) {
        let Some(inferior) = self.inferior.as_ref() else {
            return;
        };

        let reporting = self
            .runtimes(inferior)
            .into_iter()
            .filter(|runtime| !runtime.model.internal_hooks().is_empty())
            .map(|runtime| runtime.id)
            .collect::<Vec<_>>();
        if let Some(inferior) = self.inferior.as_mut() {
            for runtime in reporting {
                inferior.managed.mark_stale(runtime);
            }
        }

        if self.sync_runtime_arming() {
            self.publish_breakpoints_changed();
        }
    }
}
