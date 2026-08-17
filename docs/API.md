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

Reports the GStreamer version, required HTTP/CMAF plugins, and every installed H.264/AAC encoder whose source caps can produce the web output contract. Encoder order is the actual attempt order.

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
    "video_codecs": ["h264", "av1"]
  }
}
```

The response contains an opaque UUID, nanosecond duration, seekability, discovered tracks, selected rendition decisions, RFC 6381 codec identifiers when available, and a relative master playlist URL. Each rendition reports its source track index, `transmux`/`transcode` mode, output codec, and whether discovered HDR metadata is being preserved by passthrough. Source headers are never serialized back to the client.

Input constraints:

- URL: at most 16 KiB; `http`, `https`, and `file` are understood by the media layer.
- Headers: at most 64 valid HTTP fields and 64 KiB total.
- Output dimensions: both at least two pixels.
- Selected track indexes must identify a discovered track of the corresponding kind.
- `video_codecs` is the target's declared decode surface. It defaults to `["h264"]`; accepted values are `h264`, `h265`, and `av1`. A matching source is transmuxed only when it also fits the requested dimensions.
- A finite duration is required. Unknown-duration streams are rejected as non-VOD.

Track metadata includes the codec family, full GStreamer caps, bit depth, colorimetry, and any HDR format discoverable from negotiated caps. Dolby Vision and HDR10+ bitstream metadata may require deeper parsing and is not guessed from filenames.

## `GET /v1/sessions/{id}`

Returns public session metadata and refreshes the inactivity deadline.

## `DELETE /v1/sessions/{id}`

Destroys the session and its recoverable segment cache.

## HLS routes

- `GET /v1/sessions/{id}/master.m3u8`
- `GET /v1/sessions/{id}/video.m3u8`
- `GET /v1/sessions/{id}/audio.m3u8`
- `GET /v1/sessions/{id}/{track}/init.mp4`
- `GET /v1/sessions/{id}/{track}/segments/{sequence}`

Media playlists are HLS v7 VOD playlists. Init and media responses are immutable for a session. Segment endpoints coalesce duplicate requests and may return `408 cancelled` if the viewer makes a far seek and the work is no longer useful. After a successful request, the next segment is prefetched only when a pipeline permit remains idle.
