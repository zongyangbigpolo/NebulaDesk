//! Direct-path negotiation carried inside an already authenticated session.

use std::net::{IpAddr, SocketAddr};

use ndp_proto::control::{CandidateKind, ControlMessage, DirectPathBinding, PathCandidate};
use nebula_common::SessionId;
use serde::{Deserialize, Serialize};

use crate::SignalError;

/// The authenticated Noise greeting accepting multipath record envelopes.
pub const MULTIPATH_GREETING: &[u8] = b"agent-ready multipath/1";
/// A separate ALPN keeps direct session listeners distinct from edge services.
pub const ALPN_DIRECT: &[u8] = b"ndp-direct/3";
/// Maximum number of explicitly advertised addresses; discovery is not a scan.
pub const MAX_DIRECT_ADDRESSES: usize = 8;

/// Optional capabilities in the initial, encrypted Noise handshake payload.
/// Older agents accept this nonempty payload but return their legacy greeting.
#[derive(Serialize, Deserialize)]
pub struct SessionHello {
    /// The manager-issued session ticket.
    pub ticket: String,
    /// Whether the client understands multipath envelope version 1.
    pub multipath: bool,
}

/// A per-session listener offer, sent as an encrypted `PathCandidates` record.
#[derive(Debug, Clone)]
pub struct DirectOffer {
    /// Wire schema version, currently 1.
    pub version: u8,
    /// The already authorised logical session.
    pub session: SessionId,
    /// Binds a direct Noise handshake to this particular listener.
    pub id: uuid::Uuid,
    /// Explicit unicast addresses of the agent's local network interfaces.
    pub addresses: Vec<SocketAddr>,
    /// SHA-256 pin for the listener's ephemeral TLS certificate.
    pub certificate_pin: String,
}

impl DirectOffer {
    /// Validate the bounded offer before making any network connection.
    pub fn validate(&self, session: SessionId) -> Result<(), SignalError> {
        let invalid = |reason| Err(SignalError::InvalidDirect(reason));
        if self.version != 1 || self.session != session || self.id.is_nil() {
            return invalid("wrong direct-path version, session, or listener id");
        }
        if self.addresses.is_empty() || self.addresses.len() > MAX_DIRECT_ADDRESSES {
            return invalid("direct-path address count is outside the allowed range");
        }
        if self.certificate_pin.len() != 64
            || !self
                .certificate_pin
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return invalid("invalid direct-path certificate pin");
        }
        for (index, address) in self.addresses.iter().enumerate() {
            if address.port() == 0
                || !candidate_ip(address.ip())
                || self.addresses[..index].contains(address)
            {
                return invalid("invalid or duplicated direct-path address");
            }
            if let SocketAddr::V6(address) = address {
                if address.scope_id() != 0 {
                    return invalid("a remote interface scope cannot be used locally");
                }
            }
        }
        Ok(())
    }

    /// Decode a small offer without accepting an arbitrarily large JSON body.
    pub fn decode(payload: &[u8]) -> Result<Self, SignalError> {
        if payload.len() > 4096 {
            return Err(SignalError::TooLarge(payload.len()));
        }
        let ControlMessage::PathCandidates {
            mut candidates,
            direct_binding: Some(binding),
        } = serde_json::from_slice(payload)?
        else {
            return Err(SignalError::InvalidDirect(
                "missing authenticated direct binding",
            ));
        };
        if candidates.len() > MAX_DIRECT_ADDRESSES {
            return Err(SignalError::InvalidDirect("too many direct candidates"));
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.priority));
        let addresses = candidates
            .into_iter()
            .map(|candidate| {
                if candidate.kind != CandidateKind::Host {
                    return Err(SignalError::InvalidDirect(
                        "only host candidates are supported",
                    ));
                }
                candidate.addr.parse().map_err(|_| {
                    SignalError::InvalidDirect("candidate is not a literal socket address")
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            version: binding.version,
            session: binding
                .session
                .parse()
                .map_err(|_| SignalError::InvalidDirect("invalid session UUID"))?,
            id: binding
                .listener
                .parse()
                .map_err(|_| SignalError::InvalidDirect("invalid listener UUID"))?,
            addresses,
            certificate_pin: binding.certificate_pin,
        })
    }

    /// Encode through the shared, versioned control-message schema.
    pub fn encode(&self) -> Result<Vec<u8>, SignalError> {
        self.validate(self.session)?;
        let candidates = self
            .addresses
            .iter()
            .enumerate()
            .map(|(index, address)| PathCandidate {
                addr: address.to_string(),
                kind: CandidateKind::Host,
                priority: (MAX_DIRECT_ADDRESSES - index) as u32,
            })
            .collect();
        Ok(serde_json::to_vec(&ControlMessage::PathCandidates {
            candidates,
            direct_binding: Some(DirectPathBinding {
                version: self.version,
                session: self.session.to_string(),
                listener: self.id.to_string(),
                certificate_pin: self.certificate_pin.clone(),
            }),
        })?)
    }

    /// Fresh Noise keys on each path, bound to the authorised session/listener.
    #[must_use]
    pub fn prologue(&self) -> Vec<u8> {
        format!("ndp/3 session {} direct {}", self.session, self.id).into_bytes()
    }
}

/// Whether an address can be advertised without machine-local scope guessing.
#[must_use]
pub fn candidate_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !ip.is_broadcast()
        }
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(ip) => candidate_ip(IpAddr::V4(ip)),
            None => {
                !ip.is_unspecified()
                    && !ip.is_loopback()
                    && !ip.is_multicast()
                    && !ip.is_unicast_link_local()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> DirectOffer {
        DirectOffer {
            version: 1,
            session: SessionId::new(),
            id: uuid::Uuid::now_v7(),
            addresses: vec!["10.20.30.40:1234".parse().unwrap()],
            certificate_pin: "ab".repeat(32),
        }
    }

    #[test]
    fn direct_offer_round_trip_and_binding() {
        let offer = offer();
        let payload = offer.encode().unwrap();
        let decoded = DirectOffer::decode(&payload).unwrap();
        decoded.validate(offer.session).unwrap();
        assert_eq!(decoded.prologue(), offer.prologue());
        assert!(decoded.validate(SessionId::new()).is_err());
        assert!(DirectOffer::decode(&vec![b' '; 4097]).is_err());
        assert!(matches!(
            ControlMessage::decode(&payload).unwrap(),
            ControlMessage::PathCandidates {
                direct_binding: Some(_),
                ..
            }
        ));
        assert!(matches!(
            ControlMessage::decode(br#"{"t":"path_candidates","candidates":[]}"#).unwrap(),
            ControlMessage::PathCandidates {
                direct_binding: None,
                ..
            }
        ));
    }

    #[test]
    fn refuses_unbounded_invalid_or_machine_local_candidates() {
        for address in [
            "0.0.0.0:1",
            "127.0.0.1:1",
            "255.255.255.255:1",
            "224.0.0.1:1",
            "[::1]:1",
            "[fe80::1]:1",
            "[ff02::1]:1",
            "[::ffff:127.0.0.1]:1",
            "10.0.0.1:0",
        ] {
            let mut offer = offer();
            offer.addresses = vec![address.parse().unwrap()];
            assert!(offer.validate(offer.session).is_err(), "{address}");
        }
        let mut offer = offer();
        offer.addresses = vec![offer.addresses[0]; MAX_DIRECT_ADDRESSES + 1];
        assert!(offer.validate(offer.session).is_err());
    }

    #[test]
    fn refuses_wrong_versions_pins_and_duplicate_addresses() {
        let mut value = offer();
        value.version = 2;
        assert!(value.validate(value.session).is_err());
        value.version = 1;
        value.certificate_pin = "zz".repeat(32);
        assert!(value.validate(value.session).is_err());
        value.certificate_pin = "ab".repeat(32);
        value.addresses.push(value.addresses[0]);
        assert!(value.validate(value.session).is_err());
    }
}
