//! The signals that ask microscope to end, for the CLI, the debug adapter,
//! and the web server.

use std::io;
use std::task::Poll;

use tokio::signal::unix::{SignalKind, signal};

/// Completes with whichever of `kinds` arrives first. Each is caught from
/// this call on, before the future is polled, so one that arrives in
/// between waits for it instead of taking its default action.
pub fn first_of<const N: usize>(
    kinds: [SignalKind; N],
) -> io::Result<impl Future<Output = SignalKind>> {
    let mut signals = Vec::with_capacity(N);
    for kind in kinds {
        signals.push((kind, signal(kind)?));
    }

    Ok(std::future::poll_fn(move |context| {
        for (kind, signal) in &mut signals {
            if signal.poll_recv(context) == Poll::Ready(Some(())) {
                return Poll::Ready(*kind);
            }
        }

        Poll::Pending
    }))
}
