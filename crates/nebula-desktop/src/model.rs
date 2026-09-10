pub use nebula_desktop_protocol::{
    ConnectionPath, Policy, SessionState, TransferDirection as Direction, TransferState,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ConnectionSettings,
    Users,
    SetUserDisabled {
        user_id: Uuid,
        disabled: bool,
    },
    Groups,
    CreateGroup {
        name: String,
    },
    DeleteGroup {
        group_id: Uuid,
    },
    GroupMembers {
        group_id: Uuid,
    },
    AddGroupMember {
        group_id: Uuid,
        user_id: Uuid,
    },
    RemoveGroupMember {
        group_id: Uuid,
        user_id: Uuid,
    },
    RegistrationOptions {
        manager_url: String,
        #[serde(default)]
        allow_insecure_http: bool,
    },
    Register {
        manager_url: String,
        workspace_slug: String,
        workspace_name: String,
        workspace_kind: WorkspaceKind,
        display_name: String,
        email: String,
        password: String,
        #[serde(default)]
        allow_insecure_http: bool,
    },
    AcceptInvitation {
        manager_url: String,
        token: String,
        display_name: String,
        email: String,
        password: String,
        #[serde(default)]
        allow_insecure_http: bool,
    },
    CreateInvitation {
        email: String,
    },
    Invitations,
    RevokeInvitation {
        id: Uuid,
    },
    Login {
        manager_url: String,
        tenant: String,
        email: String,
        password: String,
        #[serde(default)]
        allow_insecure_http: bool,
    },
    Logout,
    Account,
    Resources,
    Machines,
    Resource {
        id: Uuid,
    },
    Connect {
        resource_id: Uuid,
        #[serde(default)]
        keyboard_profile: ApplicationKeyboardProfile,
    },
    Sessions,
    FocusSession {
        session_id: Uuid,
    },
    DisconnectSession {
        session_id: Uuid,
    },
    LocalHost,
    OpenPermissionSettings {
        permission: Permission,
    },
    EnrollLocal {
        manager_url: String,
        token: String,
        name: String,
        #[serde(default)]
        allow_insecure_http: bool,
    },
    SetHostEnabled {
        enabled: bool,
    },
    CreateEnrollment {
        name: String,
    },
    RenameMachine {
        machine_id: Uuid,
        name: String,
    },
    RemoveMachine {
        machine_id: Uuid,
    },
    MachineResources {
        machine_id: Uuid,
    },
    PublishResource {
        machine_id: Uuid,
        resource: Publication,
    },
    UpdateResource {
        resource_id: Uuid,
        changes: ResourceChanges,
    },
    Grants {
        resource_id: Uuid,
    },
    GrantAccess {
        resource_id: Uuid,
        email: String,
        role: Role,
        allow_clipboard: bool,
        allow_file_transfer: bool,
        allow_audio: bool,
    },
    RevokeAccess {
        entitlement_id: Uuid,
    },
    Transfers,
    SendFiles {
        session_id: Uuid,
    },
}

#[derive(Deserialize, Serialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "UPPERCASE")]
pub enum WorkspaceKind {
    Personal,
    Organization,
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Workspace {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub kind: WorkspaceKind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationKeyboardProfile {
    #[default]
    Physical,
    Editing,
    Terminal,
}

impl ApplicationKeyboardProfile {
    pub fn client_args(self) -> [&'static str; 4] {
        let (mode, profile) = match self {
            Self::Physical => ("physical", "physical"),
            Self::Editing => ("semantic", "editing"),
            Self::Terminal => ("semantic", "terminal"),
        };
        ["--keyboard-mode", mode, "--keyboard-profile", profile]
    }
}

#[derive(Deserialize, Serialize)]
pub struct RegistrationOptions {
    pub self_registration_enabled: bool,
}

#[derive(Serialize)]
pub struct ConnectionSettings {
    pub manager_url: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct DirectoryUser {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: String,
    pub disabled: bool,
}

#[derive(Deserialize, Serialize)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
}

#[derive(Deserialize, Serialize)]
pub struct Invitation {
    pub id: Uuid,
    pub email: String,
    pub expires_at: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub accepted_at: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct CreatedInvitation {
    #[serde(flatten)]
    pub invitation: Invitation,
    pub token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Screen,
    Input,
    Audio,
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
pub enum Kind {
    Desktop,
    App,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Role {
    Viewer,
    Controller,
    Admin,
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
    pub user_email: Option<String>,
    pub user_display_name: Option<String>,
    pub group_name: Option<String>,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
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
    pub user_email: Option<String>,
    pub user_display_name: Option<String>,
    pub group_name: Option<String>,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub role: String,
    pub allow_clipboard: bool,
    pub allow_file_transfer: bool,
    pub allow_audio: bool,
}

impl From<Entitlement> for Grant {
    fn from(value: Entitlement) -> Self {
        Self {
            id: value.id,
            resource_id: value.resource_id,
            user_id: (value.subject_kind == "USER").then_some(value.subject_id),
            group_id: (value.subject_kind == "GROUP").then_some(value.subject_id),
            user_email: value.user_email,
            user_display_name: value.user_display_name,
            group_name: value.group_name,
            expires_at: value.expires_at,
            revoked_at: value.revoked_at,
            role: value.role,
            allow_clipboard: value.allow_clipboard,
            allow_file_transfer: value.allow_file_transfer,
            allow_audio: value.allow_audio,
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
    pub workspace: Workspace,
}

pub trait StateExt {
    fn active(self) -> bool;
}
impl StateExt for SessionState {
    fn active(self) -> bool {
        matches!(self, Self::Connecting | Self::Connected)
    }
}

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
