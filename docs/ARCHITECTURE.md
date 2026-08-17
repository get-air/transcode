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
  -> appsink encoded buffers
  -> normalize local timestamps
  -> appsrc -> cmafmux -> filesink
  -> split complete CMAF into init + media sections
  -> apply global tfdt decode-time offset
  -> validate ftyp/moov or styp/moof/tfdt/mdat
  -> atomically expose cache artifact
```

## Media decisions

Video is transmuxable only when it is H.264 baseline, constrained-baseline, main, or high profile; 4:2:0; eight-bit; and within the requested dimensions. Audio is transmuxable only when it is MPEG-4 AAC with no more than two channels. Otherwise the track is transcoded.

Encoder selection is registry-driven rather than operating-system-name-driven. Factories are filtered by output caps, hardware implementations sort first, and each candidate is attempted until one qualifies under the real pipeline. This also finds Android MediaCodec factories whose names are device-specific.

`uridecodebin3` receives the selected discovered stream ID and rejects every other stream in its `select-stream` callback. This prevents multilingual sources from instantiating unused decoders and makes the single video/audio HLS renditions deterministic.

## Concurrency and cancellation

A Tokio semaphore bounds native pipelines. A per-session/track/sequence mutex prevents duplicate work. Requests more than two segments away cancel older work for that track; cancellation is checked during preroll, state transitions, encoder fallback, and bus waits. All encoder attempts share one total deadline rather than multiplying the timeout by the number of candidates. HTTP client disconnect detection still needs an explicit job/lease protocol because Hyper can keep the service future alive after the socket closes.

One adjacent segment is scheduled after a short delay only when a permit is idle. Prefetched work is marked so it cannot recursively walk the full title, and active requests always prevent new idle prefetch from starting.

## Cache integrity

Each segment has a private generation directory. Cached artifacts are parsed before reuse. Truncated or malformed artifacts are deleted and regenerated. Session count, TTL, pipeline count, and cached segment count are independently bounded.

## Known hard problem: arbitrary-keyframe transmux maps

Independent range-seek transmuxing must use real source keyframe boundaries. A fixed four-second timeline is exact only when keyframes align with those boundaries. The release design requires a GStreamer-backed byte-range reader plus MP4/Matroska index extraction, or an equivalent GStreamer index API, before arbitrary-keyframe sources can be advertised as exact direct transmux. Until then this is a documented release gate and is covered by adversarial fixture work rather than hidden behind optimistic metadata.
