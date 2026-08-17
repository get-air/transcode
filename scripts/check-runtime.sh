#!/usr/bin/env bash
set -euo pipefail

required=(
  uridecodebin3
  souphttpsrc
  h264parse
  aacparse
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

if gst-inspect-1.0 hlscmafsink >/dev/null 2>&1; then
  printf '%-18s available (optional)\n' hlscmafsink
fi

printf '\nH.264 encoders\n'
gst-inspect-1.0 | rg '264.*enc|enc.*264' || true

printf '\nAAC encoders\n'
gst-inspect-1.0 | rg 'aac.*enc|enc.*aac' || true

exit "$missing"
