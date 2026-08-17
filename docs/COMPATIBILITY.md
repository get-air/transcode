# Compatibility and release gates

## Output contract

- HLS protocol version 7.
- CMAF-compatible fragmented MP4.
- Separate video and audio renditions.
- Multiple named/language-tagged audio renditions with independent caches.
- Multiple text-subtitle renditions normalized to segmented WebVTT.
- H.264 AVC in `avc`/access-unit alignment by default.
- Opt-in HEVC `hvc1` and AV1 OBU-stream CMAF passthrough for targets that declare those decoders.
- AAC-LC raw access units, stereo at most.
- Explicit RFC 6381 `CODECS` declarations for both direct and transcoded renditions.
- Complete finite VOD duration and `ENDLIST`.
- A keyframe at every video fragment boundary.
- Monotonic `tfdt` across independently generated fragments.

## Tested automatically

- Authenticated HTTP origin with byte ranges.
- H.264/AAC direct transmux.
- VP9/Opus Matroska transcode.
- HEVC/AAC and AV1/AAC Matroska video transmux without reencoding.
- Ten-bit 4:4:4 H.264 incompatibility classification.
- Random access by requesting segment two before segment one.
- Eight concurrent identical requests with one generation.
- Exact selection between two AAC tracks with different encoded payloads.
- Runtime switching between two audio and two subtitle renditions without recreating a session.
- SRT-in-Matroska discovery, key-unit seeking, WebVTT conversion, later-segment timing, subtitle cache reuse, and invalid-index rejection.
- Idle-only adjacent segment prefetch followed by a verified cache hit.
- Encoded timestamp duration bounds independent of demuxer end-of-segment behavior.
- CMAF box structure and decode-time validation.
- Full master-playlist decode through GStreamer's HLS player.
- Deliberately corrupted cache detection and repair.
- Invalid scheme, invalid header, malformed media, unknown session, and out-of-range errors.
- Property tests against arbitrary malformed ISO BMFF input.
- Two-hour manifest generation benchmark.

## Real remote 2160p results

Two signed remote Matroska sources were exercised without persisting their URLs:

- 3840×2160 HEVC with four stereo AAC tracks: full-duration probing, segment 1 and segment 700 random access, AAC transmux, HEVC-to-1920×1080 H.264 conversion, distinct media-payload validation, and complete HLS discovery passed. On the test RX 7900 XT host, four seconds of video took approximately 4.3–4.7 seconds after pipeline startup.
- 3840×1600 Dolby Vision/HDR10+ Main-10 HEVC with ten 5.1 E-AC-3 tracks: duration and all tracks probe correctly, and GStreamer negotiates `vah265dec -> vapostproc -> vah264enc`. It still produces fewer than 96 frames in 30 seconds on the same host and is therefore a failed performance case, not claimed compatibility.

## Platform intent

| Platform | GStreamer transport | Preferred hardware path | Software fallback |
|---|---|---|---|
| Linux | soup/reqwest HTTP source | VA, NVENC, QSV, V4L2 as registered | x264/OpenH264 + available AAC encoder |
| Windows | soup/reqwest HTTP source | Media Foundation, NVENC, QSV, AMF as registered | x264/OpenH264 + available AAC encoder |
| Android | soup/reqwest HTTP source | device MediaCodec factories from `androidmedia` | bundled OpenH264/AAC plugins where licensed |

Factory discovery is caps-based, so the server does not depend on a hard-coded Android codec name.

## Gates before a stable release

- Exact MP4 and Matroska keyframe maps for arbitrary-GOP direct transmuxing.
- Color-correct Dolby Vision/HDR10/HDR10+ to SDR tone mapping. Eight-bit conversion without tone mapping is not considered visually correct.
- Explicit segment job leases so abandoned HTTP requests cancel immediately after a client disconnect.
- Multiple selectable video/audio/subtitle renditions.
- Bitmap subtitle support (PGS/VobSub/DVB) through a switchable overlay or OCR path; these are discovered but not advertised as WebVTT.
- GStreamer Android distribution packaging and on-device MediaCodec tests.
- Windows runner tests against official MSVC GStreamer binaries.
- Chromium, Firefox, WebKit, Android WebView, and Safari/hls.js playback automation.
- Sustained 4K concurrency, memory, cancellation latency, and cache-pressure benchmarks.
