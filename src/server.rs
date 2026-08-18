use std::net::{IpAddr, SocketAddr};

use axum::Router;
use tokio::{net::TcpListener, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AppState, Config, Error, Result, app, initialize, media_app};

/// Running in-process HTTP server suitable for Tauri and other native hosts.
pub struct EmbeddedServer {
    address: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<Result<()>>>,
}

impl EmbeddedServer {
    /// Actual listener address. Ephemeral port requests are resolved here.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// HTTP origin for loopback clients such as a Tauri `WebView`.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Gracefully stop accepting requests and wait for the server task.
    ///
    /// # Errors
    ///
    /// Returns an error when the server task or listener fails.
    pub async fn shutdown(mut self) -> Result<()> {
        self.shutdown.cancel();
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await.map_err(|error| Error::Task(error.to_string()))?
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Paired loopback administration and tokenized LAN media listeners for Tauri casting.
pub struct EmbeddedCastHost {
    admin: Option<EmbeddedServer>,
    media: Option<EmbeddedServer>,
    access_token: String,
}

impl EmbeddedCastHost {
    /// Loopback origin injected into `@get-air/transcode` in the Tauri `WebView`.
    #[must_use]
    pub fn admin_origin(&self) -> String {
        self.admin
            .as_ref()
            .map_or_else(String::new, EmbeddedServer::origin)
    }

    /// LAN listener address. Use its port with the host's advertised LAN address.
    #[must_use]
    pub fn media_address(&self) -> Option<SocketAddr> {
        self.media.as_ref().map(EmbeddedServer::address)
    }

    /// Tokenized media URL for a session master path returned by the admin API.
    #[must_use]
    pub fn cast_url(&self, advertised_ip: IpAddr, master_path: &str) -> Option<String> {
        let port = self.media_address()?.port();
        let address = SocketAddr::new(advertised_ip, port);
        Some(format!(
            "http://{address}/cast/{}/{}",
            self.access_token,
            master_path.trim_start_matches('/'),
        ))
    }

    /// Stop both listeners and wait for their tasks.
    ///
    /// # Errors
    ///
    /// Returns the first listener shutdown failure.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(admin) = self.admin.take() {
            admin.shutdown().await?;
        }
        if let Some(media) = self.media.take() {
            media.shutdown().await?;
        }
        Ok(())
    }
}

/// Start the `GStreamer` service inside the current Tokio runtime.
///
/// Use [`Config::loopback`] for a safe desktop-local origin. A LAN cast
/// listener requires an application-owned access policy and should not expose
/// the administrative session routes directly.
///
/// # Errors
///
/// Returns an error when `GStreamer`, CMAF support, cache setup, or binding fails.
pub async fn spawn_server(config: Config) -> Result<EmbeddedServer> {
    initialize()?;
    let state = AppState::new(config.clone())?;
    if !state.capabilities.cmaf {
        return Err(Error::MissingElement("cmafmux".to_owned()));
    }
    let listener = TcpListener::bind(config.bind).await?;
    spawn_listener(listener, app(state))
}

/// Start loopback administration plus a tokenized read-only LAN media surface.
///
/// # Errors
///
/// Returns an error when the admin bind is not loopback or either listener fails.
pub async fn spawn_tauri_host(config: Config, media_bind: SocketAddr) -> Result<EmbeddedCastHost> {
    if !config.bind.ip().is_loopback() {
        return Err(Error::InvalidOutput(
            "the embedded Tauri admin listener must bind to loopback".to_owned(),
        ));
    }
    initialize()?;
    let state = AppState::new(config.clone())?;
    if !state.capabilities.cmaf {
        return Err(Error::MissingElement("cmafmux".to_owned()));
    }
    let admin_listener = TcpListener::bind(config.bind).await?;
    let media_listener = TcpListener::bind(media_bind).await?;
    let access_token = Uuid::new_v4().simple().to_string();
    let media_router =
        Router::new().nest(&format!("/cast/{access_token}"), media_app(state.clone()));
    Ok(EmbeddedCastHost {
        admin: Some(spawn_listener(admin_listener, app(state))?),
        media: Some(spawn_listener(media_listener, media_router)?),
        access_token,
    })
}

fn spawn_listener(listener: TcpListener, router: Router) -> Result<EmbeddedServer> {
    let address = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let shutdown_signal = shutdown.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown_signal.cancelled_owned())
            .await
            .map_err(Error::Io)
    });
    Ok(EmbeddedServer {
        address,
        shutdown,
        task: Some(task),
    })
}
