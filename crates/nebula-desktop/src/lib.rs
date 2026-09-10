//! Rust-only desktop credentials, manager operations and supervised native sessions.

pub mod error;
#[cfg(feature = "gui")]
pub mod gui;
pub mod local_host;
pub mod manager;
pub mod model;
pub mod sessions;

use std::{path::PathBuf, sync::Arc};

use reqwest::Method;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use error::{DesktopError, Result};
use manager::{Account, Manager};
use model::*;
use sessions::{ChildCommand, Sessions, Ticket};

#[derive(Default)]
struct Authentication {
    generation: u64,
    cancelled: CancellationToken,
    account: Option<Arc<Account>>,
    tenant: String,
    manager_url: Option<String>,
}

pub struct Desktop {
    auth: Mutex<Authentication>,
    connect_gate: Mutex<()>,
    pub sessions: Sessions,
    pub local_agent: local_host::LocalAgent,
}

impl Desktop {
    pub fn new(client_binary: PathBuf, agent_binary: PathBuf, agent_state: PathBuf) -> Self {
        Self {
            auth: Mutex::new(Authentication {
                manager_url: configured_manager_url(),
                ..Authentication::default()
            }),
            connect_gate: Mutex::new(()),
            sessions: Sessions::new(client_binary),
            local_agent: local_host::LocalAgent::new(agent_state, agent_binary),
        }
    }

    async fn invalidate(&self) -> (u64, CancellationToken, Option<Arc<Account>>) {
        let mut auth = self.auth.lock().await;
        auth.cancelled.cancel();
        auth.generation += 1;
        auth.cancelled = CancellationToken::new();
        auth.tenant.clear();
        self.sessions.stop_all().await;
        (auth.generation, auth.cancelled.clone(), auth.account.take())
    }

    async fn context(&self) -> Result<(Arc<Account>, CancellationToken)> {
        let auth = self.auth.lock().await;
        Ok((
            auth.account
                .clone()
                .ok_or_else(|| DesktopError::new("unauthorized", "Sign in to continue."))?,
            auth.cancelled.clone(),
        ))
    }

    async fn api<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let (account, cancelled) = self.context().await?;
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => Err(DesktopError::cancelled()),
            result = account.request(method, path, body) => result,
        }
    }

    async fn authenticate(&self, manager: Manager, path: &str, mut body: Value) -> Result<Value> {
        let (generation, cancelled, previous) = self.invalidate().await;
        if let Some(previous) = previous {
            tokio::spawn(async move {
                if previous.logout().await.is_err() {
                    eprintln!("The previous manager login could not be revoked remotely.");
                }
            });
        }
        let result = tokio::select! {
            biased;
            _ = cancelled.cancelled() => Err(DesktopError::cancelled()),
            account = manager.authenticate(path, &body) => account,
        };
        // Requests never enter persistent state; wipe secret input copies promptly.
        for field in ["password", "token"] {
            if let Some(Value::String(secret)) = body.get_mut(field) {
                zeroize::Zeroize::zeroize(secret);
            }
        }
        let account = Arc::new(result?);
        let mut auth = self.auth.lock().await;
        if auth.generation != generation {
            drop(auth);
            if account.logout().await.is_err() {
                eprintln!("The cancelled manager login could not be revoked remotely.");
            }
            return Err(DesktopError::cancelled());
        }
        let tenant = account.workspace.slug.clone();
        let view = account_view(&account, &tenant);
        auth.manager_url = Some(account.manager.url().to_owned());
        auth.account = Some(account);
        auth.tenant = tenant;
        value(view)
    }

    async fn organization_api<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T> {
        let (account, cancelled) = self.context().await?;
        if !matches!(account.user.role.as_str(), "ADMIN" | "OWNER")
            || account.workspace.kind != WorkspaceKind::Organization
        {
            return Err(DesktopError::new(
                "forbidden",
                "Only organization administrators can manage the organization directory and invitations.",
            ));
        }
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => Err(DesktopError::cancelled()),
            result = account.request(method, path, body) => result,
        }
    }

    pub async fn request(&self, request: Request) -> Result<Value> {
        match request {
            Request::ConnectionSettings => value(ConnectionSettings { manager_url: self.auth.lock().await.manager_url.clone() }),
            Request::Users => value(self.organization_api::<Vec<DirectoryUser>>(Method::GET, "v1/users", None).await?),
            Request::SetUserDisabled { user_id, disabled } => self.organization_api(Method::PATCH, &format!("v1/users/{user_id}"), Some(&json!({"disabled":disabled}))).await,
            Request::Groups => value(self.organization_api::<Vec<Group>>(Method::GET, "v1/groups", None).await?),
            Request::CreateGroup { name } => {
                nonempty(&name, 200)?;
                if name.chars().any(char::is_control) {
                    return Err(DesktopError::new("invalid_input", "Group names cannot contain control characters."));
                }
                value(self.organization_api::<Group>(Method::POST, "v1/groups", Some(&json!({"name":name}))).await?)
            }
            Request::DeleteGroup { group_id } => self.organization_api(Method::DELETE, &format!("v1/groups/{group_id}"), None).await,
            Request::GroupMembers { group_id } => value(self.organization_api::<Vec<DirectoryUser>>(Method::GET, &format!("v1/groups/{group_id}/members"), None).await?),
            Request::AddGroupMember { group_id, user_id } => self.organization_api(Method::POST, &format!("v1/groups/{group_id}/members"), Some(&json!({"user_id":user_id}))).await,
            Request::RemoveGroupMember { group_id, user_id } => self.organization_api(Method::DELETE, &format!("v1/groups/{group_id}/members/{user_id}"), None).await,
            Request::RegistrationOptions { manager_url, allow_insecure_http } => {
                let manager = Manager::new(&manager_url, allow_insecure_http)?;
                value(manager.request::<RegistrationOptions>(Method::GET, "v1/auth/registration", None, None).await?)
            }
            Request::Register { manager_url, workspace_slug, workspace_name, workspace_kind, display_name, email, password, allow_insecure_http } => {
                let password = zeroize::Zeroizing::new(password);
                validate_signup(&display_name, &email, &password)?;
                validate_identity_name(&workspace_name)?;
                if workspace_slug.is_empty() || workspace_slug.len() > 63 || !workspace_slug.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                    || workspace_slug.starts_with('-') || workspace_slug.ends_with('-') {
                    return Err(DesktopError::new("invalid_input", "Use a workspace identifier of 1–63 lowercase letters, digits or internal hyphens."));
                }
                let manager = Manager::new(&manager_url, allow_insecure_http)?;
                self.authenticate(manager, "v1/auth/register", json!({
                    "workspace_slug":workspace_slug,"workspace_name":workspace_name,"workspace_kind":workspace_kind,
                    "display_name":display_name,"email":email,"password":password.as_str(),
                })).await
            }
            Request::AcceptInvitation { manager_url, token, display_name, email, password, allow_insecure_http } => {
                let password = zeroize::Zeroizing::new(password);
                let token = zeroize::Zeroizing::new(token);
                validate_signup(&display_name, &email, &password)?;
                nonempty(&token, 128)?;
                let manager = Manager::new(&manager_url, allow_insecure_http)?;
                self.authenticate(manager, "v1/auth/accept-invitation", json!({
                    "token":token.as_str(),"display_name":display_name,"email":email,"password":password.as_str(),
                })).await
            }
            Request::CreateInvitation { email } => {
                validate_email(&email)?;
                value(self.organization_api::<CreatedInvitation>(Method::POST, "v1/workspace/invitations", Some(&json!({"email":email}))).await?)
            }
            Request::Invitations => value(self.organization_api::<Vec<Invitation>>(Method::GET, "v1/workspace/invitations", None).await?),
            Request::RevokeInvitation { id } => self.organization_api(Method::DELETE, &format!("v1/workspace/invitations/{id}"), None).await,
            Request::Login { manager_url, tenant, email, password, allow_insecure_http } => {
                let password = zeroize::Zeroizing::new(password);
                let manager = Manager::new(&manager_url, allow_insecure_http)?;
                if tenant.len() > 256 || email.len() > 320 || password.len() > 4096 {
                    return Err(DesktopError::new("invalid_input", "Login fields exceed their size limit."));
                }
                self.authenticate(manager, "v1/auth/login", json!({"tenant":tenant,"email":email,"password":password.as_str()})).await
            }
            Request::Logout => {
                let (_, _, account) = self.invalidate().await;
                if let Some(account) = account { account.logout().await?; }
                Ok(Value::Null)
            }
            Request::Account => {
                let auth = self.auth.lock().await;
                value(auth.account.as_ref().map(|account| account_view(account, &auth.tenant)))
            }
            Request::Resources => value(self.api::<Vec<Resource>>(Method::GET, "v1/resources?application_windows=true", None).await?),
            Request::Resource { id } => value(self.api::<Resource>(Method::GET, &format!("v1/resources/{id}?application_windows=true"), None).await?),
            Request::Machines => value(self.api::<Vec<Machine>>(Method::GET, "v1/machines", None).await?),
            Request::CreateEnrollment { name } => {
                nonempty(&name, 255)?;
                let (account, cancelled) = self.context().await?;
                let body = enrollment_body(&name, account.user.id);
                tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => Err(DesktopError::cancelled()),
                    enrollment = account.request::<Enrollment>(Method::POST, "v1/machines/enrollment-tokens", Some(&body)) => value(enrollment?),
                }
            }
            Request::RenameMachine { machine_id, name } => {
                nonempty(&name, 255)?;
                self.api(Method::PATCH, &format!("v1/machines/{machine_id}"), Some(&json!({"name":name}))).await
            }
            Request::RemoveMachine { machine_id } => self.api(Method::DELETE, &format!("v1/machines/{machine_id}"), None).await,
            Request::MachineResources { machine_id } => value(self.api::<Vec<PublishedResource>>(Method::GET, &format!("v1/machines/{machine_id}/resources"), None).await?),
            Request::PublishResource { machine_id, resource } => {
                nonempty(&resource.name, 255)?;
                value(self.api::<PublishedResource>(Method::POST, &format!("v1/machines/{machine_id}/resources"), Some(&value(resource)?)).await?)
            }
            Request::UpdateResource { resource_id, changes } => self.api(Method::PATCH, &format!("v1/resources/{resource_id}"), Some(&value(changes)?)).await,
            Request::Grants { resource_id } => {
                let grants: Vec<Entitlement> = self.api(Method::GET, &format!("v1/resources/{resource_id}/entitlements"), None).await?;
                value(grants.into_iter().map(Grant::from).collect::<Vec<_>>())
            }
            Request::GrantAccess { resource_id, email, role, allow_clipboard, allow_file_transfer, allow_audio } => {
                nonempty(&email, 320)?;
                let grant: Entitlement = self.api(Method::POST, &format!("v1/resources/{resource_id}/entitlements"), Some(&json!({
                    "email":email,"role":role,"allow_clipboard":allow_clipboard,"allow_file_transfer":allow_file_transfer,"allow_audio":allow_audio,
                }))).await?;
                value(Grant::from(grant))
            }
            Request::RevokeAccess { entitlement_id } => self.api(Method::DELETE, &format!("v1/entitlements/{entitlement_id}"), None).await,
            Request::Connect { resource_id, keyboard_profile } => {
                let (account, cancelled) = self.context().await?;
                let _gate = tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => return Err(DesktopError::cancelled()),
                    gate = self.connect_gate.lock() => gate,
                };
                if cancelled.is_cancelled() { return Err(DesktopError::cancelled()); }
                if let Some(existing) = self.sessions.existing(resource_id).await {
                    self.sessions.command(existing.session_id, ChildCommand::Focus {}).await?;
                    return value(existing);
                }
                let (resource, ticket) = tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => return Err(DesktopError::cancelled()),
                    result = async {
                        let resource: Resource = account.request(Method::GET, &format!("v1/resources/{resource_id}?application_windows=true"), None).await?;
                        if resource.id != resource_id { return Err(DesktopError::protocol()); }
                        if !resource.launch_supported {
                            return Err(DesktopError::new("unsupported", "This resource is not supported by the remote host's native session backend."));
                        }
                        if !matches!(resource.kind, Kind::App) && keyboard_profile != ApplicationKeyboardProfile::Physical {
                            return Err(DesktopError::new("unsupported", "Keyboard adaptation is available only for application sessions."));
                        }
                        let ticket: Ticket = account.request(Method::POST, "v1/sessions", Some(&json!({"resource_id":resource_id,"client_os":std::env::consts::OS,"application_windows":true}))).await?;
                        Ok((resource, ticket))
                    } => result?,
                };
                // Serialize the final spawn with invalidation. A cancelled login cannot launch later.
                let auth = self.auth.lock().await;
                let id = ticket.session_id;
                if cancelled.is_cancelled() {
                    drop(auth);
                    close_unused_ticket(account, id);
                    return Err(DesktopError::cancelled());
                }
                let session = self.sessions.start(resource, ticket, keyboard_profile).await;
                drop(auth);
                match session {
                    Ok(session) => value(session),
                    Err(error) => {
                        close_unused_ticket(account, id);
                        Err(error)
                    }
                }
            }
            Request::Sessions => value(self.sessions.list().await),
            Request::Transfers => value(self.sessions.transfers().await),
            Request::FocusSession { session_id } => {
                self.sessions.command(session_id, ChildCommand::Focus {}).await?;
                Ok(Value::Null)
            }
            Request::DisconnectSession { session_id } => {
                self.sessions.disconnect(session_id).await?;
                Ok(Value::Null)
            }
            Request::LocalHost => value(self.local_agent.snapshot().await?),
            Request::OpenPermissionSettings { .. } => Err(DesktopError::new("unsupported", "Open your operating system privacy settings to manage capture and input permissions.")),
            Request::EnrollLocal { manager_url, token, name, allow_insecure_http } => {
                nonempty(&name, 255)?;
                nonempty(&token, 4096)?;
                let (_, cancelled) = self.context().await?;
                tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => Err(DesktopError::cancelled()),
                    result = self.local_agent.enroll(manager_url, token, name, allow_insecure_http) => value(result?),
                }
            }
            Request::SetHostEnabled { enabled } => value(self.local_agent.set_enabled(enabled).await?),
            Request::SendFiles { .. } => Err(DesktopError::new("native_picker_required", "File selection is available only in the desktop application.")),
        }
    }

    pub async fn shutdown(&self) {
        let (_, _, account) = self.invalidate().await;
        if let Some(account) = account {
            if account.logout().await.is_err() {
                eprintln!("Manager logout failed during shutdown; local credentials were cleared.");
            }
        }
    }
}

fn nonempty(input: &str, max: usize) -> Result<()> {
    if input.trim().is_empty() || input.len() > max {
        return Err(DesktopError::new(
            "invalid_input",
            "A required field is empty or too long.",
        ));
    }
    Ok(())
}

fn enrollment_body(name: &str, user_id: uuid::Uuid) -> Value {
    json!({"machine_name":name, "owner_user_id":user_id})
}

fn close_unused_ticket(account: Arc<Account>, session: uuid::Uuid) {
    tokio::spawn(async move {
        let result = account
            .request::<Value>(Method::DELETE, &format!("v1/sessions/{session}"), None)
            .await;
        if result.is_err_and(|error| error.code != "unavailable") {
            eprintln!("The unused session admission could not be closed remotely.");
        }
    });
}

fn value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| DesktopError::protocol())
}

fn configured_manager_url() -> Option<String> {
    let input = std::env::var("NEBULA_MANAGER_URL")
        .ok()
        .or_else(|| option_env!("NEBULA_MANAGER_URL").map(str::to_owned))?;
    manager::validate_url(&input, false)
        .ok()
        .map(|url| url.as_str().trim_end_matches('/').to_owned())
}

fn account_view(account: &Account, tenant: &str) -> AccountView {
    AccountView {
        id: account.user.id,
        email: account.user.email.clone(),
        display_name: account.user.display_name.clone(),
        role: account.user.role.clone(),
        tenant: tenant.into(),
        manager_url: account.manager.url().into(),
        workspace: account.workspace.clone(),
    }
}

fn validate_email(email: &str) -> Result<()> {
    nonempty(email, 254)?;
    let valid = email.split_once('@').is_some_and(|(local, domain)| {
        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
    });
    if !valid || email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(DesktopError::new(
            "invalid_input",
            "Enter a complete email address.",
        ));
    }
    Ok(())
}

fn validate_identity_name(name: &str) -> Result<()> {
    nonempty(name, 200)?;
    if name.chars().any(char::is_control) {
        return Err(DesktopError::new(
            "invalid_input",
            "Names cannot contain control characters.",
        ));
    }
    Ok(())
}

fn validate_signup(display_name: &str, email: &str, password: &str) -> Result<()> {
    validate_identity_name(display_name)?;
    validate_email(email)?;
    if password.chars().count() < 12 || password.len() > 1024 {
        return Err(DesktopError::new(
            "invalid_input",
            "Use a password of at least 12 characters (maximum 1024 bytes).",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct MockReply {
        method: &'static str,
        path: &'static str,
        status: u16,
        body: Value,
    }

    async fn mock_manager(
        replies: Vec<MockReply>,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 1024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&chunk[..read]);
                    if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                assert!(
                    headers.starts_with(&format!("{} {} HTTP/1.1\r\n", reply.method, reply.path))
                );
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let mut chunk = [0u8; 1024];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&chunk[..read]);
                }
                let body = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
                };
                requests.push((headers, body));
                let body = if reply.status == 204 {
                    String::new()
                } else {
                    reply.body.to_string()
                };
                stream.write_all(format!("HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.status, body.len(), body).as_bytes()).await.unwrap();
            }
            requests
        });
        (url, task)
    }

    fn reply(method: &'static str, path: &'static str, status: u16, body: Value) -> MockReply {
        MockReply {
            method,
            path,
            status,
            body,
        }
    }

    fn auth_fixture(role: &str, kind: &str) -> (Value, Value) {
        let tenant_id = uuid::Uuid::new_v4();
        (
            json!({
                "access_token":"ACCESS_SECRET","refresh_token":"REFRESH_SECRET",
                "user":{"id":uuid::Uuid::new_v4(),"tenant_id":tenant_id,"email":"exact@example.test","display_name":"Exact","role":role}
            }),
            json!({"id":tenant_id,"slug":"acme","name":"Acme Workspace","kind":kind}),
        )
    }

    fn desktop() -> Desktop {
        Desktop::new(
            "missing-client".into(),
            "missing-agent".into(),
            "unused-identity".into(),
        )
    }

    #[tokio::test]
    async fn application_admission_uses_resource_capability_without_desktop_fallback() {
        for supported in [false, true] {
            let (pair, workspace) = auth_fixture("ADMIN", "PERSONAL");
            let resource_id = uuid::Uuid::nil();
            let mut replies = vec![
                reply("POST", "/v1/auth/login", 200, pair),
                reply("GET", "/v1/workspace", 200, workspace),
                reply(
                    "GET",
                    "/v1/resources/00000000-0000-0000-0000-000000000000?application_windows=true",
                    200,
                    json!({
                        "id":resource_id,"name":"Editor","kind":"APP",
                        "description":"","machine_status":"ONLINE","role":"CONTROLLER",
                        "policy":{"input":true,"audio":false,"clipboard":false,"file_transfer":false},
                        "owned":false,"launch_supported":supported
                    }),
                ),
            ];
            if supported {
                replies.push(reply(
                    "POST",
                    "/v1/sessions",
                    409,
                    json!({"error":"application backend is busy"}),
                ));
            }
            let (url, server) = mock_manager(replies).await;
            let host = desktop();
            host.request(Request::Login {
                manager_url: url,
                tenant: "acme".into(),
                email: "exact@example.test".into(),
                password: "test-password".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap();
            let error = host
                .request(Request::Connect {
                    resource_id,
                    keyboard_profile: ApplicationKeyboardProfile::Editing,
                })
                .await
                .unwrap_err();
            assert_eq!(
                error.code,
                if supported { "conflict" } else { "unsupported" }
            );
            let requests = tokio::time::timeout(std::time::Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(requests.len(), if supported { 4 } else { 3 });
            if supported {
                assert_eq!(requests[3].1["resource_id"], resource_id.to_string());
                assert_eq!(requests[3].1["application_windows"], true);
                assert!(requests[3].1.get("launch_path").is_none());
                assert!(requests[3].1.get("launch_args").is_none());
                assert!(requests[3].1.get("keyboard_profile").is_none());
            }
            assert!(host.sessions.list().await.is_empty());
        }
    }

    #[tokio::test]
    async fn registration_and_invitation_management_use_exact_http_contract_without_leaking_credentials(
    ) {
        let (pair, workspace) = auth_fixture("ADMIN", "ORGANIZATION");
        let invitation = json!({"id":uuid::Uuid::nil(),"email":"member@example.test","expires_at":"2026-09-10T00:00:00Z","created_at":"2026-09-08T00:00:00Z","revoked_at":null,"accepted_at":null});
        let mut issued = invitation.clone();
        issued["token"] = json!("ONE_TIME_INVITATION");
        let mut metadata_with_unexpected_token = invitation.clone();
        metadata_with_unexpected_token["token"] = json!("MUST_NOT_LEAK");
        let (url, server) = mock_manager(vec![
            reply(
                "GET",
                "/v1/auth/registration",
                200,
                json!({"self_registration_enabled":true}),
            ),
            reply("POST", "/v1/auth/register", 200, pair.clone()),
            reply("GET", "/v1/workspace", 200, workspace.clone()),
            reply("POST", "/v1/workspace/invitations", 201, issued),
            reply(
                "GET",
                "/v1/workspace/invitations",
                200,
                json!([metadata_with_unexpected_token]),
            ),
            reply(
                "DELETE",
                "/v1/workspace/invitations/00000000-0000-0000-0000-000000000000",
                204,
                Value::Null,
            ),
            reply("POST", "/v1/auth/logout", 204, Value::Null),
        ])
        .await;
        let desktop = desktop();
        assert!(desktop.request(Request::Account).await.unwrap().is_null());
        assert_eq!(
            desktop
                .request(Request::RegistrationOptions {
                    manager_url: url.clone(),
                    allow_insecure_http: true
                })
                .await
                .unwrap()["self_registration_enabled"],
            true
        );
        let account = desktop
            .request(Request::Register {
                manager_url: url,
                workspace_slug: "acme".into(),
                workspace_name: "Acme Workspace".into(),
                workspace_kind: WorkspaceKind::Organization,
                display_name: "Exact".into(),
                email: "exact@example.test".into(),
                password: "long-password-123".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap();
        assert_eq!(account["workspace"], workspace);
        assert_eq!(account["tenant"], "acme");
        assert!(!account.to_string().contains("SECRET"));
        assert!(account.get("access_token").is_none());
        assert_eq!(
            desktop
                .request(Request::CreateInvitation {
                    email: "member@example.test".into()
                })
                .await
                .unwrap()["token"],
            "ONE_TIME_INVITATION"
        );
        let list = desktop.request(Request::Invitations).await.unwrap();
        assert_eq!(list, json!([invitation]));
        assert!(!list.to_string().contains("MUST_NOT_LEAK"));
        assert!(desktop
            .request(Request::RevokeInvitation {
                id: uuid::Uuid::nil()
            })
            .await
            .unwrap()
            .is_null());
        desktop.request(Request::Logout).await.unwrap();
        assert!(desktop.request(Request::Account).await.unwrap().is_null());
        assert_eq!(
            desktop.request(Request::ConnectionSettings).await.unwrap()["manager_url"],
            account["manager_url"]
        );
        let requests = server.await.unwrap();
        assert_eq!(
            requests[1].1,
            json!({"workspace_slug":"acme","workspace_name":"Acme Workspace","workspace_kind":"ORGANIZATION","display_name":"Exact","email":"exact@example.test","password":"long-password-123"})
        );
        assert_eq!(requests[3].1, json!({"email":"member@example.test"}));
        assert!(!requests[1].0.to_lowercase().contains("authorization:"));
        assert!(requests[2]
            .0
            .to_lowercase()
            .contains("authorization: bearer access_secret"));
        assert_eq!(requests[6].1, json!({"refresh_token":"REFRESH_SECRET"}));
    }

    #[tokio::test]
    async fn invitation_acceptance_installs_fixed_workspace_and_user_role() {
        let (pair, workspace) = auth_fixture("USER", "ORGANIZATION");
        let (url, server) = mock_manager(vec![
            reply("POST", "/v1/auth/accept-invitation", 200, pair),
            reply("GET", "/v1/workspace", 200, workspace.clone()),
        ])
        .await;
        let desktop = desktop();
        let account = desktop
            .request(Request::AcceptInvitation {
                manager_url: url,
                token: "EXACT_INVITE".into(),
                display_name: "Exact".into(),
                email: "exact@example.test".into(),
                password: "long-password-123".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap();
        assert_eq!(account["workspace"], workspace);
        assert_eq!(account["role"], "USER");
        for request in [
            Request::Invitations,
            Request::Users,
            Request::Groups,
            Request::GroupMembers {
                group_id: uuid::Uuid::nil(),
            },
            Request::SetUserDisabled {
                user_id: uuid::Uuid::nil(),
                disabled: true,
            },
            Request::CreateGroup {
                name: "Team".into(),
            },
            Request::DeleteGroup {
                group_id: uuid::Uuid::nil(),
            },
            Request::AddGroupMember {
                group_id: uuid::Uuid::nil(),
                user_id: uuid::Uuid::nil(),
            },
            Request::RemoveGroupMember {
                group_id: uuid::Uuid::nil(),
                user_id: uuid::Uuid::nil(),
            },
            Request::CreateInvitation {
                email: "member@example.test".into(),
            },
            Request::RevokeInvitation {
                id: uuid::Uuid::nil(),
            },
        ] {
            assert_eq!(
                desktop.request(request).await.unwrap_err().code,
                "forbidden"
            );
        }
        let requests = server.await.unwrap();
        assert_eq!(
            requests[0].1,
            json!({"token":"EXACT_INVITE","display_name":"Exact","email":"exact@example.test","password":"long-password-123"})
        );
    }

    #[tokio::test]
    async fn organization_directory_commands_use_scoped_routes_and_strip_unexpected_fields() {
        let (pair, workspace) = auth_fixture("ADMIN", "ORGANIZATION");
        let user_id = uuid::Uuid::from_u128(1);
        let group_id = uuid::Uuid::nil();
        let user = json!({"id":user_id,"email":"member@example.test","display_name":"Member","role":"USER","disabled":false});
        let mut server_user = user.clone();
        server_user["password_hash"] = json!("MUST_NOT_LEAK");
        let group = json!({"id":group_id,"name":"Team"});
        let (url, server) = mock_manager(vec![
            reply("POST", "/v1/auth/login", 200, pair),
            reply("GET", "/v1/workspace", 200, workspace),
            reply("GET", "/v1/users", 200, json!([server_user.clone()])),
            reply("PATCH", "/v1/users/00000000-0000-0000-0000-000000000001", 204, Value::Null),
            reply("GET", "/v1/groups", 200, json!([group.clone()])),
            reply("POST", "/v1/groups", 201, group.clone()),
            reply("GET", "/v1/groups/00000000-0000-0000-0000-000000000000/members", 200, json!([server_user])),
            reply("POST", "/v1/groups/00000000-0000-0000-0000-000000000000/members", 204, Value::Null),
            reply("DELETE", "/v1/groups/00000000-0000-0000-0000-000000000000/members/00000000-0000-0000-0000-000000000001", 204, Value::Null),
            reply("DELETE", "/v1/groups/00000000-0000-0000-0000-000000000000", 204, Value::Null),
        ]).await;
        let desktop = desktop();
        desktop
            .request(Request::Login {
                manager_url: url,
                tenant: "acme".into(),
                email: "exact@example.test".into(),
                password: "password".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap();
        assert_eq!(
            desktop.request(Request::Users).await.unwrap(),
            json!([user.clone()])
        );
        assert!(desktop
            .request(Request::SetUserDisabled {
                user_id,
                disabled: true
            })
            .await
            .unwrap()
            .is_null());
        assert_eq!(
            desktop.request(Request::Groups).await.unwrap(),
            json!([group.clone()])
        );
        assert_eq!(
            desktop
                .request(Request::CreateGroup {
                    name: "Team".into()
                })
                .await
                .unwrap(),
            group
        );
        assert_eq!(
            desktop
                .request(Request::GroupMembers { group_id })
                .await
                .unwrap(),
            json!([user])
        );
        assert!(desktop
            .request(Request::AddGroupMember { group_id, user_id })
            .await
            .unwrap()
            .is_null());
        assert!(desktop
            .request(Request::RemoveGroupMember { group_id, user_id })
            .await
            .unwrap()
            .is_null());
        assert!(desktop
            .request(Request::DeleteGroup { group_id })
            .await
            .unwrap()
            .is_null());
        let requests = server.await.unwrap();
        assert_eq!(requests[3].1, json!({"disabled":true}));
        assert_eq!(requests[5].1, json!({"name":"Team"}));
        assert_eq!(requests[7].1, json!({"user_id":user_id}));
        assert!(requests[2..].iter().all(|(headers, _)| headers
            .to_lowercase()
            .contains("authorization: bearer access_secret")));
    }

    #[tokio::test]
    async fn login_requires_workspace_fetch_and_personal_admin_cannot_manage_invitations() {
        let (pair, workspace) = auth_fixture("ADMIN", "PERSONAL");
        let (url, server) = mock_manager(vec![
            reply("POST", "/v1/auth/login", 200, pair),
            reply("GET", "/v1/workspace", 200, workspace.clone()),
        ])
        .await;
        let desktop = desktop();
        let account = desktop
            .request(Request::Login {
                manager_url: url,
                tenant: "acme".into(),
                email: "exact@example.test".into(),
                password: "password".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap();
        assert_eq!(account["workspace"], workspace);
        assert_eq!(
            desktop
                .request(Request::Invitations)
                .await
                .unwrap_err()
                .code,
            "forbidden"
        );
        assert_eq!(
            desktop.request(Request::Users).await.unwrap_err().code,
            "forbidden"
        );
        assert_eq!(
            desktop.request(Request::Groups).await.unwrap_err().code,
            "forbidden"
        );
        let requests = server.await.unwrap();
        assert_eq!(requests[0].1["tenant"], "acme");
    }

    #[tokio::test]
    async fn legacy_organization_owner_retains_administrator_capabilities() {
        let (pair, workspace) = auth_fixture("OWNER", "ORGANIZATION");
        let (url, server) = mock_manager(vec![
            reply("POST", "/v1/auth/login", 200, pair),
            reply("GET", "/v1/workspace", 200, workspace),
            reply("GET", "/v1/users", 200, json!([])),
            reply("GET", "/v1/workspace/invitations", 200, json!([])),
        ])
        .await;
        let desktop = desktop();
        let account = desktop
            .request(Request::Login {
                manager_url: url,
                tenant: "acme".into(),
                email: "exact@example.test".into(),
                password: "password".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap();
        assert_eq!(account["role"], "OWNER");
        assert_eq!(desktop.request(Request::Users).await.unwrap(), json!([]));
        assert_eq!(
            desktop.request(Request::Invitations).await.unwrap(),
            json!([])
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn failed_or_mismatched_workspace_fetch_never_installs_half_authenticated_account() {
        for mismatch in [false, true] {
            let (pair, mut workspace) = auth_fixture("ADMIN", "ORGANIZATION");
            workspace["id"] = json!(uuid::Uuid::new_v4());
            let (url, server) = mock_manager(vec![
                reply("POST", "/v1/auth/login", 200, pair),
                reply(
                    "GET",
                    "/v1/workspace",
                    if mismatch { 200 } else { 503 },
                    workspace,
                ),
                reply("POST", "/v1/auth/logout", 204, Value::Null),
            ])
            .await;
            let desktop = desktop();
            assert!(desktop
                .request(Request::Login {
                    manager_url: url,
                    tenant: "acme".into(),
                    email: "exact@example.test".into(),
                    password: "password".into(),
                    allow_insecure_http: true,
                })
                .await
                .is_err());
            assert!(desktop.request(Request::Account).await.unwrap().is_null());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn duplicate_and_disabled_registration_errors_do_not_create_accounts_or_echo_secrets() {
        for status in [403, 409] {
            let (url, server) = mock_manager(vec![reply(
                "POST",
                "/v1/auth/register",
                status,
                json!({"error":"SECRET_PASSWORD"}),
            )])
            .await;
            let desktop = desktop();
            let error = desktop
                .request(Request::Register {
                    manager_url: url,
                    workspace_slug: "acme".into(),
                    workspace_name: "Acme".into(),
                    workspace_kind: WorkspaceKind::Personal,
                    display_name: "Exact".into(),
                    email: "exact@example.test".into(),
                    password: "long-password-123".into(),
                    allow_insecure_http: true,
                })
                .await
                .unwrap_err();
            assert_eq!(
                error.code,
                if status == 403 {
                    "forbidden"
                } else {
                    "conflict"
                }
            );
            assert!(!format!("{error:?}").contains("SECRET_PASSWORD"));
            assert!(desktop.request(Request::Account).await.unwrap().is_null());
            server.await.unwrap();
        }
    }

    #[test]
    fn signup_bridge_shapes_are_closed_and_validation_is_explicit() {
        let register = json!({"op":"register","manager_url":"https://manager.test","workspace_slug":"acme","workspace_name":"Acme","workspace_kind":"PERSONAL","display_name":"Exact","email":"exact@example.test","password":"long-password-123"});
        assert!(serde_json::from_value::<Request>(register.clone()).is_ok());
        let mut unknown_role = register.clone();
        unknown_role["role"] = json!("ADMIN");
        assert!(serde_json::from_value::<Request>(unknown_role).is_err());
        let mut invalid_kind = register;
        invalid_kind["workspace_kind"] = json!("UNKNOWN");
        assert!(serde_json::from_value::<Request>(invalid_kind).is_err());
        assert!(serde_json::from_value::<Request>(json!({"op":"accept_invitation","manager_url":"https://manager.test","token":"code","display_name":"Exact","email":"exact@example.test","password":"long-password-123","tenant":"other"})).is_err());
        assert!(serde_json::from_value::<Request>(
            json!({"op":"create_invitation","email":"exact@example.test","role":"ADMIN"})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(
            json!({"op":"revoke_invitation","id":"../../auth"})
        )
        .is_err());
        assert!(validate_signup("Exact", "exact@example.test", "short").is_err());
        assert!(validate_signup(" ", "exact@example.test", "long-password-123").is_err());
        assert!(validate_signup("Exact", "not-email", "long-password-123").is_err());
        assert!(validate_signup("Exact", "exact@example.test", "long-password-123").is_ok());
        assert!(
            validate_signup(&"名".repeat(67), "exact@example.test", "long-password-123").is_err()
        );
        assert!(validate_signup("Exact", "exact@example.test", &"a".repeat(1025)).is_err());
        assert!(validate_signup("Exact", "multiple@@example.test", "long-password-123").is_err());
        assert!(serde_json::from_value::<Request>(json!({"op":"set_user_disabled","user_id":uuid::Uuid::nil(),"disabled":true,"role":"ADMIN"})).is_err());
        assert!(serde_json::from_value::<Request>(
            json!({"op":"group_members","group_id":"../../users"})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(json!({"op":"add_group_member","group_id":uuid::Uuid::nil(),"user_id":uuid::Uuid::nil(),"tenant":"other"})).is_err());
    }

    #[tokio::test]
    async fn rejected_invitation_has_actionable_error_without_echoing_secret_response() {
        let (url, server) = mock_manager(vec![reply(
            "POST",
            "/v1/auth/accept-invitation",
            401,
            json!({"error":"SECRET_TOKEN"}),
        )])
        .await;
        let desktop = desktop();
        let error = desktop
            .request(Request::AcceptInvitation {
                manager_url: url,
                token: "expired-code".into(),
                display_name: "Member".into(),
                email: "member@example.test".into(),
                password: "long-password-123".into(),
                allow_insecure_http: true,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_invitation");
        assert!(!format!("{error:?}").contains("SECRET_TOKEN"));
        assert!(desktop.request(Request::Account).await.unwrap().is_null());
        server.await.unwrap();
    }

    #[test]
    fn command_ids_cannot_inject_paths_and_commands_have_no_shell_surface() {
        assert!(serde_json::from_value::<Request>(
            json!({"op":"resource","id":"../../auth/login"})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(
            json!({"op":"send_files","session_id":uuid::Uuid::new_v4(),"paths":["/etc/passwd"]})
        )
        .is_err());
        assert!(serde_json::from_value::<Request>(json!({"op":"shell","command":"rm"})).is_err());
        assert!(serde_json::from_value::<Request>(
            json!({"op":"open_permission_settings","permission":"https://evil.test"})
        )
        .is_err());
    }

    #[tokio::test]
    async fn authentication_invalidation_cancels_inflight_work() {
        let desktop = Desktop::new(
            "missing-client".into(),
            "missing-agent".into(),
            "unused-identity".into(),
        );
        let (_, old, _) = desktop.invalidate().await;
        let (generation, current, _) = desktop.invalidate().await;
        assert!(old.is_cancelled());
        assert!(!current.is_cancelled());
        assert_eq!(generation, 2);
        assert!(desktop.context().await.is_err());
        assert!(desktop.sessions.list().await.is_empty());
    }

    #[test]
    fn desktop_enrollment_always_assigns_the_current_account() {
        let id = uuid::Uuid::new_v4();
        let body = enrollment_body("Office Mac", id);
        assert_eq!(body["owner_user_id"], id.to_string());
        assert_eq!(body["machine_name"], "Office Mac");
    }

    #[test]
    fn historical_grant_lifecycle_and_labels_survive_normalization() {
        let user = uuid::Uuid::new_v4();
        let grant: Entitlement = serde_json::from_value(json!({
            "id":uuid::Uuid::new_v4(),"resource_id":uuid::Uuid::new_v4(),
            "subject_kind":"USER","subject_id":user,"role":"VIEWER",
            "allow_clipboard":false,"allow_audio":false,"allow_file_transfer":false,
            "user_email":"person@example.test","user_display_name":"Person","group_name":null,
            "expires_at":"2026-09-08T12:00:00Z","revoked_at":"2026-09-07T12:00:00Z"
        }))
        .unwrap();
        let grant = value(Grant::from(grant)).unwrap();
        assert_eq!(grant["user_id"], user.to_string());
        assert_eq!(grant["user_email"], "person@example.test");
        assert_eq!(grant["expires_at"], "2026-09-08T12:00:00Z");
        assert_eq!(grant["revoked_at"], "2026-09-07T12:00:00Z");
    }
}
