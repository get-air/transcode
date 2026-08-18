use std::{net::SocketAddr, path::PathBuf, time::Duration};

use clap::Parser;

/// Runtime configuration for the transcoding server.
#[derive(Clone, Debug, Parser)]
#[command(name = "air-transcode", version, about)]
pub struct Config {
    /// Address to bind. Loopback is the safe default.
    #[arg(long, env = "AIR_TRANSCODE_BIND", default_value = "127.0.0.1:11471")]
    pub bind: SocketAddr,

    /// Directory used for init fragments and bounded segment caching.
    #[arg(
        long,
        env = "AIR_TRANSCODE_CACHE_DIR",
        default_value = ".cache/air-transcode"
    )]
    pub cache_dir: PathBuf,

    /// Target HLS segment duration.
    #[arg(long, env = "AIR_TRANSCODE_SEGMENT_SECONDS", default_value_t = 4)]
    pub segment_seconds: u32,

    /// Maximum number of sessions held in memory.
    #[arg(long, env = "AIR_TRANSCODE_MAX_SESSIONS", default_value_t = 16)]
    pub max_sessions: usize,

    /// Maximum simultaneously executing `GStreamer` pipelines.
    #[arg(long, env = "AIR_TRANSCODE_MAX_PIPELINES", default_value_t = 2)]
    pub max_pipelines: usize,

    /// Maximum cached media segments retained per session.
    #[arg(long, env = "AIR_TRANSCODE_MAX_CACHED_SEGMENTS", default_value_t = 64)]
    pub max_cached_segments: usize,

    /// Inactive session lifetime in seconds.
    #[arg(long, env = "AIR_TRANSCODE_SESSION_TTL_SECONDS", default_value_t = 300)]
    pub session_ttl_seconds: u64,

    /// Maximum time allowed for source discovery.
    #[arg(
        long,
        env = "AIR_TRANSCODE_PROBE_TIMEOUT_SECONDS",
        default_value_t = 20
    )]
    pub probe_timeout_seconds: u64,
}

impl Config {
    /// Safe ephemeral loopback configuration for embedding in desktop applications.
    #[must_use]
    pub fn loopback(cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            cache_dir: cache_dir.into(),
            segment_seconds: 4,
            max_sessions: 16,
            max_pipelines: 2,
            max_cached_segments: 64,
            session_ttl_seconds: 300,
            probe_timeout_seconds: 20,
        }
    }

    #[must_use]
    pub const fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.session_ttl_seconds)
    }

    #[must_use]
    pub const fn probe_timeout(&self) -> Duration {
        Duration::from_secs(self.probe_timeout_seconds)
    }
}
