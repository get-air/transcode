//! GStreamer-only HTTP VOD transmuxing and transcoding.

pub mod api;
pub mod config;
pub mod error;
pub mod gst;
pub mod hls;
pub mod mp4;
pub mod server;
pub mod session;
pub mod source;

pub use api::{AppState, app, media_app};
pub use config::Config;
pub use error::{Error, Result};
pub use server::{EmbeddedCastHost, EmbeddedServer, spawn_server, spawn_tauri_host};

/// Initialize `GStreamer` once for the current process.
///
/// # Errors
///
/// Returns an error when the native `GStreamer` runtime cannot initialize.
pub fn initialize() -> Result<()> {
    gstreamer::init().map_err(Error::GStreamerInitialization)?;
    #[cfg(target_os = "android")]
    prefer_android_hardware_decoders();
    Ok(())
}

#[cfg(target_os = "android")]
fn prefer_android_hardware_decoders() {
    use gstreamer::prelude::*;

    for factory in gstreamer::ElementFactory::factories_with_type(
        gstreamer::ElementFactoryType::DECODER | gstreamer::ElementFactoryType::MEDIA_VIDEO,
        gstreamer::Rank::NONE,
    ) {
        if factory.name().starts_with("amc") {
            factory.set_rank(gstreamer::Rank::PRIMARY + 100);
        }
    }
}
