//! Endpoint construction.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::TransportConfig;
use crate::tls;
use crate::{Result, TransportError};

pub use crate::tls::ServerCredentials;

/// Bind a QUIC server endpoint.
///
/// `alpn` lists every protocol this endpoint serves; a gateway serves both the
/// agent tunnel and the client session ALPN on one socket, so the accept loop
/// can dispatch on `handshake_data().protocol`.
pub fn server_endpoint(
    bind: SocketAddr,
    creds: &ServerCredentials,
    alpn: &[&[u8]],
    config: &TransportConfig,
) -> Result<quinn::Endpoint> {
    let rustls_cfg = tls::server_config(creds, alpn)?;
    let quic = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
        .map_err(|e| TransportError::Config(e.to_string()))?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(quic));
    server.transport_config(config.to_quinn());
    Ok(quinn::Endpoint::server(server, bind)?)
}

/// Dial a QUIC server, pinning its certificate.
///
/// `server_name` only feeds SNI; authenticity comes from the pin, so an
/// endpoint reachable solely by IP is fine.
pub async fn connect(
    endpoint: &quinn::Endpoint,
    remote: SocketAddr,
    server_name: &str,
    pin: tls::CertificateFingerprint,
    alpn: &[u8],
    config: &TransportConfig,
) -> Result<quinn::Connection> {
    let rustls_cfg = tls::client_config(pin, alpn)?;
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
        .map_err(|e| TransportError::Config(e.to_string()))?;
    let mut client = quinn::ClientConfig::new(Arc::new(quic));
    client.transport_config(config.to_quinn());
    Ok(endpoint
        .connect_with(client, remote, server_name)
        .map_err(|e| TransportError::Config(e.to_string()))?
        .await?)
}

/// Bind a client-side endpoint with no server certificate of its own.
pub fn client_endpoint(bind: SocketAddr) -> Result<quinn::Endpoint> {
    Ok(quinn::Endpoint::client(bind)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn server_endpoint_binds_an_ephemeral_port() {
        let creds = tls::dev_credentials(&[]).unwrap();
        let ep = server_endpoint(
            "127.0.0.1:0".parse().unwrap(),
            &creds,
            &[crate::ALPN_SESSION],
            &TransportConfig::default(),
        )
        .unwrap();
        assert_ne!(ep.local_addr().unwrap().port(), 0);
    }
}
