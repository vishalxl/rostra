use std::future::Future;

use futures::stream::{FuturesUnordered, StreamExt as _};

/// Race at most four reads without discarding peers that lack buffer capacity.
///
/// Acquire before consuming a peer from the pending iterator. If active reads
/// own capacity, wait for one to finish and retry the same pending peer.
/// Return a temporary pause only when no owned read can make progress.
pub(crate) async fn race_payload_reads<P: Copy, G, T, E, F: Future<Output = Option<T>>>(
    peers: &[P],
    mut reserve: impl FnMut() -> Result<G, E>,
    mut attempt: impl FnMut(P, G) -> F,
) -> Result<Option<T>, E> {
    let mut pending = peers.iter().copied().peekable();
    let mut active = FuturesUnordered::new();
    loop {
        while active.len() < 4 {
            let Some(peer) = pending.peek().copied() else {
                break;
            };
            let guard = match reserve() {
                Ok(guard) => guard,
                Err(reason) if active.is_empty() => return Err(reason),
                Err(_) => break,
            };
            pending.next();
            active.push(attempt(peer, guard));
        }
        match active.next().await {
            Some(Some(winner)) => return Ok(Some(winner)),
            Some(None) => {}
            None => return Ok(None),
        }
    }
}

#[cfg(test)]
mod tests;
