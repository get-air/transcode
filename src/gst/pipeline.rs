use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
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
}

#[derive(Clone, Debug)]
pub struct SegmentArtifact {
    pub init_path: PathBuf,
    pub segment_path: PathBuf,
    pub mode: PipelineMode,
    pub cached: bool,
}

/// Generates and validates one independent CMAF media segment.
///
/// # Errors
///
/// Returns an error for unavailable plugins, failed source seeks, pipeline
/// failures, timeouts, malformed generated CMAF, or cache I/O failures.
pub fn generate_segment(request: &SegmentRequest) -> Result<SegmentArtifact> {
    if request.cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !matches!(request.mode, PipelineMode::Transcode) {
        return generate_segment_once(request, None);
    }
    let candidates = encoder_candidates(request.track);
    if candidates.is_empty() {
        return Err(Error::MissingElement(format!(
            "{} encoder producing browser-compatible output",
            request.track.as_str()
        )));
    }
    let mut failures = Vec::new();
    let mut first_attempt = true;
    let deadline = std::time::Instant::now() + request.timeout;
    for candidate in candidates {
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
        attempt.timeout = remaining;
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
        (TrackKind::Video, PipelineMode::Transmux, _) => gst::Caps::builder("video/x-h264").build(),
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
    let source = gst::ElementFactory::make("uridecodebin")
        .name("source")
        .property("uri", request.source.as_str())
        .property("caps", &desired_caps)
        .property("expose-all-streams", false)
        .build()
        .map_err(|_| Error::MissingElement("uridecodebin".to_owned()))?;
    configure_source_headers(&source, request.headers.clone());
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
    let flags = match request.mode {
        PipelineMode::Transmux => gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
        PipelineMode::Transcode => gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
    };
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
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Pipeline(error.to_string()))?;

    let encoded =
        collect_encoded_track(&pipeline, &app_sink, request.timeout, &request.cancellation);
    let _ = pipeline.set_state(gst::State::Null);
    let encoded = encoded?;

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
    })
}

struct EncodedTrack {
    caps: gst::Caps,
    buffers: Vec<gst::Buffer>,
}

fn collect_encoded_track(
    pipeline: &gst::Pipeline,
    sink: &gst_app::AppSink,
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<EncodedTrack> {
    let bus = pipeline
        .bus()
        .ok_or_else(|| Error::Pipeline("pipeline has no bus".to_owned()))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut caps = None;
    let mut buffers = Vec::new();
    loop {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if std::time::Instant::now() >= deadline {
            return Err(Error::Pipeline("pipeline timed out".to_owned()));
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            if caps.is_none() {
                caps = sample.caps_owned();
            }
            if let Some(buffer) = sample.buffer_owned() {
                buffers.push(buffer);
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

fn build_track_chain(
    track: TrackKind,
    mode: PipelineMode,
    encoder_name: Option<&str>,
    video_dimensions: Option<(u32, u32)>,
) -> Result<Vec<gst::Element>> {
    match (track, mode) {
        (TrackKind::Video, PipelineMode::Transmux) => {
            let parser = make("h264parse")?;
            parser.set_property("config-interval", -1_i32);
            let caps = gst::Caps::builder("video/x-h264")
                .field("stream-format", "avc")
                .field("alignment", "au")
                .build();
            Ok(vec![parser, caps_filter(&caps)?])
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
            configure_video_encoder(&encoder);
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
            let width = i32::try_from(width).unwrap_or(i32::MAX);
            let height = i32::try_from(height).unwrap_or(i32::MAX);
            let mut chain = if encoder_name.is_some_and(|name| name.starts_with("va"))
                && gst::ElementFactory::find("vapostproc").is_some()
            {
                let postprocess = make("vapostproc")?;
                postprocess.set_property("disable-passthrough", true);
                vec![
                    postprocess,
                    caps_filter(
                        &gst::Caps::builder("video/x-raw")
                            .features(["memory:VAMemory"])
                            .field("format", "NV12")
                            .field("width", width)
                            .field("height", height)
                            .build(),
                    )?,
                ]
            } else {
                vec![
                    make("videoconvert")?,
                    make("videoscale")?,
                    caps_filter(
                        &gst::Caps::builder("video/x-raw")
                            .field("width", width)
                            .field("height", height)
                            .build(),
                    )?,
                ]
            };
            chain.extend([encoder, parser, caps_filter(&output_caps)?]);
            Ok(chain)
        }
        (TrackKind::Audio, PipelineMode::Transcode) => {
            let encoder = make(encoder_name.ok_or_else(|| {
                Error::MissingElement("AAC encoder was not selected".to_owned())
            })?)?;
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

fn caps_filter(caps: &gst::Caps) -> Result<gst::Element> {
    gst::ElementFactory::make("capsfilter")
        .property("caps", caps)
        .build()
        .map_err(|_| Error::MissingElement("capsfilter".to_owned()))
}

fn configure_video_encoder(encoder: &gst::Element) {
    let factory_name = encoder
        .factory()
        .map(|factory| factory.name().to_string())
        .unwrap_or_default();
    match factory_name.as_str() {
        "x264enc" => {
            encoder.set_property_from_str("speed-preset", "veryfast");
            encoder.set_property_from_str("tune", "zerolatency");
            encoder.set_property("key-int-max", 120_u32);
            encoder.set_property("bframes", 0_u32);
        }
        "openh264enc" => {
            encoder.set_property("gop-size", 120_u32);
            encoder.set_property_from_str("complexity", "low");
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

fn configure_source_headers(source: &gst::Element, headers: BTreeMap<String, String>) {
    if headers.is_empty() {
        return;
    }
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
        None
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
