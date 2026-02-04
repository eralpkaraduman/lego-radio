# LEGO Radio

A LEGO-based internet radio with single button control, powered by Raspberry Pi.

## Features

- Single button cycles through radio channels
- Text-to-speech announces each channel
- Self-updating binary
- Runs as a systemd service

## Hardware

- Raspberry Pi 4 (or 3B+)
- [Audio Amp SHIM](https://shop.pimoroni.com/products/audio-amp-shim-3w-mono-amp) (3W mono I2S amp)
- Momentary push button
- 4-8Ω speaker (3W or less)

### Wiring

```
Button wiring (uses internal pull-up resistor):

    GPIO 17 (pin 11) ──────┤ ├────── GND (pin 9)
                          Button

Pin layout:
    ┌─────────────────────────────┐
    │ (1)  (2)                    │
    │  ○    ○   ...               │
    │ (3)  (4)                    │
    │  ○    ○   ...               │
    │  ...                        │
    │ (9)  (10)                   │
    │ GND   ○   ...               │
    │(11) (12)                    │
    │GPIO17 ○   ...               │
    └─────────────────────────────┘
```

## Installation

**Requires:** Raspberry Pi OS Lite (64-bit) - Bookworm or newer

```bash
curl -sL https://raw.githubusercontent.com/eralpkaraduman/lego-radio/main/install.sh | sudo bash
```

The installer configures I2S audio, installs dependencies, and sets up the service. **Reboot required after install.**

## Usage

On power-on, the radio automatically:
1. Says "Hello! Checking for updates..."
2. Auto-updates if available
3. Starts playing channel 1

Then press the button to:
- Cycle through channels
- After last channel → radio off
- Press again → restarts from welcome sequence

## Commands

```bash
# Check status
sudo systemctl status lego-radio

# View logs
sudo journalctl -u lego-radio -f

# Restart
sudo systemctl restart lego-radio
```

## Development

### macOS Setup (Required)

On macOS, TTS requires Docker with a Piper container. **This must be set up before running the app.**

1. **Install Docker Desktop** from https://docker.com

2. **Build the Piper Docker image:**
   ```bash
   docker build -f Dockerfile.piper -t lego-radio-piper .
   ```

3. **Verify it works:**
   ```bash
   cargo run -- --test-tts
   ```

The voice model (~63MB) will be downloaded automatically on first run to `~/.local/share/lego-radio/`.

### Changing the Voice

Edit `src/tts.rs` to change the Piper voice:

```rust
// Voice Configuration
const VOICE_MODEL: &str = "en_US-joe-medium";
const VOICE_BASE_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/joe/medium";
```

Browse available voices at: https://huggingface.co/rhasspy/piper-voices/tree/main/en

After changing, delete the old model and restart:
```bash
rm ~/.local/share/lego-radio/*.onnx*
cargo run -- --test-tts
```

### Running Locally

```bash
# Run the radio (press Enter to cycle channels)
cargo run

# Test TTS only
cargo run -- --test-tts

# Test a single stream
cargo run -- --test-stream
```

### Editing Channels

Edit `src/channels.rs` to change radio stations. Bump version in `Cargo.toml` and push to trigger a new release.

### Cross-Compiling for Raspberry Pi

```bash
# Build (requires Docker)
./scripts/dev.sh build
```

## License

MIT
