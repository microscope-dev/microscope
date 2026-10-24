//! Interrupts that pause running JavaScript: an interrupt asks every
//! isolate to pause and waits a moment for one to, before every thread is
//! stopped natively, as one that runs no JavaScript must be (F19). A pause
//! asked for cannot be withdrawn, so one V8 takes late is absorbed.

use super::super::Controller;
use super::super::cdp::{Connection, Session};
use super::super::native::LinuxTraceOps;
use super::{Isolate, PauseRequest, send};
use crate::Result;
use crate::runtime_model::inspector_protocol;

impl<P: LinuxTraceOps> Controller<P> {
    /// Asks every isolate of every connected inspector to pause its
    /// running JavaScript for a client's interrupt, and arms the deadline
    /// after which every thread is stopped natively: whether any was asked.
    pub(in crate::backend::linux) fn interrupt_javascript(&mut self) -> bool {
        let Some(inferior) = self.inferior.as_mut() else {
            return false;
        };
        // A second interrupt does not wait again.
        if inferior.inspectors.end_interrupt() {
            return false;
        }
        let mut asked = false;
        for inspector in inferior.inspectors.records.values_mut() {
            let Some(link) = inspector.connection.as_ref().filter(|link| link.connected) else {
                continue;
            };
            for (session, isolate) in &mut inspector.isolates {
                asked |= ask_pause(&link.connection, session, isolate);
            }
        }
        if asked {
            inferior.inspectors.interrupt = Some(std::time::Instant::now() + INTERRUPT_WAIT);
        }
        asked
    }

    /// Stops every thread natively once an interrupt's deadline passes
    /// with no pause from V8, as when no JavaScript runs (F19).
    pub(in crate::backend::linux) fn interrupt_expired(
        &mut self,
        now: std::time::Instant,
    ) -> Result<()> {
        let Some(inferior) = self.inferior.as_mut() else {
            return Ok(());
        };
        if inferior
            .inspectors
            .interrupt
            .is_none_or(|deadline| deadline > now)
        {
            return Ok(());
        }
        inferior.inspectors.end_interrupt();
        record!("V8 did not pause in time; every thread stops natively");
        if inferior.active.is_none() {
            return Ok(());
        }
        self.pause_natively()
    }
}

/// Asks the isolate `session` reaches to pause, at once or once it leaves
/// the pause it is leaving: whether it was asked.
pub(super) fn ask_pause(connection: &Connection, session: &Session, isolate: &mut Isolate) -> bool {
    if isolate.resuming {
        isolate.pause = PauseRequest::Deferred;
        return true;
    }
    let sent = send(
        connection,
        session,
        inspector_protocol::PAUSE,
        serde_json::json!({}),
    )
    .is_ok();
    if sent {
        isolate.pause = PauseRequest::Sent;
    }
    sent
}

/// How long an interrupt waits for V8 to pause running JavaScript, which
/// takes about a millisecond (F19), before every thread stops natively.
const INTERRUPT_WAIT: std::time::Duration = std::time::Duration::from_millis(50);
