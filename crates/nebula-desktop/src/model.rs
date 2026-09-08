use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Login { manager_url: String, tenant: String, email: String, password: String, #[serde(default)] allow_insecure_http: bool },
    Logout,
    Account,
    Resources,
    Machines,
    Resource { id: Uuid },
    Connect { resource_id: Uuid },
    Sessions,
    FocusSession { session_id: Uuid },
    DisconnectSession { session_id: Uuid },
    LocalHost,
    EnrollLocal { manager_url: String, token: String, name: String, #[serde(default)] allow_insecure_http: bool },
    SetHostEnabled { enabled: bool },
    CreateEnrollment { name: String },
    RenameMachine { machine_id: Uuid, name: String },
    RemoveMachine { machine_id: Uuid },
    MachineResources { machine_id: Uuid },
    PublishResource { machine_id: Uuid, resource: Publication },
    UpdateResource { resource_id: Uuid, changes: ResourceChanges },
    Grants { resource_id: Uuid },
    GrantAccess { resource_id: Uuid, email: String, role: Role, allow_clipboard: bool, allow_file_transfer: bool, allow_audio: bool },
    RevokeAccess { entitlement_id: Uuid },
    Transfers,
    SendFiles { session_id: Uuid },
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub kind: Kind,
    pub name: String,
    pub description: String,
    pub launch_path: Option<String>,
    #[serde(default)]
    pub launch_args: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceChanges {
    pub name: Option<String>,
    pub description: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "UPPERCASE")]
pub enum Kind { Desktop, App }

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Role { Viewer, Controller, Admin }

#[derive(Deserialize, Serialize, Clone)]
pub struct Policy {
    pub input: bool,
    pub audio: bool,
    pub clipboard: bool,
    pub file_transfer: bool,
}

#[derive(Deserialize, Serialize)]
pub struct Resource {
    pub id: Uuid,
    pub name: String,
    pub kind: Kind,
    pub description: String,
    pub machine_status: String,
    pub role: Option<String>,
    pub policy: Policy,
    pub owner_name: Option<String>,
    pub owned: bool,
    pub machine_id: Option<Uuid>,
    pub os: Option<String>,
    pub os_version: Option<String>,
    pub last_seen_at: Option<String>,
    pub launch_supported: bool,
}

#[derive(Deserialize, Serialize)]
pub struct Machine {
    pub id: Uuid,
    pub name: String,
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub status: String,
    pub owner_user_id: Option<Uuid>,
    pub last_seen_at: Option<String>,
    pub capabilities: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
pub struct PublishedResource {
    pub id: Uuid,
    pub machine_id: Uuid,
    pub kind: Kind,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    pub launch_path: Option<String>,
}

#[derive(Deserialize)]
pub struct Entitlement {
    pub id: Uuid,
    pub resource_id: Uuid,
    pub subject_kind: String,
    pub subject_id: Uuid,
    pub role: String,
    pub allow_clipboard: bool,
    pub allow_file_transfer: bool,
    pub allow_audio: bool,
}

#[derive(Serialize)]
pub struct Grant {
    pub id: Uuid,
    pub resource_id: Uuid,
    pub user_id: Option<Uuid>,
    pub group_id: Option<Uuid>,
    pub role: String,
    pub allow_clipboard: bool,
    pub allow_file_transfer: bool,
    pub allow_audio: bool,
}

impl From<Entitlement> for Grant {
    fn from(value: Entitlement) -> Self {
        Self {
            id: value.id, resource_id: value.resource_id,
            user_id: (value.subject_kind == "USER").then_some(value.subject_id),
            group_id: (value.subject_kind == "GROUP").then_some(value.subject_id),
            role: value.role, allow_clipboard: value.allow_clipboard,
            allow_file_transfer: value.allow_file_transfer, allow_audio: value.allow_audio,
        }
    }
}

#[derive(Deserialize, Serialize)]
pub struct Enrollment {
    pub token: String,
    pub expires_at: String,
}

#[derive(Serialize)]
pub struct AccountView {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub tenant: String,
    pub manager_url: String,
}

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionState { Connecting, Connected, Disconnected, Failed }

impl SessionState {
    pub fn active(self) -> bool {
        matches!(self, Self::Connecting | Self::Connected)
    }
}

#[derive(Deserialize, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionPath { Direct, Relay }

#[derive(Serialize, Clone)]
pub struct Session {
    pub session_id: Uuid,
    pub resource_id: Uuid,
    pub name: String,
    pub state: SessionState,
    pub path: Option<ConnectionPath>,
    pub started_at: String,
    pub error: Option<String>,
    pub rtt_ms: Option<f64>,
}

#[derive(Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Direction { Send, Receive }

#[derive(Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TransferState { Offered, Transferring, Complete, Failed }

#[derive(Serialize, Clone)]
pub struct Transfer {
    pub id: String,
    pub session_id: Uuid,
    pub name: String,
    pub direction: Direction,
    pub transferred: u64,
    pub total: u64,
    pub state: TransferState,
    pub error: Option<String>,
}
