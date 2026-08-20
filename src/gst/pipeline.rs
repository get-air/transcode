use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    error::{Error, Result},
    hls::{SegmentSpec, TrackKind},
    mp4::{split_cmaf, validate_init_segment, validate_media_segment},
};

use super::VideoCodec;
use super::capabilities::encoder_candidates;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineMode {
    Transmux,
    Transcode,
}

#[derive(Clone, Debug)]
pub struct SegmentRequest {
    pub source: Url,
    pub headers: BTreeMap<String, String>,
    pub track: TrackKind,
    pub segment: SegmentSpec,
    pub mode: PipelineMode,
    pub output_dir: PathBuf,
    pub timeout: Duration,
    pub cancellation: CancellationToken,
    pub video_dimensions: Option<(u32, u32)>,
    pub video_max_fps: Option<u32>,
    pub selected_stream_id: Option<String>,
    pub transmux_video_codec: Option<VideoCodec>,
    pub tone_map_hdr: bool,
}

#[derive(Clone, Debug)]
pub struct SegmentArtifact {
    pub init_path: PathBuf,
    pub segment_path: PathBuf,
    pub mode: PipelineMode,
    pub cached: bool,
    pub processing_time_ns: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct SubtitleRequest {
    pub source: Url,
    pub headers: BTreeMap<String, String>,
    pub segment: SegmentSpec,
    pub output_path: PathBuf,
    pub timeout: Duration,
    pub cancellation: CancellationToken,
    pub selected_stream_id: Option<String>,
    pub timestamp_offset_ns: i64,
}

#[derive(Clone, Debug)]
pub struct SubtitleArtifact {
    pub path: PathBuf,
    pub cached: bool,
}

/// Generates and validates one independent CMAF media segment.
///
/// # Errors
///
/// Returns an error for unavailable plugins, failed source seeks, pipeline
/// failures, timeouts, malformed generated CMAF, or cache I/O failures.
pub fn generate_segment(request: &SegmentRequest) -> Result<SegmentArtifact> {
    let started = std::time::Instant::now();
    if request.cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !matches!(request.mode, PipelineMode::Transcode) {
        return match generate_segment_once(request, None) {
            Ok(artifact) => Ok(artifact),
            Err(Error::MisalignedKeyframe { .. })
                if request.track == TrackKind::Video
                    && request.transmux_video_codec == Some(VideoCodec::H264) =>
            {
                cleanup_attempt_files(&request.output_dir)?;
                let mut fallback = request.clone();
                fallback.mode = PipelineMode::Transcode;
                fallback.timeout = request.timeout.saturating_sub(started.elapsed());
                if fallback.timeout.is_zero() {
                    return Err(Error::Pipeline(
                        "transmux keyframe check exhausted the segment deadline".to_owned(),
                    ));
                }
                generate_segment(&fallback)
            }
            Err(error) => Err(error),
        };
    }
    let candidates = encoder_candidates(request.track)
        .into_iter()
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(Error::MissingElement(format!(
            "{} encoder producing browser-compatible output",
            request.track.as_str()
        )));
    }
    let mut failures = Vec::new();
    let mut first_attempt = true;
    let deadline = std::time::Instant::now() + request.timeout;
    let candidate_count = candidates.len();
    for (index, candidate) in candidates.into_iter().enumerate() {
        if request.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if first_attempt {
            first_attempt = false;
        } else {
            cleanup_attempt_files(&request.output_dir)?;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let mut attempt = request.clone();
        attempt.timeout = if index + 1 == candidate_count {
            remaining
        } else {
            remaining.min(Duration::from_secs(3))
        };
        match generate_segment_once(&attempt, Some(&candidate.element)) {
            Ok(artifact) => return Ok(artifact),
            Err(error) => failures.push(format!("{}: {error}", candidate.element)),
        }
    }
    Err(Error::Pipeline(format!(
        "all compatible encoders failed: {}",
        failures.join("; ")
    )))
}

/// Extracts one selected text subtitle interval as a standalone `WebVTT` segment.
///
/// # Errors
///
/// Returns an error when the subtitle cannot be selected, decoded, sought, or
/// written within the configured deadline.
#[allow(clippy::too_many_lines)]
pub fn generate_subtitle_segment(request: &SubtitleRequest) -> Result<SubtitleArtifact> {
    if request.cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if request.output_path.is_file() {
        let bytes = fs::read(&request.output_path)?;
        if bytes.starts_with(b"WEBVTT") {
            return Ok(SubtitleArtifact {
                path: request.output_path.clone(),
                cached: true,
            });
        }
        let _ = fs::remove_file(&request.output_path);
    }
    if let Some(parent) = request.output_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let pipeline = gst::Pipeline::with_name("air-transcode-subtitle-segment");
    let _pipeline_cleanup = PipelineCleanup(pipeline.clone());
    let desired_caps = gst::Caps::builder("text/x-raw").build();
    let source = gst::ElementFactory::make("uridecodebin3")
        .name("subtitle-source")
        .property("uri", request.source.as_str())
        .property("caps", &desired_caps)
        .build()
        .map_err(|_| Error::MissingElement("uridecodebin3".to_owned()))?;
    configure_source(&source, request.headers.clone(), request.timeout);
    configure_stream_type_selection(
        &source,
        gst::StreamType::TEXT,
        request.selected_stream_id.clone(),
    );
    let queue = make("queue")?;
    let sink = gst::ElementFactory::make("appsink")
        .name("subtitle-sink")
        .property("sync", false)
        .property("max-buffers", 64_u32)
        .build()
        .map_err(|_| Error::MissingElement("appsink".to_owned()))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| Error::Pipeline("subtitle appsink has an unexpected type".to_owned()))?;
    let sink_element = sink.clone().upcast::<gst::Element>();
    pipeline
        .add_many([&source, &queue, &sink_element])
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    gst::Element::link_many([&queue, &sink_element])
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    let queue_sink = queue
        .static_pad("sink")
        .ok_or_else(|| Error::Pipeline("subtitle queue has no sink pad".to_owned()))?;
    let linked = Arc::new(AtomicBool::new(false));
    let linked_callback = Arc::clone(&linked);
    source.connect_pad_added(move |_, pad| {
        if linked_callback.load(Ordering::Acquire) {
            return;
        }
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if caps.can_intersect(&desired_caps) && pad.link(&queue_sink).is_ok() {
            linked_callback.store(true, Ordering::Release);
        }
    });

    pipeline
        .set_state(gst::State::Paused)
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    wait_for_state(
        &pipeline,
        gst::State::Paused,
        request.timeout,
        &request.cancellation,
    )?;
    if !linked.load(Ordering::Acquire) {
        return Err(Error::Pipeline(
            "source did not expose the selected text subtitle pad".to_owned(),
        ));
    }
    let source_start_ns = apply_signed_offset(
        request.segment.start_ns,
        request.timestamp_offset_ns.saturating_neg(),
    );
    let start = gst::ClockTime::from_nseconds(source_start_ns);
    let stop =
        gst::ClockTime::from_nseconds(request.segment.start_ns + request.segment.duration_ns);
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    let seeked = pipeline
        .seek(
            1.0,
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_BEFORE,
            gst::SeekType::Set,
            start,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        )
        .is_ok()
        || pipeline
            .seek(
                1.0,
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                gst::SeekType::Set,
                start,
                gst::SeekType::None,
                gst::ClockTime::NONE,
            )
            .is_ok();
    if !seeked {
        return Err(Error::Pipeline(
            "subtitle source rejected key-unit and accurate seeks".to_owned(),
        ));
    }
    let cues = collect_subtitle_cues(
        &pipeline,
        &sink,
        request.segment.start_ns,
        stop.nseconds(),
        request.timestamp_offset_ns,
        request.timeout,
        &request.cancellation,
    );
    let _ = pipeline.set_state(gst::State::Null);
    let body = render_webvtt(&cues?);
    fs::write(&request.output_path, body)?;
    Ok(SubtitleArtifact {
        path: request.output_path.clone(),
        cached: false,
    })
}

fn collect_subtitle_cues(
    pipeline: &gst::Pipeline,
    sink: &gst_app::AppSink,
    start_ns: u64,
    stop_ns: u64,
    timestamp_offset_ns: i64,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<Vec<SubtitleCue>> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| Error::Pipeline("subtitle pipeline has no bus".to_owned()))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut idle_deadline = std::time::Instant::now() + Duration::from_secs(1);
    let mut cues = Vec::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Pipeline("subtitle pipeline timed out".to_owned()));
        }
        if std::time::Instant::now() >= idle_deadline {
            return Ok(cues);
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            idle_deadline = std::time::Instant::now() + Duration::from_secs(1);
            if let Some(buffer) = sample.buffer() {
                let Some(pts) = buffer.pts() else {
                    continue;
                };
                let source_cue_start_ns = sample
                    .segment()
                    .and_then(|segment| segment.downcast_ref::<gst::ClockTime>())
                    .and_then(|segment| segment.to_stream_time(pts))
                    .map_or_else(|| pts.nseconds(), gst::ClockTime::nseconds);
                let cue_start_ns = apply_signed_offset(source_cue_start_ns, timestamp_offset_ns);
                let cue_end_ns = cue_start_ns.saturating_add(
                    buffer
                        .duration()
                        .map_or(2_000_000_000, gst::ClockTime::nseconds),
                );
                if cue_start_ns >= stop_ns {
                    return Ok(cues);
                }
                if cue_end_ns <= start_ns {
                    continue;
                }
                let map = buffer
                    .map_readable()
                    .map_err(|_| Error::Pipeline("subtitle buffer is not readable".to_owned()))?;
                cues.push(SubtitleCue {
                    start_ns: cue_start_ns.max(start_ns),
                    end_ns: cue_end_ns.min(stop_ns),
                    text: plain_subtitle_text(&String::from_utf8_lossy(map.as_slice())),
                });
            }
            continue;
        }
        if sink.is_eos() {
            return Ok(cues);
        }
        if let Some(message) =
            bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
            && let gst::MessageView::Error(error) = message.view()
        {
            return Err(Error::Pipeline(format!(
                "{} ({:?})",
                error.error(),
                error.debug()
            )));
        }
    }
}

const fn apply_signed_offset(timestamp_ns: u64, offset_ns: i64) -> u64 {
    if offset_ns >= 0 {
        timestamp_ns.saturating_add(offset_ns.unsigned_abs())
    } else {
        timestamp_ns.saturating_sub(offset_ns.unsigned_abs())
    }
}

struct SubtitleCue {
    start_ns: u64,
    end_ns: u64,
    text: String,
}

fn render_webvtt(cues: &[SubtitleCue]) -> String {
    let mut output = String::from("WEBVTT\n\n");
    for (index, cue) in cues.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = write!(
            output,
            "{}\n{} --> {}\n{}\n\n",
            index + 1,
            webvtt_timestamp(cue.start_ns),
            webvtt_timestamp(cue.end_ns),
            cue.text
        );
    }
    output
}

fn webvtt_timestamp(nanoseconds: u64) -> String {
    let milliseconds = nanoseconds / 1_000_000;
    let hours = milliseconds / 3_600_000;
    let minutes = milliseconds / 60_000 % 60;
    let seconds = milliseconds / 1_000 % 60;
    let milliseconds = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
}

fn plain_subtitle_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for character in input.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(character),
            _ => {}
        }
    }
    output
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .trim_matches(['\0', '\n', '\r'])
        .to_owned()
}

#[allow(clippy::too_many_lines)]
fn generate_segment_once(
    request: &SegmentRequest,
    encoder_name: Option<&str>,
) -> Result<SegmentArtifact> {
    fs::create_dir_all(&request.output_dir)?;
    let init_path = request.output_dir.join("init.mp4");
    let segment_path = request.output_dir.join("segment.m4s");
    let combined_path = request.output_dir.join("combined.mp4");
    if init_path.is_file() && segment_path.is_file() {
        let valid = fs::read(&init_path)
            .ok()
            .and_then(|data| validate_init_segment(&data).ok())
            .is_some()
            && fs::read(&segment_path)
                .ok()
                .and_then(|data| validate_media_segment(&data).ok())
                .is_some();
        if valid {
            return Ok(SegmentArtifact {
                init_path,
                segment_path,
                mode: request.mode,
                cached: true,
                processing_time_ns: None,
            });
        }
        let _ = fs::remove_file(&init_path);
        let _ = fs::remove_file(&segment_path);
    }

    let pipeline = gst::Pipeline::with_name("air-transcode-segment");
    let _pipeline_cleanup = PipelineCleanup(pipeline.clone());
    let va_memory = matches!(request.track, TrackKind::Video)
        && matches!(request.mode, PipelineMode::Transcode)
        && encoder_name.is_some_and(|name| name.starts_with("va"))
        && gst::ElementFactory::find("vapostproc").is_some();
    let desired_caps = match (request.track, request.mode, va_memory) {
        (TrackKind::Video, PipelineMode::Transmux, _) => gst::Caps::builder(
            request
                .transmux_video_codec
                .unwrap_or(VideoCodec::H264)
                .caps_name(),
        )
        .build(),
        (TrackKind::Audio, PipelineMode::Transmux, _) => gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4_i32)
            .build(),
        (TrackKind::Video, PipelineMode::Transcode, true) => {
            gst::Caps::builder("video/x-raw").any_features().build()
        }
        (TrackKind::Video, PipelineMode::Transcode, false) => {
            gst::Caps::builder("video/x-raw").build()
        }
        (TrackKind::Audio, PipelineMode::Transcode, _) => gst::Caps::builder("audio/x-raw").build(),
    };
    let av1_transcode = request.track == TrackKind::Video
        && matches!(request.mode, PipelineMode::Transcode)
        && request.transmux_video_codec == Some(VideoCodec::Av1);
    let source_factory = if av1_transcode {
        "uridecodebin"
    } else {
        "uridecodebin3"
    };
    let source = gst::ElementFactory::make(source_factory)
        .name("source")
        .property("uri", request.source.as_str())
        .property("caps", &desired_caps)
        .build()
        .map_err(|_| Error::MissingElement(source_factory.to_owned()))?;
    configure_source(&source, request.headers.clone(), request.timeout);
    if !av1_transcode {
        configure_stream_selection(&source, request.track, request.selected_stream_id.clone());
    }
    let queue = make("queue")?;
    let app_sink = gst::ElementFactory::make("appsink")
        .name("encoded-sink")
        .property("sync", false)
        .property("max-buffers", 128_u32)
        .build()
        .map_err(|_| Error::MissingElement("appsink".to_owned()))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| Error::Pipeline("appsink has an unexpected type".to_owned()))?;
    let app_sink_element = app_sink.clone().upcast::<gst::Element>();

    pipeline
        .add_many([&source, &queue])
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    let chain = build_track_chain(
        request.track,
        request.mode,
        encoder_name,
        request.video_dimensions,
        request.video_max_fps,
        request.transmux_video_codec,
        request.tone_map_hdr,
    )?;
    for element in &chain {
        pipeline
            .add(element)
            .map_err(|error| Error::Pipeline(error.to_string()))?;
    }
    pipeline
        .add(&app_sink_element)
        .map_err(|error| Error::Pipeline(error.to_string()))?;

    let mut elements: Vec<&gst::Element> = Vec::with_capacity(chain.len() + 2);
    elements.push(&queue);
    elements.extend(chain.iter());
    elements.push(&app_sink_element);
    gst::Element::link_many(&elements).map_err(|error| Error::Pipeline(error.to_string()))?;

    let queue_sink = queue
        .static_pad("sink")
        .ok_or_else(|| Error::Pipeline("queue has no sink pad".to_owned()))?;
    let desired_for_pad = desired_caps;
    let linked_source_pad = Arc::new(Mutex::new(None::<gst::Pad>));
    let linked_source_pad_callback = Arc::clone(&linked_source_pad);
    source.connect_pad_added(move |_, pad| {
        if queue_sink.is_linked() {
            return;
        }
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if caps.can_intersect(&desired_for_pad) && pad.link(&queue_sink).is_ok() {
            *linked_source_pad_callback.lock() = Some(pad.clone());
        }
    });

    pipeline
        .set_state(gst::State::Paused)
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    wait_for_state(
        &pipeline,
        gst::State::Paused,
        request.timeout,
        &request.cancellation,
    )?;

    let start = gst::ClockTime::from_nseconds(request.segment.start_ns);
    let stop =
        gst::ClockTime::from_nseconds(request.segment.start_ns + request.segment.duration_ns);
    let av1_video =
        request.track == TrackKind::Video && request.transmux_video_codec == Some(VideoCodec::Av1);
    let flags = match (request.mode, av1_video, av1_transcode) {
        // Each segment uses a fresh pipeline, so AV1 does not need a flushing
        // seek. GstBaseParse can abort on a flush seek for real Matroska AV1.
        (PipelineMode::Transmux, true, _) => gst::SeekFlags::KEY_UNIT,
        // The simpler uridecodebin path avoids decodebin3's AV1 multiqueue
        // assertion and accepts an accurate flushing seek.
        (PipelineMode::Transcode, true, true) | (PipelineMode::Transcode, false, _) => {
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE
        }
        (PipelineMode::Transcode, true, false) => gst::SeekFlags::ACCURATE,
        (PipelineMode::Transmux, false, _) => gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
    };
    if request.segment.start_ns > 0 {
        let seek_pad = linked_source_pad.lock().clone().ok_or_else(|| {
            Error::Pipeline("source did not expose the selected track pad".to_owned())
        })?;
        let seek_result = pipeline
            .seek(
                1.0,
                flags,
                gst::SeekType::Set,
                start,
                gst::SeekType::Set,
                stop,
            )
            .is_ok()
            || seek_pad.send_event(gst::event::Seek::new(
                1.0,
                flags,
                gst::SeekType::Set,
                start,
                gst::SeekType::Set,
                stop,
            ));
        if !seek_result {
            let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
            return Err(Error::Pipeline(format!(
                "source rejected segment seek; current={current:?}; pending={pending:?}"
            )));
        }
    }
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Pipeline(error.to_string()))?;

    let encoded = collect_encoded_track(
        &pipeline,
        &app_sink,
        request.segment.duration_ns,
        request.timeout,
        &request.cancellation,
    );
    let processing_time_ns = chain.iter().find_map(|element| {
        let is_tone_mapper = element
            .factory()
            .is_some_and(|factory| factory.name() == "hdrtonemap");
        (is_tone_mapper && element.find_property("processing-time-ns").is_some())
            .then(|| element.property::<u64>("processing-time-ns"))
    });
    let _ = pipeline.set_state(gst::State::Null);
    let encoded = encoded?;
    validate_transmux_keyframe(request, &encoded)?;

    mux_encoded_track(request, encoded, &combined_path)?;

    let combined = fs::read(&combined_path)?;
    let (init, media) = split_cmaf(&combined)?;
    fs::write(&init_path, init)?;
    fs::write(&segment_path, media)?;
    let _ = fs::remove_file(&combined_path);
    validate_init_segment(&fs::read(&init_path)?)?;
    validate_media_segment(&fs::read(&segment_path)?)?;
    Ok(SegmentArtifact {
        init_path,
        segment_path,
        mode: request.mode,
        cached: false,
        processing_time_ns,
    })
}

fn validate_transmux_keyframe(request: &SegmentRequest, encoded: &EncodedTrack) -> Result<()> {
    if request.track != TrackKind::Video
        || !matches!(request.mode, PipelineMode::Transmux)
        || request.segment.start_ns == 0
    {
        return Ok(());
    }
    let first = encoded
        .buffers
        .first()
        .ok_or_else(|| Error::Pipeline("pipeline produced no video buffer".to_owned()))?;
    let actual_ns = encoded_timestamp_ns(first).ok_or_else(|| {
        Error::Pipeline("first transmuxed video buffer has no timestamp".to_owned())
    })?;
    let tolerance_ns = first
        .duration()
        .map_or(100_000_000, gst::ClockTime::nseconds)
        .max(100_000_000);
    if first.flags().contains(gst::BufferFlags::DELTA_UNIT)
        || actual_ns.abs_diff(request.segment.start_ns) > tolerance_ns
    {
        return Err(Error::MisalignedKeyframe {
            sequence: request.segment.sequence,
            requested_ns: request.segment.start_ns,
            actual_ns,
        });
    }
    Ok(())
}

fn encoded_timestamp_ns(buffer: &gst::Buffer) -> Option<u64> {
    match (buffer.dts(), buffer.pts()) {
        (Some(dts), Some(pts)) => Some(dts.max(pts).nseconds()),
        (Some(dts), None) => Some(dts.nseconds()),
        (None, Some(pts)) => Some(pts.nseconds()),
        (None, None) => None,
    }
}

struct EncodedTrack {
    caps: gst::Caps,
    buffers: Vec<gst::Buffer>,
}

fn collect_encoded_track(
    pipeline: &gst::Pipeline,
    sink: &gst_app::AppSink,
    target_duration_ns: u64,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<EncodedTrack> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| Error::Pipeline("pipeline has no bus".to_owned()))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut caps = None;
    let mut buffers = Vec::new();
    let mut first_timestamp_ns = None;
    let mut last_timestamp_ns = None;
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Pipeline(format!(
                "pipeline timed out collecting encoded media; buffers={}; first_timestamp_ns={first_timestamp_ns:?}; last_timestamp_ns={last_timestamp_ns:?}; caps_seen={}",
                buffers.len(),
                caps.is_some(),
            )));
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            if caps.is_none() {
                caps = sample.caps_owned();
            }
            if let Some(buffer) = sample.buffer_owned() {
                let timestamp_ns = encoded_timestamp_ns(&buffer);
                if first_timestamp_ns.is_none() {
                    first_timestamp_ns = timestamp_ns;
                }
                if timestamp_ns.is_some() {
                    last_timestamp_ns = timestamp_ns;
                }
                let reaches_target = encoded_buffer_reaches_target(
                    timestamp_ns,
                    buffer.duration().map_or(0, gst::ClockTime::nseconds),
                    first_timestamp_ns,
                    target_duration_ns,
                );
                buffers.push(buffer);
                if reaches_target {
                    break;
                }
            }
            continue;
        }
        if sink.is_eos() {
            break;
        }
        if let Some(message) =
            bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error])
            && let gst::MessageView::Error(error) = message.view()
        {
            return Err(Error::Pipeline(format!(
                "{} ({:?})",
                error.error(),
                error.debug()
            )));
        }
    }
    let caps =
        caps.ok_or_else(|| Error::Pipeline("pipeline produced no encoded caps".to_owned()))?;
    if buffers.is_empty() {
        return Err(Error::Pipeline(
            "pipeline produced no encoded media buffers".to_owned(),
        ));
    }
    Ok(EncodedTrack { caps, buffers })
}

fn encoded_buffer_reaches_target(
    timestamp_ns: Option<u64>,
    duration_ns: u64,
    first_timestamp_ns: Option<u64>,
    target_duration_ns: u64,
) -> bool {
    // Real remuxes commonly have audio ending a few frames before the nominal
    // segment boundary. Accept a sub-100ms tail gap instead of waiting forever
    // for a buffer that does not exist.
    const TAIL_TOLERANCE_NS: u64 = 100_000_000;
    timestamp_ns
        .zip(first_timestamp_ns)
        .is_some_and(|(timestamp, first)| {
            timestamp
                .saturating_add(duration_ns)
                .saturating_sub(first)
                .saturating_add(TAIL_TOLERANCE_NS)
                >= target_duration_ns
        })
}

fn mux_encoded_track(
    request: &SegmentRequest,
    mut encoded: EncodedTrack,
    output: &Path,
) -> Result<()> {
    normalize_timestamps(&mut encoded.buffers);
    let pipeline = gst::Pipeline::with_name("air-transcode-cmaf-mux");
    let _pipeline_cleanup = PipelineCleanup(pipeline.clone());
    let source = gst::ElementFactory::make("appsrc")
        .name("encoded-source")
        .build()
        .map_err(|_| Error::MissingElement("appsrc".to_owned()))?
        .downcast::<gst_app::AppSrc>()
        .map_err(|_| Error::Pipeline("appsrc has an unexpected type".to_owned()))?;
    source.set_caps(Some(&encoded.caps));
    source.set_format(gst::Format::Time);
    source.set_is_live(false);
    source.set_block(true);
    let source_element = source.clone().upcast::<gst::Element>();
    let muxer = gst::ElementFactory::make("cmafmux")
        .name("muxer")
        .property("fragment-duration", request.segment.duration_ns)
        .property(
            "decode-time-offset",
            i64::try_from(request.segment.start_ns).unwrap_or(i64::MAX),
        )
        .property("start-fragment-sequence-number", request.segment.sequence)
        .build()
        .map_err(|_| Error::MissingElement("cmafmux".to_owned()))?;
    let sink = gst::ElementFactory::make("filesink")
        .name("sink")
        .property("location", output.to_string_lossy().as_ref())
        .property("sync", false)
        .build()
        .map_err(|_| Error::MissingElement("filesink".to_owned()))?;
    pipeline
        .add_many([&source_element, &muxer, &sink])
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    gst::Element::link_many([&source_element, &muxer, &sink])
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    for buffer in encoded.buffers {
        if request.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        source
            .push_buffer(buffer)
            .map_err(|error| Error::Pipeline(format!("failed to push encoded buffer: {error}")))?;
    }
    source
        .end_of_stream()
        .map_err(|error| Error::Pipeline(format!("failed to end encoded stream: {error}")))?;
    let result = wait_for_pipeline(&pipeline, request.timeout, &request.cancellation);
    let _ = pipeline.set_state(gst::State::Null);
    result
}

fn normalize_timestamps(buffers: &mut [gst::Buffer]) {
    let base_ns = buffers
        .iter()
        .find_map(|buffer| buffer.dts().or_else(|| buffer.pts()))
        .map_or(0, gst::ClockTime::nseconds);
    for buffer in buffers {
        let buffer = buffer.make_mut();
        if let Some(pts) = buffer.pts() {
            buffer.set_pts(gst::ClockTime::from_nseconds(
                pts.nseconds().saturating_sub(base_ns),
            ));
        }
        if let Some(dts) = buffer.dts() {
            buffer.set_dts(gst::ClockTime::from_nseconds(
                dts.nseconds().saturating_sub(base_ns),
            ));
        }
    }
}

fn make(name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|_| Error::MissingElement(name.to_owned()))
}

#[allow(clippy::too_many_lines)]
fn build_track_chain(
    track: TrackKind,
    mode: PipelineMode,
    encoder_name: Option<&str>,
    video_dimensions: Option<(u32, u32)>,
    video_max_fps: Option<u32>,
    transmux_video_codec: Option<VideoCodec>,
    tone_map_hdr: bool,
) -> Result<Vec<gst::Element>> {
    match (track, mode) {
        (TrackKind::Video, PipelineMode::Transmux) => {
            build_video_transmux_chain(transmux_video_codec.unwrap_or(VideoCodec::H264))
        }
        (TrackKind::Audio, PipelineMode::Transmux) => {
            let parser = make("aacparse")?;
            let caps = gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4_i32)
                .field("stream-format", "raw")
                .build();
            Ok(vec![parser, caps_filter(&caps)?])
        }
        (TrackKind::Video, PipelineMode::Transcode) => {
            let encoder = make(encoder_name.ok_or_else(|| {
                Error::MissingElement("H.264 encoder was not selected".to_owned())
            })?)?;
            let parser = make("h264parse")?;
            parser.set_property("config-interval", -1_i32);
            let output_caps = gst::Caps::builder("video/x-h264")
                .field("stream-format", "avc")
                .field("alignment", "au")
                .field("profile", "high")
                .build();
            let (width, height) = video_dimensions.ok_or_else(|| {
                Error::Pipeline("video track has no output dimensions".to_owned())
            })?;
            let max_fps = video_max_fps.ok_or_else(|| {
                Error::Pipeline("video track has no output frame-rate".to_owned())
            })?;
            configure_video_encoder(&encoder, width, height, max_fps);
            let rate = make("videorate")?;
            rate.set_property("drop-only", true);
            rate.set_property("max-rate", i32::try_from(max_fps).unwrap_or(i32::MAX));
            let width = i32::try_from(width).unwrap_or(i32::MAX);
            let height = i32::try_from(height).unwrap_or(i32::MAX);
            let mut chain = if encoder_name.is_some_and(|name| name.starts_with("va"))
                && gst::ElementFactory::find("vapostproc").is_some()
            {
                let postprocess = make("vapostproc")?;
                postprocess.set_property("disable-passthrough", true);
                if tone_map_hdr && postprocess.find_property("hdr-tone-mapping").is_some() {
                    postprocess.set_property("hdr-tone-mapping", true);
                }
                let mut raw_caps = gst::Caps::builder("video/x-raw")
                    .features(["memory:VAMemory"])
                    .field("format", "NV12")
                    .field("width", width)
                    .field("height", height);
                if tone_map_hdr {
                    raw_caps = raw_caps.field("colorimetry", "bt709");
                }
                vec![rate, postprocess, caps_filter(&raw_caps.build())?]
            } else if tone_map_hdr {
                let scaled_hdr_caps = gst::Caps::builder("video/x-raw")
                    .field("width", width)
                    .field("height", height)
                    .build();
                let sdr_caps = gst::Caps::builder("video/x-raw")
                    .field("width", width)
                    .field("height", height)
                    .field("format", "I420")
                    .field("colorimetry", "bt709")
                    .build();
                vec![
                    rate,
                    make("videoscale")?,
                    caps_filter(&scaled_hdr_caps)?,
                    make("hdrtonemap")?,
                    caps_filter(&sdr_caps)?,
                ]
            } else {
                let raw_caps = gst::Caps::builder("video/x-raw")
                    .field("width", width)
                    .field("height", height)
                    .build();
                vec![
                    rate,
                    make("videoconvert")?,
                    make("videoscale")?,
                    caps_filter(&raw_caps)?,
                ]
            };
            chain.extend([encoder, parser, caps_filter(&output_caps)?]);
            Ok(chain)
        }
        (TrackKind::Audio, PipelineMode::Transcode) => {
            let encoder = make(encoder_name.ok_or_else(|| {
                Error::MissingElement("AAC encoder was not selected".to_owned())
            })?)?;
            configure_audio_encoder(&encoder);
            let output_caps = gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4_i32)
                .field("stream-format", "raw")
                .build();
            Ok(vec![
                make("audioconvert")?,
                make("audioresample")?,
                caps_filter(
                    &gst::Caps::builder("audio/x-raw")
                        .field("channels", 2_i32)
                        .field("rate", 48_000_i32)
                        .build(),
                )?,
                encoder,
                make("aacparse")?,
                caps_filter(&output_caps)?,
            ])
        }
    }
}

fn build_video_transmux_chain(codec: VideoCodec) -> Result<Vec<gst::Element>> {
    let (parser, caps, timestamper) = match codec {
        VideoCodec::H264 => {
            let parser = make("h264parse")?;
            parser.set_property("config-interval", -1_i32);
            let caps = gst::Caps::builder("video/x-h264")
                .field("stream-format", "avc")
                .field("alignment", "au")
                .build();
            (parser, caps, Some(make("h264timestamper")?))
        }
        VideoCodec::H265 => {
            let parser = make("h265parse")?;
            parser.set_property("config-interval", -1_i32);
            let caps = gst::Caps::builder("video/x-h265")
                .field("stream-format", "hvc1")
                .field("alignment", "au")
                .build();
            (parser, caps, Some(make("h265timestamper")?))
        }
        VideoCodec::Av1 => {
            let caps = gst::Caps::builder("video/x-av1")
                .field("stream-format", "obu-stream")
                .field("alignment", "tu")
                .build();
            (make("av1parse")?, caps, None)
        }
    };
    let mut chain = vec![parser, caps_filter(&caps)?];
    if let Some(timestamper) = timestamper {
        chain.push(timestamper);
    }
    Ok(chain)
}

fn caps_filter(caps: &gst::Caps) -> Result<gst::Element> {
    gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .map_err(|_| Error::MissingElement("capsfilter".to_owned()))
}

fn configure_video_encoder(encoder: &gst::Element, width: u32, height: u32, max_fps: u32) {
    let factory_name = encoder
        .factory()
        .map(|factory| factory.name().to_string())
        .unwrap_or_default();
    let bitrate_kbps = target_video_bitrate_kbps(width, height);
    let target_bitrate_bits = bitrate_kbps.saturating_mul(1_000);
    let keyframe_interval = max_fps.saturating_mul(4).max(1);
    match factory_name.as_str() {
        "x264enc" => {
            encoder.set_property("bitrate", bitrate_kbps);
            encoder.set_property_from_str("speed-preset", "veryfast");
            encoder.set_property_from_str("tune", "zerolatency");
            encoder.set_property("key-int-max", keyframe_interval);
            encoder.set_property("bframes", 0_u32);
        }
        "openh264enc" => {
            encoder.set_property("bitrate", target_bitrate_bits);
            encoder.set_property("gop-size", keyframe_interval);
            encoder.set_property_from_str("complexity", "low");
        }
        _ if factory_name.starts_with("va") => {
            if encoder.find_property("bitrate").is_some() {
                encoder.set_property("bitrate", bitrate_kbps);
            }
            if encoder.find_property("key-int-max").is_some() {
                encoder.set_property("key-int-max", keyframe_interval);
            }
        }
        _ if factory_name.starts_with("amc") => {
            encoder.set_property("bitrate", target_bitrate_bits);
            encoder.set_property("i-frame-interval", 4_u32);
        }
        _ => {}
    }
}

pub fn target_video_bitrate_kbps(width: u32, height: u32) -> u32 {
    let pixels = u64::from(width) * u64::from(height);
    if pixels >= 3840 * 2160 {
        20_000
    } else if pixels >= 2560 * 1440 {
        12_000
    } else if pixels >= 1920 * 1080 {
        8_000
    } else if pixels >= 1280 * 720 {
        5_000
    } else {
        3_000
    }
}

fn configure_audio_encoder(encoder: &gst::Element) {
    let factory_name = encoder
        .factory()
        .map(|factory| factory.name().to_string())
        .unwrap_or_default();
    match factory_name.as_str() {
        "avenc_aac" | "fdkaacenc" | "voaacenc" | "faac"
            if encoder.find_property("bitrate").is_some() =>
        {
            encoder.set_property("bitrate", 192_000_i32);
        }
        _ => {}
    }
}

struct PipelineCleanup(gst::Pipeline);

impl Drop for PipelineCleanup {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn cleanup_attempt_files(directory: &Path) -> Result<()> {
    if !directory.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn configure_source(source: &gst::Element, headers: BTreeMap<String, String>, timeout: Duration) {
    let timeout_seconds = timeout.as_secs().clamp(1, 15) as u32;
    source.connect("source-setup", false, move |values| {
        let child = values
            .get(1)
            .and_then(|value| value.get::<gst::Element>().ok())?;
        if child.find_property("extra-headers").is_some() {
            let mut structure = gst::Structure::new_empty("headers");
            for (name, value) in &headers {
                structure.set(name, value.as_str());
            }
            child.set_property("extra-headers", structure);
        }
        if child.find_property("timeout").is_some() {
            child.set_property("timeout", timeout_seconds);
        }
        if child.find_property("retries").is_some() {
            child.set_property("retries", 0_i32);
        }
        if child.find_property("compress").is_some() {
            child.set_property("compress", false);
        }
        None
    });
}

fn configure_stream_selection(
    source: &gst::Element,
    track: TrackKind,
    selected_stream_id: Option<String>,
) {
    let wanted = match track {
        TrackKind::Video => gst::StreamType::VIDEO,
        TrackKind::Audio => gst::StreamType::AUDIO,
    };
    configure_stream_type_selection(source, wanted, selected_stream_id);
}

fn configure_stream_type_selection(
    source: &gst::Element,
    wanted: gst::StreamType,
    selected_stream_id: Option<String>,
) {
    let selected = Arc::new(AtomicBool::new(false));
    source.connect("select-stream", false, move |values| {
        let stream = values
            .get(2)
            .and_then(|value| value.get::<gst::Stream>().ok())?;
        let id_matches = selected_stream_id
            .as_deref()
            .is_none_or(|selected| stream.stream_id().as_deref() == Some(selected));
        let should_select = stream.stream_type().contains(wanted)
            && id_matches
            && selected
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        Some(i32::from(should_select).to_value())
    });
}

fn wait_for_state(
    pipeline: &gst::Pipeline,
    desired: gst::State,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Pipeline(format!(
                "pipeline timed out entering {desired:?}"
            )));
        }
        let (change, current, pending) = pipeline.state(gst::ClockTime::from_mseconds(100));
        change.map_err(|error| {
            Error::Pipeline(format!(
                "failed to enter {desired:?}: {error}; current={current:?}; pending={pending:?}"
            ))
        })?;
        if current == desired {
            return Ok(());
        }
    }
}

fn wait_for_pipeline(
    pipeline: &gst::Pipeline,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<()> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| Error::Pipeline("pipeline has no bus".to_owned()))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::Pipeline("pipeline timed out".to_owned()));
        }
        let wait = gst::ClockTime::from_mseconds(
            u64::try_from(remaining.min(Duration::from_millis(100)).as_millis()).unwrap_or(100),
        );
        let Some(message) = bus.timed_pop(wait) else {
            continue;
        };
        match message.view() {
            gst::MessageView::Eos(_) => return Ok(()),
            gst::MessageView::Error(error) => {
                return Err(Error::Pipeline(format!(
                    "{} ({:?})",
                    error.error(),
                    error.debug()
                )));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::encoded_buffer_reaches_target;

    #[test]
    fn encoded_buffer_duration_can_cross_segment_boundary() {
        assert!(encoded_buffer_reaches_target(
            Some(3_988_999_999),
            21_333_333,
            Some(0),
            4_000_000_000,
        ));
    }

    #[test]
    fn encoded_buffer_before_segment_boundary_keeps_collecting() {
        assert!(!encoded_buffer_reaches_target(
            Some(3_850_000_000),
            20_000_000,
            Some(0),
            4_000_000_000,
        ));
    }

    #[test]
    fn encoded_audio_tail_gap_can_finish_segment() {
        assert!(encoded_buffer_reaches_target(
            Some(7_903_666_666),
            21_333_333,
            Some(4_000_000_000),
            4_000_000_000,
        ));
    }
}
