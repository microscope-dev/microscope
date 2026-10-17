//! Shared libraries: following the dynamic loader as it maps and unmaps
//! them, and keeping breakpoints resolved in them.
//!
//! The loader calls `_dl_debug_state` before and after each change to the
//! loaded libraries, as gdb relies on. A breakpoint there stops every thread
//! internally, brings the module registry up to date, re-resolves function
//! and source breakpoints across every loaded image, and resumes; no stop
//! is published.

use std::sync::Arc;

use crate::protocol::BreakpointSpec;
use crate::{BreakpointLocation, Error, Result, VirtualAddress};

use super::breakpoints::{remove_breakpoint_owner_from, runtime_breakpoint_address};
use super::native::LinuxTraceOps;
use super::{BreakpointOwner, Controller, Edit, Inferior};

/// The loader function called around each change to the loaded libraries.
const LOADER_HOOK: &str = "_dl_debug_state";

impl<P: LinuxTraceOps> Controller<P> {
    /// Installs the loader's breakpoint once a module that defines the hook,
    /// the dynamic loader, is loaded. A program without one loads no
    /// libraries after it starts.
    pub(super) fn ensure_loader_breakpoint(&mut self) -> Result<()> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(());
        };

        if inferior.loader_site.is_some() || inferior.exec_unsupported {
            return Ok(());
        }

        let Some(address) = self
            .modules
            .values()
            .filter(|module| module.loaded.id != crate::ModuleId::new(0))
            .find_map(|module| {
                let symbol = module.image.symbol_named(LOADER_HOOK).ok()?;
                module.loaded.virtual_address(symbol.address()).ok()
            })
        else {
            return Ok(());
        };

        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
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

    /// Whether a site is the loader's breakpoint.
    pub(super) fn is_loader_site(&self, address: VirtualAddress) -> bool {
        self.inferior
            .as_ref()
            .is_some_and(|inferior| inferior.loader_site == Some(address))
    }

    /// Brings modules and breakpoints up to date once every thread is
    /// stopped, after the loader reported a change.
    pub(super) fn queue_module_refresh(&mut self) -> Result<()> {
        self.queue_internal_edit(Edit::RefreshModules)
    }

    /// Brings the module registry, the loader's breakpoint, and every
    /// breakpoint's locations up to date while every thread is stopped.
    pub(super) fn refresh_libraries(&mut self) -> Result<()> {
        let phase = crate::span!("modules");
        let lost_locations = self.refresh_modules()?;
        drop(phase);
        self.ensure_loader_breakpoint()?;
        self.sync_runtime_hooks()?;
        self.sync_stack_movers()?;
        self.learn_from_got(None);
        self.sync_resolvers()?;
        self.reresolve_breakpoints(lost_locations)?;
        self.sync_runtime_arming();
        Ok(())
    }

    /// Re-resolves function and source breakpoints across the loaded
    /// images: a library that loaded may add locations, and one that
    /// unloaded takes its locations with it, which leaves a breakpoint with
    /// none pending until a module with code for it loads. Publishes the
    /// change, or one already `changed`.
    fn reresolve_breakpoints(&mut self, mut changed: bool) -> Result<()> {
        for index in 0..self.breakpoints.len() {
            let breakpoint = &self.breakpoints[index];

            // A disabled breakpoint is resolved again when it is enabled.
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
            let previous = std::mem::replace(
                &mut self.breakpoints[index].locations,
                Arc::clone(&resolved),
            );
            changed = true;
            let Some(inferior) = self.inferior.as_mut() else {
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

                // An unmapped library's trap went with its memory.
                if location
                    .library
                    .is_some_and(|library| !self.modules.contains_key(&library))
                {
                    forget_owner(inferior, address, owner);
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

        if changed {
            self.publish_breakpoints_changed();
        }

        Ok(())
    }
}

impl<P: super::native::InspectionOps> Controller<P> {
    /// Drops every breakpoint's locations in shared libraries, whose
    /// addresses mean nothing once their process is gone.
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

/// Removes an owner from a site whose memory is gone, without restoring
/// the bytes its trap replaced.
fn forget_owner(inferior: &mut Inferior, address: VirtualAddress, owner: BreakpointOwner) {
    let Some(site) = inferior.breakpoints.get_mut(&address) else {
        return;
    };

    site.owners.remove(&owner);
    if site.owners.is_empty() {
        let original_byte = site.original_byte;
        inferior.breakpoints.remove(&address);
        super::breakpoints::forget_removed_site(inferior, address, original_byte);
    }
}
