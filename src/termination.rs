use std::io;
use std::task::Poll;

use tokio::signal::unix::{SignalKind, signal};

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
