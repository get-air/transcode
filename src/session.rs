use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
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
        HdrToneMapping, MediaInfo, PipelineMode, SegmentArtifact, SegmentRequest, SubtitleArtifact,
        SubtitleRequest, VideoCodec, generate_segment, generate_subtitle_segment, hdr_tone_mapping,
        target_video_bitrate_kbps,
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
    #[serde(default = "default_max_fps")]
    pub max_fps: u32,
    #[serde(default)]
    pub video_track_index: Option<usize>,
    #[serde(default)]
    pub audio_track_index: Option<usize>,
    #[serde(default)]
    pub subtitle_track_index: Option<usize>,
    #[serde(default)]
    pub preferred_audio_languages: Vec<String>,
    #[serde(default)]
    pub preferred_subtitle_languages: Vec<String>,
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
            max_fps: default_max_fps(),
            video_track_index: None,
            audio_track_index: None,
            subtitle_track_index: None,
            preferred_audio_languages: Vec::new(),
            preferred_subtitle_languages: Vec::new(),
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
    3840
}

const fn default_max_height() -> u32 {
    2160
}

const fn default_max_fps() -> u32 {
    30
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
    pub playback_url: String,
    pub delivery: DeliveryMode,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Proxy,
    Hls,
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
    subtitle_locks: DashMap<(usize, u32), Arc<tokio::sync::Mutex<()>>>,
    cancellation: CancellationToken,
    adaptive_max_height: AtomicU32,
    touched: Mutex<Instant>,
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
        let master_url = format!("/v1/sessions/{}/master.m3u8", self.id);
        let (playback_url, delivery) = if self.browser_direct_playable() {
            (
                format!("/v1/sources/{}/relay", self.source.id),
                DeliveryMode::Proxy,
            )
        } else {
            (master_url.clone(), DeliveryMode::Hls)
        };
        SessionView {
            id: self.id,
            source_id: self.source.id,
            duration_ns: self.media.duration_ns,
            seekable: self.media.seekable,
            tracks: self.media.tracks.clone(),
            renditions,
            master_url,
            playback_url,
            delivery,
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
        let max_height = self
            .adaptive_max_height
            .load(Ordering::Relaxed)
            .min(self.output.max_height);
        if width <= self.output.max_width && height <= max_height {
            return Some((width, height));
        }
        let width_limited_height =
            u64::from(height) * u64::from(self.output.max_width) / u64::from(width.max(1));
        let (scaled_width, scaled_height) = if width_limited_height <= u64::from(max_height) {
            (u64::from(self.output.max_width), width_limited_height)
        } else {
            (
                u64::from(width) * u64::from(max_height) / u64::from(height.max(1)),
                u64::from(max_height),
            )
        };
        Some((
            u32::try_from(scaled_width).unwrap_or(u32::MAX).max(2) & !1,
            u32::try_from(scaled_height).unwrap_or(u32::MAX).max(2) & !1,
        ))
    }

    fn reduce_adaptive_resolution(&self) -> Option<u32> {
        let current = self.adaptive_max_height.load(Ordering::Relaxed);
        let next = match current {
            value if value > 1440 => 1440,
            value if value > 1080 => 1080,
            value if value > 720 => 720,
            _ => return None,
        };
        self.adaptive_max_height.store(next, Ordering::Relaxed);
        Some(next)
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
    pub fn estimated_bandwidth(&self) -> u64 {
        let video_kbps = self
            .video_output_dimensions()
            .map_or(0_u32, |(width, height)| {
                target_video_bitrate_kbps(width, height)
            });
        u64::from(video_kbps.saturating_add(192)) * 1_000
    }

    #[must_use]
    pub fn selected_track(&self, kind: TrackKind) -> Option<&crate::gst::MediaTrack> {
        let requested_index = match kind {
            TrackKind::Video => self.output.video_track_index,
            TrackKind::Audio => self.default_track_index("audio"),
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
    pub fn browser_direct_playable(&self) -> bool {
        let is_remote = matches!(self.source.original.url.scheme(), "http" | "https");
        let is_mp4 = self
            .media
            .container
            .as_deref()
            .is_some_and(|container| container.contains("video/quicktime"));
        is_remote
            && is_mp4
            && self
                .selected_track(TrackKind::Video)
                .is_none_or(|track| track_is_compatible(track, TrackKind::Video, &self.output))
            && self.tracks("audio").all(|track| track.web_compatible)
            && self.external_subtitles.is_empty()
    }

    #[must_use]
    pub fn default_track_index(&self, kind: &str) -> Option<usize> {
        let requested = match kind {
            "video" => self.output.video_track_index,
            "audio" => self.output.audio_track_index,
            "subtitle" => self.output.subtitle_track_index,
            _ => None,
        };
        let preferred = match kind {
            "audio" => self.preferred_track_index(kind, &self.output.preferred_audio_languages),
            "subtitle" => {
                self.preferred_track_index(kind, &self.output.preferred_subtitle_languages)
            }
            _ => None,
        };
        if kind == "subtitle" {
            return requested.or(preferred);
        }
        requested
            .or(preferred)
            .or_else(|| self.tracks(kind).next().map(|track| track.index))
    }

    fn preferred_track_index(&self, kind: &str, preferred: &[String]) -> Option<usize> {
        preferred.iter().find_map(|wanted| {
            self.tracks(kind)
                .find(|track| {
                    track
                        .language
                        .as_deref()
                        .is_some_and(|actual| language_matches(actual, wanted))
                })
                .map(|track| track.index)
        })
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

fn language_matches(actual: &str, wanted: &str) -> bool {
    actual.eq_ignore_ascii_case(wanted)
        || actual
            .split(['-', '_'])
            .next()
            .zip(wanted.split(['-', '_']).next())
            .is_some_and(|(actual, wanted)| actual.eq_ignore_ascii_case(wanted))
}

#[derive(Clone)]
pub struct SessionManager {
    config: Config,
    sessions: Arc<DashMap<Uuid, Arc<Session>>>,
    pipelines: Arc<Semaphore>,
    global_pipelines: Arc<Semaphore>,
    metrics: Arc<Metrics>,
    hdr_tone_mapping: HdrToneMapping,
    sources: SourceManager,
    cancellation: CancellationToken,
}

// Hardware codecs and native decoder pools are process resources even when an
// embedder creates more than one server instance.
static GLOBAL_PIPELINES: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));

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
        Self::new_with_cancellation(config, sources, CancellationToken::new())
    }

    pub(crate) fn new_with_cancellation(
        config: Config,
        sources: SourceManager,
        cancellation: CancellationToken,
    ) -> Result<Self> {
        std::fs::create_dir_all(&config.cache_dir)?;
        Ok(Self {
            pipelines: Arc::new(Semaphore::new(config.max_pipelines.max(1))),
            global_pipelines: Arc::clone(&GLOBAL_PIPELINES),
            config,
            metrics: Arc::new(Metrics::default()),
            sessions: Arc::new(DashMap::new()),
            hdr_tone_mapping: hdr_tone_mapping(),
            sources,
            cancellation,
        })
    }

    /// Probes a source and registers a finite VOD session.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, task execution, or cache setup fails.
    #[allow(clippy::too_many_lines)]
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
        if request.output.max_width < 2
            || request.output.max_height < 2
            || request.output.max_fps == 0
        {
            return Err(Error::InvalidOutput(
                "max_width/max_height must be at least 2 and max_fps must be positive".to_owned(),
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
                frame_rate_num: None,
                frame_rate_denom: None,
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
        let initial_max_height = request.output.max_height;
        let session = Arc::new(Session {
            id,
            source,
            output: request.output,
            segments: segment_map(media.duration_ns, target_ns),
            media,
            directory,
            external_subtitles,
            segment_locks: DashMap::new(),
            subtitle_locks: DashMap::new(),
            cancellation: self.cancellation.child_token(),
            adaptive_max_height: AtomicU32::new(initial_max_height),
            touched: Mutex::new(Instant::now()),
        });
        self.sessions.insert(id, Arc::clone(&session));
        if !session.browser_direct_playable() {
            for kind in [TrackKind::Video, TrackKind::Audio] {
                if let Some(track_index) = session.selected_track(kind).map(|track| track.index) {
                    self.spawn_preload(Arc::clone(&session), kind, track_index, 1);
                }
            }
        }
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
        session.cancellation.cancel();
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
        self.generate_segment_for(session, track, track_index, sequence, true)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn generate_segment_for(
        &self,
        session: Arc<Session>,
        track: TrackKind,
        track_index: usize,
        sequence: u32,
        preload_after: bool,
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
        let segment = session
            .segments
            .iter()
            .find(|segment| segment.sequence == sequence)
            .cloned()
            .ok_or(Error::SegmentOutOfRange { sequence })?;
        let segment_duration_ns = segment.duration_ns;
        let generation_lock = session
            .segment_locks
            .entry((track, track_index, sequence))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _generation_guard = generation_lock.lock().await;
        let cancellation = session.cancellation.child_token();
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
            timeout: Duration::from_mins(1),
            cancellation: cancellation.clone(),
            video_dimensions: session.video_output_dimensions(),
            video_max_fps: (track == TrackKind::Video).then_some(session.output.max_fps),
            selected_stream_id: selected_track.stream_id.clone(),
            transmux_video_codec: selected_track.video_codec,
            tone_map_hdr,
        };
        let cancel_on_drop = cancellation.drop_guard();
        let queue_started = Instant::now();
        let permit = if preload_after {
            Arc::clone(&self.pipelines)
                .acquire_owned()
                .await
                .map_err(|error| Error::Task(error.to_string()))?
        } else {
            Arc::clone(&self.pipelines)
                .try_acquire_owned()
                .map_err(|_| Error::Cancelled)?
        };
        let global_permit = if preload_after {
            Arc::clone(&self.global_pipelines)
                .acquire_owned()
                .await
                .map_err(|error| Error::Task(error.to_string()))?
        } else {
            Arc::clone(&self.global_pipelines)
                .try_acquire_owned()
                .map_err(|_| Error::Cancelled)?
        };
        self.metrics.pipeline_queue_ns.fetch_add(
            u64::try_from(queue_started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let metrics = Arc::clone(&self.metrics);
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _global_permit = global_permit;
            let _active = metrics.enter();
            generate_segment(&request)
        })
        .await
        .map_err(|error| Error::Task(error.to_string()))?;
        if tone_map_hdr
            && self.hdr_tone_mapping == HdrToneMapping::Software
            && result
                .as_ref()
                .ok()
                .and_then(|artifact| artifact.processing_time_ns)
                .is_some_and(|processing_time_ns| {
                    processing_time_ns > segment_duration_ns.saturating_mul(3) / 4
                })
            && let Some(max_height) = session.reduce_adaptive_resolution()
        {
            tracing::warn!(
                session_id = %session.id,
                track_index,
                max_height,
                "software HDR tone mapping missed its real-time budget; lowering resolution"
            );
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
        if preload_after && result.is_ok() {
            self.spawn_preload(
                Arc::clone(&session),
                track,
                track_index,
                sequence.saturating_add(1),
            );
        }
        result
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
        let cancellation = session.cancellation.child_token();
        let request = SubtitleRequest {
            source: source.url,
            headers: source.headers,
            segment,
            output_path: session
                .directory
                .join("subtitles")
                .join(track_index.to_string())
                .join(format!("{sequence}.vtt")),
            timeout: Duration::from_mins(1),
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
        let global_permit = Arc::clone(&self.global_pipelines)
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
            let _global_permit = global_permit;
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

    fn spawn_preload(
        &self,
        session: Arc<Session>,
        track: TrackKind,
        track_index: usize,
        first_sequence: u32,
    ) {
        let segment_count = self
            .config
            .preload_seconds
            .div_ceil(self.config.segment_seconds.max(1));
        let manager = self.clone();
        let _preload = tokio::spawn(async move {
            for offset in 0..segment_count.max(1) {
                if session.cancellation.is_cancelled() {
                    break;
                }
                let sequence = first_sequence.saturating_add(offset);
                if !session
                    .segments
                    .iter()
                    .any(|segment| segment.sequence == sequence)
                {
                    break;
                }
                if manager.pipelines.available_permits() <= 1
                    || manager.global_pipelines.available_permits() <= 1
                {
                    break;
                }
                if Box::pin(manager.generate_segment_for(
                    Arc::clone(&session),
                    track,
                    track_index,
                    sequence,
                    false,
                ))
                .await
                .is_err()
                {
                    break;
                }
            }
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
                    .filter(|entry| {
                        if kind == "subtitles" {
                            entry.path().is_file()
                        } else {
                            entry.path().is_dir()
                        }
                    })
                    .filter_map(|entry| {
                        let modified = cache_entry_modified(&entry.path())?;
                        Some((modified, entry.path()))
                    })
                    .collect::<Vec<_>>();
                if entries.len() <= limit {
                    continue;
                }
                entries.sort_by_key(|(modified, _)| *modified);
                let remove_count = entries.len() - limit;
                let recent_cutoff = std::time::SystemTime::now()
                    .checked_sub(Duration::from_secs(2))
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                for (_, path) in entries
                    .into_iter()
                    .filter(|(modified, _)| *modified < recent_cutoff)
                    .take(remove_count)
                {
                    if path.is_dir() {
                        std::fs::remove_dir_all(path)?;
                    } else {
                        std::fs::remove_file(path)?;
                    }
                }
            }
        }
        enforce_cache_byte_budget(&session.directory, self.config.max_cache_bytes.max(1))?;
        Ok(())
    }
}

fn seconds_to_ns(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    u64::try_from(Duration::from_secs_f64(value).as_nanos()).unwrap_or(u64::MAX)
}

fn enforce_cache_byte_budget(root: &std::path::Path, limit: u64) -> Result<()> {
    let mut files = Vec::new();
    collect_cache_files(root, &mut files)?;
    let mut total = files
        .iter()
        .fold(0_u64, |total, (_, size, _)| total.saturating_add(*size));
    if total <= limit {
        return Ok(());
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let recent_cutoff = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(2))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    for (modified, size, path) in files {
        if total <= limit {
            break;
        }
        if modified >= recent_cutoff {
            continue;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    Ok(())
}

fn cache_entry_modified(path: &std::path::Path) -> Option<std::time::SystemTime> {
    if path.is_file() {
        return path.metadata().ok()?.modified().ok();
    }
    std::fs::read_dir(path)
        .ok()?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.metadata().ok()?.modified().ok())
        .max()
        .or_else(|| path.metadata().ok()?.modified().ok())
}

fn collect_cache_files(
    directory: &std::path::Path,
    files: &mut Vec<(std::time::SystemTime, u64, PathBuf)>,
) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_cache_files(&path, files)?;
        } else if let Ok(metadata) = entry.metadata()
            && let Ok(modified) = metadata.modified()
        {
            files.push((modified, metadata.len(), path));
        }
    }
    Ok(())
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
    let frame_rate_compatible = kind != TrackKind::Video
        || track.frame_rate_num.zip(track.frame_rate_denom).is_none_or(
            |(numerator, denominator)| {
                denominator > 0
                    && u64::from(numerator) <= u64::from(output.max_fps) * u64::from(denominator)
            },
        );
    let hdr_compatible = kind != TrackKind::Video
        || track.hdr_format.as_ref().is_none_or(|format| {
            output
                .hdr_formats
                .iter()
                .any(|supported| supported.eq_ignore_ascii_case(format))
        });
    codec_compatible && dimensions_compatible && frame_rate_compatible && hdr_compatible
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
            frame_rate_num: Some(30),
            frame_rate_denom: Some(1),
            channels: None,
            sample_rate: None,
            web_compatible: codec == VideoCodec::H264 && bit_depth <= 8,
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
    fn frame_rate_above_target_requires_transcoding() {
        let output = OutputOptions {
            max_width: 3840,
            max_height: 2160,
            max_fps: 24,
            ..OutputOptions::default()
        };
        assert!(!track_is_compatible(
            &video(VideoCodec::H264, 8, None),
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
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Software).is_ok());
        assert!(validate_hdr_output(&media, &output, HdrToneMapping::Va).is_ok());
    }
}
