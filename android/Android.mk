LOCAL_PATH := $(call my-dir)

ifndef GSTREAMER_ROOT
$(error GSTREAMER_ROOT must point to one architecture from the GStreamer Android SDK)
endif

GSTREAMER_PLUGINS := \
  coreelements app audioconvert audioresample autodetect typefindfunctions \
  videoconvertscale playback uriplaylistbin \
  subparse ogg vorbis opus audioparsers avi flac flv id3demux isomp4 jpeg \
  matroska mpg123 multipart png speex vpx wavparse openh264 opusparse \
  videoparsersbad openjpeg spandsp sbc androidmedia dav1d ffv1 isobmff \
  soup reqwest encoding

ifeq ($(AIR_GSTREAMER_RESTRICTED),1)
GSTREAMER_PLUGINS += dtsdec a52dec x264 x265 mpegtsdemux mpegtsmux voaacenc libav
endif

G_IO_MODULES := openssl
GSTREAMER_EXTRA_DEPS := gstreamer-app-1.0 gstreamer-pbutils-1.0
GSTREAMER_EXTRA_LIBS := $(GSTREAMER_ROOT)/lib/libssl.a $(GSTREAMER_ROOT)/lib/libcrypto.a
GSTREAMER_INCLUDE_FONTS := no

include $(GSTREAMER_ROOT)/share/gst-android/ndk-build/gstreamer-1.0.mk
