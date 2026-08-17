use air_transcode::{AppState, Config, app};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("air_transcode=info,tower_http=info")),
        )
        .json()
        .init();
    let config = Config::parse();
    air_transcode::initialize()?;
    let state = AppState::new(config.clone())?;
    if !state.capabilities.cmaf {
        return Err("GStreamer cmafmux plugin is required".into());
    }
    let listener = TcpListener::bind(config.bind).await?;
    info!(address = %config.bind, "air-transcode listening");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
