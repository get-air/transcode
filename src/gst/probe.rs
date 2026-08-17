use std::{collections::BTreeMap, time::Duration};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_pbutils as gst_pbutils;
use gstreamer_pbutils::prelude::*;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{Error, Result};

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
    pub kind: String,
    pub codec: Option<String>,
    pub rfc6381_codec: Option<String>,
    pub caps: Option<String>,
    pub language: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
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
    discoverer.connect_source_setup(move |_, source| {
        if source.find_property("extra-headers").is_some() && !headers.is_empty() {
            let mut structure = gst::Structure::new_empty("headers");
            for (name, value) in &headers {
                structure.set(name, value.as_str());
            }
            source.set_property("extra-headers", structure);
        }
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

    let caps = stream.caps();
    let caps_string = caps
        .as_ref()
        .and_then(|caps| caps.structure(0))
        .map(|structure| structure.name().to_string());
    let codec = stream
        .tags()
        .and_then(|tags| {
            tags.get::<gst::tags::Codec>()
                .map(|value| value.get().to_owned())
        })
        .or_else(|| caps_string.clone());
    let language = stream.tags().and_then(|tags| {
        tags.get::<gst::tags::LanguageCode>()
            .map(|value| value.get().to_owned())
    });

    if let Ok(video) = stream
        .clone()
        .downcast::<gst_pbutils::DiscovererVideoInfo>()
    {
        let web_compatible = caps
            .as_ref()
            .and_then(|caps| caps.structure(0))
            .is_some_and(|structure| {
                let profile = structure.get::<&str>("profile").unwrap_or_default();
                let chroma = structure.get::<&str>("chroma-format").unwrap_or("4:2:0");
                let bit_depth = structure.get::<u32>("bit-depth-luma").unwrap_or(8);
                structure.name() == "video/x-h264"
                    && matches!(
                        profile,
                        "baseline" | "constrained-baseline" | "main" | "high"
                    )
                    && chroma == "4:2:0"
                    && bit_depth <= 8
            });
        tracks.push(MediaTrack {
            index: tracks.len(),
            kind: "video".to_owned(),
            codec,
            rfc6381_codec: caps
                .as_ref()
                .and_then(|caps| caps.structure(0))
                .and_then(h264_codec),
            caps: caps_string,
            language,
            width: Some(video.width()),
            height: Some(video.height()),
            channels: None,
            sample_rate: None,
            web_compatible,
        });
    } else if let Ok(audio) = stream
        .clone()
        .downcast::<gst_pbutils::DiscovererAudioInfo>()
    {
        let web_compatible = caps
            .as_ref()
            .and_then(|caps| caps.structure(0))
            .is_some_and(|structure| {
                structure.name() == "audio/mpeg"
                    && structure.get::<i32>("mpegversion").ok() == Some(4)
                    && audio.channels() <= 2
            });
        tracks.push(MediaTrack {
            index: tracks.len(),
            kind: "audio".to_owned(),
            codec,
            rfc6381_codec: web_compatible.then(|| "mp4a.40.2".to_owned()),
            caps: caps_string,
            language,
            width: None,
            height: None,
            channels: Some(audio.channels()),
            sample_rate: Some(audio.sample_rate()),
            web_compatible,
        });
    }
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
