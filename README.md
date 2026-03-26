# LEGO Radio

A LEGO-based internet radio with single button control, powered by Raspberry Pi.

## Features

- Browse-then-commit channel switching with TTS announcements
- Stream ducks to 20% while browsing, switches only on commit
- Per-language TTS voices (Finnish, Turkish, English) via Piper
- Instant beep feedback on press, confirm blip on release
- Auto-reconnect with exponential backoff
- Self-updating binary with SHA256 checksum verification
- Runs as a systemd service

## How It Works

**On power-on:**
1. "Hello!" → checks for updates → announces first channel → starts playing

**Short press:** Enter browse mode
- Stream keeps playing (ducked to 20%)
- TTS announces next channel
- Press again to advance, channels wrap around
- Stop pressing → TTS finishes + 0.5s → channel commits and switches

**Long press (2s):** Turn off
- "Radio off" → idle, waiting for next press

**From off:** Short press → full startup sequence → first channel

## Configuration

Everything is configured in `radio.toml`:

```toml
[[channels]]
name = "YLE Klassinen"
url = "https://icecast.live.yle.fi/radio/YleKlassinen/icecast.audio"
text = "YLE Klassinen"
voice = "fi_FI-asmo-medium"
file = "yle_classical"

[[ui]]
text = "Hello!"
voice = "en_GB-alan-medium"
file = "hello"
```

- **Channels:** name, stream URL, TTS text, Piper voice model, audio filename
- **UI phrases:** system announcements (hello, update status, radio off, etc.)
- Adding a channel = add a `[[channels]]` entry and rebuild

## Hardware

- Raspberry Pi 4B
- [Audio Amp SHIM](https://shop.pimoroni.com/products/audio-amp-shim-3w-mono-amp) (3W mono I2S amp)
- Momentary switch on GPIO 17 (LEGO channel dial actuates this)
- 4-8Ω speaker (3W or less)

### Wiring

```
GPIO 17 (pin 11) ──────┤ ├────── GND (pin 9)
                       Switch
```

## Installation

**Requires:** Raspberry Pi OS Lite (64-bit) - Bookworm or newer

```bash
curl -sL https://raw.githubusercontent.com/eralpkaraduman/lego-radio/main/install.sh | sudo bash
```

The installer configures I2S audio, installs dependencies, and sets up the service. Reboot required after install.

## Development

### Prerequisites

- [Docker](https://docker.com) (for building)
- [Rust](https://rustup.rs) (optional, for local dev)

### Build Release Binary

Single command builds everything (TTS audio generation + cross-compilation):

```bash
docker build --output=out .
# Binary: out/lego-radio-arm64
```

### Local Development (macOS)

```bash
# Generate TTS audio files (first time / after changing radio.toml)
docker build --target audio-gen -t lego-radio-audio .
docker run --rm -v ./audio:/output lego-radio-audio cp -r /audio/. /output/

# Run locally (opens a GUI button window for input)
cargo run

# Test TTS playback
cargo run -- --test-tts

# Test a stream
cargo run -- --test-stream
```

On macOS, a small GUI window appears with a red button for input:
- Click = short press (browse channels)
- Click and hold 2s = long press (turn off)

### Audio Architecture

Three independent audio sinks:
- **Stream sink:** radio playback, volume duckable during browse
- **Voice sink:** TTS announcements, interruptible
- **Beep sink:** button feedback sounds

Stream threads use an epoch counter to prevent stale audio after channel switches.

### Adding/Changing Channels

1. Edit `radio.toml` — add a `[[channels]]` entry
2. Rebuild: `docker build --output=out .`
3. Audio files are auto-generated from the `text` and `voice` fields

### Changing Voices

Per-phrase voice selection in `radio.toml`. Available voices:
- `en_GB-alan-medium` — English (default)
- `fi_FI-asmo-medium` — Finnish ([AsmoKoskinen/Piper_Finnish_Model](https://huggingface.co/AsmoKoskinen/Piper_Finnish_Model))
- `tr_TR-dfki-medium` — Turkish
- Browse more at [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices)

### Service Management

```bash
sudo systemctl status lego-radio
sudo journalctl -u lego-radio -f
sudo systemctl restart lego-radio
```

## License

MIT
