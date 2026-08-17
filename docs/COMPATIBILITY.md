# Compatibility and release gates

## Output contract

- HLS protocol version 7.
- CMAF-compatible fragmented MP4.
- Separate video and audio renditions.
- H.264 AVC in `avc`/access-unit alignment.
- AAC-LC raw access units, stereo at most.
- Complete finite VOD duration and `ENDLIST`.
- A keyframe at every video fragment boundary.
- Monotonic `tfdt` across independently generated fragments.

## Tested automatically

- Authenticated HTTP origin with byte ranges.
- H.264/AAC direct transmux.
- VP9/Opus Matroska transcode.
- Ten-bit 4:4:4 H.264 incompatibility classification.
- Random access by requesting segment two before segment one.
- Eight concurrent identical requests with one generation.
- CMAF box structure and decode-time validation.
- Full master-playlist decode through GStreamer's HLS player.
- Deliberately corrupted cache detection and repair.
- Invalid scheme, invalid header, malformed media, unknown session, and out-of-range errors.
- Property tests against arbitrary malformed ISO BMFF input.
- Two-hour manifest generation benchmark.

## Platform intent

| Platform | GStreamer transport | Preferred hardware path | Software fallback |
|---|---|---|---|
| Linux | soup/reqwest HTTP source | VA, NVENC, QSV, V4L2 as registered | x264/OpenH264 + available AAC encoder |
| Windows | soup/reqwest HTTP source | Media Foundation, NVENC, QSV, AMF as registered | x264/OpenH264 + available AAC encoder |
| Android | soup/reqwest HTTP source | device MediaCodec factories from `androidmedia` | bundled OpenH264/AAC plugins where licensed |

Factory discovery is caps-based, so the server does not depend on a hard-coded Android codec name.

## Gates before a stable release

- Exact MP4 and Matroska keyframe maps for arbitrary-GOP direct transmuxing.
- Multiple selectable video/audio/subtitle renditions.
- WebVTT conversion and subtitle playlists.
- GStreamer Android distribution packaging and on-device MediaCodec tests.
- Windows runner tests against official MSVC GStreamer binaries.
- Chromium, Firefox, WebKit, Android WebView, and Safari/hls.js playback automation.
- Sustained 4K concurrency, memory, cancellation latency, and cache-pressure benchmarks.

