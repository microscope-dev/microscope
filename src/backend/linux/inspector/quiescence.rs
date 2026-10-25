//! A watchdog iteration posts both a libuv async and a V8 interrupt. `Agent::Stop`
//! only clears `io_`; either posting can reopen it. Observe the semaphore wait's
//! return and its next call (also when `RequestIoThreadStart` was inlined), then
//! a whole main-thread `CheckImmediate` invocation. Its normal native drain has
//! returned, including any stack-local interrupt queue; Node 26's empty-queue
//! fast path is safe too. libuv clears `async.pending` before calling `async_cb`,
//! so `pending=0` is checked only after that outer check phase, at all-stop.
//! As required by libuv, `uv_run` must not be called reentrantly.

use nix::unistd::Pid;

use crate::backend::linux::native::LinuxTraceOps;
use crate::backend::linux::{BreakpointOwner, Controller, Edit};
use crate::{Error, Result, RuntimeId, VirtualAddress};

#[derive(Debug)]
pub(super) struct Startup {
    sites: [VirtualAddress; 3],
    posting: Option<Pid>,
    posted: bool,
    drain: Option<Drain>,
}

#[derive(Debug, Default)]
struct Drain {
    check: Option<Call>,
    returned: bool,
    verified: bool,
}

#[derive(Debug, Clone, Copy)]
struct Call {
    pid: Pid,
    stack: u64,
    returns_to: VirtualAddress,
}

impl Call {
    fn returned(self, pid: Pid, address: VirtualAddress, stack: u64) -> bool {
        pid == self.pid && address == self.returns_to && stack == self.stack.wrapping_add(8)
    }
}

impl Startup {
    pub(super) fn sites(&self) -> Vec<VirtualAddress> {
        let mut sites = self.sites[..2].to_vec();
        if let Some(drain) = &self.drain {
            sites.extend_from_slice(&self.sites[2..]);
            sites.extend(drain.check.map(|call| call.returns_to));
        }
        sites
    }

    pub(super) fn begin_drain(&mut self) {
        self.drain = Some(Drain::default());
    }

    fn verified(&self) -> bool {
        self.posted && self.drain.as_ref().is_some_and(|drain| drain.verified)
    }

    fn observe(&mut self, address: VirtualAddress, call: Call) -> bool {
        let [before, after, check] = self.sites;
        let pid = call.pid;
        if address == after {
            self.posting = Some(pid);
            self.posted = false;
            if let Some(drain) = self.drain.as_mut() {
                *drain = Drain::default();
            }
            record!("{pid} begins inspector startup postings");
            return false;
        }
        if address == before {
            if self.posting == Some(pid) {
                self.posted = true;
                record!("{pid} completed inspector startup postings");
            }
            return false;
        }
        let Some(drain) = self.drain.as_mut() else {
            return false;
        };
        if drain
            .check
            .is_some_and(|frame| frame.returned(pid, address, call.stack))
        {
            drain.check = None;
            drain.returned = true;
            record!("{pid} completed the outer check-phase native drain");
            true
        } else if address == check && self.posted && drain.check.is_none() && !drain.returned {
            drain.check = Some(call);
            true
        } else {
            false
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn prepare_inspector_startup(
        &mut self,
        bound: &crate::backend::linux::runtimes::BoundRuntime,
    ) -> Result<()> {
        let addresses = bound.model.inspector_startup_hooks().ok_or_else(|| {
            Error::RuntimeAgent("Node's inspector startup quiescence cannot be proven".into())
        })?;
        let sites = addresses
            .map(|address| bound.module.virtual_address(address))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let sites: [VirtualAddress; 3] = sites
            .try_into()
            .map_err(|_| Error::internal("three inspector startup checkpoints"))?;
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for &site in &sites[..2] {
            self.ptrace.install_breakpoint(
                inferior.memory_thread(),
                &mut inferior.breakpoints,
                site,
                BreakpointOwner::InspectorHook,
            )?;
            inferior.inspectors.startup_planted.insert(site);
        }
        inferior.inspectors.startup.insert(
            bound.id,
            Startup {
                sites,
                posting: None,
                posted: false,
                drain: None,
            },
        );
        Ok(())
    }

    pub(super) fn install_inspector_startup_hooks(&mut self) -> Result<()> {
        let inferior = self.inferior.as_mut().ok_or(Error::NotRunning)?;
        for address in inferior
            .inspectors
            .startup
            .values()
            .flat_map(Startup::sites)
        {
            self.ptrace.install_breakpoint(
                inferior.memory_thread(),
                &mut inferior.breakpoints,
                address,
                BreakpointOwner::InspectorHook,
            )?;
            inferior.inspectors.startup_planted.insert(address);
        }
        Ok(())
    }

    pub(super) fn note_inspector_startup_hook(
        &mut self,
        pid: Pid,
        address: VirtualAddress,
    ) -> Result<bool> {
        let Some(inferior) = self.inferior.as_ref() else {
            return Ok(false);
        };
        let Some(runtime) = inferior
            .inspectors
            .startup
            .iter()
            .find_map(|(runtime, startup)| startup.sites().contains(&address).then_some(*runtime))
        else {
            return Ok(false);
        };
        if pid != inferior.tgid
            && inferior
                .inspectors
                .startup
                .get(&runtime)
                .is_some_and(|startup| address == startup.sites[2])
        {
            return Ok(true);
        }
        let registers = self.ptrace.registers(pid)?;
        let call = Call {
            pid,
            stack: registers.rsp,
            returns_to: VirtualAddress::new(self.ptrace.read_word(pid, registers.rsp)?),
        };
        let startup = self
            .inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .inspectors
            .startup
            .get_mut(&runtime)
            .ok_or(Error::NotRunning)?;
        if startup.observe(address, call) {
            self.queue_internal_edit(Edit::InspectorCleanup)?;
        }
        Ok(true)
    }

    pub(super) fn inspector_startup_verified(&self, runtime: RuntimeId) -> bool {
        self.inferior.as_ref().is_some_and(|inferior| {
            inferior
                .inspectors
                .startup
                .get(&runtime)
                .is_none_or(Startup::verified)
        })
    }

    pub(super) fn verify_inspector_startup(
        &mut self,
        runtime: RuntimeId,
        agent: VirtualAddress,
    ) -> Result<bool> {
        let inferior = self.inferior.as_ref().ok_or(Error::NotRunning)?;
        let Some(startup) = inferior.inspectors.startup.get(&runtime) else {
            return Ok(true);
        };
        if !startup.posted || !startup.drain.as_ref().is_some_and(|drain| drain.returned) {
            return Ok(false);
        }
        let bound = self
            .runtimes(inferior)
            .into_iter()
            .find(|bound| bound.id == runtime)
            .ok_or(Error::NotRunning)?;
        let pending = self
            .with_runtime_stop(inferior, &bound, inferior.memory_thread(), |stop| {
                bound.model.inspector_startup_pending(stop, agent)
            })
            .map_err(Error::RuntimeAgent)?;
        let drain = self
            .inferior
            .as_mut()
            .ok_or(Error::NotRunning)?
            .inspectors
            .startup
            .get_mut(&runtime)
            .and_then(|startup| startup.drain.as_mut())
            .ok_or(Error::NotRunning)?;
        drain.verified = !pending;
        drain.returned = !pending;
        if !pending {
            record!("inspector startup async is clear after the outer interrupt drain");
        }
        Ok(!pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup() -> Startup {
        Startup {
            sites: [1, 2, 3].map(VirtualAddress::new),
            posting: None,
            posted: false,
            drain: Some(Drain::default()),
        }
    }

    fn call(pid: i32, stack: u64) -> Call {
        Call {
            pid: Pid::from_raw(pid),
            stack,
            returns_to: VirtualAddress::new(4),
        }
    }

    #[test]
    fn startup_needs_a_completed_posting_cycle_before_a_whole_outer_check() {
        let mut startup = startup();
        assert!(!startup.observe(VirtualAddress::new(1), call(10, 200)));
        assert!(
            !startup.posted,
            "the watchdog's initial wait is not completion"
        );
        assert!(!startup.observe(VirtualAddress::new(3), call(11, 100)));
        startup.observe(VirtualAddress::new(2), call(10, 200));
        startup.observe(VirtualAddress::new(1), call(12, 200));
        assert!(
            !startup.posted,
            "a different thread cannot complete the posting"
        );
        startup.observe(VirtualAddress::new(1), call(10, 200));
        assert!(startup.posted);
        assert!(startup.observe(VirtualAddress::new(3), call(11, 100)));
        assert!(!startup.observe(VirtualAddress::new(3), call(11, 80)));
        assert!(!startup.observe(VirtualAddress::new(4), call(11, 88)));
        assert!(!startup.observe(VirtualAddress::new(4), call(12, 108)));
        assert!(!startup.drain.as_ref().unwrap().returned);
        assert!(startup.observe(VirtualAddress::new(4), call(11, 108)));
        assert!(startup.drain.as_ref().unwrap().returned);
        assert!(
            !startup.verified(),
            "the pending flag still needs an all-stop read"
        );
        startup.drain.as_mut().unwrap().verified = true;
        assert!(startup.verified());
        startup.observe(VirtualAddress::new(2), call(10, 200));
        assert!(
            !startup.verified(),
            "a new posting invalidates the drain proof"
        );
        startup.observe(VirtualAddress::new(1), call(10, 200));
        assert!(startup.posted);
        assert!(!startup.verified());
    }
}
