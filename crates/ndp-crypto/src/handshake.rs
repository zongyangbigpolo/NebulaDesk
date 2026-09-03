//! The Noise IK handshake that bootstraps a session.

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use crate::keys::{PublicKey, StaticKeypair};
use crate::record::{RecordOpener, RecordSealer, SessionKeys};
use crate::{CryptoError, Result};

/// The Noise pattern NebulaDesk speaks.
pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

/// Largest handshake payload accepted, bounding memory before authentication.
/// A session ticket is a few hundred bytes; 4 KiB is generous.
pub const MAX_HANDSHAKE_PAYLOAD: usize = 4096;

/// Buffer size for one Noise message: payload plus pattern overhead.
const MSG_BUF: usize = MAX_HANDSHAKE_PAYLOAD + 256;

pub(crate) fn noise_params() -> NoiseParams {
    NOISE_PARAMS
        .parse()
        .expect("the compiled-in Noise pattern is valid")
}

fn map_err(e: snow::Error) -> CryptoError {
    CryptoError::Handshake(e.to_string())
}

/// A completed handshake: the peer's payload plus the session's record layer.
pub struct HandshakeResult {
    /// The payload the peer sent in its final handshake message.
    pub peer_payload: Vec<u8>,
    /// The peer's static public key, cryptographically proven.
    ///
    /// For the responder this is how the client's ephemeral session identity
    /// is learned; for the initiator it confirms the agent it expected.
    pub peer_static: Option<PublicKey>,
    /// Seals outbound records.
    pub sealer: RecordSealer,
    /// Opens inbound records.
    pub opener: RecordOpener,
}

impl std::fmt::Debug for HandshakeResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandshakeResult")
            .field("peer_payload_len", &self.peer_payload.len())
            .field("peer_static", &self.peer_static)
            .finish_non_exhaustive()
    }
}

fn finish(
    mut hs: HandshakeState,
    peer_payload: Vec<u8>,
    is_initiator: bool,
) -> Result<HandshakeResult> {
    let peer_static = hs
        .get_remote_static()
        .map(PublicKey::from_slice)
        .transpose()?;

    // Take the raw traffic keys rather than Noise transport mode: NDP needs
    // explicit per-channel nonces so that audio can ride lossy datagrams.
    let (k1, k2) = hs.dangerously_get_raw_split();
    let keys = if is_initiator {
        SessionKeys { send: k1, recv: k2 }
    } else {
        SessionKeys { send: k2, recv: k1 }
    };
    let (sealer, opener) = keys.split();

    Ok(HandshakeResult {
        peer_payload,
        peer_static,
        sealer,
        opener,
    })
}

/// The client side of the handshake.
///
/// It knows the agent's static public key in advance (from the session
/// ticket), so the very first message is already encrypted to that agent and
/// an impostor relay cannot read it.
pub struct Initiator {
    hs: HandshakeState,
}

impl Initiator {
    /// Start a handshake towards `remote_static`.
    ///
    /// `prologue` is mixed into the transcript and must match on both sides;
    /// bind it to the session id so a handshake cannot be transplanted into a
    /// different session.
    pub fn new(local: &StaticKeypair, remote_static: &PublicKey, prologue: &[u8]) -> Result<Self> {
        let hs = Builder::new(noise_params())
            .local_private_key(local.secret_bytes())
            .remote_public_key(remote_static.as_bytes())
            .prologue(prologue)
            .build_initiator()
            .map_err(map_err)?;
        Ok(Self { hs })
    }

    /// Produce the first handshake message, carrying `payload`.
    ///
    /// `payload` is where the manager-signed session ticket travels: it is
    /// already encrypted to the agent's static key at this point.
    pub fn write_first(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() > MAX_HANDSHAKE_PAYLOAD {
            return Err(CryptoError::PayloadTooLarge(payload.len()));
        }
        let mut buf = vec![0u8; MSG_BUF];
        let n = self.hs.write_message(payload, &mut buf).map_err(map_err)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume the agent's reply and derive the session keys.
    pub fn read_second(mut self, message: &[u8]) -> Result<HandshakeResult> {
        if message.len() > MSG_BUF {
            return Err(CryptoError::PayloadTooLarge(message.len()));
        }
        let mut buf = vec![0u8; MSG_BUF];
        let n = self.hs.read_message(message, &mut buf).map_err(map_err)?;
        buf.truncate(n);
        if !self.hs.is_handshake_finished() {
            return Err(CryptoError::Handshake(
                "peer reply did not complete the IK pattern".into(),
            ));
        }
        finish(self.hs, buf, true)
    }
}

/// The agent side of the handshake.
pub struct Responder {
    hs: HandshakeState,
    client_payload: Vec<u8>,
}

impl Responder {
    /// Prepare to accept a handshake using the agent's long-lived keypair.
    pub fn new(local: &StaticKeypair, prologue: &[u8]) -> Result<Self> {
        let hs = Builder::new(noise_params())
            .local_private_key(local.secret_bytes())
            .prologue(prologue)
            .build_responder()
            .map_err(map_err)?;
        Ok(Self {
            hs,
            client_payload: Vec::new(),
        })
    }

    /// Read the client's first message and recover its payload.
    ///
    /// Decryption succeeding already proves the sender encrypted to *this*
    /// agent's static key. The recovered payload — the session ticket — must
    /// still be verified by the caller before the session is allowed.
    pub fn read_first(&mut self, message: &[u8]) -> Result<&[u8]> {
        if message.len() > MSG_BUF {
            return Err(CryptoError::PayloadTooLarge(message.len()));
        }
        let mut buf = vec![0u8; MSG_BUF];
        let n = self.hs.read_message(message, &mut buf).map_err(map_err)?;
        buf.truncate(n);
        self.client_payload = buf;
        Ok(&self.client_payload)
    }

    /// Produce the reply and derive the session keys.
    ///
    /// Returns the bytes to send back to the client alongside the completed
    /// handshake. Call only after the ticket recovered by
    /// [`Self::read_first`] has been accepted.
    pub fn write_second(mut self, payload: &[u8]) -> Result<(Vec<u8>, HandshakeResult)> {
        if payload.len() > MAX_HANDSHAKE_PAYLOAD {
            return Err(CryptoError::PayloadTooLarge(payload.len()));
        }
        let mut buf = vec![0u8; MSG_BUF];
        let n = self.hs.write_message(payload, &mut buf).map_err(map_err)?;
        buf.truncate(n);
        let reply = buf;

        if !self.hs.is_handshake_finished() {
            return Err(CryptoError::Handshake(
                "IK pattern not complete after the reply".into(),
            ));
        }
        // The responder never receives a payload of its own: the client's
        // payload was already surfaced by `read_first`.
        let result = finish(self.hs, Vec::new(), false)?;
        Ok((reply, result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndp_proto::{Channel, MsgHeader, MsgKind};

    struct Handshaken {
        client: HandshakeResult,
        agent: HandshakeResult,
    }

    impl std::fmt::Debug for Handshaken {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Handshaken")
        }
    }

    fn run(prologue: &[u8], ticket: &[u8]) -> Result<Handshaken> {
        let agent_kp = StaticKeypair::generate();
        run_with(&agent_kp, agent_kp.public(), prologue, prologue, ticket)
    }

    fn run_with(
        agent_kp: &StaticKeypair,
        client_expects: PublicKey,
        client_prologue: &[u8],
        agent_prologue: &[u8],
        ticket: &[u8],
    ) -> Result<Handshaken> {
        let client_kp = StaticKeypair::generate();

        let mut initiator = Initiator::new(&client_kp, &client_expects, client_prologue)?;
        let msg1 = initiator.write_first(ticket)?;

        let mut responder = Responder::new(agent_kp, agent_prologue)?;
        let seen_ticket = responder.read_first(&msg1)?.to_vec();
        assert_eq!(seen_ticket, ticket);

        let (msg2, agent) = responder.write_second(b"agent-hello")?;
        let client = initiator.read_second(&msg2)?;
        assert_eq!(client.peer_payload, b"agent-hello");

        Ok(Handshaken { client, agent })
    }

    #[test]
    fn handshake_establishes_a_working_record_layer() {
        let h = run(b"session-1", b"ticket-bytes").unwrap();

        let record = h
            .client
            .sealer
            .seal(
                Channel::Control,
                MsgHeader::new(MsgKind::Hello, 0, 1),
                b"hello agent",
            )
            .unwrap();
        let (_, plaintext) = h.agent.opener.open(Channel::Control, &record).unwrap();
        assert_eq!(plaintext, b"hello agent");

        let back = h
            .agent
            .sealer
            .seal(
                Channel::Video,
                MsgHeader::new(MsgKind::VideoFrame, 0, 2),
                b"frame",
            )
            .unwrap();
        let (_, plaintext) = h.client.opener.open(Channel::Video, &back).unwrap();
        assert_eq!(plaintext, b"frame");
    }

    #[test]
    fn client_learns_the_agents_proven_static_key() {
        let agent_kp = StaticKeypair::generate();
        let h = run_with(&agent_kp, agent_kp.public(), b"p", b"p", b"t").unwrap();
        assert_eq!(h.client.peer_static, Some(agent_kp.public()));
        // The agent also learns the client's session identity.
        assert!(h.agent.peer_static.is_some());
    }

    #[test]
    fn an_impostor_agent_cannot_complete_the_handshake() {
        let real_agent = StaticKeypair::generate();
        let impostor = StaticKeypair::generate();
        // The client encrypts to the real agent's key; the impostor holds a
        // different secret and cannot decrypt the first message.
        let err = run_with(&impostor, real_agent.public(), b"p", b"p", b"t").unwrap_err();
        assert!(matches!(err, CryptoError::Handshake(_)));
    }

    #[test]
    fn mismatched_prologue_breaks_the_handshake() {
        let agent_kp = StaticKeypair::generate();
        let err = run_with(
            &agent_kp,
            agent_kp.public(),
            b"session-a",
            b"session-b",
            b"t",
        )
        .unwrap_err();
        assert!(matches!(err, CryptoError::Handshake(_)));
    }

    #[test]
    fn oversized_handshake_payloads_are_refused() {
        let agent_kp = StaticKeypair::generate();
        let client_kp = StaticKeypair::generate();
        let mut initiator = Initiator::new(&client_kp, &agent_kp.public(), b"p").unwrap();
        let huge = vec![0u8; MAX_HANDSHAKE_PAYLOAD + 1];
        assert!(matches!(
            initiator.write_first(&huge),
            Err(CryptoError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn corrupted_first_message_is_rejected() {
        let agent_kp = StaticKeypair::generate();
        let client_kp = StaticKeypair::generate();
        let mut initiator = Initiator::new(&client_kp, &agent_kp.public(), b"p").unwrap();
        let mut msg1 = initiator.write_first(b"ticket").unwrap();
        let last = msg1.len() - 1;
        msg1[last] ^= 0xff;

        let mut responder = Responder::new(&agent_kp, b"p").unwrap();
        assert!(responder.read_first(&msg1).is_err());
    }

    #[test]
    fn two_sessions_derive_different_keys() {
        let agent_kp = StaticKeypair::generate();
        let a = run_with(&agent_kp, agent_kp.public(), b"s1", b"s1", b"t").unwrap();
        let b = run_with(&agent_kp, agent_kp.public(), b"s2", b"s2", b"t").unwrap();

        let record = a
            .client
            .sealer
            .seal(Channel::Control, MsgHeader::new(MsgKind::Ping, 0, 0), b"x")
            .unwrap();
        assert!(
            b.agent.opener.open(Channel::Control, &record).is_err(),
            "keys must not be reusable across sessions"
        );
    }
}
