use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Serialize;

use crate::hls::TrackKind;

use super::VideoCodec;

#[derive(Clone, Debug, Serialize)]
pub struct EncoderCandidate {
    pub element: String,
    pub hardware: bool,
    pub rank: i32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Capabilities {
    pub gstreamer_version: String,
    pub cmaf: bool,
    pub hls_cmaf: bool,
    pub http: bool,
    pub transmux_video_codecs: Vec<VideoCodec>,
    pub hdr_tone_mapping: HdrToneMapping,
    pub subtitle_inputs: Vec<String>,
    pub subtitle_output: String,
    pub h264_encoders: Vec<EncoderCandidate>,
    pub aac_encoders: Vec<EncoderCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HdrToneMapping {
    Va,
    Unavailable,
}

fn available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

#[must_use]
pub fn hdr_tone_mapping() -> HdrToneMapping {
    let va_encoder = encoder_candidates(TrackKind::Video)
        .iter()
        .any(|candidate| candidate.element.starts_with("va"));
    let va_postprocess = gst::ElementFactory::make("vapostproc")
        .build()
        .ok()
        .is_some_and(|element| element.find_property("hdr-tone-mapping").is_some());
    if va_encoder && va_postprocess {
        HdrToneMapping::Va
    } else {
        HdrToneMapping::Unavailable
    }
}

pub fn encoder_candidates(track: TrackKind) -> Vec<EncoderCandidate> {
    let (factory_type, desired_caps) = match track {
        TrackKind::Video => (
            gst::ElementFactoryType::VIDEO_ENCODER,
            gst::Caps::builder("video/x-h264").build(),
        ),
        TrackKind::Audio => (
            gst::ElementFactoryType::AUDIO_ENCODER,
            gst::Caps::builder("audio/mpeg")
                .field("mpegversion", 4_i32)
                .build(),
        ),
    };
    let mut candidates = gst::ElementFactory::factories_with_type(factory_type, gst::Rank::NONE)
        .into_iter()
        .filter(|factory| {
            factory.static_pad_templates().iter().any(|template| {
                template.direction() == gst::PadDirection::Src
                    && template.caps().can_intersect(&desired_caps)
            })
        })
        .map(|factory| EncoderCandidate {
            element: factory.name().to_string(),
            hardware: factory.has_type(gst::ElementFactoryType::HARDWARE)
                || factory.klass().contains("Hardware"),
            rank: i32::from(factory.rank()),
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .hardware
            .cmp(&left.hardware)
            .then_with(|| right.rank.cmp(&left.rank))
            .then_with(|| {
                software_preference(&left.element).cmp(&software_preference(&right.element))
            })
            .then_with(|| left.element.cmp(&right.element))
    });
    candidates
}

fn software_preference(name: &str) -> u8 {
    match name {
        "x264enc" | "fdkaacenc" => 0,
        "openh264enc" | "avenc_aac" => 1,
        "voaacenc" | "faac" => 2,
        _ => 3,
    }
}

#[must_use]
pub fn inspect_capabilities() -> Capabilities {
    let mut transmux_video_codecs = vec![VideoCodec::H264];
    if available("h265parse") && available("h265timestamper") {
        transmux_video_codecs.push(VideoCodec::H265);
    }
    if available("av1parse") {
        transmux_video_codecs.push(VideoCodec::Av1);
    }
    let mut subtitle_inputs = Vec::new();
    if available("subparse") {
        subtitle_inputs
            .extend(["srt", "webvtt", "microdvd", "sami", "mpl2", "dks", "lrc"].map(str::to_owned));
    }
    if available("ssaparse") {
        subtitle_inputs.extend(["ssa", "ass"].map(str::to_owned));
    }
    if available("ttmlparse") {
        subtitle_inputs.push("ttml".to_owned());
    }
    Capabilities {
        gstreamer_version: gst::version_string().to_string(),
        cmaf: available("cmafmux"),
        hls_cmaf: available("hlscmafsink"),
        http: [
            "souphttpsrc",
            "reqwesthttpsrc",
            "curlhttpsrc",
            "neonhttpsrc",
        ]
        .into_iter()
        .any(available),
        transmux_video_codecs,
        hdr_tone_mapping: hdr_tone_mapping(),
        subtitle_inputs,
        subtitle_output: "webvtt".to_owned(),
        h264_encoders: encoder_candidates(TrackKind::Video),
        aac_encoders: encoder_candidates(TrackKind::Audio),
    }
}
