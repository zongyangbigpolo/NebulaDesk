use std::time::Duration;

use reqwest::{Method, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use crate::error::{DesktopError, Result};
use crate::model::Workspace;

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct Manager {
    http: reqwest::Client,
    base: Url,
}

pub struct Credentials {
    pub access: Zeroizing<String>,
    pub refresh: Zeroizing<String>,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct User {
    pub id: uuid::Uuid,
    pub tenant_id: uuid::Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Deserialize)]
struct TokenPair {
    access_token: String,
    refresh_token: String,
    user: User,
}

pub struct Account {
    pub manager: Manager,
    pub user: User,
    pub workspace: Workspace,
    credentials: Mutex<Credentials>,
}

pub fn validate_url(input: &str, allow_http: bool) -> Result<Url> {
    let mut url = Url::parse(input)
        .map_err(|_| DesktopError::new("invalid_url", "Enter an absolute manager URL."))?;
    let local = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_private(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unique_local(),
        Some(url::Host::Domain(host)) => host == "localhost" || host.ends_with(".localhost"),
        None => false,
    };
    if (url.scheme() != "https" && !(url.scheme() == "http" && allow_http && local))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err(DesktopError::new(
            "invalid_url",
            "Use HTTPS. Explicit development HTTP is allowed only for loopback or private IP addresses; credentials, queries and fragments are forbidden.",
        ));
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

impl Manager {
    pub fn new(input: &str, allow_http: bool) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(DesktopError::network)?,
            base: validate_url(input, allow_http)?,
        })
    }

    pub fn url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        if !path.starts_with("v1/") || path.contains("..") || path.contains(['?', '#', '\\']) {
            return Err(DesktopError::protocol());
        }
        let url = self.base.join(path).map_err(|_| DesktopError::protocol())?;
        if url.origin() != self.base.origin() {
            return Err(DesktopError::protocol());
        }
        Ok(url)
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        let value = self.raw(method, path, token, body).await?;
        serde_json::from_value(value).map_err(|_| DesktopError::protocol())
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let url = self.endpoint(path)?;
        let mut request = self.http.request(method, url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        parse_response(request.send().await.map_err(DesktopError::network)?).await
    }

    pub async fn authenticate(self, path: &str, body: &serde_json::Value) -> Result<Account> {
        let pair: TokenPair = self.request(Method::POST, path, None, Some(body)).await
            .map_err(|error| {
                if path == "v1/auth/accept-invitation" && error.code == "unauthorized" {
                    DesktopError::new(
                        "invalid_invitation",
                        "The invitation is invalid, expired, used, revoked, or does not match the email. Ask your organization administrator for a new invitation.",
                    )
                } else {
                    error
                }
            })?;
        let credentials = Credentials {
            access: Zeroizing::new(pair.access_token),
            refresh: Zeroizing::new(pair.refresh_token),
        };
        // Resolve canonical ownership metadata before exposing any authenticated account.
        let workspace = self
            .request::<Workspace>(Method::GET, "v1/workspace", Some(&credentials.access), None)
            .await
            .and_then(|workspace| {
                if workspace.id != pair.user.tenant_id || workspace.slug.trim().is_empty() {
                    Err(DesktopError::protocol())
                } else {
                    Ok(workspace)
                }
            });
        let workspace = match workspace {
            Ok(workspace) => workspace,
            Err(error) => {
                let _ = self
                    .request::<serde_json::Value>(
                        Method::POST,
                        "v1/auth/logout",
                        None,
                        Some(&serde_json::json!({"refresh_token":credentials.refresh.as_str()})),
                    )
                    .await;
                return Err(error);
            }
        };
        Ok(Account {
            manager: self,
            user: pair.user,
            workspace,
            credentials: Mutex::new(credentials),
        })
    }
}

async fn parse_response(mut response: reqwest::Response) -> Result<serde_json::Value> {
    if !response.status().is_success() {
        return Err(match response.status() {
            StatusCode::UNAUTHORIZED => {
                DesktopError::new("unauthorized", "Sign in again to continue.")
            }
            StatusCode::FORBIDDEN => DesktopError::new(
                "forbidden",
                "Your account is not permitted to perform this action, or self-registration is disabled.",
            ),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED => DesktopError::new(
                "unavailable",
                "The resource or manager capability is unavailable.",
            ),
            StatusCode::CONFLICT => DesktopError::new(
                "conflict",
                "This workspace identifier or account may already exist, or the operation conflicts with its current state.",
            ),
            StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => DesktopError::new(
                "invalid_input",
                "Check the fields and password policy. Invitations must be valid, unused and match the invited email.",
            ),
            _ => DesktopError::new("manager_error", "The manager rejected this request."),
        });
    }
    if response.status() == StatusCode::NO_CONTENT {
        return Ok(serde_json::Value::Null);
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE as u64)
    {
        return Err(DesktopError::protocol());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(DesktopError::network)? {
        if bytes.len() + chunk.len() > MAX_RESPONSE {
            return Err(DesktopError::protocol());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| DesktopError::protocol())
}

impl Account {
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        // Serialize refresh rotation with authenticated requests: never race two refreshes.
        let mut credentials = self.credentials.lock().await;
        let first = self
            .manager
            .request(method.clone(), path, Some(&credentials.access), body)
            .await;
        match first {
            Err(ref error) if error.code == "unauthorized" => {
                let pair: TokenPair = self
                    .manager
                    .request(
                        Method::POST,
                        "v1/auth/refresh",
                        None,
                        Some(&serde_json::json!({"refresh_token":credentials.refresh.as_str()})),
                    )
                    .await?;
                credentials.access = Zeroizing::new(pair.access_token);
                credentials.refresh = Zeroizing::new(pair.refresh_token);
                self.manager
                    .request(method, path, Some(&credentials.access), body)
                    .await
            }
            result => result,
        }
    }

    pub async fn logout(&self) -> Result<()> {
        let mut credentials = self.credentials.lock().await;
        let result = self
            .manager
            .request::<serde_json::Value>(
                Method::POST,
                "v1/auth/logout",
                None,
                Some(&serde_json::json!({"refresh_token":credentials.refresh.as_str()})),
            )
            .await;
        credentials.access.clear();
        credentials.refresh.clear();
        result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_url_safety() {
        for input in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com/?token=secret",
            "file:///tmp/a",
        ] {
            assert!(validate_url(input, true).is_err());
        }
        assert!(validate_url("http://127.0.0.1:8080", false).is_err());
        assert!(validate_url("http://127.0.0.1:8080", true).is_ok());
        assert!(validate_url("http://192.168.1.2:8080", true).is_ok());
        assert_eq!(
            validate_url("https://manager.example/proxy", false)
                .unwrap()
                .as_str(),
            "https://manager.example/proxy/"
        );
    }

    #[test]
    fn errors_are_bounded_and_do_not_include_response_secrets() {
        assert!(format!("{:?}", DesktopError::protocol()).len() < 256);
    }

    #[test]
    fn endpoints_cannot_change_origin_or_escape_api_prefix() {
        let manager = Manager::new("https://manager.example/proxy", false).unwrap();
        assert_eq!(
            manager.endpoint("v1/auth/login").unwrap().as_str(),
            "https://manager.example/proxy/v1/auth/login"
        );
        for path in [
            "https://evil.test",
            "//evil.test",
            "../auth",
            "v1/../auth",
            "v1/resources?token=secret",
            "v1/\\evil.test",
        ] {
            assert!(manager.endpoint(path).is_err());
        }
    }

    #[tokio::test]
    async fn response_limits_and_errors_never_reflect_secret_bodies() {
        let response = http::Response::builder()
            .status(401)
            .body("SECRET".to_owned())
            .unwrap();
        let error = parse_response(response.into()).await.unwrap_err();
        assert_eq!(error.code, "unauthorized");
        assert!(!format!("{error:?}").contains("SECRET"));
        let response = http::Response::builder()
            .status(200)
            .body(vec![b'x'; MAX_RESPONSE + 1])
            .unwrap();
        assert!(parse_response(response.into()).await.is_err());
        let response = http::Response::builder()
            .status(200)
            .body("not json SECRET".to_owned())
            .unwrap();
        assert!(
            !format!("{:?}", parse_response(response.into()).await.unwrap_err()).contains("SECRET")
        );
        let response = http::Response::builder()
            .status(302)
            .header("Location", "https://evil.test")
            .body("SECRET".to_owned())
            .unwrap();
        assert_eq!(
            parse_response(response.into()).await.unwrap_err().code,
            "manager_error"
        );
    }
}
