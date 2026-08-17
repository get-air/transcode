use gstreamer as gst;
use gstreamer::prelude::*;
use serde::Serialize;

use crate::hls::TrackKind;

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
    pub h264_encoders: Vec<EncoderCandidate>,
    pub aac_encoders: Vec<EncoderCandidate>,
}

fn available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
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
    Capabilities {
        gstreamer_version: gst::version_string().to_string(),
        cmaf: available("cmafmux"),
        hls_cmaf: available("hlscmafsink"),
        http: available("souphttpsrc") || available("reqwesthttpsrc"),
        h264_encoders: encoder_candidates(TrackKind::Video),
        aac_encoders: encoder_candidates(TrackKind::Audio),
    }
}
