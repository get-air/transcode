use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::{
    config::Config,
    error::{Error, Result},
    gst::{
        MediaInfo, PipelineMode, ProbeRequest, SegmentArtifact, SegmentRequest, generate_segment,
        probe,
    },
    hls::{SegmentSpec, TrackKind, segment_map},
};

#[derive(Clone, Debug, Deserialize)]
pub struct CreateSession {
    pub source: Source,
    #[serde(default)]
    pub output: OutputOptions,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Source {
    pub url: Url,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OutputOptions {
    #[serde(default = "default_true")]
    pub transmux: bool,
    #[serde(default)]
    pub force_transcode: bool,
    #[serde(default = "default_max_width")]
    pub max_width: u32,
    #[serde(default = "default_max_height")]
    pub max_height: u32,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            transmux: true,
            force_transcode: false,
            max_width: default_max_width(),
            max_height: default_max_height(),
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn default_max_width() -> u32 {
    1920
}

const fn default_max_height() -> u32 {
    1080
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionView {
    pub id: Uuid,
    pub duration_ns: u64,
    pub seekable: bool,
    pub tracks: Vec<crate::gst::MediaTrack>,
    pub master_url: String,
}

pub struct Session {
    pub id: Uuid,
    pub source: Source,
    pub output: OutputOptions,
    pub media: MediaInfo,
    pub segments: Vec<SegmentSpec>,
    pub directory: PathBuf,
    segment_locks: DashMap<(TrackKind, u32), Arc<tokio::sync::Mutex<()>>>,
    active_requests: Mutex<HashMap<TrackKind, Vec<ActiveRequest>>>,
    touched: Mutex<Instant>,
}

struct ActiveRequest {
    id: Uuid,
    sequence: u32,
    cancellation: CancellationToken,
}

impl Session {
    #[must_use]
    pub fn view(&self) -> SessionView {
        SessionView {
            id: self.id,
            duration_ns: self.media.duration_ns,
            seekable: self.media.seekable,
            tracks: self.media.tracks.clone(),
            master_url: format!("/v1/sessions/{}/master.m3u8", self.id),
        }
    }

    pub fn touch(&self) {
        *self.touched.lock() = Instant::now();
    }

    #[must_use]
    pub fn inactive_for(&self) -> Duration {
        self.touched.lock().elapsed()
    }

    #[must_use]
    pub fn has_track(&self, kind: TrackKind) -> bool {
        self.media
            .tracks
            .iter()
            .any(|track| track.kind == kind.as_str())
    }

    #[must_use]
    pub fn mode(&self, kind: TrackKind) -> PipelineMode {
        if self.output.force_transcode || !self.output.transmux {
            return PipelineMode::Transcode;
        }
        let compatible = self
            .media
            .tracks
            .iter()
            .find(|track| track.kind == kind.as_str())
            .is_some_and(|track| {
                track.web_compatible
                    && (kind != TrackKind::Video
                        || track.width.unwrap_or(0) <= self.output.max_width
                            && track.height.unwrap_or(0) <= self.output.max_height)
            });
        if compatible {
            PipelineMode::Transmux
        } else {
            PipelineMode::Transcode
        }
    }

    #[must_use]
    pub fn video_output_dimensions(&self) -> Option<(u32, u32)> {
        let track = self
            .media
            .tracks
            .iter()
            .find(|track| track.kind == "video")?;
        let width = track.width?;
        let height = track.height?;
        if width <= self.output.max_width && height <= self.output.max_height {
            return Some((width, height));
        }
        let width_limited_height =
            u64::from(height) * u64::from(self.output.max_width) / u64::from(width.max(1));
        let (scaled_width, scaled_height) =
            if width_limited_height <= u64::from(self.output.max_height) {
                (u64::from(self.output.max_width), width_limited_height)
            } else {
                (
                    u64::from(width) * u64::from(self.output.max_height) / u64::from(height.max(1)),
                    u64::from(self.output.max_height),
                )
            };
        Some((
            u32::try_from(scaled_width).unwrap_or(u32::MAX).max(2) & !1,
            u32::try_from(scaled_height).unwrap_or(u32::MAX).max(2) & !1,
        ))
    }

    #[must_use]
    pub fn output_codecs(&self) -> Option<String> {
        let mut codecs = Vec::new();
        for kind in [TrackKind::Video, TrackKind::Audio] {
            if !self.has_track(kind) {
                continue;
            }
            if matches!(self.mode(kind), PipelineMode::Transcode) {
                return None;
            }
            codecs.push(
                self.media
                    .tracks
                    .iter()
                    .find(|track| track.kind == kind.as_str())
                    .and_then(|track| track.rfc6381_codec.clone())?,
            );
        }
        (!codecs.is_empty()).then(|| codecs.join(","))
    }
}

#[derive(Clone)]
pub struct SessionManager {
    config: Config,
    sessions: Arc<DashMap<Uuid, Arc<Session>>>,
    pipelines: Arc<Semaphore>,
    metrics: Arc<Metrics>,
}

#[derive(Default)]
struct Metrics {
    active_pipelines: AtomicUsize,
    peak_active_pipelines: AtomicUsize,
    generated_segments: AtomicU64,
    cache_hits: AtomicU64,
    failed_pipelines: AtomicU64,
    transmux_segments: AtomicU64,
    transcode_segments: AtomicU64,
    cancelled_pipelines: AtomicU64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MetricsSnapshot {
    pub active_pipelines: usize,
    pub peak_active_pipelines: usize,
    pub generated_segments: u64,
    pub cache_hits: u64,
    pub failed_pipelines: u64,
    pub transmux_segments: u64,
    pub transcode_segments: u64,
    pub cancelled_pipelines: u64,
}

struct ActivePipelineGuard<'a>(&'a Metrics);

impl Drop for ActivePipelineGuard<'_> {
    fn drop(&mut self) {
        self.0.active_pipelines.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    fn enter(&self) -> ActivePipelineGuard<'_> {
        let active = self.active_pipelines.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_active_pipelines
            .fetch_max(active, Ordering::Relaxed);
        ActivePipelineGuard(self)
    }

    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            active_pipelines: self.active_pipelines.load(Ordering::Relaxed),
            peak_active_pipelines: self.peak_active_pipelines.load(Ordering::Relaxed),
            generated_segments: self.generated_segments.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            failed_pipelines: self.failed_pipelines.load(Ordering::Relaxed),
            transmux_segments: self.transmux_segments.load(Ordering::Relaxed),
            transcode_segments: self.transcode_segments.load(Ordering::Relaxed),
            cancelled_pipelines: self.cancelled_pipelines.load(Ordering::Relaxed),
        }
    }
}

impl SessionManager {
    /// Creates a session manager and its cache root.
    ///
    /// # Errors
    ///
    /// Returns an error when the cache root cannot be created.
    pub fn new(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.cache_dir)?;
        Ok(Self {
            pipelines: Arc::new(Semaphore::new(config.max_pipelines.max(1))),
            config,
            metrics: Arc::new(Metrics::default()),
            sessions: Arc::new(DashMap::new()),
        })
    }

    /// Probes a source and registers a finite VOD session.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, task execution, or cache setup fails.
    pub async fn create(&self, request: CreateSession) -> Result<Arc<Session>> {
        validate_source(&request.source)?;
        if request.output.max_width < 2 || request.output.max_height < 2 {
            return Err(Error::InvalidOutput(
                "max_width and max_height must both be at least 2".to_owned(),
            ));
        }
        self.evict_expired();
        self.evict_over_capacity();
        let probe_request = ProbeRequest {
            url: request.source.url.clone(),
            headers: request.source.headers.clone(),
            timeout: self.config.probe_timeout(),
        };
        let media = tokio::task::spawn_blocking(move || probe(&probe_request))
            .await
            .map_err(|error| Error::Task(error.to_string()))??;
        let id = Uuid::new_v4();
        let directory = self.config.cache_dir.join(id.to_string());
        std::fs::create_dir_all(&directory)?;
        let target_ns = u64::from(self.config.segment_seconds) * 1_000_000_000;
        let session = Arc::new(Session {
            id,
            source: request.source,
            output: request.output,
            segments: segment_map(media.duration_ns, target_ns),
            media,
            directory,
            segment_locks: DashMap::new(),
            active_requests: Mutex::new(HashMap::new()),
            touched: Mutex::new(Instant::now()),
        });
        self.sessions.insert(id, Arc::clone(&session));
        Ok(session)
    }

    /// Retrieves and touches a session.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SessionNotFound`] when the ID is unknown.
    pub fn get(&self, id: Uuid) -> Result<Arc<Session>> {
        let session = self
            .sessions
            .get(&id)
            .map(|entry| Arc::clone(entry.value()))
            .ok_or(Error::SessionNotFound(id))?;
        session.touch();
        Ok(session)
    }

    /// Removes a session and its cached artifacts.
    ///
    /// # Errors
    ///
    /// Returns an error when the session is unknown or cache cleanup fails.
    pub fn remove(&self, id: Uuid) -> Result<()> {
        let (_, session) = self
            .sessions
            .remove(&id)
            .ok_or(Error::SessionNotFound(id))?;
        if session.directory.is_dir() {
            std::fs::remove_dir_all(&session.directory)?;
        }
        Ok(())
    }

    /// Returns a cached segment or generates it under the pipeline limit.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown tracks or segments, scheduler failures, or
    /// any pipeline/media-processing failure.
    pub async fn segment(
        &self,
        session: Arc<Session>,
        track: TrackKind,
        sequence: u32,
    ) -> Result<SegmentArtifact> {
        if !session.has_track(track) {
            return Err(Error::TrackNotFound(track.as_str().to_owned()));
        }
        let segment = session
            .segments
            .iter()
            .find(|segment| segment.sequence == sequence)
            .cloned()
            .ok_or(Error::SegmentOutOfRange { sequence })?;
        let generation_lock = session
            .segment_locks
            .entry((track, sequence))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _generation_guard = generation_lock.lock().await;
        let request_id = Uuid::new_v4();
        let cancellation = CancellationToken::new();
        {
            let mut active = session.active_requests.lock();
            let requests = active.entry(track).or_default();
            for request in requests.iter() {
                if request.sequence.abs_diff(sequence) > 2 {
                    request.cancellation.cancel();
                }
            }
            requests.push(ActiveRequest {
                id: request_id,
                sequence,
                cancellation: cancellation.clone(),
            });
            drop(active);
        }
        let mode = session.mode(track);
        let request = SegmentRequest {
            source: session.source.url.clone(),
            headers: session.source.headers.clone(),
            track,
            segment,
            mode,
            output_dir: session
                .directory
                .join(track.as_str())
                .join(sequence.to_string()),
            timeout: Duration::from_secs(60),
            cancellation: cancellation.clone(),
            video_dimensions: session.video_output_dimensions(),
        };
        let cancel_on_drop = cancellation.drop_guard();
        let permit = Arc::clone(&self.pipelines)
            .acquire_owned()
            .await
            .map_err(|error| Error::Task(error.to_string()))?;
        let metrics = Arc::clone(&self.metrics);
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _active = metrics.enter();
            generate_segment(&request)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?;
        {
            let mut active = session.active_requests.lock();
            if let Some(requests) = active.get_mut(&track) {
                requests.retain(|request| request.id != request_id);
                if requests.is_empty() {
                    active.remove(&track);
                }
            }
        }
        cancel_on_drop.disarm();
        match &result {
            Ok(artifact) if artifact.cached => {
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            }
            Ok(artifact) => {
                self.metrics
                    .generated_segments
                    .fetch_add(1, Ordering::Relaxed);
                match artifact.mode {
                    PipelineMode::Transmux => &self.metrics.transmux_segments,
                    PipelineMode::Transcode => &self.metrics.transcode_segments,
                }
                .fetch_add(1, Ordering::Relaxed);
                self.prune_session_cache(&session)?;
            }
            Err(Error::Cancelled) => {
                self.metrics
                    .cancelled_pipelines
                    .fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.metrics
                    .failed_pipelines
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        result
    }

    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    fn evict_expired(&self) {
        let ttl = self.config.session_ttl();
        let expired = self
            .sessions
            .iter()
            .filter(|entry| entry.value().inactive_for() >= ttl)
            .map(|entry| *entry.key())
            .collect::<Vec<_>>();
        for id in expired {
            let _ = self.remove(id);
        }
    }

    fn evict_over_capacity(&self) {
        if self.sessions.len() < self.config.max_sessions.max(1) {
            return;
        }
        if let Some(oldest) = self
            .sessions
            .iter()
            .max_by_key(|entry| entry.value().inactive_for())
            .map(|entry| *entry.key())
        {
            let _ = self.remove(oldest);
        }
    }

    fn prune_session_cache(&self, session: &Session) -> Result<()> {
        let limit = self.config.max_cached_segments.max(1);
        for track in [TrackKind::Video, TrackKind::Audio] {
            let track_dir = session.directory.join(track.as_str());
            if !track_dir.is_dir() {
                continue;
            }
            let mut entries = std::fs::read_dir(&track_dir)?
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| {
                    let modified = entry.metadata().ok()?.modified().ok()?;
                    Some((modified, entry.path()))
                })
                .collect::<Vec<_>>();
            if entries.len() <= limit {
                continue;
            }
            entries.sort_by_key(|(modified, _)| *modified);
            let remove_count = entries.len() - limit;
            for (_, path) in entries.into_iter().take(remove_count) {
                std::fs::remove_dir_all(path)?;
            }
        }
        Ok(())
    }
}

fn validate_source(source: &Source) -> Result<()> {
    if source.url.as_str().len() > 16 * 1024 {
        return Err(Error::InvalidSource("URL exceeds 16 KiB".to_owned()));
    }
    if source.headers.len() > 64 {
        return Err(Error::InvalidSource(
            "source has more than 64 HTTP headers".to_owned(),
        ));
    }
    let mut header_bytes = 0_usize;
    for (name, value) in &source.headers {
        http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| Error::InvalidSource(format!("invalid header name: {error}")))?;
        http::HeaderValue::from_str(value)
            .map_err(|error| Error::InvalidSource(format!("invalid header value: {error}")))?;
        header_bytes = header_bytes
            .saturating_add(name.len())
            .saturating_add(value.len());
    }
    if header_bytes > 64 * 1024 {
        return Err(Error::InvalidSource(
            "source headers exceed 64 KiB".to_owned(),
        ));
    }
    Ok(())
}
