//! Interrupts request a pause in every isolate before stopping all threads
//! natively, including those running no JavaScript. Pause requests cannot
//! be withdrawn; late V8 pauses are absorbed.

use super::super::Controller;
use super::super::cdp::{Connection, Session};
use super::super::native::LinuxTraceOps;
use super::{Isolate, PauseRequest, send};
use crate::Result;
use crate::runtime_model::inspector_protocol;

impl<P: LinuxTraceOps> Controller<P> {
    /// Requests pauses in all connected isolates and arms the native-stop
    /// deadline. Returns whether any pause was requested.
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

    /// Stops all threads natively if V8 has not paused before the deadline,
    /// as when no JavaScript runs.
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

/// Requests a pause, deferred until resume completes if needed.
/// Returns whether the request was sent or deferred.
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
