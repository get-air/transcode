# HTTP API

The default origin is `http://127.0.0.1:11471`. JSON errors have the shape:

```json
{
  "error": {
    "code": "media_processing_failed",
    "message": "human-readable context"
  }
}
```

## `GET /health`

Reports process health and the selected media engine.

## `GET /v1/capabilities`

Reports the GStreamer version, required HTTP/CMAF plugins, every installed H.264/AAC encoder whose source caps can produce the web output contract, and `hdr_tone_mapping` as `va`, `software`, or `unavailable`. Encoder order is the actual attempt order; Android MediaCodec factories rank ahead of other hardware and software encoders.

## `GET /v1/metrics`

Returns in-process scheduling counters. No source URL or header value is included.

## `POST /v1/sessions`

```json
{
  "source": {
    "url": "https://media.example/movie.mkv",
    "headers": {
      "Authorization": "Bearer REDACTED",
      "Referer": "https://app.example/"
    }
  },
  "output": {
    "transmux": true,
    "force_transcode": false,
    "max_width": 3840,
    "max_height": 2160,
    "max_fps": 30,
    "video_track_index": 0,
    "preferred_audio_languages": ["en", "es"],
    "preferred_subtitle_languages": ["en"],
    "audio_track_index": 1,
    "subtitle_track_index": 3,
    "video_codecs": ["h264", "h265", "av1"],
    "hdr_formats": ["hdr10"]
  },
  "subtitles": [{
    "source": { "url": "https://media.example/subtitles/en.srt" },
    "name": "English CC",
    "language": "en",
    "offset_ms": 0
  }]
}
```

The response contains an opaque UUID, nanosecond duration, seekability, discovered tracks, every playable rendition, RFC 6381 codec identifiers when available, `master_url`, `playback_url`, and `delivery`. Browser-compatible remote MP4 uses `delivery: "proxy"`; other inputs use `delivery: "hls"`. Renditions report kind, name, language, source track index, default status, output codec, `transmux`/`transcode` mode where applicable, and HDR passthrough status. Source headers are never serialized back to the client.

## `POST /v1/sessions/{id}/warm`

Accepts `position_seconds` and `buffer_seconds` and blocks until the selected
audio/video renditions cover that playback reserve. The reserve is capped at 60
seconds. This endpoint is idempotent against cached segments and is the startup
barrier used by `@get-air/transcode`.

Input constraints:

- URL: at most 16 KiB; `http`, `https`, and `file` are understood by the media layer.
- Headers: at most 64 valid HTTP fields and 64 KiB total.
- Output dimensions: both at least two pixels. Defaults are 3840×2160; smaller sources are never upscaled.
- `max_fps` must be positive and defaults to 30. Faster sources are transcoded through `videorate` rather than proxied or transmuxed unchanged.
- Selected video, audio, and subtitle indexes must identify a discovered track of the corresponding kind. Audio and subtitle indexes choose defaults; every playable audio/text-subtitle track remains available for runtime HLS switching.
- Preferred audio/subtitle languages are evaluated in order using exact or base-language matching. Explicit track indexes take precedence.
- `video_codecs` is the target's declared decode surface. It defaults to `["h264"]`; accepted values are `h264`, `h265`, and `av1`. A matching source is transmuxed only when it also fits the requested dimensions. Ten-bit Matroska AV1 is currently forced through the H.264 fallback after real-source validation exposed unsafe GStreamer parser behavior during random-access transmux.
- `hdr_formats` is the target's case-insensitive HDR render surface, for example `["hdr10"]`. HDR is passed through only when the source codec, dimensions, frame rate, and HDR format are all declared. SDR fallback requires VA hardware tone mapping or a real `hdrtonemap` plugin. Software tone mapping monitors segment generation time and reduces its height through 1440p, 1080p, and 720p when it cannot retain real-time headroom.
- A finite duration is required. Unknown-duration streams are rejected as non-VOD.
- Up to 64 external text subtitles may be attached. Each uses the same validated source/header contract, a 1–256 character display name, optional language, and signed millisecond timing offset.

Track metadata includes the codec family, full GStreamer caps, bit depth, colorimetry, and any HDR format discoverable from negotiated caps. External subtitles receive synthetic track indexes in the same rendition list. Dolby Vision and HDR10+ bitstream metadata may require deeper parsing and is not guessed from filenames.

## `GET /v1/sessions/{id}`

Returns public session metadata and refreshes the inactivity deadline.

## `DELETE /v1/sessions/{id}`

Destroys the session and its recoverable segment cache.

## HLS routes

- `GET /v1/sessions/{id}/master.m3u8`
- `GET /v1/sessions/{id}/video.m3u8`
- `GET /v1/sessions/{id}/audio.m3u8`
- `GET /v1/sessions/{id}/audio/{track_index}/playlist.m3u8`
- `GET /v1/sessions/{id}/audio/{track_index}/init.mp4`
- `GET /v1/sessions/{id}/audio/{track_index}/segments/{sequence}`
- `GET /v1/sessions/{id}/audio/{track_index}/segments/{sequence}/init.mp4`
- `GET /v1/sessions/{id}/subtitles/{track_index}/playlist.m3u8`
- `GET /v1/sessions/{id}/subtitles/{track_index}/segments/{sequence}`
- `GET /v1/sessions/{id}/{track}/init.mp4`
- `GET /v1/sessions/{id}/{track}/segments/{sequence}`
- `GET /v1/sessions/{id}/{track}/segments/{sequence}/init.mp4`

Media playlists are HLS v7 VOD playlists. Audio renditions produce CMAF/AAC; supported text subtitles produce segmented `text/vtt`. Each independently generated media segment advertises its matching CMAF init map, allowing a long-GOP transmux segment to fall back to browser-safe transcoding without violating HLS decoder configuration. Init and media responses are immutable for a session. Segment endpoints coalesce duplicate requests. The server automatically prepares the configured playback reserve at session startup and advances that reserve after every requested video/audio segment; clients may also use the warm endpoint as an explicit playback barrier.
