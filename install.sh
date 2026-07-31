#!/usr/bin/env bash
# Install the latest `strobes` CLI release for this platform.
#   curl -fsSL https://raw.githubusercontent.com/strobes-co/strobes-agents-cli/main/install.sh | bash
#
# Env overrides:
#   STROBES_INSTALL_DIR   install location (default: /usr/local/bin)
#   STROBES_VERSION       release tag to install (default: latest)
#   STROBES_SKIP_PACK     set to 1 to skip the sandbox pack (binary only)
#   STROBES_PACK_URL      base URL to fetch the pack from (default: bridge release)
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

# The sandbox pack — bundled scanners (nmap/nuclei/httpx/…) and a standalone
# Python with the agent packages baked in. Without it the agent runs on whatever
# happens to be on your PATH, which on a fresh machine is close to nothing.
#
# Best-effort and non-fatal: a failed or skipped pack must still leave you with a
# working CLI, and the agent degrades to host tools exactly as it did before packs
# existed. `strobes pack --install` retries later.
if [ "${STROBES_SKIP_PACK:-0}" = "1" ]; then
  echo "→ skipping sandbox pack (STROBES_SKIP_PACK=1)"
elif "$INSTALL_DIR/strobes" pack 2>/dev/null | grep -q "^pack        /"; then
  echo "✔ sandbox pack already present"
else
  echo "→ installing sandbox pack (scanners + bundled python)…"
  if ! "$INSTALL_DIR/strobes" pack --install; then
    echo "⚠ sandbox pack not installed — the agent will use this host's tools."
    echo "  retry any time with: strobes pack --install"
  fi
fi

echo "  run: strobes --help"
