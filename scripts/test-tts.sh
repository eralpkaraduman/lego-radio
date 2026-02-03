#!/usr/bin/env bash
# Test TTS in Docker and play on Mac

set -e

OUTPUT_DIR="./output"
mkdir -p "$OUTPUT_DIR"

echo "Testing Piper TTS in Docker..."

PHRASES=(
    "Y L E Classical"
    "Y L E Radio 1"
    "Soma Groove Salad"
    "Soma Drone Zone"
    "Radio off"
)

for i in "${!PHRASES[@]}"; do
    phrase="${PHRASES[$i]}"
    echo "  Generating: $phrase"
    docker compose run --rm dev bash -c \
        "echo '$phrase' | /opt/piper/piper --model /opt/piper/voices/en_US-lessac-medium.onnx --output_file /output/tts_$i.wav" 2>/dev/null
done

echo ""
echo "Playing audio files on Mac..."

for i in "${!PHRASES[@]}"; do
    filename="$OUTPUT_DIR/tts_$i.wav"
    if [ -f "$filename" ]; then
        echo "  Playing: ${PHRASES[$i]}"
        afplay "$filename"
        sleep 0.3
    fi
done

echo ""
echo "Done!"
