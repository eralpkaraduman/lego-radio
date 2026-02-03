#!/bin/bash
# LEGO Radio Installer for Raspberry Pi
# Configures Audio Amp SHIM and installs lego-radio

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}╔══════════════════════════════════════╗${NC}"
echo -e "${GREEN}║     LEGO Radio Installer             ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════╝${NC}"
echo

# Check if running as root
if [ "$EUID" -ne 0 ]; then
    echo -e "${RED}Please run as root: sudo ./install.sh${NC}"
    exit 1
fi

# Detect architecture
ARCH=$(uname -m)
case $ARCH in
    aarch64)
        BINARY="lego-radio-arm64"
        ;;
    x86_64)
        BINARY="lego-radio-x86_64"
        ;;
    *)
        echo -e "${RED}Unsupported architecture: $ARCH${NC}"
        exit 1
        ;;
esac

echo -e "${YELLOW}[1/5]${NC} Configuring I2S audio for Audio Amp SHIM..."

# Determine config file location (Bookworm uses /boot/firmware/, older uses /boot/)
if [ -f /boot/firmware/config.txt ]; then
    CONFIG_FILE="/boot/firmware/config.txt"
else
    CONFIG_FILE="/boot/config.txt"
fi

# Backup config
cp "$CONFIG_FILE" "${CONFIG_FILE}.backup.$(date +%Y%m%d%H%M%S)"

# Add I2S DAC overlay if not present
if ! grep -q "^dtoverlay=hifiberry-dac" "$CONFIG_FILE"; then
    echo "dtoverlay=hifiberry-dac" >> "$CONFIG_FILE"
    echo "  Added: dtoverlay=hifiberry-dac"
fi

# Enable GPIO 25 for amp (Audio Amp SHIM specific)
if ! grep -q "^gpio=25=op,dh" "$CONFIG_FILE"; then
    echo "gpio=25=op,dh" >> "$CONFIG_FILE"
    echo "  Added: gpio=25=op,dh (enables amp)"
fi

# Disable onboard audio to avoid conflicts (comment it out if present)
if grep -q "^dtparam=audio=on" "$CONFIG_FILE"; then
    sed -i 's/^dtparam=audio=on/#dtparam=audio=on/' "$CONFIG_FILE"
    echo "  Disabled: onboard audio"
fi

echo -e "${GREEN}  ✓ Audio configured${NC}"

echo -e "${YELLOW}[2/5]${NC} Installing dependencies..."
apt-get update -qq
apt-get install -y -qq espeak-ng > /dev/null
echo -e "${GREEN}  ✓ espeak-ng installed${NC}"

echo -e "${YELLOW}[3/5]${NC} Downloading lego-radio..."
DOWNLOAD_URL="https://github.com/eralpkaraduman/lego-radio/releases/latest/download/${BINARY}"
curl -sL "$DOWNLOAD_URL" -o /usr/local/bin/lego-radio
chmod +x /usr/local/bin/lego-radio
echo -e "${GREEN}  ✓ Binary installed to /usr/local/bin/lego-radio${NC}"

echo -e "${YELLOW}[4/5]${NC} Creating config directory..."
mkdir -p /etc/lego-radio
# Create default config if not exists
if [ ! -f /etc/lego-radio/config.json ]; then
    echo '{"volume": 0.8}' > /etc/lego-radio/config.json
    echo "  Created default config (volume: 80%)"
fi
echo -e "${GREEN}  ✓ Config ready${NC}"

echo -e "${YELLOW}[5/5]${NC} Installing systemd service..."
/usr/local/bin/lego-radio --install
echo -e "${GREEN}  ✓ Service installed${NC}"

echo
echo -e "${GREEN}╔══════════════════════════════════════╗${NC}"
echo -e "${GREEN}║     Installation Complete!           ║${NC}"
echo -e "${GREEN}╚══════════════════════════════════════╝${NC}"
echo
echo -e "${YELLOW}IMPORTANT: You must reboot for audio to work!${NC}"
echo
echo "After reboot:"
echo "  • Press the button to cycle through radio channels"
echo "  • Check status: sudo systemctl status lego-radio"
echo "  • View logs:    sudo journalctl -u lego-radio -f"
echo "  • Set volume:   sudo lego-radio --set-volume 80"
echo "  • Update:       sudo lego-radio --update"
echo
read -p "Reboot now? [y/N] " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    reboot
fi
