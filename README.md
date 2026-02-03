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

Press the button to cycle through channels. After the last channel, radio turns off. Press again to restart from channel 1.

## Commands

```bash
# Check status
sudo systemctl status lego-radio

# View logs
sudo journalctl -u lego-radio -f

# Restart
sudo systemctl restart lego-radio

# Update to latest version
sudo lego-radio --update
sudo systemctl restart lego-radio
```

## Development

Edit `src/channels.rs` to change radio stations. Bump version in `Cargo.toml` and push to trigger a new release.

```bash
# Build (requires Docker)
./scripts/dev.sh build
```

## License

MIT
