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
            auth: Mutex::new(Authentication::default()),
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

    pub async fn request(&self, request: Request) -> Result<Value> {
        match request {
            Request::Login { manager_url, tenant, email, password, allow_insecure_http } => {
                let manager = Manager::new(&manager_url, allow_insecure_http)?;
                if tenant.len() > 256 || email.len() > 320 || password.len() > 4096 {
                    return Err(DesktopError::new("invalid_input", "Login fields exceed their size limit."));
                }
                let (generation, cancelled, previous) = self.invalidate().await;
                if let Some(previous) = previous {
                    tokio::spawn(async move {
                        if previous.logout().await.is_err() {
                            eprintln!("The previous manager login could not be revoked remotely.");
                        }
                    });
                }
                let account = tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => return Err(DesktopError::cancelled()),
                    account = manager.login(tenant.clone(), email, password) => Arc::new(account?),
                };
                let mut auth = self.auth.lock().await;
                if auth.generation != generation {
                    drop(auth);
                    if account.logout().await.is_err() {
                        eprintln!("The cancelled manager login could not be revoked remotely.");
                    }
                    return Err(DesktopError::cancelled());
                }
                let view = account_view(&account, &tenant);
                auth.account = Some(account);
                auth.tenant = tenant;
                value(view)
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
            Request::Resources => value(self.api::<Vec<Resource>>(Method::GET, "v1/resources", None).await?),
            Request::Resource { id } => value(self.api::<Resource>(Method::GET, &format!("v1/resources/{id}"), None).await?),
            Request::Machines => value(self.api::<Vec<Machine>>(Method::GET, "v1/machines", None).await?),
            Request::CreateEnrollment { name } => {
                nonempty(&name, 255)?;
                value(self.api::<Enrollment>(Method::POST, "v1/machines/enrollment-tokens", Some(&json!({"machine_name":name}))).await?)
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
            Request::Connect { resource_id } => {
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
                        let resource: Resource = account.request(Method::GET, &format!("v1/resources/{resource_id}"), None).await?;
                        if !resource.launch_supported || matches!(resource.kind, Kind::App) {
                            return Err(DesktopError::new("unsupported", "This resource does not support native desktop streaming."));
                        }
                        let ticket: Ticket = account.request(Method::POST, "v1/sessions", Some(&json!({"resource_id":resource_id,"client_os":std::env::consts::OS}))).await?;
                        Ok((resource, ticket))
                    } => result?,
                };
                // Serialize the final spawn with invalidation. A cancelled login cannot launch later.
                let auth = self.auth.lock().await;
                if cancelled.is_cancelled() { return Err(DesktopError::cancelled()); }
                let session = self.sessions.start(resource, ticket).await?;
                drop(auth);
                value(session)
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

fn value<T: Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|_| DesktopError::protocol())
}

fn account_view(account: &Account, tenant: &str) -> AccountView {
    AccountView {
        id: account.user.id,
        email: account.user.email.clone(),
        display_name: account.user.display_name.clone(),
        role: account.user.role.clone(),
        tenant: tenant.into(),
        manager_url: account.manager.url().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
