use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::protocol::BreakpointId;
use crate::runtime_model::{Arming, ManagedArm};
use crate::{ManagedLocation, RuntimeId};

use super::Controller;
use super::native::LinuxTraceOps;

#[derive(Debug, Default)]
pub(super) struct ManagedSites {
    sites: BTreeMap<ManagedLocation, ManagedSite>,

    next: u64,

    stale: BTreeSet<RuntimeId>,

    pub(super) unarmed: BTreeMap<RuntimeId, Arc<str>>,
}

#[derive(Debug)]
struct ManagedSite {
    number: u64,
    owners: BTreeSet<BreakpointId>,
}

impl ManagedSites {
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

    pub(super) fn number(&self, location: &ManagedLocation) -> Option<u64> {
        self.sites.get(location).map(|site| site.number)
    }

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

    pub(super) fn take_stale(&mut self) -> BTreeSet<RuntimeId> {
        std::mem::take(&mut self.stale)
    }

    pub(super) fn mark_stale(&mut self, runtime: RuntimeId) {
        self.stale.insert(runtime);
    }

    pub(super) fn is_stale(&self, runtime: RuntimeId) -> bool {
        self.stale.contains(&runtime)
    }

    pub(super) fn owns(&self, location: &ManagedLocation, owner: BreakpointId) -> bool {
        self.sites
            .get(location)
            .is_some_and(|site| site.owners.contains(&owner))
    }

    pub(super) fn owned(
        &self,
    ) -> impl Iterator<Item = (&ManagedLocation, &BTreeSet<BreakpointId>)> {
        self.sites
            .iter()
            .map(|(location, site)| (location, &site.owners))
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    #[must_use]
    pub(super) fn sync_runtime_arming(&mut self) -> bool {
        let Some(inferior) = self.session.inferior_mut() else {
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

    pub(super) fn arm_runtime(&mut self, runtime: RuntimeId) -> Result<(), Arc<str>> {
        let result = self.prepare_runtime_arming(runtime);
        if let Some(inferior) = self.session.inferior_mut() {
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
        self.session
            .inferior()?
            .managed
            .unarmed
            .get(&runtime)
            .cloned()
    }

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
            .session
            .inferior()
            .ok_or_else(|| Arc::<str>::from("the process is gone"))?;
        let breakpoints = inferior.managed.arms(runtime);
        let step = inferior
            .runtime_step
            .as_ref()
            .filter(|(stepping, _)| *stepping == runtime)
            .map(|(_, step)| step.clone());
        let assignment = inferior
            .runtime_assignment
            .as_ref()
            .filter(|(assigning, _)| *assigning == runtime)
            .map(|(_, assignment)| assignment.clone());

        let Some(bound) = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
        else {
            return if breakpoints.is_empty() && step.is_none() && assignment.is_none() {
                Ok(())
            } else {
                Err("the runtime is no longer loaded".into())
            };
        };

        let arming = Arming {
            breakpoints: &breakpoints,
            step: step.as_ref(),
            assignment: assignment.as_ref(),
            exceptions: self.exception_stops,
        };

        if bound.model.inspector_hook().is_some() {
            return self.arm_inspector(bound.id, &arming);
        }
        if bound.model.agent().is_some() {
            return self.arm_agent(&bound, &arming);
        }

        let writes =
            self.with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                bound.model.arm(stop, &arming)
            })?;
        self.write_for_runtime(&writes)
    }

    pub(super) fn rearm_reporting_runtimes(&mut self) {
        let Some(inferior) = self.session.inferior() else {
            return;
        };

        let reporting = self
            .runtimes(inferior)
            .into_iter()
            .filter(|runtime| {
                !runtime.model.internal_hooks().is_empty()
                    || runtime.model.inspector_hook().is_some()
            })
            .map(|runtime| runtime.id)
            .collect::<Vec<_>>();
        if let Some(inferior) = self.session.inferior_mut() {
            for runtime in reporting {
                inferior.managed.mark_stale(runtime);
            }
        }

        if self.sync_runtime_arming() {
            self.publish_breakpoints_changed();
        }
    }
}
