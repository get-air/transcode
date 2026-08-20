# Architecture

## Clean-room reference

The installed Stremio server and its current bundled HLS v2 behavior were inspected as an interoperability reference. Its externally observable model informed the requirements: finite probing, complete VOD playlists, independent tracks, lazy fragments, direct remuxing, keyframe-aware seeking, cancellation, bounded cache state, and hardware fallback. No Stremio source is included or called at runtime.

The implementation deliberately uses GStreamer for every media operation. Rust owns HTTP, validation, scheduling, cache policy, lifecycle, and ISO BMFF boundary validation. A two-stage appsink/appsrc bridge buffers only the already encoded bounded track interval, allowing normal source seeking before CMAF muxing.

## Request flow

```text
POST source
  -> validate URL and headers
  -> GstDiscoverer probe
  -> classify each primary track
  -> publish complete HLS VOD timeline

segment request
  -> coalesce duplicate request
  -> cancel obsolete far-seek work
  -> check and validate immutable cache
  -> acquire global pipeline permit
  -> seek remote source to bounded interval
  -> parse/remux OR decode/convert/encode
  -> appsink encoded buffers, independently capped by timestamp duration
  -> normalize local timestamps
  -> appsrc -> cmafmux -> filesink
  -> split complete CMAF into init + media sections
  -> apply global tfdt decode-time offset
  -> validate ftyp/moov or styp/moof/tfdt/mdat
  -> atomically expose cache artifact
```

## Media decisions

H.264 video is transmuxable by default only when it is baseline, constrained-baseline, main, or high profile; 4:2:0; eight-bit; and within the requested dimensions. A client may also declare HEVC or AV1 decoding support; a matching track within the requested dimensions is then parsed and transmuxed without decoding. H.264 and H.265 pass through their codec timestamp reconstructors so reordered Matroska presentation timestamps become valid CMAF decode timestamps. Audio is transmuxable only when it is MPEG-4 AAC with no more than two channels. Otherwise the track is transcoded.

The inspected Stremio server bundle defaults its HLS transmux surface to H.264/AAC. Its H.264 transcode path labels/converts output as BT.709 but does not include a real tone-mapping stage. Air does not treat that metadata rewrite as HDR conversion: HEVC/AV1 passthrough preserves HDR for capable targets. VA uses the driver's real `hdr-tone-mapping` property. A separately bundled `hdrtonemap` element may provide a real software/OpenCL fallback; Air measures that path against the segment duration and reduces its output height when it loses real-time headroom. Runtimes with neither implementation reject SDR conversion.

Encoder selection is registry-driven rather than operating-system-name-driven. Factories are filtered by output caps, hardware implementations sort first, and each candidate is attempted until one qualifies under the real pipeline. This also finds Android MediaCodec factories whose names are device-specific.

Android MediaCodec factories receive first platform preference. Transcoded H.264 bitrate follows output resolution (3 Mbps below 720p, 5 Mbps at 720p, 8 Mbps at 1080p, 12 Mbps at 1440p, and 20 Mbps at 4K), with a four-second keyframe interval and a 30 fps ceiling. The master playlist advertises the matching estimated bandwidth.

`uridecodebin3` receives the selected discovered stream ID and rejects every other stream in its `select-stream` callback. This prevents multilingual sources from instantiating unused decoders and makes the single video/audio HLS renditions deterministic.

The master playlist advertises every audio track as an HLS `AUDIO` rendition and every decodable embedded or caller-supplied external text subtitle as a `SUBTITLES` rendition. Audio caches include the discovered track index, so simultaneous language requests cannot collide. Subtitle pipelines select one exact text stream, prefer a key-unit seek with an accurate-seek fallback, apply a signed timing offset, decode common text formats, strip Pango-only markup, and serialize absolute-timeline WebVTT cues. Player switching is therefore an HLS operation rather than mutable server state.

The product has no whole-source download mode. Tests use generated local files (or explicitly staged samples) to remove origin variability, while production pipelines continue to perform bounded range reads against the caller's source.

## Concurrency and cancellation

A Tokio semaphore bounds native pipelines. A per-session/track/sequence mutex prevents duplicate work. Session deletion cancels all generation owned by that session. Cancellation is checked during preroll, state transitions, encoder fallback, and bus waits. All encoder attempts share one total deadline rather than multiplying the timeout by the number of candidates.

Remote GStreamer HTTP sources use bounded blocking-I/O timeouts, one retry, and disabled content compression so range semantics remain predictable. Encoded collection also stops at the requested fragment duration even if a demuxer ignores the seek's stop boundary. Together these keep unhealthy debrid origins and malformed timelines from monopolizing a pipeline permit.

The selected video and audio renditions begin filling an automatic playback reserve as soon as an HLS session is created. Each foreground segment request advances the reserve by the configured number of seconds. Preload jobs use the same duplicate coalescing and global pipeline limit as foreground work, so clients never need a separate warm call.

## Cache integrity

Each segment has a private generation directory. Cached artifacts are parsed before reuse. Truncated or malformed artifacts are deleted and regenerated. Session count, TTL, pipeline count, and cached segment count are independently bounded.

## Arbitrary-keyframe transmux handling

Independent range-seek transmuxing must use real source keyframe boundaries. Every nonzero video segment now checks the first encoded keyframe timestamp before normalization. Default H.264 retries a misaligned segment through exact-keyframe H.264 transcoding, so seeking remains correct without scanning a whole remote file up front. Opt-in HEVC/AV1 passthrough still requires aligned source keyframes; a future source index map or same-codec encoder fallback is required to make arbitrary GOPs transparent for those formats.
