//! Headless routing diagnostic for an authorised real resource.
//! Optionally closes only the direct connection to exercise normal failover.
//! Counts received records, not decoded or physically presented pictures.

use std::time::{Duration, Instant};

use clap::Parser;
use ndp_proto::{Channel, MsgFlags, MsgHeader, MsgKind};
use ndp_transport::{PathKind, Session};
use nebula_client::ManagerClient;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "NEBULA_MANAGER_URL")]
    manager_url: String,
    #[arg(long, env = "NEBULA_TENANT")]
    tenant: String,
    #[arg(long, env = "NEBULA_EMAIL")]
    email: String,
    /// Exact resource UUID, obtained with `nebula-client list`.
    #[arg(long)]
    resource_id: String,
    #[arg(long, default_value_t = 45)]
    seconds: u64,
    /// Close direct after this many seconds on it; leave both processes alive.
    #[arg(long)]
    disconnect_direct_after: Option<u64>,
}

struct Lifetime {
    session: Session,
    gateway: quinn::Connection,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.session.close(0, b"path probe finished");
        self.gateway.close(0u32.into(), b"path probe finished");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "path_probe=info,ndp_transport=info,warn".into()),
        )
        .init();
    let args = Args::parse();
    anyhow::ensure!(
        (5..=600).contains(&args.seconds),
        "duration must be 5..=600 seconds"
    );
    if let Some(after) = args.disconnect_direct_after {
        anyhow::ensure!(
            after > 0 && after < args.seconds,
            "failure time must be inside the probe duration"
        );
    }
    let password = std::env::var("NEBULA_PASSWORD")?;
    let manager =
        ManagerClient::login(&args.manager_url, &args.tenant, &args.email, &password).await?;
    let ticket = manager.open(&args.resource_id).await?;
    let connected = nebula_client::connect_to_agent(&ticket).await?;
    let mut incoming = connected.incoming;
    let session = connected.session;
    let mut changes = session
        .path_changes()
        .ok_or_else(|| anyhow::anyhow!("the peer did not negotiate multipath"))?;
    let _lifetime = Lifetime {
        session: session.clone(),
        gateway: connected.gateway,
    };
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut direct_since = None;
    let mut failed_at = None;
    let mut failed_us = 0;
    let mut fence = None;
    let mut highest = None;
    let mut fallback = false;
    let mut fallback_keyframe = false;
    let mut fallback_pong = false;
    let mut frames = 0u64;
    let mut bytes = 0u64;
    tracing::info!(session = %ticket.session_id, "routing probe started");

    while started.elapsed() < Duration::from_secs(args.seconds) {
        tokio::select! {
            changed = changes.changed() => {
                changed?;
                let state = *changes.borrow();
                tracing::info!(session = %ticket.session_id, ?state, "observed route");
                if state.kind == PathKind::Direct {
                    direct_since.get_or_insert_with(Instant::now);
                } else if let Some(at) = failed_at {
                    fallback = true;
                    tracing::info!(elapsed_ms = Instant::now().duration_since(at).as_millis(), "relay fallback observed");
                    session.send(Channel::Control, MsgHeader::new(MsgKind::CapsUpdate, 0, 0), b"").await?;
                }
            }
            _ = tick.tick() => {
                if failed_at.is_none()
                    && matches!((direct_since, args.disconnect_direct_after),
                        (Some(since), Some(after)) if since.elapsed() >= Duration::from_secs(after))
                {
                    fence = highest;
                    failed_at = Some(Instant::now());
                    failed_us = started.elapsed().as_micros() as u64;
                    session.disconnect_direct()?;
                    tracing::info!(session = %ticket.session_id, "closed only direct connection to exercise fallback");
                }
                let echo = started.elapsed().as_micros() as u64;
                session.send(Channel::Control, MsgHeader::new(MsgKind::Ping, 0, echo), b"path-probe").await?;
                if started.elapsed().as_secs() % 5 == 0 {
                    tracing::info!(frames, bytes, paths = ?session.path_stats(), "routing probe counters");
                }
            }
            message = incoming.recv() => {
                let message = message.ok_or_else(|| anyhow::anyhow!("logical session ended during probe"))??;
                let relay = changes.borrow().kind == PathKind::Relay;
                if message.channel == Channel::Video {
                    tracing::debug!(
                        seq = message.header.seq,
                        timestamp_us = message.header.timestamp_us,
                        keyframe = message.header.flags.contains(MsgFlags::KEYFRAME),
                        nal_types = ?nebula_client::nal::units(&message.payload)
                            .filter_map(nebula_client::nal::kind).take(8).collect::<Vec<_>>(),
                        bytes = message.payload.len(),
                        relay, fallback, ?fence,
                        "received video probe frame"
                    );
                    frames += 1;
                    bytes += message.payload.len() as u64;
                    let seq = message.header.seq;
                    if highest.is_none_or(|old: u32| {
                        let ahead = seq.wrapping_sub(old);
                        ahead != 0 && ahead < u32::MAX / 2
                    }) {
                        highest = Some(seq);
                    }
                    if fallback && relay && message.header.flags.contains(MsgFlags::KEYFRAME)
                        && nebula_client::nal::units(&message.payload)
                            .any(|unit| nebula_client::nal::kind(unit) == Some(nebula_client::nal::IDR))
                        && fence.is_none_or(|old: u32| {
                            let ahead = seq.wrapping_sub(old);
                            ahead != 0 && ahead < u32::MAX / 2
                        })
                    {
                        fallback_keyframe = true;
                    }
                }
                if fallback && relay && message.channel == Channel::Control
                    && message.header.kind == MsgKind::Pong
                    && message.payload == b"path-probe"
                    && message.header.timestamp_us > failed_us
                {
                    fallback_pong = true;
                }
            }
        }
    }
    anyhow::ensure!(direct_since.is_some(), "no faster direct path was selected");
    if args.disconnect_direct_after.is_some() {
        anyhow::ensure!(
            fallback && fallback_keyframe && fallback_pong,
            "failover incomplete: relay={fallback}, keyframe={fallback_keyframe}, pong={fallback_pong}"
        );
    }
    tracing::info!(
        session = %ticket.session_id, frames, bytes, fallback, fallback_keyframe, fallback_pong,
        "routing probe completed"
    );
    Ok(())
}
