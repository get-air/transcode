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

    #[error("source {0} was not found")]
    SourceNotFound(uuid::Uuid),

    #[error("source is rate limited")]
    SourceRateLimited { retry_after_seconds: Option<u64> },

    #[error("track {0} was not found")]
    TrackNotFound(String),

    #[error("segment {sequence} is outside the media timeline")]
    SegmentOutOfRange { sequence: u32 },

    #[error("required GStreamer element is unavailable: {0}")]
    MissingElement(String),

    #[error("GStreamer pipeline failed: {0}")]
    Pipeline(String),

    #[error(
        "direct video transmux cannot start segment {sequence} at a keyframe near {requested_ns} ns (first buffer {actual_ns} ns)"
    )]
    MisalignedKeyframe {
        sequence: u32,
        requested_ns: u64,
        actual_ns: u64,
    },

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
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let retry_after_seconds = match &self {
            Self::SourceRateLimited {
                retry_after_seconds,
            } => *retry_after_seconds,
            _ => None,
        };
        let (status, code) = match self {
            Self::InvalidSource(_)
            | Self::InvalidOutput(_)
            | Self::UnknownDuration
            | Self::SegmentOutOfRange { .. } => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Cancelled => (StatusCode::REQUEST_TIMEOUT, "cancelled"),
            Self::SessionNotFound(_) | Self::SourceNotFound(_) | Self::TrackNotFound(_) => {
                (StatusCode::NOT_FOUND, "not_found")
            }
            Self::SourceRateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::MissingElement(_) => (StatusCode::SERVICE_UNAVAILABLE, "missing_runtime"),
            Self::Discovery(_) | Self::Pipeline(_) | Self::MisalignedKeyframe { .. } => {
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
                retry_after_seconds,
            },
        };
        (status, Json(body)).into_response()
    }
}
