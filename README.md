# Air Transcode

`air-transcode` is a GStreamer-only HTTP VOD transmuxing and transcoding server. It turns seekable HTTP, HTTPS, and local media sources into browser-oriented HLS v7 with CMAF fragmented MP4 tracks.

The server prefers a zero-decode path. Browser-safe H.264 and AAC tracks are parsed and transmuxed; incompatible tracks are decoded and transcoded to H.264/AAC. Encoder factories are discovered from the active GStreamer registry, hardware implementations are tried first, and runtime failures fall through to the next compatible encoder.

## Current features

- Authenticated remote HTTP sources with caller-provided headers kept in memory.
- GStreamer discovery for duration, seekability, container, codecs, dimensions, channels, and language.
- Complete VOD playlists with a stable duration and `#EXT-X-ENDLIST` from the first request.
- HLS v7 CMAF output: independent video/audio init segments and `.m4s` media fragments.
- Direct H.264/AAC transmuxing when profile, chroma format, bit depth, channel count, and requested dimensions are browser-compatible.
- H.264/AAC transcoding with runtime-ranked hardware encoders and automatic fallback.
- Default 1080p output ceiling without upscaling smaller sources.
- Lazy random-access segment generation and immutable segment caching.
- Correct per-segment `tfdt` offsets across independent seek pipelines.
- Duplicate-request coalescing, bounded concurrency, far-seek cancellation, session TTLs, and bounded per-session caches.
- Cache corruption detection and regeneration before serving.
- Linux implementation plus platform-neutral Rust code paths for Windows and Android GStreamer distributions.
- Metrics for generated, cached, transmuxed, transcoded, failed, cancelled, active, and peak-active work.

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

Register a source. Credentials belong in headers, not in the TV- or browser-facing HLS URL:

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
      "max_height": 1080
    }
  }'
```

Play the returned `master_url` with an HLS/MSE player such as hls.js. Native HTML media support alone varies by browser.

## Test

The end-to-end suite generates its own H.264/AAC MP4 and VP9/Opus Matroska fixtures, hosts them behind an authenticated byte-range HTTP origin, exercises random and concurrent requests, corrupts the cache deliberately, and decodes the resulting HLS through GStreamer.

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo bench --bench manifest
```

See [API](docs/API.md), [architecture](docs/ARCHITECTURE.md), [compatibility](docs/COMPATIBILITY.md), and the proposed [`@get-air/video` integration](docs/VIDEO_INTEGRATION.md).

## Status

This repository is under active development. The primary video and audio renditions are functional and tested. Indexed multi-audio/subtitle selection, exact arbitrary-keyframe transmux maps, Android packaging, and browser-matrix automation remain release gates rather than silently claimed support.

## License

Licensed under MIT.
