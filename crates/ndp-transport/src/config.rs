//! Transport tuning.

use std::sync::Arc;
use std::time::Duration;

/// ALPN for a client↔agent media session (possibly via a relay).
pub const ALPN_SESSION: &[u8] = b"ndp/3";
/// ALPN for an agent's persistent outbound control tunnel to a gateway.
pub const ALPN_AGENT_GATEWAY: &[u8] = b"ndp-gw/3";
/// ALPN for the relay's own byte-forwarding protocol.
pub const ALPN_RELAY: &[u8] = b"ndp-relay/3";

/// Tunables for a QUIC endpoint.
///
/// The defaults target interactive remote desktop, not bulk transfer: short
/// idle timeouts so a dead peer is noticed in seconds, and enough flow-control
/// credit that a 4K keyframe burst is never throttled by the window.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Close the connection after this long without any traffic.
    pub idle_timeout: Duration,
    /// Send a PING at this interval to keep NAT bindings alive.
    ///
    /// Must be comfortably below the smallest NAT UDP timeout in the wild
    /// (~30 s), otherwise the mapping is dropped and the agent becomes
    /// unreachable without either side noticing.
    pub keep_alive: Duration,
    /// Largest sealed record accepted on any carrier.
    pub max_record: usize,
    /// Per-connection receive window.
    pub receive_window: u64,
    /// Per-stream receive window.
    pub stream_receive_window: u64,
    /// Concurrent unidirectional streams the peer may open.
    ///
    /// One video frame is one stream, so this bounds how many frames may be
    /// in flight before the sender is blocked.
    pub max_concurrent_uni: u32,
    /// Concurrent bidirectional streams the peer may open.
    pub max_concurrent_bi: u32,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_secs(15),
            keep_alive: Duration::from_secs(5),
            max_record: 16 * 1024 * 1024,
            receive_window: 32 * 1024 * 1024,
            stream_receive_window: 8 * 1024 * 1024,
            max_concurrent_uni: 256,
            max_concurrent_bi: 16,
        }
    }
}

impl TransportConfig {
    /// Build the quinn transport parameters this configuration describes.
    pub(crate) fn to_quinn(&self) -> Arc<quinn::TransportConfig> {
        let mut t = quinn::TransportConfig::default();
        t.max_idle_timeout(Some(self.idle_timeout.try_into().unwrap_or_else(|_| {
            quinn::IdleTimeout::from(quinn::VarInt::from_u32(15_000))
        })));
        t.keep_alive_interval(Some(self.keep_alive));
        t.receive_window(varint(self.receive_window));
        t.stream_receive_window(varint(self.stream_receive_window));
        t.max_concurrent_uni_streams(self.max_concurrent_uni.into());
        t.max_concurrent_bidi_streams(self.max_concurrent_bi.into());
        // BBR keeps the bottleneck queue short instead of filling it, which
        // matters far more than throughput here: a Cubic-style loss-based
        // controller would happily add hundreds of milliseconds of standing
        // queue and make the cursor feel detached from the hand.
        t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
        // Interactive traffic is bursty by nature (a keyframe follows a long
        // idle gap); restarting from the initial window every time would show
        // up as a visible stutter after every pause.
        t.initial_rtt(Duration::from_millis(100));
        Arc::new(t)
    }
}

fn varint(v: u64) -> quinn::VarInt {
    quinn::VarInt::from_u64(v).unwrap_or(quinn::VarInt::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_build_a_valid_quinn_config() {
        let _ = TransportConfig::default().to_quinn();
    }

    #[test]
    fn keep_alive_stays_under_common_nat_timeouts() {
        let cfg = TransportConfig::default();
        assert!(
            cfg.keep_alive < Duration::from_secs(25),
            "NAT bindings would expire"
        );
        assert!(
            cfg.keep_alive < cfg.idle_timeout,
            "a healthy link would time itself out"
        );
    }

    #[test]
    fn alpns_are_distinct() {
        let mut all = [ALPN_SESSION, ALPN_AGENT_GATEWAY, ALPN_RELAY];
        all.sort_unstable();
        let before = all.len();
        let mut dedup = all.to_vec();
        dedup.dedup();
        assert_eq!(dedup.len(), before);
    }
}
