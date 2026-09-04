//! Byte forwarding between two spliced QUIC connections.
//!
//! The relay mirrors QUIC's own structure rather than flattening it into a
//! single byte pipe: a stream on one side becomes a stream on the other, a
//! datagram becomes a datagram, and a reset becomes a reset. That fidelity is
//! not cosmetic. The session protocol relies on one video frame per
//! unidirectional stream so that a lost frame cannot head-of-line block the
//! next one, and on datagrams for audio so that a late packet is dropped
//! rather than retransmitted. Collapsing either into one ordered pipe would
//! reintroduce exactly the latency the design exists to avoid.
//!
//! The relay never learns anything about the content. Client and agent run a
//! Noise handshake end to end over this pipe, so every byte the relay copies
//! is already ciphertext, and the relay holds no key that could open it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use quinn::{Connection, ReadError, RecvStream, SendStream, VarInt};
use tracing::{debug, trace};

/// How much of a stream to move in one copy step.
///
/// Large enough that a 4K keyframe is a handful of iterations, small enough
/// that a few hundred concurrent sessions do not commit a gigabyte of buffers.
const CHUNK: usize = 64 * 1024;

/// The code used when tearing down a mirrored stream after an unclear error.
const ERROR_CODE: VarInt = VarInt::from_u32(0x11);

/// Byte counters for one spliced session.
#[derive(Debug, Default)]
pub struct Counters {
    /// Bytes forwarded from the client towards the agent.
    pub client_to_agent: AtomicU64,
    /// Bytes forwarded from the agent towards the client.
    pub agent_to_client: AtomicU64,
}

impl Counters {
    /// A snapshot of both counters.
    #[must_use]
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.client_to_agent.load(Ordering::Relaxed),
            self.agent_to_client.load(Ordering::Relaxed),
        )
    }
}

/// Forward traffic between two connections until either one ends.
///
/// Returns once either side closes, at which point the other is closed too:
/// leaving a half-open session would hold a slot and let the peer keep
/// sending into nothing.
pub async fn splice(client: Connection, agent: Connection, counters: Arc<Counters>) {
    let c2a = counters.clone();
    let a2c = counters.clone();

    // Six independent pumps rather than one loop: each direction and each
    // carrier makes progress on its own, so a stalled bulk transfer cannot
    // delay an input event.
    tokio::select! {
        () = pump_uni(client.clone(), agent.clone(), c2a.clone(), true) => {},
        () = pump_uni(agent.clone(), client.clone(), a2c.clone(), false) => {},
        () = pump_bi(client.clone(), agent.clone(), c2a.clone(), true) => {},
        () = pump_bi(agent.clone(), client.clone(), a2c.clone(), false) => {},
        () = pump_datagrams(client.clone(), agent.clone(), c2a, true) => {},
        () = pump_datagrams(agent.clone(), client.clone(), a2c, false) => {},
    }

    let (up, down) = counters.snapshot();
    debug!(bytes_up = up, bytes_down = down, "session unspliced");
    client.close(VarInt::from_u32(0), b"peer closed");
    agent.close(VarInt::from_u32(0), b"peer closed");
}

async fn pump_uni(from: Connection, to: Connection, counters: Arc<Counters>, upstream: bool) {
    loop {
        let Ok(recv) = from.accept_uni().await else {
            return;
        };
        let Ok(send) = to.open_uni().await else {
            return;
        };
        let counters = counters.clone();
        tokio::spawn(async move { copy(recv, send, counters, upstream, true).await });
    }
}

async fn pump_bi(from: Connection, to: Connection, counters: Arc<Counters>, upstream: bool) {
    loop {
        let Ok((from_send, from_recv)) = from.accept_bi().await else {
            return;
        };
        let Ok((to_send, to_recv)) = to.open_bi().await else {
            return;
        };
        let forward = counters.clone();
        let backward = counters.clone();
        // Only the side that opened the stream writes the prefix, so only
        // the forward direction has an urgency to read.
        tokio::spawn(async move { copy(from_recv, to_send, forward, upstream, true).await });
        tokio::spawn(async move { copy(to_recv, from_send, backward, !upstream, false).await });
    }
}

async fn pump_datagrams(from: Connection, to: Connection, counters: Arc<Counters>, upstream: bool) {
    loop {
        let Ok(datagram) = from.read_datagram().await else {
            return;
        };
        count(&counters, upstream, datagram.len() as u64);
        // A datagram that does not fit or arrives while the path is
        // congested is dropped, exactly as it would be end to end. Audio
        // recovers with in-band FEC; queueing it here would only add delay.
        if let Err(e) = to.send_datagram(datagram) {
            trace!(error = %e, "dropped a datagram");
        }
    }
}

/// How many bytes of prefix carry the urgency, and where in them it sits.
const PREFIX: usize = 4;
const URGENCY: usize = 2;

async fn copy(
    mut recv: RecvStream,
    mut send: SendStream,
    counters: Arc<Counters>,
    upstream: bool,
    prefixed: bool,
) {
    // The payload is sealed and the relay has no key for it, so the sender
    // states in the clear how the stream should be scheduled. Without this
    // the relay forwards everything at the same standing and undoes, on the
    // hop it owns, the ordering both endpoints agreed on: a keyframe ends up
    // sharing the link evenly with the frames that depend on it, arrives
    // last, and is useless by the time it lands.
    if prefixed {
        let mut prefix = [0u8; PREFIX];
        if recv.read_exact(&mut prefix).await.is_err() {
            let _ = send.reset(ERROR_CODE);
            return;
        }
        let _ = send.set_priority(i32::from(prefix[URGENCY]));
        count(&counters, upstream, PREFIX as u64);
        if send.write_all(&prefix).await.is_err() {
            let _ = recv.stop(ERROR_CODE);
            return;
        }
    }

    loop {
        match recv.read_chunk(CHUNK, true).await {
            Ok(Some(chunk)) => {
                count(&counters, upstream, chunk.bytes.len() as u64);
                if send.write_chunk(chunk.bytes).await.is_err() {
                    // The far side is gone; stop reading so the near side
                    // learns about it through flow control rather than
                    // buffering into a void.
                    let _ = recv.stop(ERROR_CODE);
                    return;
                }
            }
            Ok(None) => {
                let _ = send.finish();
                return;
            }
            Err(ReadError::Reset(code)) => {
                // Propagating the reset verbatim is what lets the sender
                // abandon a stale video frame and have the receiver actually
                // discard it, instead of waiting for bytes that never come.
                let _ = send.reset(code);
                return;
            }
            Err(_) => {
                let _ = send.reset(ERROR_CODE);
                return;
            }
        }
    }
}

fn count(counters: &Counters, upstream: bool, n: u64) {
    if upstream {
        counters.client_to_agent.fetch_add(n, Ordering::Relaxed);
    } else {
        counters.agent_to_client.fetch_add(n, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_track_each_direction_separately() {
        let c = Counters::default();
        count(&c, true, 100);
        count(&c, false, 40);
        count(&c, true, 5);
        assert_eq!(c.snapshot(), (105, 40));
    }
}
