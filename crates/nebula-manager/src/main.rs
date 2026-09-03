//! Manager entry point.

use std::net::SocketAddr;

use nebula_manager::{router, AppState, Config};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("NEBULA_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let config = Config::from_env()?;
    let listen = config.listen;
    let state = AppState::bootstrap(config).await?;
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(addr = %listener.local_addr()?, "manager listening");

    // `into_make_service_with_connect_info` is what makes the peer address
    // available to the audit trail; without it every entry would be blank.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

/// Stop accepting on SIGINT or SIGTERM, letting in-flight requests finish.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutting down");
}
