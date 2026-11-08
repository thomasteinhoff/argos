#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

install_runtime_deps() {
    if command -v dnf >/dev/null 2>&1; then
        echo "Checking runtime dependencies (dnf)..."
        sudo dnf install -y --needed \
            pipewire \
            pipewire-utils \
            pipewire-gstreamer \
            gstreamer1-plugins-base \
            xdg-desktop-portal \
            pulseaudio-libs \
            gcc \
            gcc-c++
    elif command -v apt-get >/dev/null 2>&1; then
        echo "Checking runtime dependencies (apt)..."
        sudo apt-get update -y
        sudo apt-get install -y \
            pipewire \
            pipewire-bin \
            gstreamer1.0-pipewire \
            gstreamer1.0-plugins-base \
            gstreamer1.0-tools \
            xdg-desktop-portal \
            libpulse0 \
            build-essential
    elif command -v pacman >/dev/null 2>&1; then
        echo "Checking runtime dependencies (pacman)..."
        sudo pacman -S --needed --noconfirm \
            pipewire \
            gst-plugin-pipewire \
            gst-plugins-base \
            gstreamer \
            xdg-desktop-portal \
            libpulse \
            base-devel
    elif command -v zypper >/dev/null 2>&1; then
        echo "Checking runtime dependencies (zypper)..."
        sudo zypper --non-interactive install \
            pipewire \
            pipewire-tools \
            gstreamer-plugin-pipewire \
            gstreamer-plugins-base \
            xdg-desktop-portal \
            libpulse0 \
            gcc \
            gcc-c++
    fi
}

if [[ "${1:-}" != "--skip-deps" ]]; then
    install_runtime_deps
fi

cd "${REPO_ROOT}"
echo "Building argos-app (release)..."
cargo build --release -p argos-app

BIN_DIR="${HOME}/.local/bin"
ICON_DIR="${HOME}/.local/share/icons/hicolor/256x256/apps"
APP_DIR="${HOME}/.local/share/applications"

mkdir -p "${BIN_DIR}" "${ICON_DIR}" "${APP_DIR}"
install -m 0755 "${REPO_ROOT}/target/release/argos-app" "${BIN_DIR}/argos-app"
install -m 0644 "${REPO_ROOT}/crates/argos-app/assets/icon.png" "${ICON_DIR}/argos.png"
sed "s|^Exec=.*|Exec=${BIN_DIR}/argos-app|" \
    "${REPO_ROOT}/crates/argos-app/assets/argos.desktop" > "${APP_DIR}/argos.desktop"

echo "Installed argos-app to ${BIN_DIR}/argos-app"
