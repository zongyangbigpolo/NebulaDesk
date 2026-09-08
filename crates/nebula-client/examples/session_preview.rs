//! Open the real native controls without connecting or capturing a desktop.
use nebula_client::{manager::SessionTicket, session};
use nebula_common::SessionPolicy;
use uuid::Uuid;

fn main() -> anyhow::Result<()> {
    let ticket = SessionTicket {
        session_id: Uuid::nil(),
        ticket: "preview-not-a-ticket".into(),
        gateway_addr: "127.0.0.1:1".into(),
        gateway_pin: String::new(),
        // Key validation fails before the client can attempt a connection.
        agent_key: "preview-not-a-key".into(),
        policy: SessionPolicy::view_only(),
    };
    session::run(ticket, "NebulaDesk UI preview (offline)")
}
