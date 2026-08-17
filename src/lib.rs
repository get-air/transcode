//! GStreamer-only HTTP VOD transmuxing and transcoding.

pub mod api;
pub mod config;
pub mod error;
pub mod gst;
pub mod hls;
pub mod mp4;
pub mod session;

pub use api::{AppState, app};
pub use config::Config;
pub use error::{Error, Result};

/// Initialize `GStreamer` once for the current process.
///
/// # Errors
///
/// Returns an error when the native `GStreamer` runtime cannot initialize.
pub fn initialize() -> Result<()> {
    gstreamer::init().map_err(Error::GStreamerInitialization)
}
