#!/bin/bash
# Development helper script for lego-radio

set -e

case "$1" in
    build)
        echo "Building in Docker..."
        docker compose run --rm dev cargo build --release
        ;;

    test)
        echo "Running tests in Docker..."
        docker compose run --rm dev cargo test
        ;;

    tts)
        echo "Testing TTS..."
        ./scripts/test-tts.sh
        ;;

    run)
        echo "Running lego-radio in Docker..."
        echo "Note: Audio will be saved to ./output/ - run 'afplay output/*.wav' to hear"
        docker compose run --rm dev cargo run -- "$@"
        ;;

    shell)
        echo "Opening shell in Docker dev environment..."
        docker compose run --rm dev bash
        ;;

    clean)
        echo "Cleaning Docker volumes..."
        docker compose down -v
        ;;

    *)
        echo "Usage: $0 {build|test|tts|run|shell|clean}"
        echo ""
        echo "Commands:"
        echo "  build  - Build release binary in Docker"
        echo "  test   - Run cargo tests"
        echo "  tts    - Test TTS and play on Mac"
        echo "  run    - Run the app (with optional args)"
        echo "  shell  - Open bash shell in container"
        echo "  clean  - Remove Docker volumes"
        exit 1
        ;;
esac
