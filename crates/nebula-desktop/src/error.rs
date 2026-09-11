use serde::Serialize;

#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct DesktopError {
    pub code: &'static str,
    pub message: &'static str,
}

impl DesktopError {
    pub const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    pub fn network(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            return Self::new("network_timeout", "The request to the manager timed out.");
        }
        if let Some(error) = tls_error(&error) {
            return error;
        }
        if error.is_connect() {
            return Self::new(
                "network_connection",
                "A connection to the manager could not be established.",
            );
        }
        Self::new("network", "Communication with the manager failed.")
    }

    pub fn protocol() -> Self {
        Self::new(
            "protocol",
            "The service returned an invalid or oversized response.",
        )
    }

    pub fn cancelled() -> Self {
        Self::new("cancelled", "This operation was cancelled.")
    }
}

fn tls_error(mut error: &(dyn std::error::Error + 'static)) -> Option<DesktopError> {
    loop {
        if let Some(tls) = error.downcast_ref::<rustls::Error>() {
            return Some(match tls {
                rustls::Error::InvalidCertificate(_) | rustls::Error::NoCertificatesPresented => {
                    DesktopError::new(
                        "tls_certificate",
                        "The manager's TLS certificate could not be verified.",
                    )
                }
                _ => DesktopError::new("tls", "The HTTPS handshake with the manager failed."),
            });
        }
        if let Some(inner) = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
        {
            error = inner;
            continue;
        }
        error = error.source()?;
    }
}

pub type Result<T> = std::result::Result<T, DesktopError>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tls_failures_are_typed_and_do_not_expose_source_details() {
        let certificate = std::io::Error::other(rustls::Error::InvalidCertificate(
            rustls::CertificateError::UnknownIssuer,
        ));
        assert_eq!(tls_error(&certificate).unwrap().code, "tls_certificate");
        let handshake = rustls::Error::General("SECRET handshake details".into());
        let error = tls_error(&handshake).unwrap();
        assert_eq!(error.code, "tls");
        assert!(!format!("{error:?}").contains("SECRET"));
        assert!(tls_error(&std::io::Error::other("certificate SECRET")).is_none());
    }

    #[tokio::test]
    async fn request_timeout_is_not_reported_as_certificate_or_password_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let error = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap()
            .get(format!("http://{addr}/?secret=DO_NOT_EXPOSE"))
            .send()
            .await
            .unwrap_err();
        server.abort();
        let error = DesktopError::network(error);
        assert_eq!(error.code, "network_timeout");
        assert!(!format!("{error:?}").contains("DO_NOT_EXPOSE"));
    }

    #[tokio::test]
    async fn connection_refusal_is_not_reported_as_certificate_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let error = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(DesktopError::network(error).code, "network_connection");
    }
}
