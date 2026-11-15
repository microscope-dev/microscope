use std::sync::Arc;

use crate::protocol::BreakpointSpec;
use crate::{BreakpointLocation, Error, Result, VirtualAddress};

use super::breakpoints::{remove_breakpoint_owner_from, runtime_breakpoint_address};
use super::native::LinuxTraceOps;
use super::{BreakpointOwner, Controller, Edit, Inferior};

const LOADER_HOOK: &str = "_dl_debug_state";

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn ensure_loader_breakpoint(&mut self) -> Result<()> {
        let Some(inferior) = self.session.inferior() else {
            return Ok(());
        };

        if inferior.loader_site.is_some() || inferior.unfollowed_exec.is_some() || inferior.observer
        {
            return Ok(());
        }

        let Some(address) = self
            .modules
            .values()
            .filter(|module| module.loaded.id != self.main.id)
            .find_map(|module| {
                let symbol = module.image.symbol_named(LOADER_HOOK).ok()?;
                module.loaded.virtual_address(symbol.address()).ok()
            })
        else {
            return Ok(());
        };

        let inferior = self.session.inferior_mut().ok_or(Error::NotRunning)?;
        let pid = inferior.memory_thread();
        self.ptrace.install_breakpoint(
            pid,
            &mut inferior.breakpoints,
            address,
            BreakpointOwner::Loader,
        )?;
        inferior.loader_site = Some(address);
        Ok(())
    }

    pub(super) fn is_loader_site(&self, address: VirtualAddress) -> bool {
        self.session
            .inferior()
            .is_some_and(|inferior| inferior.loader_site == Some(address))
    }

    pub(super) fn queue_module_refresh(&mut self) -> Result<()> {
        self.queue_internal_edit(Edit::RefreshModules)
    }

    pub(super) fn refresh_libraries(&mut self) -> Result<()> {
        let phase = crate::span!("modules");
        let lost_locations = self.refresh_modules()?;
        drop(phase);
        self.ensure_loader_breakpoint()?;
        self.sync_runtime_hooks()?;
        self.sync_stack_movers()?;
        self.sync_interpreted_movers()?;
        self.learn_from_got(None);
        self.sync_resolvers()?;
        self.sync_inspector_hooks()?;
        let locations_changed = self.reresolve_breakpoints(lost_locations)?;
        self.arm_new_agents();
        let arming_changed = self.sync_runtime_arming();

        if locations_changed || arming_changed {
            self.publish_breakpoints_changed();
        }
        Ok(())
    }

    #[must_use = "publish changed locations after arming them"]
    fn reresolve_breakpoints(&mut self, mut changed: bool) -> Result<bool> {
        for index in 0..self.breakpoints.len() {
            let Some(breakpoint) = self.breakpoints.get(index) else {
                break;
            };

            if !breakpoint.enabled || matches!(breakpoint.spec, BreakpointSpec::Address(_)) {
                continue;
            }

            let resolved = self
                .resolve_breakpoint(breakpoint.id, breakpoint.spec.clone(), true)
                .map_or_else(|_| Arc::from([]), |resolved| resolved.locations);
            if resolved == breakpoint.locations {
                continue;
            }

            let id = breakpoint.id;
            let owner = BreakpointOwner::User(id);
            let listed = self
                .breakpoints
                .get_mut(index)
                .ok_or_else(|| Error::internal("a re-resolved breakpoint stays listed"))?;
            let previous = std::mem::replace(&mut listed.locations, Arc::clone(&resolved));
            changed = true;
            let Some(inferior) = self.session.inferior_mut() else {
                continue;
            };

            for location in previous
                .iter()
                .filter(|location| !resolved.contains(location))
            {
                if let BreakpointLocation::Managed(managed) = &location.location {
                    inferior.managed.release(managed, id);
                    continue;
                }

                let Some(address) = runtime_breakpoint_address(inferior, &location.location)?
                else {
                    continue;
                };

                if location
                    .library
                    .is_some_and(|library| !self.modules.contains_key(&library))
                {
                    forget_owner(inferior, address, owner)?;
                } else {
                    remove_breakpoint_owner_from(&self.ptrace, inferior, address, owner)?;
                }
            }

            for location in resolved
                .iter()
                .filter(|location| !previous.contains(location))
            {
                if let BreakpointLocation::Managed(managed) = &location.location {
                    inferior.managed.own(managed, id);
                    continue;
                }

                let Some(address) = runtime_breakpoint_address(inferior, &location.location)?
                else {
                    continue;
                };
                let pid = inferior.memory_thread();
                self.ptrace
                    .install_breakpoint(pid, &mut inferior.breakpoints, address, owner)?;
            }
        }

        Ok(changed)
    }
}

impl<P: super::native::InspectionOps> Controller<P> {
    pub(super) fn drop_library_locations(&mut self) -> bool {
        let mut changed = false;
        for breakpoint in &mut self.breakpoints {
            if breakpoint
                .locations
                .iter()
                .any(|location| location.library.is_some())
            {
                breakpoint.locations = breakpoint
                    .locations
                    .iter()
                    .filter(|location| location.library.is_none())
                    .cloned()
                    .collect();
                changed = true;
            }
        }

        changed
    }
}

fn forget_owner(
    inferior: &mut Inferior,
    address: VirtualAddress,
    owner: BreakpointOwner,
) -> Result<()> {
    let Some(site) = inferior.breakpoints.get_mut(&address) else {
        return Ok(());
    };

    site.owners.remove(&owner);
    if site.owners.is_empty() {
        let original_byte = site.original_byte;
        inferior.breakpoints.remove(&address);
        super::breakpoints::forget_removed_site(inferior, address, original_byte)?;
    }

    Ok(())
}
