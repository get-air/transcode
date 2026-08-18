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

Reports the GStreamer version, required HTTP/CMAF plugins, every installed H.264/AAC encoder whose source caps can produce the web output contract, and `hdr_tone_mapping` as `va` or `unavailable`. Encoder order is the actual attempt order.

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
    "max_width": 1920,
    "max_height": 1080,
    "video_track_index": 0,
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

The response contains an opaque UUID, nanosecond duration, seekability, discovered tracks, every playable rendition, RFC 6381 codec identifiers when available, and a relative master playlist URL. Renditions report kind, name, language, source track index, default status, output codec, `transmux`/`transcode` mode where applicable, and HDR passthrough status. Source headers are never serialized back to the client.

Input constraints:

- URL: at most 16 KiB; `http`, `https`, and `file` are understood by the media layer.
- Headers: at most 64 valid HTTP fields and 64 KiB total.
- Output dimensions: both at least two pixels.
- Selected video, audio, and subtitle indexes must identify a discovered track of the corresponding kind. Audio and subtitle indexes choose defaults; every playable audio/text-subtitle track remains available for runtime HLS switching.
- `video_codecs` is the target's declared decode surface. It defaults to `["h264"]`; accepted values are `h264`, `h265`, and `av1`. A matching source is transmuxed only when it also fits the requested dimensions. Ten-bit Matroska AV1 is currently forced through the H.264 fallback after real-source validation exposed unsafe GStreamer parser behavior during random-access transmux.
- `hdr_formats` is the target's case-insensitive HDR render surface, for example `["hdr10"]`. HDR is passed through only when the source codec, dimensions, and HDR format are all declared. An HDR source that must become SDR uses VA hardware tone mapping only when the runtime reports it; otherwise the request is rejected instead of silently producing incorrect eight-bit color.
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
- `GET /v1/sessions/{id}/subtitles/{track_index}/playlist.m3u8`
- `GET /v1/sessions/{id}/subtitles/{track_index}/segments/{sequence}`
- `GET /v1/sessions/{id}/{track}/init.mp4`
- `GET /v1/sessions/{id}/{track}/segments/{sequence}`

Media playlists are HLS v7 VOD playlists. Audio renditions produce CMAF/AAC; supported text subtitles produce segmented `text/vtt`. Init and media responses are immutable for a session. Segment endpoints coalesce duplicate requests and may return `408 cancelled` if the viewer makes a far seek and the work is no longer useful. After a successful video/audio request, the next segment is prefetched only when a pipeline permit remains idle.
