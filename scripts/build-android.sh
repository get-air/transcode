#!/usr/bin/env bash
set -euo pipefail

: "${GSTREAMER_ROOT_ANDROID:?set GSTREAMER_ROOT_ANDROID to the unpacked universal GStreamer Android SDK}"
: "${ANDROID_NDK_HOME:?set ANDROID_NDK_HOME to an installed Android NDK}"

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
rust_target=${AIR_ANDROID_RUST_TARGET:-aarch64-linux-android}
api_level=${AIR_ANDROID_API_LEVEL:-24}

case "$rust_target" in
  aarch64-linux-android)
    android_abi=arm64-v8a
    sdk_arch=arm64
    clang_prefix=aarch64-linux-android
    ;;
  x86_64-linux-android)
    android_abi=x86_64
    sdk_arch=x86_64
    clang_prefix=x86_64-linux-android
    ;;
  armv7-linux-androideabi)
    android_abi=armeabi-v7a
    sdk_arch=armv7
    clang_prefix=armv7a-linux-androideabi
    ;;
  *)
    printf 'unsupported AIR_ANDROID_RUST_TARGET: %s\n' "$rust_target" >&2
    exit 2
    ;;
esac

gstreamer_root="$GSTREAMER_ROOT_ANDROID/$sdk_arch"
ndk_bin="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
linker="$ndk_bin/${clang_prefix}${api_level}-clang"
archiver="$ndk_bin/llvm-ar"
readelf="$ndk_bin/llvm-readelf"
build_root="$repository_root/target/android-gstreamer/$android_abi"
pkgconfig_root="$build_root/pkgconfig"
aggregate_library="$build_root/libs/$android_abi/libgstreamer_android.so"

for required in "$gstreamer_root/lib/pkgconfig/gstreamer-1.0.pc" "$linker" "$archiver"; do
  if [[ ! -e "$required" ]]; then
    printf 'required Android build input is missing: %s\n' "$required" >&2
    exit 2
  fi
done

# GStreamer 1.28.6 ships the Rust workspace archive without its libtool file,
# while isobmff's .la metadata still references it. Supply the deterministic
# static metadata shim only when the SDK has not fixed that packaging issue.
workspace_archive="$gstreamer_root/lib/libgstrsworkspace.a"
workspace_libtool="$gstreamer_root/lib/libgstrsworkspace.la"
if [[ -f "$workspace_archive" && ! -f "$workspace_libtool" ]]; then
  workspace_tmp="$workspace_libtool.tmp"
  {
    printf "dlname=''\n"
    printf "library_names=''\n"
    printf "old_library='libgstrsworkspace.a'\n"
    printf "dependency_libs=''\n"
    printf "installed=yes\n"
    printf "shouldnotlink=no\n"
    printf "dlopen=''\n"
    printf "dlpreopen=''\n"
    printf "libdir='%s/lib'\n" "$gstreamer_root"
  } > "$workspace_tmp"
  mv "$workspace_tmp" "$workspace_libtool"
fi

mkdir -p "$build_root" "$pkgconfig_root"
(
  cd "$build_root"
  "$ANDROID_NDK_HOME/ndk-build" \
    NDK_PROJECT_PATH="$build_root" \
    APP_BUILD_SCRIPT="$repository_root/android/Android.mk" \
    NDK_APPLICATION_MK="$repository_root/android/Application.mk" \
    APP_ABI="$android_abi" \
    APP_PLATFORM="android-$api_level" \
    GSTREAMER_ROOT="$gstreamer_root" \
    AIR_GSTREAMER_RESTRICTED="${AIR_GSTREAMER_RESTRICTED:-0}" \
    -j"${AIR_ANDROID_JOBS:-4}"
)

if [[ ! -f "$aggregate_library" ]]; then
  printf 'GStreamer Android aggregate was not produced: %s\n' "$aggregate_library" >&2
  exit 1
fi

glib_version=$(PKG_CONFIG_PATH="$gstreamer_root/lib/pkgconfig" pkg-config --modversion glib-2.0)
gstreamer_version=$(PKG_CONFIG_PATH="$gstreamer_root/lib/pkgconfig" pkg-config --modversion gstreamer-1.0)

write_pc() {
  local package_name=$1
  local package_version=$2
  local pc_path="$pkgconfig_root/$package_name.pc"
  {
    printf 'prefix=%s\n' "$gstreamer_root"
    printf 'includedir=${prefix}/include\n'
    printf 'libdir=%s\n' "$(dirname "$aggregate_library")"
    printf 'Name: %s Android aggregate\n' "$package_name"
    printf 'Description: Air link shim for libgstreamer_android.so\n'
    printf 'Version: %s\n' "$package_version"
    printf 'Libs: -L${libdir} -lgstreamer_android\n'
    printf 'Cflags: -I${includedir} -I${includedir}/glib-2.0 -I${prefix}/lib/glib-2.0/include -I${includedir}/gstreamer-1.0\n'
  } > "$pc_path"
}

for package_name in glib-2.0 gobject-2.0 gio-2.0; do
  write_pc "$package_name" "$glib_version"
done
for package_name in \
  gstreamer-1.0 gstreamer-base-1.0 gstreamer-audio-1.0 \
  gstreamer-video-1.0 gstreamer-app-1.0 gstreamer-pbutils-1.0; do
  write_pc "$package_name" "$gstreamer_version"
done

target_env=${rust_target//-/_}
target_env=${target_env^^}
compiler_env=${rust_target//-/_}

env \
  PKG_CONFIG_ALLOW_CROSS=1 \
  PKG_CONFIG_PATH="$pkgconfig_root" \
  "CARGO_TARGET_${target_env}_LINKER=$linker" \
  "CC_${compiler_env}=$linker" \
  "AR_${compiler_env}=$archiver" \
  CARGO_TARGET_DIR="$repository_root/target/android-rust" \
  cargo build --manifest-path "$repository_root/Cargo.toml" \
    --target "$rust_target" --bin air-transcode --release --locked

binary="$repository_root/target/android-rust/$rust_target/release/air-transcode"
if ! "$readelf" -d "$binary" | grep -q 'libgstreamer_android.so'; then
  printf 'Android binary is not linked to libgstreamer_android.so\n' >&2
  exit 1
fi

printf 'Android binary: %s\n' "$binary"
printf 'GStreamer aggregate: %s\n' "$aggregate_library"
