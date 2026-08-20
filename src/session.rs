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
        AudioBundleRequest, AudioBundleTrack, HdrToneMapping, MediaInfo, PipelineMode,
        SegmentArtifact, SegmentRequest, SubtitleArtifact, SubtitleRequest, VideoCodec,
        generate_audio_bundle, generate_segment, generate_subtitle_segment, hdr_tone_mapping,
    },
    hls::{SegmentSpec, TrackKind, segment_map},
    source::{RegisteredSource, SourceManager},
};

#[derive(Clone, Debug, Deserialize)]
pub struct CreateSession {
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub source_id: Option<Uuid>,
    #[serde(default)]
    pub output: OutputOptions,
    #[serde(default)]
    pub subtitles: Vec<ExternalSubtitle>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Source {
    pub url: Url,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExternalSubtitle {
    pub source: Source,
    pub name: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub offset_ms: i64,
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
    #[serde(default)]
    pub video_track_index: Option<usize>,
    #[serde(default)]
    pub audio_track_index: Option<usize>,
    #[serde(default)]
    pub subtitle_track_index: Option<usize>,
    #[serde(default = "default_video_codecs")]
    pub video_codecs: Vec<VideoCodec>,
    /// HDR formats the target can render, for example `hdr10`.
    #[serde(default)]
    pub hdr_formats: Vec<String>,
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            transmux: true,
            force_transcode: false,
            max_width: default_max_width(),
            max_height: default_max_height(),
            video_track_index: None,
            audio_track_index: None,
            subtitle_track_index: None,
            video_codecs: default_video_codecs(),
            hdr_formats: Vec::new(),
        }
    }
}

fn default_video_codecs() -> Vec<VideoCodec> {
    vec![VideoCodec::H264]
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
    pub source_id: Uuid,
    pub duration_ns: u64,
    pub seekable: bool,
    pub tracks: Vec<crate::gst::MediaTrack>,
    pub renditions: Vec<RenditionView>,
    pub master_url: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RenditionView {
    pub kind: String,
    pub source_track_index: usize,
    pub name: String,
    pub language: Option<String>,
    pub default: bool,
    pub mode: Option<PipelineMode>,
    pub output_codec: Option<String>,
    pub hdr_passthrough: bool,
}

pub struct Session {
    pub id: Uuid,
    pub source: Arc<RegisteredSource>,
    pub output: OutputOptions,
    pub media: MediaInfo,
    pub segments: Vec<SegmentSpec>,
    pub directory: PathBuf,
    external_subtitles: HashMap<usize, ExternalSubtitle>,
    segment_locks: DashMap<(TrackKind, usize, u32), Arc<tokio::sync::Mutex<()>>>,
    audio_bundle_locks: DashMap<u32, Arc<tokio::sync::Mutex<()>>>,
    active_audio_bundle: Mutex<Option<(u32, CancellationToken)>>,
    subtitle_locks: DashMap<(usize, u32), Arc<tokio::sync::Mutex<()>>>,
    prefetch_suppressed: DashMap<(TrackKind, usize, u32), ()>,
    active_requests: Mutex<HashMap<(TrackKind, usize), Vec<ActiveRequest>>>,
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
        let renditions = self
            .media
            .tracks
            .iter()
            .filter(|track| {
                track.kind == "audio"
                    || track.kind == "subtitle" && track.web_compatible
                    || track.kind == "video"
                        && self.selected_track(TrackKind::Video).map(|item| item.index)
                            == Some(track.index)
            })
            .map(|track| {
                let track_kind = match track.kind.as_str() {
                    "video" => Some(TrackKind::Video),
                    "audio" => Some(TrackKind::Audio),
                    _ => None,
                };
                let mode = track_kind.map(|kind| self.mode_for(kind, track.index));
                let output_codec = match (track_kind, mode) {
                    (Some(TrackKind::Video), Some(PipelineMode::Transcode)) => {
                        Some("avc1".to_owned())
                    }
                    (Some(TrackKind::Audio), Some(PipelineMode::Transcode)) => {
                        Some("mp4a.40.2".to_owned())
                    }
                    (_, Some(PipelineMode::Transmux)) => track.rfc6381_codec.clone(),
                    _ if track.kind == "subtitle" => Some("webvtt".to_owned()),
                    _ => None,
                };
                RenditionView {
                    kind: track.kind.clone(),
                    source_track_index: track.index,
                    name: Self::track_name(track),
                    language: track.language.clone(),
                    default: self.is_default_track(track),
                    mode,
                    output_codec,
                    hdr_passthrough: track.kind == "video"
                        && matches!(mode, Some(PipelineMode::Transmux))
                        && track.hdr_format.is_some(),
                }
            })
            .collect();
        SessionView {
            id: self.id,
            source_id: self.source.id,
            duration_ns: self.media.duration_ns,
            seekable: self.media.seekable,
            tracks: self.media.tracks.clone(),
            renditions,
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
        self.selected_track(kind).is_some()
    }

    #[must_use]
    pub fn mode(&self, kind: TrackKind) -> PipelineMode {
        let Some(index) = self.selected_track(kind).map(|track| track.index) else {
            return PipelineMode::Transcode;
        };
        self.mode_for(kind, index)
    }

    #[must_use]
    pub fn mode_for(&self, kind: TrackKind, index: usize) -> PipelineMode {
        if self.output.force_transcode || !self.output.transmux {
            return PipelineMode::Transcode;
        }
        let compatible = self
            .track_by_index(kind.as_str(), index)
            .is_some_and(|track| track_is_compatible(track, kind, &self.output));
        if compatible {
            PipelineMode::Transmux
        } else {
            PipelineMode::Transcode
        }
    }

    #[must_use]
    pub fn video_output_dimensions(&self) -> Option<(u32, u32)> {
        let track = self.selected_track(TrackKind::Video)?;
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
                codecs.push(match kind {
                    TrackKind::Video => "avc1.640028".to_owned(),
                    TrackKind::Audio => "mp4a.40.2".to_owned(),
                });
                continue;
            }
            codecs.push(self.selected_track(kind)?.rfc6381_codec.clone()?);
        }
        (!codecs.is_empty()).then(|| codecs.join(","))
    }

    #[must_use]
    pub fn selected_track(&self, kind: TrackKind) -> Option<&crate::gst::MediaTrack> {
        let requested_index = match kind {
            TrackKind::Video => self.output.video_track_index,
            TrackKind::Audio => self.output.audio_track_index,
        };
        self.media.tracks.iter().find(|track| {
            track.kind == kind.as_str() && requested_index.is_none_or(|index| track.index == index)
        })
    }

    #[must_use]
    pub fn track_by_index(&self, kind: &str, index: usize) -> Option<&crate::gst::MediaTrack> {
        self.media
            .tracks
            .iter()
            .find(|track| track.kind == kind && track.index == index)
    }

    pub fn tracks(&self, kind: &str) -> impl Iterator<Item = &crate::gst::MediaTrack> {
        self.media
            .tracks
            .iter()
            .filter(move |track| track.kind == kind)
    }

    #[must_use]
    pub fn default_track_index(&self, kind: &str) -> Option<usize> {
        let requested = match kind {
            "video" => self.output.video_track_index,
            "audio" => self.output.audio_track_index,
            "subtitle" => self.output.subtitle_track_index,
            _ => None,
        };
        if kind == "subtitle" {
            return requested;
        }
        requested.or_else(|| self.tracks(kind).next().map(|track| track.index))
    }

    fn is_default_track(&self, track: &crate::gst::MediaTrack) -> bool {
        self.default_track_index(&track.kind) == Some(track.index)
    }

    fn track_name(track: &crate::gst::MediaTrack) -> String {
        track
            .name
            .clone()
            .or_else(|| track.language.clone())
            .unwrap_or_else(|| format!("{} {}", track.kind, track.index))
    }
}

#[derive(Clone)]
pub struct SessionManager {
    config: Config,
    sessions: Arc<DashMap<Uuid, Arc<Session>>>,
    pipelines: Arc<Semaphore>,
    metrics: Arc<Metrics>,
    hdr_tone_mapping: HdrToneMapping,
    sources: SourceManager,
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
    subtitle_segments: AtomicU64,
    cancelled_pipelines: AtomicU64,
    pipeline_queue_ns: AtomicU64,
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
    pub subtitle_segments: u64,
    pub cancelled_pipelines: u64,
    pub pipeline_queue_wait_ms: u64,
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
            subtitle_segments: self.subtitle_segments.load(Ordering::Relaxed),
            cancelled_pipelines: self.cancelled_pipelines.load(Ordering::Relaxed),
            pipeline_queue_wait_ms: self.pipeline_queue_ns.load(Ordering::Relaxed) / 1_000_000,
        }
    }
}

impl SessionManager {
    /// Creates a session manager and its cache root.
    ///
    /// # Errors
    ///
    /// Returns an error when the cache root cannot be created.
    pub fn new(config: Config, sources: SourceManager) -> Result<Self> {
        std::fs::create_dir_all(&config.cache_dir)?;
        Ok(Self {
            pipelines: Arc::new(Semaphore::new(config.max_pipelines.max(1))),
            config,
            metrics: Arc::new(Metrics::default()),
            sessions: Arc::new(DashMap::new()),
            hdr_tone_mapping: hdr_tone_mapping(),
            sources,
        })
    }

    /// Probes a source and registers a finite VOD session.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, task execution, or cache setup fails.
    pub async fn create(&self, request: CreateSession) -> Result<Arc<Session>> {
        if request.subtitles.len() > 64 {
            return Err(Error::InvalidOutput(
                "a session may include at most 64 external subtitles".to_owned(),
            ));
        }
        for subtitle in &request.subtitles {
            validate_source(&subtitle.source)?;
            if subtitle.name.trim().is_empty() || subtitle.name.len() > 256 {
                return Err(Error::InvalidOutput(
                    "external subtitle names must contain 1 to 256 characters".to_owned(),
                ));
            }
            if subtitle
                .language
                .as_ref()
                .is_some_and(|language| language.len() > 64)
            {
                return Err(Error::InvalidOutput(
                    "external subtitle language exceeds 64 characters".to_owned(),
                ));
            }
        }
        if request.output.max_width < 2 || request.output.max_height < 2 {
            return Err(Error::InvalidOutput(
                "max_width and max_height must both be at least 2".to_owned(),
            ));
        }
        self.evict_expired();
        self.evict_over_capacity();
        let source = match (request.source_id, request.source) {
            (Some(id), None) => self.sources.acquire(id)?,
            (None, Some(source)) => {
                let registered = self.sources.register(source).await?;
                self.sources.acquire(registered.id)?
            }
            _ => {
                return Err(Error::InvalidSource(
                    "exactly one of source or source_id is required".to_owned(),
                ));
            }
        };
        let mut media = source.media.clone();
        let mut external_subtitles = HashMap::new();
        for subtitle in request.subtitles {
            let index = media.tracks.len();
            media.tracks.push(crate::gst::MediaTrack {
                index,
                stream_id: None,
                kind: "subtitle".to_owned(),
                name: Some(subtitle.name.clone()),
                codec: Some("external text subtitle".to_owned()),
                video_codec: None,
                rfc6381_codec: None,
                caps: None,
                bit_depth: None,
                colorimetry: None,
                hdr_format: None,
                language: subtitle.language.clone(),
                width: None,
                height: None,
                channels: None,
                sample_rate: None,
                web_compatible: true,
            });
            external_subtitles.insert(index, subtitle);
        }
        if let Err(error) = validate_track_selection(&media, &request.output)
            .and_then(|()| validate_hdr_output(&media, &request.output, self.hdr_tone_mapping))
        {
            let _ = self.sources.release(source.id);
            return Err(error);
        }
        let id = Uuid::new_v4();
        let directory = self.config.cache_dir.join(id.to_string());
        if let Err(error) = std::fs::create_dir_all(&directory) {
            let _ = self.sources.release(source.id);
            return Err(error.into());
        }
        let target_ns = u64::from(self.config.segment_seconds) * 1_000_000_000;
        let session = Arc::new(Session {
            id,
            source,
            output: request.output,
            segments: segment_map(media.duration_ns, target_ns),
            media,
            directory,
            external_subtitles,
            segment_locks: DashMap::new(),
            audio_bundle_locks: DashMap::new(),
            active_audio_bundle: Mutex::new(None),
            subtitle_locks: DashMap::new(),
            prefetch_suppressed: DashMap::new(),
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
        let _ = self.sources.release_session(session.source.id);
        Ok(())
    }

    /// Returns a cached segment or generates it under the pipeline limit.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown tracks or segments, scheduler failures, or
    /// any pipeline/media-processing failure.
    #[allow(clippy::too_many_lines)]
    pub async fn segment(
        &self,
        session: Arc<Session>,
        track: TrackKind,
        sequence: u32,
    ) -> Result<SegmentArtifact> {
        let track_index = session
            .selected_track(track)
            .map(|item| item.index)
            .ok_or_else(|| Error::TrackNotFound(track.as_str().to_owned()))?;
        self.segment_for(session, track, track_index, sequence)
            .await
    }

    /// Generates one video or audio rendition by its discovered source index.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid rendition, scheduler failure, or media
    /// processing failure.
    #[allow(clippy::too_many_lines)]
    pub async fn segment_for(
        &self,
        session: Arc<Session>,
        track: TrackKind,
        track_index: usize,
        sequence: u32,
    ) -> Result<SegmentArtifact> {
        let selected_track = session
            .track_by_index(track.as_str(), track_index)
            .ok_or_else(|| Error::TrackNotFound(format!("{} {track_index}", track.as_str())))?;
        if track == TrackKind::Video
            && session
                .selected_track(TrackKind::Video)
                .map(|item| item.index)
                != Some(track_index)
        {
            return Err(Error::TrackNotFound(track.as_str().to_owned()));
        }
        let mode = session.mode_for(track, track_index);
        if track == TrackKind::Audio && matches!(mode, PipelineMode::Transcode) {
            return self.audio_bundle_for(session, track_index, sequence).await;
        }
        let segment = session
            .segments
            .iter()
            .find(|segment| segment.sequence == sequence)
            .cloned()
            .ok_or(Error::SegmentOutOfRange { sequence })?;
        let generation_lock = session
            .segment_locks
            .entry((track, track_index, sequence))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _generation_guard = generation_lock.lock().await;
        let request_id = Uuid::new_v4();
        let cancellation = CancellationToken::new();
        {
            let mut active = session.active_requests.lock();
            let requests = active.entry((track, track_index)).or_default();
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
        let tone_map_hdr = track == TrackKind::Video
            && matches!(mode, PipelineMode::Transcode)
            && selected_track.hdr_format.is_some()
            && self.hdr_tone_mapping != HdrToneMapping::Unavailable;
        let (resolved_url, resolved_headers) = session.source.resolved();
        let request = SegmentRequest {
            source: resolved_url,
            headers: resolved_headers,
            track,
            segment,
            mode,
            output_dir: session
                .directory
                .join(track.as_str())
                .join(track_index.to_string())
                .join(sequence.to_string()),
            timeout: Duration::from_secs(60),
            cancellation: cancellation.clone(),
            video_dimensions: session.video_output_dimensions(),
            selected_stream_id: selected_track.stream_id.clone(),
            transmux_video_codec: selected_track.video_codec,
            tone_map_hdr,
        };
        let cancel_on_drop = cancellation.drop_guard();
        let queue_started = Instant::now();
        let permit = Arc::clone(&self.pipelines)
            .acquire_owned()
            .await
            .map_err(|error| Error::Task(error.to_string()))?;
        self.metrics.pipeline_queue_ns.fetch_add(
            u64::try_from(queue_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
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
            if let Some(requests) = active.get_mut(&(track, track_index)) {
                requests.retain(|request| request.id != request_id);
                if requests.is_empty() {
                    active.remove(&(track, track_index));
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
        let suppress_prefetch = session
            .prefetch_suppressed
            .remove(&(track, track_index, sequence))
            .is_some();
        if !suppress_prefetch
            && result.is_ok()
            && session
                .segments
                .iter()
                .any(|segment| segment.sequence == sequence.saturating_add(1))
        {
            self.spawn_prefetch(
                Arc::clone(&session),
                track,
                track_index,
                sequence.saturating_add(1),
            );
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    async fn audio_bundle_for(
        &self,
        session: Arc<Session>,
        track_index: usize,
        sequence: u32,
    ) -> Result<SegmentArtifact> {
        let segment = session
            .segments
            .iter()
            .find(|segment| segment.sequence == sequence)
            .cloned()
            .ok_or(Error::SegmentOutOfRange { sequence })?;
        let lock = session
            .audio_bundle_locks
            .entry(sequence)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        let target_dir = session
            .directory
            .join("audio")
            .join(track_index.to_string())
            .join(sequence.to_string());
        let target_init = target_dir.join("init.mp4");
        let target_segment = target_dir.join("segment.m4s");
        if target_init.is_file() && target_segment.is_file() {
            self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(SegmentArtifact {
                init_path: target_init,
                segment_path: target_segment,
                mode: PipelineMode::Transcode,
                cached: true,
            });
        }
        let cancellation = CancellationToken::new();
        {
            let mut active = session.active_audio_bundle.lock();
            if let Some((active_sequence, active_token)) = active.as_ref()
                && active_sequence.abs_diff(sequence) > 1
            {
                active_token.cancel();
            }
            *active = Some((sequence, cancellation.clone()));
        }
        let tracks = session
            .tracks("audio")
            .filter(|track| track.index == track_index)
            .map(|track| AudioBundleTrack {
                index: track.index,
                stream_id: track.stream_id.clone(),
                output_dir: session
                    .directory
                    .join("audio")
                    .join(track.index.to_string())
                    .join(sequence.to_string()),
            })
            .collect::<Vec<_>>();
        let (source, headers) = session.source.resolved();
        let request = AudioBundleRequest {
            source,
            headers,
            tracks,
            segment,
            timeout: Duration::from_secs(6),
            cancellation: cancellation.clone(),
        };
        let queue_started = Instant::now();
        let permit = Arc::clone(&self.pipelines)
            .acquire_owned()
            .await
            .map_err(|error| Error::Task(error.to_string()))?;
        self.metrics.pipeline_queue_ns.fetch_add(
            u64::try_from(queue_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let metrics = Arc::clone(&self.metrics);
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _active = metrics.enter();
            generate_audio_bundle(&request)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?;
        session.active_audio_bundle.lock().take();
        let artifacts = match result {
            Ok(artifacts) => artifacts,
            Err(Error::Cancelled) => {
                self.metrics
                    .cancelled_pipelines
                    .fetch_add(1, Ordering::Relaxed);
                return Err(Error::Cancelled);
            }
            Err(error) => {
                self.metrics
                    .failed_pipelines
                    .fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };
        let generated = artifacts.iter().filter(|artifact| !artifact.cached).count();
        self.metrics.generated_segments.fetch_add(
            u64::try_from(generated).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.metrics.transcode_segments.fetch_add(
            u64::try_from(generated).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        self.prune_session_cache(&session)?;
        artifacts
            .into_iter()
            .find(|artifact| artifact.init_path == target_init)
            .ok_or_else(|| {
                Error::Pipeline(format!(
                    "audio bundle did not produce track {track_index} segment {sequence}",
                ))
            })
    }

    /// Generates the selected A/V renditions covering an initial playback reserve.
    ///
    /// # Errors
    ///
    /// Returns an error when the position is outside the VOD timeline or a
    /// selected rendition cannot be generated.
    pub async fn warm_playback(
        &self,
        session: Arc<Session>,
        position_seconds: f64,
        buffer_seconds: f64,
    ) -> Result<Vec<u32>> {
        let start_ns = seconds_to_ns(position_seconds);
        let reserve_ns = seconds_to_ns(buffer_seconds.clamp(0.0, 60.0));
        let end_ns = start_ns.saturating_add(reserve_ns.max(1));
        let sequences = session
            .segments
            .iter()
            .filter(|segment| {
                let segment_end = segment.start_ns.saturating_add(segment.duration_ns);
                segment.start_ns < end_ns && segment_end > start_ns
            })
            .map(|segment| segment.sequence)
            .collect::<Vec<_>>();
        if sequences.is_empty() {
            return Err(Error::SegmentOutOfRange { sequence: 0 });
        }
        for sequence in &sequences {
            let jobs = [TrackKind::Video, TrackKind::Audio]
                .into_iter()
                .filter_map(|kind| {
                    session
                        .selected_track(kind)
                        .map(|track| (kind, track.index))
                })
                .map(|(kind, track_index)| {
                    self.segment_for(Arc::clone(&session), kind, track_index, *sequence)
                });
            futures_util::future::try_join_all(jobs).await?;
        }
        Ok(sequences)
    }

    /// Generates a `WebVTT` subtitle segment for a discovered text track.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid/non-text subtitle, out-of-range segment,
    /// scheduler failure, or subtitle decoding failure.
    pub async fn subtitle_segment(
        &self,
        session: Arc<Session>,
        track_index: usize,
        sequence: u32,
    ) -> Result<SubtitleArtifact> {
        let track = session
            .track_by_index("subtitle", track_index)
            .filter(|track| track.web_compatible)
            .ok_or_else(|| Error::TrackNotFound(format!("subtitle {track_index}")))?;
        let (source, selected_stream_id, timestamp_offset_ns) =
            session.external_subtitles.get(&track_index).map_or_else(
                || {
                    let (url, headers) = session.source.resolved();
                    (Source { url, headers }, track.stream_id.clone(), 0_i64)
                },
                |subtitle| {
                    (
                        subtitle.source.clone(),
                        None,
                        subtitle.offset_ms.saturating_mul(1_000_000),
                    )
                },
            );
        let segment = session
            .segments
            .iter()
            .find(|segment| segment.sequence == sequence)
            .cloned()
            .ok_or(Error::SegmentOutOfRange { sequence })?;
        let generation_lock = session
            .subtitle_locks
            .entry((track_index, sequence))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _generation_guard = generation_lock.lock().await;
        let cancellation = CancellationToken::new();
        let request = SubtitleRequest {
            source: source.url,
            headers: source.headers,
            segment,
            output_path: session
                .directory
                .join("subtitles")
                .join(track_index.to_string())
                .join(format!("{sequence}.vtt")),
            timeout: Duration::from_secs(60),
            cancellation: cancellation.clone(),
            selected_stream_id,
            timestamp_offset_ns,
        };
        let cancel_on_drop = cancellation.drop_guard();
        let queue_started = Instant::now();
        let permit = Arc::clone(&self.pipelines)
            .acquire_owned()
            .await
            .map_err(|error| Error::Task(error.to_string()))?;
        self.metrics.pipeline_queue_ns.fetch_add(
            u64::try_from(queue_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let metrics = Arc::clone(&self.metrics);
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _active = metrics.enter();
            generate_subtitle_segment(&request)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?;
        cancel_on_drop.disarm();
        match &result {
            Ok(artifact) if artifact.cached => {
                self.metrics.cache_hits.fetch_add(1, Ordering::Relaxed);
            }
            Ok(_) => {
                self.metrics
                    .generated_segments
                    .fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .subtitle_segments
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

    fn spawn_prefetch(
        &self,
        session: Arc<Session>,
        track: TrackKind,
        track_index: usize,
        sequence: u32,
    ) {
        session
            .prefetch_suppressed
            .insert((track, track_index, sequence), ());
        let manager = self.clone();
        let _prefetch = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if manager.pipelines.available_permits() <= 1 {
                session
                    .prefetch_suppressed
                    .remove(&(track, track_index, sequence));
                return;
            }
            let _ =
                Box::pin(manager.segment_for(Arc::clone(&session), track, track_index, sequence))
                    .await;
        });
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
        for kind in ["video", "audio", "subtitles"] {
            let kind_dir = session.directory.join(kind);
            if !kind_dir.is_dir() {
                continue;
            }
            for rendition in std::fs::read_dir(&kind_dir)?
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.path().is_dir())
            {
                let mut entries = std::fs::read_dir(rendition.path())?
                    .filter_map(std::result::Result::ok)
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
                    if path.is_dir() {
                        std::fs::remove_dir_all(path)?;
                    } else {
                        std::fs::remove_file(path)?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn seconds_to_ns(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    u64::try_from(Duration::from_secs_f64(value).as_nanos()).unwrap_or(u64::MAX)
}

pub(crate) fn validate_source(source: &Source) -> Result<()> {
    if !matches!(source.url.scheme(), "http" | "https" | "file") {
        return Err(Error::InvalidSource(format!(
            "scheme {} is not supported",
            source.url.scheme()
        )));
    }
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

fn validate_track_selection(media: &MediaInfo, output: &OutputOptions) -> Result<()> {
    for (kind, index) in [
        ("video", output.video_track_index),
        ("audio", output.audio_track_index),
        ("subtitle", output.subtitle_track_index),
    ] {
        if let Some(index) = index
            && !media
                .tracks
                .iter()
                .any(|track| track.index == index && track.kind == kind)
        {
            return Err(Error::InvalidOutput(format!(
                "track index {index} is not a {kind} track"
            )));
        }
    }
    Ok(())
}

fn track_is_compatible(
    track: &crate::gst::MediaTrack,
    kind: TrackKind,
    output: &OutputOptions,
) -> bool {
    let codec_compatible = track.web_compatible
        || kind == TrackKind::Video
            && track.video_codec.is_some_and(|codec| {
                codec != VideoCodec::H264
                    && output.video_codecs.contains(&codec)
                    // Real ten-bit Matroska AV1 takes the seek-safe H.264 path.
                    && !(codec == VideoCodec::Av1 && track.bit_depth.unwrap_or(8) > 8)
            });
    let dimensions_compatible = kind != TrackKind::Video
        || track.width.unwrap_or(0) <= output.max_width
            && track.height.unwrap_or(0) <= output.max_height;
    let hdr_compatible = kind != TrackKind::Video
        || track.hdr_format.as_ref().is_none_or(|format| {
            output
                .hdr_formats
                .iter()
                .any(|supported| supported.eq_ignore_ascii_case(format))
        });
    codec_compatible && dimensions_compatible && hdr_compatible
}

fn validate_hdr_output(
    media: &MediaInfo,
    output: &OutputOptions,
    tone_mapping: HdrToneMapping,
) -> Result<()> {
    let video = media.tracks.iter().find(|track| {
        track.kind == "video"
            && output
                .video_track_index
                .is_none_or(|index| track.index == index)
    });
    let Some((video, hdr_format)) =
        video.and_then(|track| track.hdr_format.as_deref().map(|format| (track, format)))
    else {
        return Ok(());
    };
    let requires_transcode = output.force_transcode
        || !output.transmux
        || !track_is_compatible(video, TrackKind::Video, output);
    if requires_transcode && tone_mapping == HdrToneMapping::Unavailable {
        return Err(Error::InvalidOutput(format!(
            "{hdr_format} video requires passthrough because HDR tone mapping is unavailable; declare target HDR support and compatible dimensions, or choose an SDR source"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{HdrToneMapping, OutputOptions, track_is_compatible, validate_hdr_output};
    use crate::{
        gst::{MediaInfo, MediaTrack, VideoCodec},
        hls::TrackKind,
    };

    fn video(codec: VideoCodec, bit_depth: u32, hdr_format: Option<&str>) -> MediaTrack {
        MediaTrack {
            index: 0,
            stream_id: Some("video-0".to_owned()),
            kind: "video".to_owned(),
            name: None,
            codec: Some(codec.caps_name().to_owned()),
            video_codec: Some(codec),
            rfc6381_codec: Some(codec.rfc6381_fallback().to_owned()),
            caps: None,
            bit_depth: Some(bit_depth),
            colorimetry: None,
            hdr_format: hdr_format.map(ToOwned::to_owned),
            language: None,
            width: Some(3840),
            height: Some(2160),
            channels: None,
            sample_rate: None,
            web_compatible: false,
        }
    }

    #[test]
    fn nonstandard_h264_does_not_bypass_compatibility_checks() {
        let output = OutputOptions {
            max_width: 3840,
            max_height: 2160,
            ..OutputOptions::default()
        };
        assert!(!track_is_compatible(
            &video(VideoCodec::H264, 10, None),
            TrackKind::Video,
            &output,
        ));
    }

    #[test]
    fn hdr_requires_declared_passthrough_and_source_dimensions() {
        let track = video(VideoCodec::H265, 10, Some("hdr10"));
        let media = MediaInfo {
            duration_ns: 10_000_000_000,
            seekable: true,
            container: Some("matroska".to_owned()),
            tracks: vec![track],
        };
        let mut output = OutputOptions {
            max_width: 3840,
            max_height: 2160,
            video_codecs: vec![VideoCodec::H264, VideoCodec::H265],
            ..OutputOptions::default()
        };
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Unavailable).is_err());
        output.hdr_formats.push("HDR10".to_owned());
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Unavailable).is_ok());
        output.max_width = 1920;
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Unavailable).is_err());
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Basic).is_ok());
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Va).is_ok());
    }
}
