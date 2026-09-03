//! Signalling messages.
//!
//! Three conversations use this framing, all of them low-rate and all of them
//! off the media path, so JSON's readability is worth more than a few saved
//! bytes:
//!
//! - **agent → gateway**: a persistent outbound tunnel. The agent dials the
//!   gateway and keeps the connection open, so a machine behind any NAT or
//!   firewall is reachable without a single inbound port.
//! - **client → gateway**: one short exchange to redeem a session ticket.
//! - **peer → relay**: a single pairing token, then raw bytes.

use nebula_common::{MachineId, SessionId};
use serde::{Deserialize, Serialize};

/// The largest signalling message accepted.
///
/// Signalling messages are small by construction; a generous but finite cap
/// stops a peer from making a gateway allocate without bound before it has
/// authenticated anything.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// The first message an agent sends on a new control tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHello {
    /// The agent's machine id.
    pub machine_id: MachineId,
    /// Its machine credential, `<id>.<secret>`, as issued at enrolment.
    pub credential: String,
    /// Agent build version, for diagnostics and staged rollouts.
    pub agent_version: String,
}

/// The gateway's answer to [`AgentHello`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum AgentHelloAck {
    /// The tunnel is established.
    Accepted {
        /// The gateway's own id, which the agent reports in its heartbeats so
        /// the manager can place sessions on the gateway already holding the
        /// tunnel.
        gateway_id: uuid::Uuid,
        /// How often the agent should send [`AgentMessage::Heartbeat`].
        heartbeat_secs: u64,
    },
    /// The tunnel was refused. The agent should not retry immediately.
    Rejected {
        /// A short, non-sensitive reason.
        reason: String,
    },
}

/// Anything an agent sends to its gateway after the handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum AgentMessage {
    /// Periodic liveness and status.
    Heartbeat {
        /// Sessions the agent currently believes are active.
        active_sessions: Vec<SessionId>,
    },
    /// The agent has connected to the relay and the session is live.
    SessionActive {
        /// Which session.
        session: SessionId,
    },
    /// The agent could not, or would no longer, serve a session.
    SessionFailed {
        /// Which session.
        session: SessionId,
        /// A short, non-sensitive reason.
        reason: String,
    },
    /// A session ended normally.
    SessionClosed {
        /// Which session.
        session: SessionId,
        /// Bytes the agent sent to the client.
        bytes_sent: u64,
        /// Bytes the agent received from the client.
        bytes_received: u64,
    },
}

/// Anything a gateway sends down an agent's control tunnel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum GatewayMessage {
    /// Connect to a relay and serve a session.
    ///
    /// The agent is told nothing about the user: authorisation was decided by
    /// the manager and is carried in the policy, and the identity that matters
    /// on the wire is proven by the Noise handshake that follows.
    StartSession(SessionRequest),
    /// Stop serving a session, whether or not it ever started.
    StopSession {
        /// Which session.
        session: SessionId,
        /// A short, non-sensitive reason.
        reason: String,
    },
    /// Liveness check; the agent replies with a heartbeat.
    Ping,
}

/// Instructions for one session, sent to the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    /// The session to serve.
    pub session: SessionId,
    /// The resource being launched, so the agent knows what to capture.
    pub resource_id: uuid::Uuid,
    /// What the session is permitted to do.
    pub policy: nebula_common::SessionPolicy,
    /// The role the client was granted.
    pub role: nebula_common::SessionRole,
    /// QUIC address of the relay to connect to.
    pub relay_addr: String,
    /// Certificate pin for that relay, hex SHA-256.
    pub relay_pin: String,
    /// The pairing token proving the agent may join this session.
    pub pair_token: String,
    /// The client's Noise static public key, hex encoded.
    ///
    /// Lets the agent complete a mutually authenticated handshake, so a relay
    /// that spliced in the wrong connection is detected rather than served.
    pub client_key: String,
}

/// The first message a client sends to a gateway.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    /// The session ticket issued by the manager.
    pub ticket: String,
    /// The client's Noise static public key, hex encoded.
    pub noise_public_key: String,
    /// Client build version.
    pub client_version: String,
}

/// The gateway's answer to [`ClientHello`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum ClientHelloAck {
    /// The session was set up; connect to the relay with this token.
    Accepted {
        /// Which session.
        session: SessionId,
        /// QUIC address of the relay.
        relay_addr: String,
        /// Certificate pin for that relay, hex SHA-256.
        relay_pin: String,
        /// The pairing token proving the client may join this session.
        pair_token: String,
        /// The agent's Noise static public key, hex encoded.
        agent_key: String,
    },
    /// The session could not be set up.
    Rejected {
        /// A short, non-sensitive reason.
        reason: String,
    },
}

/// The single message a peer sends to a relay before the pipe goes raw.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayHello {
    /// The pairing token minted by the gateway.
    pub pair_token: String,
}

/// The relay's answer, sent once the peer's other half has arrived.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum RelayHelloAck {
    /// Both halves are present; the pipe is spliced and now carries raw bytes.
    Spliced,
    /// The pairing failed.
    Rejected {
        /// A short, non-sensitive reason.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_messages_round_trip_with_their_tag() {
        let msg = AgentMessage::SessionActive {
            session: SessionId::new(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""t":"SessionActive""#));
        let back: AgentMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentMessage::SessionActive { .. }));
    }

    #[test]
    fn gateway_messages_round_trip_with_their_tag() {
        let msg = GatewayMessage::StopSession {
            session: SessionId::new(),
            reason: "revoked".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: GatewayMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, GatewayMessage::StopSession { .. }));
    }

    #[test]
    fn an_unknown_variant_is_an_error_not_a_silent_default() {
        // A peer inventing a message type must be rejected rather than
        // quietly interpreted as something else.
        assert!(serde_json::from_str::<AgentMessage>(r#"{"t":"Wat"}"#).is_err());
        assert!(serde_json::from_str::<GatewayMessage>(r#"{"t":"Wat"}"#).is_err());
    }

    #[test]
    fn a_session_request_carries_everything_the_agent_needs() {
        let req = SessionRequest {
            session: SessionId::new(),
            resource_id: uuid::Uuid::now_v7(),
            policy: nebula_common::SessionPolicy::full(),
            role: nebula_common::SessionRole::Controller,
            relay_addr: "relay.test:7444".into(),
            relay_pin: "ab".repeat(32),
            pair_token: "token".into(),
            client_key: "cd".repeat(32),
        };
        let back: SessionRequest =
            serde_json::from_str(&serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(back.session, req.session);
        assert_eq!(back.policy, req.policy);
        assert_eq!(back.client_key, req.client_key);
    }
}
