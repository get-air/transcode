# Android and Tauri packaging

Air uses the same Rust server on Android. GStreamer itself is packaged as the
official `libgstreamer_android.so` aggregate so plugin registration, JNI setup,
and native dependencies remain in the supported GStreamer integration path.

## Reproducible arm64 build

1. Install the Android NDK and Rust target.
2. Download and unpack the official GStreamer Android universal SDK.
3. Run:

```bash
rustup target add aarch64-linux-android
export ANDROID_NDK_HOME=/path/to/android-ndk
export GSTREAMER_ROOT_ANDROID=/path/to/gstreamer-android-universal
./scripts/build-android.sh
```

The script builds the Web-playable codec/plugin surface in
`android/Android.mk`, creates the aggregate shared library, links the actual
`air-transcode` executable against it, and verifies the ELF dependency. Set
`AIR_GSTREAMER_RESTRICTED=1` only after reviewing the additional GPL/patent
licensing obligations; it adds x264/x265, libav, DTS/AC-3, and related plugins.

## Tauri application integration

Package these files for each Android ABI:

- `libgstreamer_android.so` from `target/android-gstreamer/<abi>/libs/<abi>/`
- `libc++_shared.so` from the same directory
- the Tauri application library that depends on `air-transcode`

Copy GStreamer's generated Java sources and CA-certificate resource from the
aggregate build into the Android application, initialize GStreamer from the
application context before calling `air_transcode::spawn_tauri_host`, and keep
the returned host alive for the app lifecycle. The WebView uses the protected
loopback admin origin; Vizio receives only the tokenized read-only LAN media
URL.

The current CI gate cross-builds and links arm64/API 24. On-device MediaCodec,
background-lifecycle, network-security-config, and thermal tests still require
an Android device or emulator and remain a release qualification gate.
