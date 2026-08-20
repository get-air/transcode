use std::{collections::BTreeMap, time::Duration};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_pbutils as gst_pbutils;
use gstreamer_pbutils::prelude::*;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl VideoCodec {
    #[must_use]
    pub const fn caps_name(self) -> &'static str {
        match self {
            Self::H264 => "video/x-h264",
            Self::H265 => "video/x-h265",
            Self::Av1 => "video/x-av1",
        }
    }

    #[must_use]
    pub const fn rfc6381_fallback(self) -> &'static str {
        match self {
            Self::H264 => "avc1",
            Self::H265 => "hvc1",
            Self::Av1 => "av01",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProbeRequest {
    pub url: Url,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaTrack {
    pub index: usize,
    pub stream_id: Option<String>,
    pub kind: String,
    pub name: Option<String>,
    pub codec: Option<String>,
    pub video_codec: Option<VideoCodec>,
    pub rfc6381_codec: Option<String>,
    pub caps: Option<String>,
    pub bit_depth: Option<u32>,
    pub colorimetry: Option<String>,
    pub hdr_format: Option<String>,
    pub language: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate_num: Option<u32>,
    pub frame_rate_denom: Option<u32>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub web_compatible: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MediaInfo {
    pub duration_ns: u64,
    pub seekable: bool,
    pub container: Option<String>,
    pub tracks: Vec<MediaTrack>,
}

/// Discovers the finite duration, seekability, and track topology of a URI.
///
/// # Errors
///
/// Returns an error for unsupported schemes, discovery failures, timeouts, or
/// sources without a finite VOD duration.
pub fn probe(request: &ProbeRequest) -> Result<MediaInfo> {
    if !matches!(request.url.scheme(), "http" | "https" | "file") {
        return Err(Error::InvalidSource(format!(
            "scheme {} is not supported",
            request.url.scheme()
        )));
    }

    let timeout = gst::ClockTime::from_nseconds(
        u64::try_from(request.timeout.as_nanos()).unwrap_or(u64::MAX),
    );
    let discoverer = gst_pbutils::Discoverer::new(timeout)
        .map_err(|error| Error::Discovery(error.to_string()))?;
    let headers = request.headers.clone();
    let io_timeout_seconds = request.timeout.as_secs().clamp(1, 15) as u32;
    discoverer.connect_source_setup(move |_, source| {
        if source.find_property("extra-headers").is_some() && !headers.is_empty() {
            let mut structure = gst::Structure::new_empty("headers");
            for (name, value) in &headers {
                structure.set(name, value.as_str());
            }
            source.set_property("extra-headers", structure);
        }
        configure_http_source(source, io_timeout_seconds);
    });
    let info = discoverer
        .discover_uri(request.url.as_str())
        .map_err(|error| Error::Discovery(error.to_string()))?;

    let duration_ns = info.duration().map_or(0, gst::ClockTime::nseconds);
    if duration_ns == 0 {
        return Err(Error::UnknownDuration);
    }

    let container = info
        .stream_info()
        .and_then(|stream| stream.caps())
        .map(|caps| caps.to_string());
    let mut tracks = Vec::new();
    if let Some(stream) = info.stream_info() {
        collect_streams(&stream, &mut tracks);
    }

    Ok(MediaInfo {
        duration_ns,
        seekable: info.is_seekable(),
        container,
        tracks,
    })
}

fn configure_http_source(source: &gst::Element, timeout_seconds: u32) {
    if source.find_property("timeout").is_some() {
        source.set_property("timeout", timeout_seconds);
    }
    if source.find_property("retries").is_some() {
        source.set_property("retries", 0_i32);
    }
    if source.find_property("compress").is_some() {
        source.set_property("compress", false);
    }
}

fn collect_streams(stream: &gst_pbutils::DiscovererStreamInfo, tracks: &mut Vec<MediaTrack>) {
    if let Ok(container) = stream
        .clone()
        .downcast::<gst_pbutils::DiscovererContainerInfo>()
    {
        for child in container.streams() {
            collect_streams(&child, tracks);
        }
        return;
    }

    let stream_id = stream.stream_id().map(|value| value.to_string());
    let caps = stream.caps();
    let caps_name = caps
        .as_ref()
        .and_then(|caps| caps.structure(0))
        .map(|structure| structure.name().to_string());
    let caps_string = caps.as_ref().map(ToString::to_string);
    let codec = stream
        .tags()
        .and_then(|tags| {
            tags.get::<gst::tags::Codec>()
                .map(|value| value.get().to_owned())
        })
        .or_else(|| caps_name.clone());
    let language = stream.tags().and_then(|tags| {
        tags.get::<gst::tags::LanguageCode>()
            .map(|value| value.get().to_owned())
    });
    let name = stream.tags().and_then(|tags| {
        tags.get::<gst::tags::Title>()
            .map(|value| value.get().to_owned())
    });

    if let Ok(video) = stream
        .clone()
        .downcast::<gst_pbutils::DiscovererVideoInfo>()
    {
        tracks.push(video_track(
            tracks.len(),
            &video,
            caps.as_ref(),
            stream_id,
            codec,
            caps_string,
            language,
            name,
        ));
    } else if let Ok(audio) = stream
        .clone()
        .downcast::<gst_pbutils::DiscovererAudioInfo>()
    {
        tracks.push(audio_track(
            tracks.len(),
            &audio,
            caps.as_ref(),
            stream_id,
            codec,
            caps_string,
            language,
            name,
        ));
    } else if let Ok(subtitle) = stream
        .clone()
        .downcast::<gst_pbutils::DiscovererSubtitleInfo>()
    {
        tracks.push(subtitle_track(
            tracks.len(),
            &subtitle,
            caps.as_ref(),
            stream_id,
            codec,
            caps_string,
            language,
            name,
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn video_track(
    index: usize,
    video: &gst_pbutils::DiscovererVideoInfo,
    caps: Option<&gst::Caps>,
    stream_id: Option<String>,
    codec: Option<String>,
    caps_string: Option<String>,
    language: Option<String>,
    name: Option<String>,
) -> MediaTrack {
    let structure = caps.and_then(|caps| caps.structure(0));
    let frame_rate = video.framerate();
    let video_codec = structure.and_then(|structure| match structure.name().as_str() {
        "video/x-h264" => Some(VideoCodec::H264),
        "video/x-h265" => Some(VideoCodec::H265),
        "video/x-av1" => Some(VideoCodec::Av1),
        _ => None,
    });
    let bit_depth = structure.and_then(|structure| {
        structure.get::<u32>("bit-depth-luma").ok().or_else(|| {
            structure
                .get::<&str>("profile")
                .ok()
                .filter(|profile| profile.contains("10"))
                .map(|_| 10)
        })
    });
    let web_compatible = structure.is_some_and(|structure| {
        let profile = structure.get::<&str>("profile").unwrap_or_default();
        let chroma = structure.get::<&str>("chroma-format").unwrap_or("4:2:0");
        structure.name() == "video/x-h264"
            && matches!(
                profile,
                "baseline" | "constrained-baseline" | "main" | "high"
            )
            && chroma == "4:2:0"
            && bit_depth.unwrap_or(8) <= 8
    });
    MediaTrack {
        index,
        stream_id,
        kind: "video".to_owned(),
        name,
        codec,
        video_codec,
        rfc6381_codec: caps
            .and_then(|caps| gst_pbutils::codec_utils_caps_get_mime_codec(caps).ok())
            .map(|codec| codec.to_string())
            .or_else(|| {
                structure.and_then(|structure| {
                    h264_codec(structure)
                        .or_else(|| video_codec.map(|codec| codec.rfc6381_fallback().to_owned()))
                })
            }),
        caps: caps_string,
        bit_depth,
        colorimetry: structure
            .and_then(|structure| structure.get::<&str>("colorimetry").ok())
            .map(ToOwned::to_owned),
        hdr_format: structure.and_then(detect_hdr),
        language,
        width: Some(video.width()),
        height: Some(video.height()),
        frame_rate_num: u32::try_from(frame_rate.numer()).ok(),
        frame_rate_denom: u32::try_from(frame_rate.denom()).ok(),
        channels: None,
        sample_rate: None,
        web_compatible,
    }
}

#[allow(clippy::too_many_arguments)]
fn audio_track(
    index: usize,
    audio: &gst_pbutils::DiscovererAudioInfo,
    caps: Option<&gst::Caps>,
    stream_id: Option<String>,
    codec: Option<String>,
    caps_string: Option<String>,
    language: Option<String>,
    name: Option<String>,
) -> MediaTrack {
    let web_compatible = caps
        .and_then(|caps| caps.structure(0))
        .is_some_and(|structure| {
            structure.name() == "audio/mpeg"
                && structure.get::<i32>("mpegversion").ok() == Some(4)
                && audio.channels() <= 2
        });
    MediaTrack {
        index,
        stream_id,
        kind: "audio".to_owned(),
        name,
        codec,
        video_codec: None,
        rfc6381_codec: web_compatible.then(|| "mp4a.40.2".to_owned()),
        caps: caps_string,
        bit_depth: None,
        colorimetry: None,
        hdr_format: None,
        language,
        width: None,
        height: None,
        frame_rate_num: None,
        frame_rate_denom: None,
        channels: Some(audio.channels()),
        sample_rate: Some(audio.sample_rate()),
        web_compatible,
    }
}

#[allow(clippy::too_many_arguments)]
fn subtitle_track(
    index: usize,
    subtitle: &gst_pbutils::DiscovererSubtitleInfo,
    caps: Option<&gst::Caps>,
    stream_id: Option<String>,
    codec: Option<String>,
    caps_string: Option<String>,
    language: Option<String>,
    name: Option<String>,
) -> MediaTrack {
    let caps_name = caps
        .and_then(|caps| caps.structure(0))
        .map(|structure| structure.name().to_string());
    let web_compatible = caps_name.as_deref().is_some_and(|name| {
        name == "text/x-raw"
            || name.starts_with("application/x-subtitle")
            || matches!(
                name,
                "application/x-ssa" | "application/x-ass" | "application/ttml+xml"
            )
    });
    MediaTrack {
        index,
        stream_id,
        kind: "subtitle".to_owned(),
        name,
        codec,
        video_codec: None,
        rfc6381_codec: None,
        caps: caps_string,
        bit_depth: None,
        colorimetry: None,
        hdr_format: None,
        language: language.or_else(|| subtitle.language().map(|value| value.to_string())),
        width: None,
        height: None,
        frame_rate_num: None,
        frame_rate_denom: None,
        channels: None,
        sample_rate: None,
        web_compatible,
    }
}

fn detect_hdr(structure: &gst::StructureRef) -> Option<String> {
    let colorimetry = structure.get::<&str>("colorimetry").unwrap_or_default();
    if colorimetry.contains("2100-pq") || colorimetry.contains("2084") {
        return Some("hdr10".to_owned());
    }
    if colorimetry.contains("2100-hlg") || colorimetry.contains("hlg") {
        return Some("hlg".to_owned());
    }
    (structure.has_field("mastering-display-info") || structure.has_field("content-light-level"))
        .then(|| "hdr10".to_owned())
}

fn h264_codec(structure: &gst::StructureRef) -> Option<String> {
    if structure.name() != "video/x-h264" {
        return None;
    }
    let buffer = structure.get::<gst::Buffer>("codec_data").ok()?;
    let map = buffer.map_readable().ok()?;
    let bytes = map.as_slice();
    if bytes.len() < 4 || bytes[0] != 1 {
        return None;
    }
    Some(format!(
        "avc1.{:02X}{:02X}{:02X}",
        bytes[1], bytes[2], bytes[3]
    ))
}
