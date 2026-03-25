#!/bin/bash
# Play a raw PCM audio file (16-bit signed LE, 22050 Hz, mono)
# Usage: ./scripts/play_raw.sh audio/hello.raw
set -euo pipefail
FILE="${1:?Usage: $0 <file.raw>}"
ffplay -f s16le -ar 22050 -ch_layout mono -nodisp -autoexit "$FILE"
