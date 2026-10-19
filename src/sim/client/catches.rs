//! The user's catchpoints: stopping at the system calls the golden runtime
//! makes, or at every call, at either end, with conditions the kernel can
//! judge too.

use std::sync::Arc;

use super::{Client, protocol};
use crate::sim::choices::Stream;
use crate::sim::kernel::catching::UserCatch;
use crate::sim::marks::Mark;
use crate::sim::report::Failure;
use crate::syscalls::{Abi, SyscallId};
use crate::{CatchPhase, CatchpointId, Condition};

/// The calls the golden runtime makes.
const CALLS: [&str; 9] = [
    "write",
    "sched_yield",
    "getpid",
    "clone",
    "fork",
    "exit",
    "wait4",
    "getppid",
    "exit_group",
];

impl Client {
    /// Catches up to two of the runtime's calls, or every call, at a
    /// chosen end, with a chosen condition.
    pub(super) async fn add_catch(&self) -> Result<(), Failure> {
        let mut calls = Vec::new();
        for _ in 0..self.draw(3) {
            let name = *self.choices.borrow_mut().pick(Stream::Client, &CALLS);
            let call = SyscallId::named(Abi::X86_64, name)
                .ok_or_else(|| Failure::simulator("client", format!("no call {name}")))?;
            calls.push(call);
        }

        let phases = [
            CatchPhase::Entry,
            CatchPhase::Exit,
            CatchPhase::Both,
            CatchPhase::Both,
        ];
        let phase = *self.choices.borrow_mut().pick(Stream::Client, &phases);
        let (text, arg0, ret) = match self.draw(4) {
            0 => (Some("$arg0 == 1"), Some(1), None),
            1 => (Some("$ret == 0"), None, Some(0)),
            _ => (None, None, None),
        };

        let condition = text
            .map(Condition::parse)
            .transpose()
            .map_err(|error| Failure::simulator("client", error.to_string()))?;

        let rule = UserCatch {
            calls: calls.iter().map(|call| u64::from(call.number)).collect(),
            entry: phase.stops(false),
            exit: phase.stops(true),
            arg0,
            ret,
        };

        self.note(format!("catch {calls:?} at {phase:?} if {text:?}"));
        let catchpoint = self
            .handle
            .add_catchpoint(Arc::from(calls), phase, condition)
            .await
            .map_err(|error| protocol(format!("catching failed: {error}")))?;

        self.note(format!("catchpoint {}", catchpoint.id));
        self.shared
            .catches
            .borrow_mut()
            .insert(catchpoint.id.get(), rule);

        self.mark(Mark::CatchAdded);
        Ok(())
    }

    /// Removes one of the catchpoints the debugger made.
    pub(super) async fn remove_catch(&self) -> Result<(), Failure> {
        let ids = self
            .shared
            .catches
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(());
        }

        let id = *self.choices.borrow_mut().pick(Stream::Client, &ids);
        self.note(format!("remove catchpoint {id}"));

        // From the request on, the catchpoint may be gone.
        self.shared.catches.borrow_mut().remove(&id);
        self.handle
            .remove_catchpoint(CatchpointId::new(id))
            .await
            .map_err(|error| protocol(format!("removing catchpoint {id} failed: {error}")))?;

        self.mark(Mark::CatchRemoved);
        Ok(())
    }
}
