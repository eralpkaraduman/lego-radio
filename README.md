# LEGO Radio

A LEGO-based internet radio with single button control, powered by Raspberry Pi.

## Features

- Single button cycles through radio channels
- Text-to-speech announces each channel (Piper neural TTS)
- Auto-downloads TTS engine on first run
- Self-updating binary via GitHub releases
- Runs as a systemd service

## Hardware

### Requirements

- Raspberry Pi 4 (or 3B+)
- [Audio Amp SHIM](https://shop.pimoroni.com/products/audio-amp-shim-3w-mono-amp) (3W mono I2S amp)
- Momentary push button
- 4-8Ω speaker (3W or less)
- Power supply

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

## Raspberry Pi Setup

### Operating System

**Raspberry Pi OS Lite (64-bit)** - Bookworm or newer

- 64-bit is required for the TTS engine
- Desktop environment not needed

### Quick Install

```bash
# Download and run the installer (configures audio + installs everything)
curl -sL https://raw.githubusercontent.com/eralpkaraduman/lego-radio/main/install.sh | sudo bash
```

The installer will:
- Configure I2S audio for Audio Amp SHIM
- Install espeak-ng (required for TTS)
- Download lego-radio binary
- Set up systemd service
- Prompt for reboot (required for audio)

### Manual Installation

```bash
# Install espeak-ng (required for TTS)
sudo apt update && sudo apt install -y espeak-ng

# Download lego-radio
curl -L https://github.com/eralpkaraduman/lego-radio/releases/latest/download/lego-radio-arm64 \
  -o lego-radio
chmod +x lego-radio

# Test TTS (downloads piper + voice model ~165MB on first run)
./lego-radio --test-tts

# Test audio streaming
./lego-radio --test-stream

# Install as system service
sudo mv lego-radio /usr/local/bin/
sudo /usr/local/bin/lego-radio --install
```

### Audio Amp SHIM Setup

If not using the installer, manually configure I2S audio:

```bash
# Add to /boot/firmware/config.txt (or /boot/config.txt on older OS):
dtoverlay=hifiberry-dac
gpio=25=op,dh

# Comment out onboard audio:
#dtparam=audio=on

# Reboot required
sudo reboot
```

### Service Management

```bash
# Check status
sudo systemctl status lego-radio

# View logs
sudo journalctl -u lego-radio -f

# Restart
sudo systemctl restart lego-radio

# Stop
sudo systemctl stop lego-radio

# Uninstall
sudo /usr/local/bin/lego-radio --uninstall
```

## Usage

**Button controls:**
- Press 1: Channel 1 (YLE Classical)
- Press 2: Channel 2 (YLE Radio 1)
- Press 3: Channel 3 (Soma Groove Salad)
- Press 4: Channel 4 (Soma Drone Zone)
- Press 5: Radio off
- Press 6: Back to Channel 1

**Keyboard controls (for testing without GPIO):**
- Enter or Space: Cycle to next channel
- Ctrl+C: Exit

## CLI Options

```
lego-radio [OPTION]

Options:
  --version, -v     Print version
  --help, -h        Print help
  --install         Install as systemd service
  --uninstall       Remove systemd service
  --update          Download and install latest version
  --check           Check for updates
  --test-tts        Test text-to-speech
  --test-stream     Test audio streaming [URL]
  --set-volume N    Set volume (0-100)
  --get-volume      Show current volume
```

## Volume Control

Volume is stored in `/etc/lego-radio/config.json` and persists across restarts.

```bash
# Set volume to 50%
sudo lego-radio --set-volume 50
sudo systemctl restart lego-radio

# Check current volume
lego-radio --get-volume
```

## Updating

The radio checks for updates on startup. To manually update:

```bash
sudo /usr/local/bin/lego-radio --update
sudo systemctl restart lego-radio
```

## Development

### Requirements

- Docker Desktop (for cross-compilation)
- macOS or Linux host

### Build

```bash
# Build ARM64 release binary
./scripts/dev.sh build

# Test TTS
./scripts/test-tts.sh

# Open shell in dev container
./scripts/dev.sh shell
```

### Project Structure

```
src/
├── main.rs       # CLI and radio loop
├── audio.rs      # Audio streaming and TTS playback
├── tts.rs        # Piper TTS integration
├── channels.rs   # Radio channel definitions
├── button.rs     # GPIO and keyboard input
└── updater.rs    # Self-update from GitHub
```

## Channels

Edit `src/channels.rs` to change the radio stations:

```rust
pub const CHANNELS: &[Channel] = &[
    Channel {
        name: "YLE Klassinen",
        tts_name: "Y L E Classical",  // TTS-friendly name
        url: "https://icecast.live.yle.fi/radio/YleKlassinen/icecast.audio",
    },
    // Add more channels...
];
```

## License

MIT
