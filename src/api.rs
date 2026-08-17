use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use uuid::Uuid;

use crate::{
    config::Config,
    error::{Error, Result},
    gst::{Capabilities, inspect_capabilities},
    hls::{
        RenditionSpec, TrackKind, indexed_media_playlist, master_playlist, media_playlist,
        subtitle_playlist,
    },
    session::{CreateSession, SessionManager, SessionView},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub capabilities: Capabilities,
    pub sessions: SessionManager,
}

impl AppState {
    /// Builds shared application state.
    ///
    /// # Errors
    ///
    /// Returns an error when the cache directory cannot be created.
    pub fn new(config: Config) -> Result<Self> {
        let sessions = SessionManager::new(config.clone())?;
        Ok(Self {
            config,
            capabilities: inspect_capabilities(),
            sessions,
        })
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/metrics", get(metrics))
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/sessions/{id}/master.m3u8", get(master))
        .route("/v1/sessions/{id}/video.m3u8", get(video_media))
        .route("/v1/sessions/{id}/audio.m3u8", get(audio_media))
        .route(
            "/v1/sessions/{id}/audio/{track_index}/playlist.m3u8",
            get(indexed_audio_media),
        )
        .route(
            "/v1/sessions/{id}/audio/{track_index}/init.mp4",
            get(indexed_audio_init),
        )
        .route(
            "/v1/sessions/{id}/audio/{track_index}/segments/{sequence}",
            get(indexed_audio_segment),
        )
        .route(
            "/v1/sessions/{id}/subtitles/{track_index}/playlist.m3u8",
            get(indexed_subtitle_media),
        )
        .route(
            "/v1/sessions/{id}/subtitles/{track_index}/segments/{sequence}",
            get(indexed_subtitle_segment),
        )
        .route("/v1/sessions/{id}/{track}/init.mp4", get(init))
        .route(
            "/v1/sessions/{id}/{track}/segments/{sequence}",
            get(segment),
        )
        .layer(CatchPanicLayer::new())
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([http::Method::GET, http::Method::POST, http::Method::DELETE])
                .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION]),
        )
        .with_state(Arc::new(state))
}

#[derive(Serialize)]
struct Health<'a> {
    status: &'a str,
    engine: &'a str,
}

async fn health() -> Json<Health<'static>> {
    Json(Health {
        status: "ok",
        engine: "gstreamer",
    })
}

async fn capabilities(State(state): State<Arc<AppState>>) -> Json<Capabilities> {
    Json(state.capabilities.clone())
}

async fn metrics(State(state): State<Arc<AppState>>) -> Json<crate::session::MetricsSnapshot> {
    Json(state.sessions.metrics())
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CreateSession>,
) -> Result<(StatusCode, Json<SessionView>)> {
    let session = state.sessions.create(request).await?;
    Ok((StatusCode::CREATED, Json(session.view())))
}

async fn get_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionView>> {
    Ok(Json(state.sessions.get(id)?.view()))
}

async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    state.sessions.remove(id)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn master(State(state): State<Arc<AppState>>, Path(id): Path<Uuid>) -> Result<Response> {
    let session = state.sessions.get(id)?;
    let codecs = session.output_codecs();
    let rendition = |track: &crate::gst::MediaTrack| RenditionSpec {
        index: track.index,
        name: track
            .name
            .clone()
            .or_else(|| track.language.clone())
            .unwrap_or_else(|| format!("{} {}", track.kind, track.index)),
        language: track.language.clone(),
        default: session.default_track_index(&track.kind) == Some(track.index),
    };
    let audio = session.tracks("audio").map(rendition).collect::<Vec<_>>();
    let subtitles = session
        .tracks("subtitle")
        .filter(|track| track.web_compatible)
        .map(rendition)
        .collect::<Vec<_>>();
    let body = master_playlist(
        session.has_track(TrackKind::Video),
        &audio,
        &subtitles,
        8_000_000,
        codecs.as_deref(),
    );
    Ok(playlist_response(body))
}

async fn indexed_audio_media(
    State(state): State<Arc<AppState>>,
    Path((id, track_index)): Path<(Uuid, usize)>,
) -> Result<Response> {
    let session = state.sessions.get(id)?;
    session
        .track_by_index("audio", track_index)
        .ok_or_else(|| Error::TrackNotFound(format!("audio {track_index}")))?;
    Ok(playlist_response(indexed_media_playlist(
        track_index,
        &session.segments,
    )))
}

async fn indexed_subtitle_media(
    State(state): State<Arc<AppState>>,
    Path((id, track_index)): Path<(Uuid, usize)>,
) -> Result<Response> {
    let session = state.sessions.get(id)?;
    session
        .track_by_index("subtitle", track_index)
        .filter(|track| track.web_compatible)
        .ok_or_else(|| Error::TrackNotFound(format!("subtitle {track_index}")))?;
    Ok(playlist_response(subtitle_playlist(
        track_index,
        &session.segments,
    )))
}

async fn video_media(State(state): State<Arc<AppState>>, Path(id): Path<Uuid>) -> Result<Response> {
    render_media(&state, id, TrackKind::Video)
}

async fn audio_media(State(state): State<Arc<AppState>>, Path(id): Path<Uuid>) -> Result<Response> {
    render_media(&state, id, TrackKind::Audio)
}

fn render_media(state: &AppState, id: Uuid, track: TrackKind) -> Result<Response> {
    let session = state.sessions.get(id)?;
    if !session.has_track(track) {
        return Err(Error::TrackNotFound(track.as_str().to_owned()));
    }
    Ok(playlist_response(media_playlist(track, &session.segments)))
}

async fn init(
    State(state): State<Arc<AppState>>,
    Path((id, track)): Path<(Uuid, String)>,
) -> Result<Response> {
    let track = parse_track(&track)?;
    let session = state.sessions.get(id)?;
    let artifact = state.sessions.segment(session, track, 1).await?;
    file_response(&artifact.init_path, "video/mp4").await
}

async fn segment(
    State(state): State<Arc<AppState>>,
    Path((id, track, sequence)): Path<(Uuid, String, u32)>,
) -> Result<Response> {
    let track = parse_track(&track)?;
    let session = state.sessions.get(id)?;
    let artifact = state.sessions.segment(session, track, sequence).await?;
    file_response(&artifact.segment_path, "video/iso.segment").await
}

async fn indexed_audio_init(
    State(state): State<Arc<AppState>>,
    Path((id, track_index)): Path<(Uuid, usize)>,
) -> Result<Response> {
    let session = state.sessions.get(id)?;
    let artifact = state
        .sessions
        .segment_for(session, TrackKind::Audio, track_index, 1)
        .await?;
    file_response(&artifact.init_path, "video/mp4").await
}

async fn indexed_audio_segment(
    State(state): State<Arc<AppState>>,
    Path((id, track_index, sequence)): Path<(Uuid, usize, u32)>,
) -> Result<Response> {
    let session = state.sessions.get(id)?;
    let artifact = state
        .sessions
        .segment_for(session, TrackKind::Audio, track_index, sequence)
        .await?;
    file_response(&artifact.segment_path, "video/iso.segment").await
}

async fn indexed_subtitle_segment(
    State(state): State<Arc<AppState>>,
    Path((id, track_index, sequence)): Path<(Uuid, usize, u32)>,
) -> Result<Response> {
    let session = state.sessions.get(id)?;
    let artifact = state
        .sessions
        .subtitle_segment(session, track_index, sequence)
        .await?;
    file_response(&artifact.path, "text/vtt; charset=utf-8").await
}

fn parse_track(track: &str) -> Result<TrackKind> {
    match track {
        "video" => Ok(TrackKind::Video),
        "audio" => Ok(TrackKind::Audio),
        _ => Err(Error::TrackNotFound(track.to_owned())),
    }
}

fn playlist_response(body: String) -> Response {
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=30"),
    );
    response
}

async fn file_response(path: &std::path::Path, content_type: &'static str) -> Result<Response> {
    let bytes = tokio::fs::read(path).await?;
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok(response)
}
