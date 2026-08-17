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
    let max_duration_ns = segments
        .iter()
        .map(|segment| segment.duration_ns)
        .max()
        .unwrap_or(1_000_000_000);
    let target_duration = max_duration_ns.div_ceil(1_000_000_000).max(1);
    let mut output = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{target_duration}\n#EXT-X-MEDIA-SEQUENCE:1\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-MAP:URI=\"{}/init.mp4\"\n",
        track.as_str()
    );
    for segment in segments {
        let seconds = segment.duration_ns / 1_000_000_000;
        let microseconds = segment.duration_ns % 1_000_000_000 / 1_000;
        let _ = write!(
            output,
            "#EXTINF:{seconds}.{microseconds:06},\n{}/segments/{}\n",
            track.as_str(),
            segment.sequence
        );
    }
    output.push_str("#EXT-X-ENDLIST\n");
    output
}

#[must_use]
pub fn master_playlist(
    has_video: bool,
    has_audio: bool,
    bandwidth: u64,
    codecs: Option<&str>,
) -> String {
    let mut output = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    if has_audio {
        output.push_str(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Default\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n",
        );
    }
    if has_video {
        let audio = if has_audio { ",AUDIO=\"audio\"" } else { "" };
        let codecs = codecs.map_or_else(String::new, |value| format!(",CODECS=\"{value}\""));
        let _ = write!(
            output,
            "#EXT-X-STREAM-INF:BANDWIDTH={}{codecs}{audio}\nvideo.m3u8\n",
            bandwidth.max(1_000_000)
        );
    } else if has_audio {
        let codecs = codecs.map_or_else(String::new, |value| format!(",CODECS=\"{value}\""));
        let _ = write!(
            output,
            "#EXT-X-STREAM-INF:BANDWIDTH=256000{codecs},AUDIO=\"audio\"\naudio.m3u8\n"
        );
    }
    output
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
        assert!(playlist.contains("#EXT-X-MAP:URI=\"video/init.mp4\""));
        assert!(playlist.contains("#EXTINF:0.500000"));
    }
}
