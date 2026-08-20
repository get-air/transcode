# `@get-air/video` integration

The first integration should be a new HLS/CMAF backend in `@get-air/video`, not GStreamer code inside the browser package.

1. Register the original source with `air-transcode` through the native application or trusted server boundary.
2. Give the video controller the opaque `playback_url`; compatible MP4 is
   relayed directly and other inputs use the returned HLS master.
3. Use native HLS where reliable and hls.js/MSE elsewhere.
4. Read duration and tracks from the session response immediately rather than waiting for HTML media metadata.
5. Map remote-control keys in the existing controller layer; native HTML controls are not a transcoder responsibility.
6. Destroy the session when playback ends or let the server TTL reclaim it.

The future client should be a separate `@get-air/transcode` repository under the workspace rules. Its package root should expose ordinary Promise values, `/effect` should expose the Effect-native service, and both should delegate to one Effect implementation using `@get-air/http` for transport injection.

Tauri applications can bind the Rust library in-process and inject the returned
loopback origin plus its process-local bearer token. Pass the token through the
client's `Authorization` header; never put it in a URL. No Tauri plugin is
required for the media engine itself.
