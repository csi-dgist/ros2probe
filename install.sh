#!/bin/sh
set -e

REPO="csi-dgist/ros2probe"
BIN="rp"
INSTALL_DIR="/usr/local/bin"

case "$(uname -m)" in
  x86_64)  ARTIFACT="rp-linux-x86_64" ;;
  aarch64) ARTIFACT="rp-linux-aarch64" ;;
  armv7l)  ARTIFACT="rp-linux-armv7" ;;
  *)
    echo "Unsupported architecture: $(uname -m)"
    exit 1
    ;;
esac

URL="https://github.com/${REPO}/releases/latest/download/${ARTIFACT}"

echo "Downloading ${ARTIFACT}..."
curl -fsSL "$URL" -o "/tmp/${BIN}"
chmod +x "/tmp/${BIN}"

echo "Installing to ${INSTALL_DIR}/${BIN} (requires sudo)..."
sudo mv "/tmp/${BIN}" "${INSTALL_DIR}/${BIN}"

echo "Done. Run: rp --help"
