//! The registry of live agent control tunnels.
//!
//! An agent dials *out* to its gateway and keeps one QUIC connection open for
//! as long as it is available. That is what makes NebulaDesk work behind NAT
//! without any port forwarding: when a client wants a session, the gateway
//! already has a path to the machine and simply pushes a request down it.

use std::collections::HashMap;
use std::sync::Arc;

use ndp_signal::GatewayMessage;
use nebula_common::MachineId;
use tokio::sync::{mpsc, Mutex};

/// One agent's attachment to this gateway.
#[derive(Debug, Clone)]
pub struct Tunnel {
    /// Messages queued for the agent. Bounded, because an agent that stops
    /// reading must not be able to grow the gateway's memory without limit.
    pub outbound: mpsc::Sender<GatewayMessage>,
    /// The QUIC connection's stable id, used to tell two tunnels for the same
    /// machine apart.
    pub connection: u64,
    /// The machine's credential, retained only for the tunnel's lifetime.
    ///
    /// Holding it lets the gateway tell the manager the machine is draining
    /// the moment the tunnel drops, instead of leaving the machine advertised
    /// as reachable until its heartbeat goes stale. It is dropped with the
    /// tunnel and never written anywhere.
    pub credential: String,
    /// Live peer opt-in; prevents stale machine capability data reaching old agents.
    pub application_windows: bool,
}

/// Every agent currently attached to this gateway.
#[derive(Debug, Default)]
pub struct Tunnels {
    inner: Mutex<HashMap<MachineId, Tunnel>>,
}

/// What happened to the tunnel a machine previously had.
#[derive(Debug)]
pub enum Replaced {
    /// The machine had no tunnel here.
    None,
    /// An older tunnel was displaced and should be closed.
    Older(Tunnel),
}

impl Tunnels {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Attach a tunnel, displacing any previous one for the same machine.
    ///
    /// The newest connection wins: an agent that reconnects after a network
    /// change has a working path, and the connection it replaced almost
    /// certainly does not. Refusing the new one would strand the machine
    /// until the dead connection's idle timeout expired.
    pub async fn insert(&self, machine: MachineId, tunnel: Tunnel) -> Replaced {
        match self.inner.lock().await.insert(machine, tunnel) {
            Some(old) => Replaced::Older(old),
            None => Replaced::None,
        }
    }

    /// Look up a machine's tunnel.
    pub async fn get(&self, machine: MachineId) -> Option<Tunnel> {
        self.inner.lock().await.get(&machine).cloned()
    }

    /// Detach a tunnel, but only if it is still the one registered.
    ///
    /// A reconnecting agent may already have installed its replacement by the
    /// time the old connection notices it is dead; removing blindly would
    /// unregister a perfectly good tunnel.
    pub async fn remove(&self, machine: MachineId, connection: u64) -> Option<Tunnel> {
        let mut guard = self.inner.lock().await;
        match guard.get(&machine) {
            Some(current) if current.connection == connection => guard.remove(&machine),
            _ => None,
        }
    }

    /// How many agents are attached.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Whether no agents are attached.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel(connection: u64) -> Tunnel {
        let (outbound, _rx) = mpsc::channel(1);
        Tunnel {
            outbound,
            connection,
            credential: "cred".into(),
            application_windows: false,
        }
    }

    #[tokio::test]
    async fn a_reconnecting_agent_displaces_its_old_tunnel() {
        let tunnels = Tunnels::new();
        let machine = MachineId::new();

        assert!(matches!(
            tunnels.insert(machine, tunnel(1)).await,
            Replaced::None
        ));
        let Replaced::Older(old) = tunnels.insert(machine, tunnel(2)).await else {
            panic!("the older tunnel should have been displaced");
        };
        assert_eq!(old.connection, 1);
        assert_eq!(tunnels.get(machine).await.unwrap().connection, 2);
    }

    #[tokio::test]
    async fn a_dead_connection_cannot_unregister_its_replacement() {
        let tunnels = Tunnels::new();
        let machine = MachineId::new();

        tunnels.insert(machine, tunnel(1)).await;
        tunnels.insert(machine, tunnel(2)).await;

        // Connection 1 finally notices it is gone and tries to clean up.
        assert!(tunnels.remove(machine, 1).await.is_none());
        assert_eq!(tunnels.get(machine).await.unwrap().connection, 2);

        assert!(tunnels.remove(machine, 2).await.is_some());
        assert!(tunnels.is_empty().await);
    }
}
