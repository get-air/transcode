use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Media kind exposed as an independent HLS rendition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
}

impl TrackKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SegmentSpec {
    pub sequence: u32,
    pub start_ns: u64,
    pub duration_ns: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenditionSpec {
    pub index: usize,
    pub name: String,
    pub language: Option<String>,
    pub default: bool,
}

#[must_use]
pub fn segment_map(duration_ns: u64, target_duration_ns: u64) -> Vec<SegmentSpec> {
    if duration_ns == 0 || target_duration_ns == 0 {
        return Vec::new();
    }

    let segment_count = duration_ns.div_ceil(target_duration_ns);
    (0..segment_count)
        .map(|index| {
            let start_ns = index * target_duration_ns;
            SegmentSpec {
                sequence: u32::try_from(index + 1).unwrap_or(u32::MAX),
                start_ns,
                duration_ns: target_duration_ns.min(duration_ns - start_ns),
            }
        })
        .collect()
}

#[must_use]
pub fn media_playlist(track: TrackKind, segments: &[SegmentSpec]) -> String {
    let prefix = format!("{}/segments", track.as_str());
    rendition_playlist(Some(&prefix), segments)
}

#[must_use]
pub fn indexed_media_playlist(_index: usize, segments: &[SegmentSpec]) -> String {
    rendition_playlist(Some("segments"), segments)
}

#[must_use]
pub fn subtitle_playlist(_index: usize, segments: &[SegmentSpec]) -> String {
    rendition_playlist(None, segments)
}

fn rendition_playlist(segment_prefix: Option<&str>, segments: &[SegmentSpec]) -> String {
    let max_duration_ns = segments
        .iter()
        .map(|segment| segment.duration_ns)
        .max()
        .unwrap_or(1_000_000_000);
    let target_duration = max_duration_ns.div_ceil(1_000_000_000).max(1);
    let independent = segment_prefix.map_or("", |_| "#EXT-X-INDEPENDENT-SEGMENTS\n");
    let mut output = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-PLAYLIST-TYPE:VOD\n{independent}"
    );
    for segment in segments {
        if let Some(prefix) = segment_prefix {
            let _ = writeln!(
                output,
                "#EXT-X-MAP:URI=\"{prefix}/{}/init.mp4\"",
                segment.sequence
            );
        }
        let seconds = segment.duration_ns / 1_000_000_000;
        let microseconds = segment.duration_ns % 1_000_000_000 / 1_000;
        let _ = writeln!(output, "#EXTINF:{seconds}.{microseconds:06},");
        let prefix = segment_prefix.unwrap_or("segments");
        let _ = writeln!(output, "{prefix}/{}", segment.sequence);
    }
    output.push_str("#EXT-X-ENDLIST\n");
    output
}

#[must_use]
pub fn master_playlist(
    has_video: bool,
    audio: &[RenditionSpec],
    subtitles: &[RenditionSpec],
    bandwidth: u64,
    codecs: Option<&str>,
) -> String {
    let mut output = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    for (renditions, media_type, group, forced) in [
        (audio, "AUDIO", "audio", ""),
        (subtitles, "SUBTITLES", "subtitles", ",FORCED=NO"),
    ] {
        for rendition in renditions {
            let language = rendition
                .language
                .as_deref()
                .map_or_else(String::new, |value| {
                    format!(",LANGUAGE=\"{}\"", hls_attribute(value))
                });
            let default = if rendition.default { "YES" } else { "NO" };
            let _ = writeln!(
                output,
                "#EXT-X-MEDIA:TYPE={media_type},GROUP-ID=\"{group}\",NAME=\"{}\"{language},DEFAULT={default},AUTOSELECT=YES{forced},URI=\"{group}/{}/playlist.m3u8\"",
                hls_attribute(&rendition.name),
                rendition.index
            );
        }
    }
    if has_video {
        let audio_group = if audio.is_empty() {
            ""
        } else {
            ",AUDIO=\"audio\""
        };
        let subtitle_group = if subtitles.is_empty() {
            ""
        } else {
            ",SUBTITLES=\"subtitles\""
        };
        let codecs = codecs.map_or_else(String::new, |value| format!(",CODECS=\"{value}\""));
        let _ = write!(
            output,
            "#EXT-X-STREAM-INF:BANDWIDTH={}{codecs}{audio_group}{subtitle_group}\nvideo.m3u8\n",
            bandwidth.max(1_000_000)
        );
    } else if let Some(default_audio) = audio
        .iter()
        .find(|rendition| rendition.default)
        .or_else(|| audio.first())
    {
        let codecs = codecs.map_or_else(String::new, |value| format!(",CODECS=\"{value}\""));
        let _ = write!(
            output,
            "#EXT-X-STREAM-INF:BANDWIDTH=256000{codecs},AUDIO=\"audio\"\naudio/{}/playlist.m3u8\n",
            default_audio.index
        );
    }
    output
}

fn hls_attribute(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_covers_duration_without_zero_length_segments() {
        let segments = segment_map(9_100_000_000, 4_000_000_000);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[2].start_ns, 8_000_000_000);
        assert_eq!(segments[2].duration_ns, 1_100_000_000);
        assert_eq!(
            segments
                .iter()
                .map(|segment| segment.duration_ns)
                .sum::<u64>(),
            9_100_000_000
        );
    }

    #[test]
    fn playlist_is_finished_vod_from_first_response() {
        let playlist = media_playlist(TrackKind::Video, &segment_map(8_500_000_000, 4_000_000_000));
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        assert!(playlist.contains("#EXT-X-MAP:URI=\"video/segments/1/init.mp4\""));
        assert!(playlist.contains("#EXTINF:0.500000"));
    }

    #[test]
    fn master_exposes_all_audio_and_subtitle_renditions() {
        let audio = vec![
            RenditionSpec {
                index: 1,
                name: "English".to_owned(),
                language: Some("en".to_owned()),
                default: true,
            },
            RenditionSpec {
                index: 2,
                name: "Español".to_owned(),
                language: Some("es".to_owned()),
                default: false,
            },
        ];
        let subtitles = vec![RenditionSpec {
            index: 3,
            name: "English CC".to_owned(),
            language: Some("en".to_owned()),
            default: false,
        }];
        let playlist = master_playlist(true, &audio, &subtitles, 8_000_000, Some("avc1"));
        assert_eq!(playlist.matches("TYPE=AUDIO").count(), 2);
        assert_eq!(playlist.matches("TYPE=SUBTITLES").count(), 1);
        assert!(playlist.contains("audio/2/playlist.m3u8"));
        assert!(playlist.contains("subtitles/3/playlist.m3u8"));
        assert!(playlist.contains("AUDIO=\"audio\",SUBTITLES=\"subtitles\""));
    }
}
