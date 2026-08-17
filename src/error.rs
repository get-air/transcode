use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to initialize GStreamer: {0}")]
    GStreamerInitialization(#[source] gstreamer::glib::Error),

    #[error("invalid source URL: {0}")]
    InvalidSource(String),

    #[error("invalid output options: {0}")]
    InvalidOutput(String),

    #[error("source discovery failed: {0}")]
    Discovery(String),

    #[error("source has no finite duration and cannot be served as VOD")]
    UnknownDuration,

    #[error("session {0} was not found")]
    SessionNotFound(uuid::Uuid),

    #[error("track {0} was not found")]
    TrackNotFound(String),

    #[error("segment {sequence} is outside the media timeline")]
    SegmentOutOfRange { sequence: u32 },

    #[error("required GStreamer element is unavailable: {0}")]
    MissingElement(String),

    #[error("GStreamer pipeline failed: {0}")]
    Pipeline(String),

    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal task failed: {0}")]
    Task(String),

    #[error("segment generation was cancelled because playback moved elsewhere")]
    Cancelled,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorPayload<'a>,
}

#[derive(Serialize)]
struct ErrorPayload<'a> {
    code: &'a str,
    message: String,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::InvalidSource(_)
            | Self::InvalidOutput(_)
            | Self::UnknownDuration
            | Self::SegmentOutOfRange { .. } => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Cancelled => (StatusCode::REQUEST_TIMEOUT, "cancelled"),
            Self::SessionNotFound(_) | Self::TrackNotFound(_) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Self::MissingElement(_) => (StatusCode::SERVICE_UNAVAILABLE, "missing_runtime"),
            Self::Discovery(_) | Self::Pipeline(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "media_processing_failed")
            }
            Self::GStreamerInitialization(_) | Self::Io(_) | Self::Task(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        };
        let body = ErrorBody {
            error: ErrorPayload {
                code,
                message: self.to_string(),
            },
        };
        (status, Json(body)).into_response()
    }
}
