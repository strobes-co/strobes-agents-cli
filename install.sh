#!/usr/bin/env bash
# Install the latest `strobes` CLI release for this platform.
#   curl -fsSL https://raw.githubusercontent.com/strobes-co/strobes-agents-cli/main/install.sh | bash
#
# Env overrides:
#   STROBES_INSTALL_DIR   install location (default: /usr/local/bin)
#   STROBES_VERSION       release tag to install (default: latest)
set -euo pipefail

REPO="strobes-co/strobes-agents-cli"
INSTALL_DIR="${STROBES_INSTALL_DIR:-/usr/local/bin}"
VERSION="${STROBES_VERSION:-latest}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Darwin-arm64)        target=aarch64-apple-darwin ;;
  Darwin-x86_64)       target=x86_64-apple-darwin ;;
  Linux-x86_64)        target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64)       target=aarch64-unknown-linux-gnu ;;
  *) echo "strobes: unsupported platform '$os-$arch'." >&2
     echo "Build from source instead: https://github.com/$REPO#build-from-source" >&2
     exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
  url="https://github.com/$REPO/releases/latest/download/strobes-$target.tar.gz"
else
  url="https://github.com/$REPO/releases/download/$VERSION/strobes-$target.tar.gz"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "↓ downloading strobes ($target, $VERSION)…"
curl -fsSL "$url" | tar -xz -C "$tmp"

bin="$tmp/strobes-$target/strobes"
[ -f "$bin" ] || { echo "strobes: binary not found in archive" >&2; exit 1; }
chmod +x "$bin"

if [ -w "$INSTALL_DIR" ]; then
  install -m755 "$bin" "$INSTALL_DIR/strobes"
else
  echo "→ installing to $INSTALL_DIR (needs sudo)…"
  sudo install -m755 "$bin" "$INSTALL_DIR/strobes"
fi

echo "✔ installed: $("$INSTALL_DIR/strobes" --help 2>/dev/null | head -1)"
echo "  run: strobes --help"
