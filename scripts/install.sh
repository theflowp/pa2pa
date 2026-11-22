#!/usr/bin/env bash
set -euo pipefail

# pa2pa one-line installer for macOS & Linux
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/theflowp/pa2pa/main/scripts/install.sh | bash
# Or from a local git clone:
#   ./scripts/install.sh

INSTALL_DIR="${PA2PA_INSTALL_DIR:-$HOME/.local/bin}"
REPO="${PA2PA_REPO:-theflowp/pa2pa}"
mkdir -p "$INSTALL_DIR"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64)        TARGET="x86_64-apple-darwin" ;;
      *) echo "Unsupported macOS arch: $ARCH"; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
      *)      TARGET="source" ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (use scripts/install.ps1 on Windows)"
    exit 1
    ;;
esac

install_from_release() {
  local repo="$1"
  local archive="pa2pa-${TARGET}.tar.gz"
  local base_url="https://github.com/${repo}/releases/latest/download"
  local url="${base_url}/${archive}"
  local sums_url="${base_url}/SHA256SUMS.txt"
  local tmp
  tmp="$(mktemp -d)"
  echo "==> Downloading prebuilt pa2pa (${TARGET}) from ${url}..."
  if curl -fsSL "$url" -o "$tmp/$archive"; then
    if curl -fsSL "$sums_url" -o "$tmp/SHA256SUMS.txt"; then
      local expected actual
      expected="$(grep " ${archive}\$" "$tmp/SHA256SUMS.txt" | awk '{print $1}' | head -n 1 || true)"
      if [ -n "$expected" ]; then
        if command -v sha256sum >/dev/null 2>&1; then
          actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
        elif command -v shasum >/dev/null 2>&1; then
          actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
        else
          actual=""
        fi
        if [ -n "$actual" ] && [ "$expected" != "$actual" ]; then
          echo "ERROR: SHA256 checksum mismatch for ${archive}!" >&2
          echo "  Expected: $expected" >&2
          echo "  Actual  : $actual" >&2
          rm -rf "$tmp"
          exit 1
        fi
        echo "==> Verified SHA256 checksum (${actual:0:12}...)"
      fi
    fi
    tar -xzf "$tmp/$archive" -C "$tmp"
    install -m 755 "$tmp/pa2pa" "$INSTALL_DIR/pa2pa"
    if [ -f "$tmp/pa2pa-pill" ]; then
      install -m 755 "$tmp/pa2pa-pill" "$INSTALL_DIR/pa2pa-pill"
    fi
    rm -rf "$tmp"
    return 0
  fi
  rm -rf "$tmp"
  return 1
}

build_from_source() {
  local src_dir="$1"
  if ! command -v cargo >/dev/null 2>&1; then
    echo "==> Rust toolchain not found; installing rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi

  if [ "$OS" = "Linux" ]; then
    echo "==> Building pa2pa from source (Linux)..."
    if pkg-config --exists webkit2gtk-4.1 2>/dev/null || pkg-config --exists webkit2gtk-4.0 2>/dev/null; then
      cargo build --manifest-path "$src_dir/Cargo.toml" --release --bin pa2pa --bin pa2pa-pill
      install -m 755 "$src_dir/target/release/pa2pa" "$INSTALL_DIR/pa2pa"
      install -m 755 "$src_dir/target/release/pa2pa-pill" "$INSTALL_DIR/pa2pa-pill"
    else
      echo "Note: WebKitGTK dev headers not found; installing headless `pa2pa` CLI/daemon/MCP/relay."
      echo "      To enable the native desktop tray + floating pill on Linux, install:"
      echo "      sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev"
      cargo build --manifest-path "$src_dir/Cargo.toml" --release --bin pa2pa
      install -m 755 "$src_dir/target/release/pa2pa" "$INSTALL_DIR/pa2pa"
    fi
  else
    echo "==> Building pa2pa + pa2pa-pill (macOS) from source..."
    cargo build --manifest-path "$src_dir/Cargo.toml" --release --bin pa2pa --bin pa2pa-pill
    install -m 755 "$src_dir/target/release/pa2pa" "$INSTALL_DIR/pa2pa"
    install -m 755 "$src_dir/target/release/pa2pa-pill" "$INSTALL_DIR/pa2pa-pill"
  fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)"
LOCAL_ROOT="$(cd "$SCRIPT_DIR/.." 2>/dev/null && pwd || true)"

INSTALLED=0
if [ -n "$REPO" ] && [ "$TARGET" != "source" ]; then
  if install_from_release "$REPO"; then
    INSTALLED=1
  fi
fi

if [ "$INSTALLED" -eq 0 ]; then
  if [ -f "${LOCAL_ROOT}/Cargo.toml" ] && grep -q 'pa2pa-cli' "${LOCAL_ROOT}/Cargo.toml" 2>/dev/null; then
    build_from_source "$LOCAL_ROOT"
  elif [ -n "$REPO" ]; then
    TMP_SRC="$(mktemp -d)"
    trap 'rm -rf "$TMP_SRC"' EXIT
    git clone --depth 1 "https://github.com/${REPO}.git" "$TMP_SRC/pa2pa"
    build_from_source "$TMP_SRC/pa2pa"
  else
    echo "Run this script from inside a cloned pa2pa repo, or set PA2PA_REPO=<owner/repo>."
    exit 1
  fi
fi

echo ""
echo "Installed pa2pa into: $INSTALL_DIR"
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  echo "Add this to your shell profile (~/.zshrc or ~/.bashrc):"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo ""
echo "Next steps:"
echo "  pa2pa init --name <your-name> --relay <wss://your-relay/ws>"
echo "  pa2pa setup-mcp --apply"
echo "  pa2pa service install"
