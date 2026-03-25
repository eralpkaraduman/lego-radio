#!/bin/bash
# Generate TTS audio files from radio.toml using Piper
# Usage: ./generate_audio.sh <piper_binary> <models_dir> <config_toml>
#
# Reads [[channels]] and [[ui]] entries from TOML.
# Downloads voice models as needed.
# Produces raw PCM files (16-bit signed LE, 22050 Hz, mono).

set -euo pipefail

PIPER_BIN="${1:?Usage: $0 <piper_binary> <models_dir> <config_toml>}"
MODELS_DIR="${2:?Usage: $0 <piper_binary> <models_dir> <config_toml>}"
CONFIG_TOML="${3:?Usage: $0 <piper_binary> <models_dir> <config_toml>}"

VOICE_BASE="https://huggingface.co/rhasspy/piper-voices/resolve/main"

# Parse output_dir from TOML
OUTPUT_DIR=$(grep '^output_dir' "$CONFIG_TOML" | head -1 | sed 's/.*= *"\(.*\)"/\1/')
mkdir -p "$OUTPUT_DIR"

# Count total entries (both [[channels]] and [[ui]])
TOTAL=$(grep -c '^\[\[channels\]\]\|^\[\[ui\]\]' "$CONFIG_TOML")

echo "Generating $TOTAL TTS audio files from $(basename "$CONFIG_TOML")..."

# Track downloaded voices
DOWNLOADED_VOICES=""

voice_url() {
    local voice="$1"
    # Custom voice repos (not in rhasspy/piper-voices)
    case "$voice" in
        fi_FI-asmo-medium)
            echo "https://huggingface.co/AsmoKoskinen/Piper_Finnish_Model/resolve/main/${voice}.onnx"
            return ;;
    esac

    # Standard piper-voices URL: en_GB-alan-medium -> en/en_GB/alan/medium
    local lang="${voice%%_*}"
    local rest="${voice#*_}"
    local region="${lang}_${rest%%-*}"
    rest="${rest#*-}"
    local speaker="${rest%%-*}"
    local quality="${rest#*-}"
    echo "${VOICE_BASE}/${lang}/${region}/${speaker}/${quality}/${voice}.onnx"
}

ensure_voice() {
    local voice="$1"
    local model_path="$MODELS_DIR/${voice}.onnx"

    case "$DOWNLOADED_VOICES" in
        *"$voice"*) return ;;
    esac

    if [ ! -f "$model_path" ]; then
        local url
        url=$(voice_url "$voice")
        echo "  Downloading voice: $voice"
        curl -sL "$url" -o "$model_path"
        curl -sL "${url}.json" -o "${model_path}.json"
    fi

    DOWNLOADED_VOICES="$DOWNLOADED_VOICES $voice"
}

# Current entry fields
TEXT=""
VOICE=""
FILE=""

process_entry() {
    [ -z "$TEXT" ] || [ -z "$VOICE" ] || [ -z "$FILE" ] && return 0

    local filepath="$OUTPUT_DIR/${FILE}.raw"

    if [ -f "$filepath" ] && [ -s "$filepath" ]; then
        echo "  Skip (exists): $filepath"
        return 0
    fi

    ensure_voice "$VOICE"

    echo "  Generating: $TEXT -> $filepath"
    echo "$TEXT" | "$PIPER_BIN" \
        --model "$MODELS_DIR/${VOICE}.onnx" \
        --output-raw \
        > "$filepath" 2>/dev/null

    if [ ! -s "$filepath" ]; then
        echo "  ERROR: Failed to generate $filepath"
        rm -f "$filepath"
        exit 1
    fi
}

while IFS= read -r line; do
    line=$(echo "$line" | sed 's/#.*//' | sed 's/^[[:space:]]*//' | sed 's/[[:space:]]*$//')
    [ -z "$line" ] && continue

    # New entry marker
    if [ "$line" = "[[channels]]" ] || [ "$line" = "[[ui]]" ]; then
        process_entry
        TEXT=""
        VOICE=""
        FILE=""
        continue
    fi

    case "$line" in
        text\ =*)   TEXT=$(echo "$line" | sed 's/^text *= *"\(.*\)"/\1/') ;;
        voice\ =*)  VOICE=$(echo "$line" | sed 's/^voice *= *"\(.*\)"/\1/') ;;
        file\ =*)   FILE=$(echo "$line" | sed 's/^file *= *"\(.*\)"/\1/') ;;
    esac
done < "$CONFIG_TOML"

# Process last entry
process_entry

echo "Done."
