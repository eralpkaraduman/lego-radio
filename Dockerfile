# Development Dockerfile for lego-radio
# Runs on ARM64 Linux (same as Raspberry Pi)
FROM --platform=linux/arm64 rust:1.83-bookworm

# Install dependencies
RUN apt-get update && apt-get install -y \
    espeak-ng \
    libespeak-ng-dev \
    libasound2-dev \
    pkg-config \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Download and install piper
RUN mkdir -p /opt/piper && \
    curl -sL "https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_aarch64.tar.gz" | \
    tar -xzf - -C /opt/piper --strip-components=1

# Download English voice model
RUN mkdir -p /opt/piper/voices && \
    curl -sL "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx" \
    -o /opt/piper/voices/en_US-lessac-medium.onnx && \
    curl -sL "https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/lessac/medium/en_US-lessac-medium.onnx.json" \
    -o /opt/piper/voices/en_US-lessac-medium.onnx.json

# Set up piper in PATH and library path
ENV PATH="/opt/piper:${PATH}"
ENV LD_LIBRARY_PATH="/opt/piper:${LD_LIBRARY_PATH}"

# Create working directory
WORKDIR /app

# Create output directory for audio files
RUN mkdir -p /output

# Default command
CMD ["bash"]
