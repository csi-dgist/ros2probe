#!/bin/sh
set -e

REPO="csi-dgist/ros2probe"
BIN="rp"
INSTALL_DIR="/usr/local/bin"
GUI=0

for arg in "$@"; do
  case "$arg" in
    --gui) GUI=1 ;;
  esac
done

case "$(uname -m)" in
  x86_64)  BASE="rp-linux-x86_64" ;;
  aarch64) BASE="rp-linux-aarch64" ;;
  *)
    echo "Unsupported architecture: $(uname -m)"
    exit 1
    ;;
esac

if [ "$GUI" = "1" ]; then
  if [ "$(uname -m)" = "aarch64" ]; then
    echo "GUI build is not available for aarch64. Installing CLI version."
    ARTIFACT="$BASE"
  else
    ARTIFACT="${BASE}-gui"
  fi
else
  ARTIFACT="$BASE"
fi

URL="https://github.com/${REPO}/releases/latest/download/${ARTIFACT}"

echo "Downloading ${ARTIFACT}..."
curl -fsSL "$URL" -o "/tmp/${BIN}"
chmod +x "/tmp/${BIN}"

echo "Installing to ${INSTALL_DIR}/${BIN} (requires sudo)..."
sudo mv "/tmp/${BIN}" "${INSTALL_DIR}/${BIN}"

echo "Done. Run: rp --help"
