use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use gstreamer as gst;
use gstreamer::prelude::*;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    error::{Error, Result},
    hls::{SegmentSpec, TrackKind},
    mp4::{validate_init_segment, validate_media_segment},
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
    for candidate in candidates {
        if request.cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if first_attempt {
            first_attempt = false;
        } else {
            cleanup_attempt_files(&request.output_dir)?;
        }
        match generate_segment_once(request, Some(&candidate.element)) {
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
    let init_pattern = request.output_dir.join("init%05d.mp4");
    let segment_pattern = request.output_dir.join("segment%05d.m4s");
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
    let desired_caps = match (request.track, request.mode) {
        (TrackKind::Video, PipelineMode::Transmux) => gst::Caps::builder("video/x-h264").build(),
        (TrackKind::Audio, PipelineMode::Transmux) => gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4_i32)
            .build(),
        (TrackKind::Video, PipelineMode::Transcode) => gst::Caps::builder("video/x-raw").build(),
        (TrackKind::Audio, PipelineMode::Transcode) => gst::Caps::builder("audio/x-raw").build(),
    };
    let source = gst::ElementFactory::make("uridecodebin")
        .name("source")
        .property("uri", request.source.as_str())
        .property("caps", &desired_caps)
        .build()
        .map_err(|_| Error::MissingElement("uridecodebin".to_owned()))?;
    configure_source_headers(&source, request.headers.clone());
    let queue = make("queue")?;
    let sink = gst::ElementFactory::make("hlscmafsink")
        .name("sink")
        .property(
            "target-duration",
            u32::try_from(request.segment.duration_ns.div_ceil(1_000_000_000)).unwrap_or(u32::MAX),
        )
        .property("playlist-length", 0_u32)
        .property("max-files", 0_u32)
        .property("sync", true)
        .property("init-location", init_pattern.to_string_lossy().as_ref())
        .property("location", segment_pattern.to_string_lossy().as_ref())
        .property(
            "playlist-location",
            request
                .output_dir
                .join("generated.m3u8")
                .to_string_lossy()
                .as_ref(),
        )
        .build()
        .map_err(|_| Error::MissingElement("hlscmafsink".to_owned()))?;
    sink.set_property_from_str("playlist-type", "vod");
    let muxer = sink
        .clone()
        .downcast::<gst::Bin>()
        .ok()
        .and_then(|bin| bin.by_name("muxer"))
        .ok_or_else(|| Error::Pipeline("hlscmafsink has no CMAF muxer child".to_owned()))?;
    muxer.set_property(
        "decode-time-offset",
        i64::try_from(request.segment.start_ns).unwrap_or(i64::MAX),
    );

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
        .add(&sink)
        .map_err(|error| Error::Pipeline(error.to_string()))?;

    let mut elements: Vec<&gst::Element> = Vec::with_capacity(chain.len() + 2);
    elements.push(&queue);
    elements.extend(chain.iter());
    elements.push(&sink);
    gst::Element::link_many(&elements).map_err(|error| Error::Pipeline(error.to_string()))?;

    let queue_sink = queue
        .static_pad("sink")
        .ok_or_else(|| Error::Pipeline("queue has no sink pad".to_owned()))?;
    let desired_for_pad = desired_caps;
    source.connect_pad_added(move |_, pad| {
        if queue_sink.is_linked() {
            return;
        }
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        if caps.can_intersect(&desired_for_pad) {
            let _ = pad.link(&queue_sink);
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

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| Error::Pipeline(error.to_string()))?;
    wait_for_state(
        &pipeline,
        gst::State::Playing,
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
    let seek_result = source.seek(
        1.0,
        flags,
        gst::SeekType::Set,
        start,
        gst::SeekType::Set,
        stop,
    );
    if seek_result.is_err() {
        let (_, current, pending) = pipeline.state(gst::ClockTime::ZERO);
        return Err(Error::Pipeline(format!(
            "source rejected segment seek; current={current:?}; pending={pending:?}"
        )));
    }
    sink.set_property("sync", false);

    let result = wait_for_pipeline(&pipeline, request.timeout, &request.cancellation);
    let _ = pipeline.set_state(gst::State::Null);
    result?;

    normalize_generated_paths(&request.output_dir, &init_path, &segment_path)?;
    if !init_path.is_file() || !segment_path.is_file() {
        return Err(Error::Pipeline(
            "pipeline reached EOS without producing CMAF init and media fragments".to_owned(),
        ));
    }
    validate_init_segment(&fs::read(&init_path)?)?;
    validate_media_segment(&fs::read(&segment_path)?)?;
    Ok(SegmentArtifact {
        init_path,
        segment_path,
        mode: request.mode,
        cached: false,
    })
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
            Ok(vec![
                make("videoconvert")?,
                make("videoscale")?,
                caps_filter(
                    &gst::Caps::builder("video/x-raw")
                        .field("width", i32::try_from(width).unwrap_or(i32::MAX))
                        .field("height", i32::try_from(height).unwrap_or(i32::MAX))
                        .build(),
                )?,
                encoder,
                parser,
                caps_filter(&output_caps)?,
            ])
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
            encoder.set_property("complexity", 0_u32);
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

fn normalize_generated_paths(directory: &Path, init: &Path, segment: &Path) -> Result<()> {
    if !init.is_file()
        && let Some(found) = find_prefixed(directory, "init", "mp4")?
    {
        fs::rename(found, init)?;
    }
    if !segment.is_file()
        && let Some(found) = find_prefixed(directory, "segment", "m4s")?
    {
        fs::rename(found, segment)?;
    }
    Ok(())
}

fn find_prefixed(directory: &Path, prefix: &str, extension: &str) -> Result<Option<PathBuf>> {
    let mut matches = fs::read_dir(directory)?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.starts_with(prefix))
                && path.extension().and_then(|ext| ext.to_str()) == Some(extension)
        })
        .collect::<Vec<_>>();
    matches.sort();
    Ok(matches.into_iter().next())
}
