# Soundboard

A simple soundboard script that plays random sound effects when you press a button on an 8BitDo Micro controller.

## Features

- Plays random MP3 files from the `sounds/` directory
- Works with 8BitDo Micro controller in keyboard mode (K mode)
- Plays sounds alongside Mopidy without interruption
- Auto-reconnects when controller disconnects/reconnects
- Can run as a systemd user service

## Requirements

- Raspberry Pi 4 with RaspberryOS or M1 macOS
- Python 3
- 8BitDo Micro controller (Bluetooth)
- `mpv` player: `sudo apt-get install mpv`
- HifiBerry DAC (or compatible audio device)

## Setup

### 1. Configure Audio

Mopidy needs to be configured to use dmix for parallel audio playback:

Edit `~/.config/mopidy/mopidy.conf`:
```ini
[audio]
output = alsasink device=dmix:CARD=sndrpihifiberry,DEV=0
```

Restart mopidy:
```bash
systemctl --user restart mopidy
```

### 2. Pair Controller

Put 8BitDo Micro in **K mode** (keyboard mode):
- Hold **B + Start** while turning on the controller

Pair via Bluetooth:
```bash
sudo rfkill unblock bluetooth
bluetoothctl
> power on
> agent on
> default-agent
> scan on
```

Put controller in pairing mode:
- Hold **Star + Y** for 3 seconds

In bluetoothctl:
```
> pair <MAC_ADDRESS>
> trust <MAC_ADDRESS>
> connect <MAC_ADDRESS>
> exit
```

### 3. Add User to Input Group

```bash
sudo usermod -a -G input $USER
```

**Log out and back in** for this to take effect.

### 4. Add Sound Files

Place MP3 files in the `sounds/` directory:
```bash
cp /path/to/your/sound.mp3 sounds/
```

## Usage

### Manual Mode

Run the soundboard directly:
```bash
./soundboard
```

Press the **A button** on the controller to play a random sound.
Press **Ctrl+C** to exit.

### Service Mode (Autostart)

Install as a systemd user service:

```bash
# Copy service file
mkdir -p ~/.config/systemd/user
cp soundboard.service ~/.config/systemd/user/

# Enable and start service
systemctl --user daemon-reload
systemctl --user enable soundboard
systemctl --user start soundboard
```

#### Service Commands

**Check status:**
```bash
systemctl --user status soundboard
```

**View logs:**
```bash
journalctl --user -u soundboard -f
```

**Stop service:**
```bash
systemctl --user stop soundboard
```

**Restart service:**
```bash
systemctl --user restart soundboard
```

**Disable autostart:**
```bash
systemctl --user disable soundboard
```

## Configuration

### Change Volume

Edit the `soundboard` script and modify the `--volume` parameter (0-100):
```python
cmd = ["mpv", "--no-video", "--audio-device=" + AUDIO_DEVICE, "--volume=80", "--really-quiet", sound_file]
```

### Change Controller Button

Edit the `KEY_G` constant to use a different button. Use `evtest` to find key codes:
```bash
sudo evtest /dev/input/event4
```

## Troubleshooting

### No sound playing

- Check mopidy is using dmix: `grep output ~/.config/mopidy/mopidy.conf`
- Test mpv directly: `mpv --audio-device=alsa/dmix:CARD=sndrpihifiberry,DEV=0 sounds/your-file.mp3`

### Controller not detected

- Check Bluetooth connection: `bluetoothctl info <MAC_ADDRESS>`
- Verify event device: `cat /proc/bus/input/devices | grep -A 5 "8BitDo"`
- Check permissions: `groups` (should include "input")

### Service not starting

- Check logs: `journalctl --user -u soundboard -n 50`
- Verify script is executable: `chmod +x soundboard`
- Check paths in service file match your installation

## Technical Details

- Uses Python's built-in libraries (no pip dependencies)
- Reads raw input events from `/dev/input/event4`
- Plays audio via `mpv` using ALSA's dmix plugin
- Triggers on button release to prevent multiple sounds
- Auto-reconnects when controller disconnects
