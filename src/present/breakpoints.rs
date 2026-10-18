//! Pending runtime arming, described consistently by every client.

use std::collections::BTreeSet;

use microscope::Breakpoint;

pub fn pending_message(breakpoint: &Breakpoint) -> Option<String> {
    if breakpoint.pending_locations.is_empty() {
        return None;
    }
    let reasons = breakpoint
        .pending_locations
        .iter()
        .map(|pending| pending.reason.as_ref())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join("; ");
    let what = if breakpoint.ready_locations().next().is_some() {
        "some locations pending confirmation"
    } else {
        "runtime configuration pending confirmation"
    };
    Some(format!("{what}: {reasons}"))
}
