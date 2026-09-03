//! Observability bootstrap: structured logging shared by all binaries.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Initialise process-wide tracing.
///
/// `NEBULA_LOG` (falling back to `RUST_LOG`) controls filtering; set
/// `NEBULA_LOG_FORMAT=json` for machine-readable output in production.
pub fn init(service: &'static str) {
    let filter = EnvFilter::try_from_env("NEBULA_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info,quinn=warn,rustls=warn"));

    let json = std::env::var("NEBULA_LOG_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("json"));

    let registry = tracing_subscriber::registry().with(filter);
    if json {
        registry
            .with(tracing_subscriber::fmt::layer().json().with_target(true))
            .init();
    } else {
        registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_target(true)
                    .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr())),
            )
            .init();
    }

    tracing::info!(service, version = env!("CARGO_PKG_VERSION"), "starting");
}
