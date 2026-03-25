# lego-radio build
#
# Single Dockerfile: generates TTS audio with Piper, then cross-compiles the Rust binary.
#
# Usage:
#   docker build --output=out .                                # arm64 (default)
#   docker build --build-arg TARGET=x86_64 --output=out .      # x86_64
#   Binary lands in ./out/lego-radio-<arch>

ARG TARGET=arm64

# =============================================================================
# Stage 1: Generate TTS audio with Piper
# =============================================================================
FROM debian:bookworm-slim AS audio-gen

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install Piper - pick the right binary for the build platform
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "arm64" ]; then \
        PIPER_URL="https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_aarch64.tar.gz"; \
    else \
        PIPER_URL="https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz"; \
    fi && \
    curl -sL "$PIPER_URL" | tar xz -C /opt && \
    chmod +x /opt/piper/piper

ENV LD_LIBRARY_PATH=/opt/piper

# Copy generation script (changes rarely)
COPY scripts/generate_audio.sh /opt/generate_audio.sh
RUN chmod +x /opt/generate_audio.sh

# Copy phrase manifest (layer cache: regenerates only when this file changes)
COPY radio.toml /opt/radio.toml

# Generate all TTS audio (voice models downloaded on demand)
RUN /opt/generate_audio.sh /opt/piper/piper /opt/piper /opt/radio.toml

# =============================================================================
# Stage 2: Cross-compile Rust binary
# =============================================================================
FROM rust:1-bookworm AS builder

ARG TARGET

# Install cross-compilation dependencies
RUN if [ "$TARGET" = "arm64" ]; then \
        dpkg --add-architecture arm64 && \
        apt-get update && \
        apt-get install -y gcc-aarch64-linux-gnu libasound2-dev:arm64 && \
        rustup target add aarch64-unknown-linux-gnu; \
    else \
        apt-get update && \
        apt-get install -y libasound2-dev; \
    fi && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy manifests and build script first for dependency caching
COPY Cargo.toml ./
COPY Cargo.lock* ./
COPY build.rs ./
COPY radio.toml ./

# Dummy build to cache dependencies
RUN mkdir src && \
    echo 'fn main() {}' > src/main.rs && \
    if [ "$TARGET" = "arm64" ]; then \
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
        export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig && \
        export PKG_CONFIG_SYSROOT_DIR=/ && \
        cargo build --release --target aarch64-unknown-linux-gnu 2>/dev/null || true; \
    else \
        cargo build --release --target x86_64-unknown-linux-gnu 2>/dev/null || true; \
    fi

# Copy source and generated audio from stage 1
COPY src/ src/
COPY --from=audio-gen /audio/ audio/

# Build the real binary
RUN if [ "$TARGET" = "arm64" ]; then \
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc && \
        export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig && \
        export PKG_CONFIG_SYSROOT_DIR=/ && \
        touch src/main.rs && \
        cargo build --release --target aarch64-unknown-linux-gnu && \
        cp target/aarch64-unknown-linux-gnu/release/lego-radio /lego-radio-arm64; \
    else \
        touch src/main.rs && \
        cargo build --release --target x86_64-unknown-linux-gnu && \
        cp target/x86_64-unknown-linux-gnu/release/lego-radio /lego-radio-x86_64; \
    fi

# =============================================================================
# Stage 3: Output
# =============================================================================
FROM scratch AS output
COPY --from=builder /lego-radio-* /
