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
    "max_height": 1080
  }
}
```

The response contains an opaque UUID, nanosecond duration, seekability, discovered tracks, RFC 6381 codec identifiers when available, and a relative master playlist URL. Source headers are never serialized back to the client.

Input constraints:

- URL: at most 16 KiB; `http`, `https`, and `file` are understood by the media layer.
- Headers: at most 64 valid HTTP fields and 64 KiB total.
- Output dimensions: both at least two pixels.
- A finite duration is required. Unknown-duration streams are rejected as non-VOD.

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

Media playlists are HLS v7 VOD playlists. Init and media responses are immutable for a session. Segment endpoints coalesce duplicate requests and may return `408 cancelled` if the viewer makes a far seek and the work is no longer useful.
