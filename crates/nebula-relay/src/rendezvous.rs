//! The rendezvous table.
//!
//! Two peers arrive independently and in either order. The first to arrive
//! parks; the second finds it and splices the pair. Everything here is
//! in-memory and per-process: a relay holds no durable state, so it can be
//! restarted or replaced at any time, and the worst consequence is that
//! in-flight sessions reconnect.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ndp_signal::Side;
use nebula_common::SessionId;
use quinn::Connection;
use tokio::sync::{oneshot, Mutex};
use tracing::warn;

use crate::splice::Counters;

/// How long a peer waits for its counterpart before giving up.
///
/// Matched to the pairing token's lifetime: waiting longer would keep a slot
/// alive for a token that can no longer be redeemed anyway.
pub const RENDEZVOUS_TIMEOUT: Duration = Duration::from_secs(30);

/// What happened when a peer presented itself.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Both halves are present and forwarding has started.
    Spliced,
    /// The counterpart never arrived.
    TimedOut,
    /// Another connection already claimed this side of the session.
    Duplicate,
    /// The peer disconnected while waiting.
    Abandoned,
}

struct Waiting {
    side: Side,
    connection: Connection,
    ready: oneshot::Sender<()>,
}

/// The set of half-open sessions waiting for a counterpart.
#[derive(Default)]
pub struct Rendezvous {
    waiting: Mutex<HashMap<SessionId, Waiting>>,
}

impl std::fmt::Debug for Rendezvous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rendezvous").finish_non_exhaustive()
    }
}

impl Rendezvous {
    /// How many sessions are currently waiting for a counterpart.
    pub async fn pending(&self) -> usize {
        self.waiting.lock().await.len()
    }

    /// Present one half of a session and wait until it is spliced.
    ///
    /// Returns as soon as forwarding has started, so the caller can
    /// acknowledge the peer and stop touching the connection.
    pub async fn join(
        self: &Arc<Self>,
        session: SessionId,
        side: Side,
        connection: Connection,
    ) -> Outcome {
        let receiver = {
            let mut waiting = self.waiting.lock().await;
            match waiting.remove(&session) {
                // The counterpart is here: splice and release it.
                Some(peer) if peer.side == side.peer() => {
                    let (client, agent) = match side {
                        Side::Client => (connection, peer.connection),
                        Side::Agent => (peer.connection, connection),
                    };
                    // The first arrival is told to acknowledge only after the
                    // pumps exist, so neither peer can start sending into a
                    // connection nothing is reading yet.
                    let counters = Arc::new(Counters::default());
                    tokio::spawn(crate::splice::splice(client, agent, counters));
                    let _ = peer.ready.send(());
                    return Outcome::Spliced;
                }
                // Same side twice: someone is replaying a token, or a peer
                // reconnected without the gateway reissuing one. Put the
                // original back — the honest peer is more likely the one
                // already waiting.
                Some(peer) => {
                    waiting.insert(session, peer);
                    return Outcome::Duplicate;
                }
                None => {
                    let (tx, rx) = oneshot::channel();
                    waiting.insert(
                        session,
                        Waiting {
                            side,
                            connection: connection.clone(),
                            ready: tx,
                        },
                    );
                    rx
                }
            }
        };

        // Give up if the counterpart never shows, or if this peer vanishes
        // first; either way the slot must not leak.
        let outcome = tokio::select! {
            result = tokio::time::timeout(RENDEZVOUS_TIMEOUT, receiver) => match result {
                Ok(Ok(())) => return Outcome::Spliced,
                Ok(Err(_)) => Outcome::Abandoned,
                Err(_) => Outcome::TimedOut,
            },
            _ = connection.closed() => Outcome::Abandoned,
        };

        // Only remove our own entry: by now the slot may hold a newer
        // connection for the same session.
        let mut waiting = self.waiting.lock().await;
        if let Some(entry) = waiting.get(&session) {
            if entry.side == side && entry.connection.stable_id() == connection.stable_id() {
                waiting.remove(&session);
            }
        }
        if outcome == Outcome::TimedOut {
            warn!(%session, ?side, "counterpart never arrived");
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fresh_table_holds_nothing() {
        let table = Arc::new(Rendezvous::default());
        assert_eq!(table.pending().await, 0);
    }

    #[test]
    fn outcomes_are_distinguishable() {
        // The relay reports these back to the peer, so conflating them would
        // make a replayed token look like a slow counterpart.
        assert_ne!(Outcome::Spliced, Outcome::Duplicate);
        assert_ne!(Outcome::TimedOut, Outcome::Abandoned);
    }
}
