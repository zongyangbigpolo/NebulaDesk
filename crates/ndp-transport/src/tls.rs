//! QUIC TLS setup.
//!
//! # Why pinning instead of a public PKI
//!
//! Gateways and relays are reached by address handed out by the manager, which
//! also hands out the endpoint's certificate fingerprint. Pinning that
//! fingerprint gives a stronger guarantee than a WebPKI chain (no CA can
//! mis-issue for us), removes any dependency on hostnames, and lets an
//! operator run a relay on a bare IP with no ACME plumbing.
//!
//! This is only the hop's authenticity. The session's real identity check is
//! the Noise handshake in [`ndp_crypto`], which a relay cannot forge because
//! it never holds the agent's static key.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

use crate::{Result, TransportError};

/// A SHA-256 hash of a peer certificate's DER encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CertificateFingerprint(pub [u8; 32]);

impl CertificateFingerprint {
    /// Hash a DER-encoded certificate.
    #[must_use]
    pub fn of(der: &CertificateDer<'_>) -> Self {
        Self(Sha256::digest(der.as_ref()).into())
    }

    /// Lowercase hex, the form carried in manager API responses.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse the hex form.
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s)
            .map_err(|e| TransportError::Config(format!("bad fingerprint hex: {e}")))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| TransportError::Config("fingerprint must be 32 bytes".into()))?;
        Ok(Self(arr))
    }
}

impl std::fmt::Debug for CertificateFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sha256:{}", &self.to_hex()[..16])
    }
}

/// A server's certificate chain and private key, plus the fingerprint clients
/// must pin.
#[derive(Debug)]
pub struct ServerCredentials {
    /// DER certificate chain, leaf first.
    pub chain: Vec<CertificateDer<'static>>,
    /// DER private key for the leaf.
    pub key: PrivateKeyDer<'static>,
    /// Fingerprint of the leaf, to be published to clients out of band.
    pub fingerprint: CertificateFingerprint,
}

impl Clone for ServerCredentials {
    fn clone(&self) -> Self {
        Self {
            chain: self.chain.clone(),
            key: self.key.clone_key(),
            fingerprint: self.fingerprint,
        }
    }
}

/// Mint a self-signed certificate for development and for relay/gateway nodes
/// that are pinned rather than chained to a CA.
pub fn dev_credentials(subject_alt_names: &[String]) -> Result<ServerCredentials> {
    let names = if subject_alt_names.is_empty() {
        vec!["localhost".to_string()]
    } else {
        subject_alt_names.to_vec()
    };
    let cert = rcgen::generate_simple_self_signed(names)
        .map_err(|e| TransportError::Config(format!("certificate generation failed: {e}")))?;
    let der = cert.cert.der().clone();
    let key = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| TransportError::Config(format!("bad generated key: {e}")))?;
    Ok(ServerCredentials {
        fingerprint: CertificateFingerprint::of(&der),
        chain: vec![der],
        key,
    })
}

/// Accepts exactly one certificate, identified by its fingerprint.
#[derive(Debug)]
struct PinnedVerifier {
    expected: CertificateFingerprint,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        // Constant-time comparison is not required (the expected value is not
        // secret) but costs nothing and avoids a needless timing question.
        let actual = CertificateFingerprint::of(end_entity);
        let mut diff = 0u8;
        for (a, b) in actual.0.iter().zip(self.expected.0.iter()) {
            diff |= a ^ b;
        }
        if diff == 0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "server certificate fingerprint does not match the pin".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub(crate) fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Build a rustls client configuration that trusts exactly one certificate.
pub fn client_config(pin: CertificateFingerprint, alpn: &[u8]) -> Result<rustls::ClientConfig> {
    let provider = provider();
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Config(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            expected: pin,
            provider,
        }))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![alpn.to_vec()];
    // The session is authenticated by Noise; resuming the TLS hop only saves a
    // round trip and never grants access on its own.
    cfg.enable_early_data = true;
    Ok(cfg)
}

pub(crate) fn server_config(
    creds: &ServerCredentials,
    alpn: &[&[u8]],
) -> Result<rustls::ServerConfig> {
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| TransportError::Config(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(creds.chain.clone(), creds.key.clone_key())
        .map_err(|e| TransportError::Config(e.to_string()))?;
    cfg.alpn_protocols = alpn.iter().map(|a| a.to_vec()).collect();
    cfg.max_early_data_size = u32::MAX;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_hex_roundtrips() {
        let creds = dev_credentials(&["localhost".into()]).unwrap();
        let hex = creds.fingerprint.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(
            CertificateFingerprint::from_hex(&hex).unwrap(),
            creds.fingerprint
        );
    }

    #[test]
    fn bad_fingerprint_hex_is_rejected() {
        assert!(CertificateFingerprint::from_hex("nothex").is_err());
        assert!(CertificateFingerprint::from_hex("aabb").is_err());
    }

    #[test]
    fn each_generated_certificate_is_unique() {
        let a = dev_credentials(&[]).unwrap();
        let b = dev_credentials(&[]).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn pinned_verifier_rejects_a_different_certificate() {
        let good = dev_credentials(&[]).unwrap();
        let evil = dev_credentials(&[]).unwrap();
        let verifier = PinnedVerifier {
            expected: good.fingerprint,
            provider: provider(),
        };
        let now = UnixTime::now();
        let name = ServerName::try_from("localhost").unwrap();

        assert!(verifier
            .verify_server_cert(&good.chain[0], &[], &name, &[], now)
            .is_ok());
        assert!(verifier
            .verify_server_cert(&evil.chain[0], &[], &name, &[], now)
            .is_err());
    }

    #[test]
    fn configs_advertise_the_requested_alpn() {
        let creds = dev_credentials(&[]).unwrap();
        let s = server_config(&creds, &[crate::ALPN_SESSION]).unwrap();
        assert_eq!(s.alpn_protocols, vec![crate::ALPN_SESSION.to_vec()]);
        let c = client_config(creds.fingerprint, crate::ALPN_SESSION).unwrap();
        assert_eq!(c.alpn_protocols, vec![crate::ALPN_SESSION.to_vec()]);
    }
}
