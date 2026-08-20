#!/usr/bin/env bash
set -euo pipefail

required=(
  uridecodebin3
  h264parse
  h264timestamper
  aacparse
  audioconvert
  audioresample
  videoconvert
  videoscale
  videorate
  cmafmux
)

missing=0
for element in "${required[@]}"; do
  if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
    printf '%-18s available\n' "$element"
  else
    printf '%-18s MISSING\n' "$element"
    missing=1
  fi
done

if gst-inspect-1.0 souphttpsrc >/dev/null 2>&1 ||
  gst-inspect-1.0 reqwesthttpsrc >/dev/null 2>&1 ||
  gst-inspect-1.0 curlhttpsrc >/dev/null 2>&1; then
  printf '%-18s available\n' 'HTTP source'
else
  printf '%-18s MISSING\n' 'HTTP source'
  missing=1
fi

if gst-inspect-1.0 hlscmafsink >/dev/null 2>&1; then
  printf '%-18s available (optional)\n' hlscmafsink
fi

for element in h265parse h265timestamper av1parse; do
  if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
    printf '%-18s available (optional modern-codec transmux)\n' "$element"
  fi
done

for element in subparse ssaparse ttmlparse; do
  if gst-inspect-1.0 "$element" >/dev/null 2>&1; then
    printf '%-18s available (optional text-subtitle input)\n' "$element"
  fi
done

printf '\nH.264 encoders\n'
if ! gst-inspect-1.0 | rg '264.*enc|enc.*264'; then
  printf 'No H.264 encoder is installed\n'
  missing=1
fi

printf '\nAAC encoders\n'
if ! gst-inspect-1.0 | rg 'aac.*enc|enc.*aac'; then
  printf 'No AAC encoder is installed\n'
  missing=1
fi

exit "$missing"
