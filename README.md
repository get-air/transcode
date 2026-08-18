# Air Transcode

`air-transcode` is a GStreamer-only HTTP VOD transmuxing and transcoding server. It turns seekable HTTP, HTTPS, and local media sources into browser-oriented HLS v7 with CMAF fragmented MP4 tracks.

The server prefers a zero-decode path. H.264/AAC are the conservative defaults; clients can explicitly declare HEVC or validated eight-bit AV1 support and receive those tracks by direct CMAF transmux instead of an expensive H.264 reencode. Incompatible tracks are decoded and transcoded to H.264/AAC. Encoder factories are discovered from the active GStreamer registry, hardware implementations are tried first, and runtime failures fall through to the next compatible encoder.

## Current features

- Authenticated remote HTTP sources with caller-provided headers kept in memory.
- A tokenized, range-preserving source relay for browser demuxers such as
  MediaBunny when the upstream media origin does not grant CORS access.
- Deduplicated source registration resolves redirecting providers once, pins
  the final CDN URL, shares probe metadata across sessions, and circuit-breaks
  HTTP 429 responses using `Retry-After`.
- GStreamer discovery for duration, seekability, container, codecs, complete caps, bit depth, colorimetry, HDR signals, dimensions, channels, and language.
- Complete VOD playlists with a stable duration and `#EXT-X-ENDLIST` from the first request.
- HLS v7 CMAF output: independent video/audio init segments and `.m4s` media fragments.
- Direct H.264/AAC transmuxing when profile, chroma format, bit depth, channel count, and requested dimensions are browser-compatible, with H.264 decode-timestamp reconstruction for reordered Matroska streams.
- Per-segment keyframe verification: aligned H.264 stays zero-copy, while a long-GOP seek that lands on an older keyframe automatically retries through exact-keyframe H.264 transcoding within the original deadline.
- Opt-in HEVC and validated eight-bit AV1 CMAF transmux for capable clients, including H.265 DTS reconstruction for Matroska sources. Ten-bit Matroska AV1 takes the conservative H.264 path through a seek-safe `uridecodebin` pipeline because `decodebin3` can abort on real-source AV1 flush seeks.
- H.264/AAC transcoding with runtime-ranked hardware encoders and automatic fallback.
- Default 1080p output ceiling without upscaling smaller sources.
- Exact discovered audio/video track selection by index through `uridecodebin3`.
- Every discovered audio track exposed as an independent HLS rendition for instant player-side language switching.
- Embedded or external SRT, WebVTT, SSA/ASS, TTML, and related text subtitles normalized to segmented WebVTT renditions for player-side enable/disable and switching.
- Bounded remote-source I/O timeouts with one retry, preventing a stalled debrid origin from occupying a pipeline indefinitely.
- Lazy random-access segment generation and immutable segment caching.
- Idle-only adjacent-segment prefetch so sequential playback usually hits cache.
- Correct per-segment `tfdt` offsets across independent seek pipelines.
- Duplicate-request coalescing, bounded concurrency, far-seek cancellation, session TTLs, and bounded per-session caches.
- Cache corruption detection and regeneration before serving.
- Linux runtime implementation, a native Windows CI lane against the official MSVC GStreamer SDK, and a reproducible Android arm64/API 24 link against the official `libgstreamer_android.so` aggregate.
- Metrics for generated, cached, transmuxed, transcoded, failed, cancelled, active, and peak-active work.
- No production source downloader: local fixture generation and any downloaded samples are test-only.

## Runtime requirements

- Rust 1.96.1 for building this checkout.
- GStreamer 1.28 or newer.
- `gstreamer`, `gstreamer-app`, `gstreamer-pbutils`, a `soup` or `reqwest` HTTP source, and the `isobmff` plugin.
- At least one H.264 encoder and one AAC encoder for incompatible inputs.

On Arch Linux:

```bash
sudo pacman -S gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad \
  gst-plugins-ugly gst-libav gst-plugin-isobmff
```

Verify the critical runtime surface:

```bash
./scripts/check-runtime.sh
```

## Run

```bash
cargo run --release -- \
  --bind 127.0.0.1:11471 \
  --cache-dir .cache/air-transcode
```

### Embed in Tauri or another Tokio host

The crate owns listener lifecycle as well as the media engine, so a native app
does not need to shell out to the CLI:

```rust
let config = air_transcode::Config::loopback(app_cache_dir);
let server = air_transcode::spawn_server(config).await?;
let origin = server.origin();

// Inject `origin` into the WebView-side @get-air/transcode client.

server.shutdown().await?;
```

`Config::loopback` requests an ephemeral loopback port and is the safe default
for local Tauri playback. For Vizio casting, start the paired host instead:

```rust
let host = air_transcode::spawn_tauri_host(
    air_transcode::Config::loopback(app_cache_dir),
    "0.0.0.0:0".parse()?,
).await?;

let admin_origin = host.admin_origin();
let admin_token = host.admin_token();
let tv_url = host.cast_url(lan_ip, &session.master_url);
```

Inject `admin_origin` plus `Authorization: Bearer ${admin_token}` into the
WebView-side `@get-air/transcode` client. The administrative session API remains
loopback-only and bearer-protected. The LAN listener is read-only and mounted
beneath a different random per-process token, so the TV can fetch only manifests
and media for session IDs it receives. `host.shutdown()` stops both listeners.

Register a source. Credentials belong in headers, not in the TV- or browser-facing HLS URL. The server reads the source on demand and never materializes the complete input as a product feature:

```bash
SOURCE_ID=$(curl -fsS http://127.0.0.1:11471/v1/sources \
  --header 'content-type: application/json' \
  --data '{"url":"https://media.example/video.mkv"}' | jq -r .id)

curl http://127.0.0.1:11471/v1/sessions \
  --header 'content-type: application/json' \
  --data "{\"source_id\":\"$SOURCE_ID\",\"output\":{\"max_width\":1920}}"
```

Inline `source` session requests remain supported and are internally
deduplicated through the same registry. Browser byte-range readers use
`GET /v1/sources/{id}/relay`; release the source with `DELETE /v1/sources/{id}`.

The legacy inline form is:

```bash
curl http://127.0.0.1:11471/v1/sessions \
  --header 'content-type: application/json' \
  --data '{
    "source": {
      "url": "https://media.example/video.mkv",
      "headers": { "Authorization": "Bearer REDACTED" }
    },
    "output": {
      "transmux": true,
      "max_width": 1920,
      "max_height": 1080,
      "video_codecs": ["h264"],
      "hdr_formats": [],
      "audio_track_index": 1,
      "subtitle_track_index": 3
    },
    "subtitles": [{
      "source": { "url": "https://media.example/subtitles/en.srt" },
      "name": "English CC",
      "language": "en",
      "offset_ms": 0
    }]
  }'
```

Play the returned `master_url` with an HLS/MSE player such as hls.js. Its audio and subtitle track lists map directly to the HLS rendition groups, so changing `audioTrack` or `subtitleTrack` does not recreate the server session. Only add `"h265"` or `"av1"` to `video_codecs` after checking the actual browser/device decoder. For HDR, passthrough preserves the encoded signal. Machines whose VA driver exposes GStreamer's `hdr-tone-mapping` property report `hdr_tone_mapping: "va"` and can produce SDR through the hardware path; other machines report `"unavailable"` and reject fake eight-bit conversion.

## Test

The end-to-end suite locally generates H.264/AAC, HEVC/AAC, AV1/AAC, VP9/Opus, bilingual audio, and bilingual subtitle fixtures; hosts them behind an authenticated byte-range HTTP origin; switches rendition payloads; validates WebVTT timelines; exercises random and concurrent requests; corrupts the cache deliberately; and decodes the conservative H.264/AAC result through GStreamer.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo bench --bench manifest
```

See [API](docs/API.md), [architecture](docs/ARCHITECTURE.md), [compatibility](docs/COMPATIBILITY.md), [Android/Tauri packaging](docs/ANDROID.md), and the [`@get-air/video` integration](docs/VIDEO_INTEGRATION.md).

## Status

This repository is under active development. Primary video plus indexed multi-audio and embedded/external text-subtitle renditions are functional and tested. Bitmap subtitles, exact arbitrary-keyframe handling for opt-in HEVC/AV1 passthrough, Android on-device qualification, and browser-matrix automation remain release gates rather than silently claimed support.

## License

Licensed under MIT.
