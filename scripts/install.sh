#!/usr/bin/env bash
set -euo pipefail

# pa2pa one-line installer for macOS & Linux
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/theflowp/pa2pa/main/scripts/install.sh | bash
# Or from a local git clone:
#   ./scripts/install.sh --from-source
# Options:
#   --from-source          Build from the local checkout (or clone) instead of downloading a GitHub release
#   -h, --help             Show this help message
# Environment:
#   PA2PA_INSTALL_DIR      Destination bin directory (default: ~/.local/bin)
#   PA2PA_REPO             GitHub repo slug (default: theflowp/pa2pa)
#   PA2PA_FROM_SOURCE=1    Same as --from-source
#   PA2PA_SKIP_CHECKSUM=1  Skip SHA-256 verification (NOT recommended; prints a warning)

FROM_SOURCE="${PA2PA_FROM_SOURCE:-0}"
for arg in "$@"; do
  case "$arg" in
    --from-source)
      FROM_SOURCE=1
      ;;
    -h|--help)
      sed -n '4,16p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "Unknown option: $arg (use --help for usage)" >&2
      exit 1
      ;;
  esac
done

INSTALL_DIR="${PA2PA_INSTALL_DIR:-$HOME/.local/bin}"
REPO="${PA2PA_REPO:-theflowp/pa2pa}"
mkdir -p "$INSTALL_DIR"

OLD_VERSION=""
if [ -x "$INSTALL_DIR/pa2pa" ]; then
  OLD_VERSION="$("$INSTALL_DIR/pa2pa" --version 2>/dev/null | head -n 1 || true)"
fi

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin)
    case "$ARCH" in
      arm64|aarch64) TARGET="aarch64-apple-darwin" ;;
      x86_64)        TARGET="x86_64-apple-darwin" ;;
      *) echo "Unsupported macOS arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Linux)
    case "$ARCH" in
      x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
      *)      TARGET="source" ;;
    esac
    ;;
  *)
    echo "Unsupported OS: $OS (use scripts/install.ps1 on Windows)" >&2
    exit 1
    ;;
esac

verify_sha256() {
  local archive_path="$1"
  local archive_name="$2"
  local sums_url="$3"
  local tmp_dir="$4"

  if [ "${PA2PA_SKIP_CHECKSUM:-0}" = "1" ]; then
    echo "WARNING: PA2PA_SKIP_CHECKSUM=1 set — skipping SHA-256 verification for ${archive_name}!" >&2
    return 0
  fi

  if ! curl -fsSL "$sums_url" -o "$tmp_dir/SHA256SUMS.txt"; then
    echo "ERROR: Could not download ${sums_url} to verify ${archive_name}." >&2
    echo "       Refusing to install an unverified binary (set PA2PA_SKIP_CHECKSUM=1 to override)." >&2
    return 1
  fi

  local expected actual
  expected="$(grep " ${archive_name}\$" "$tmp_dir/SHA256SUMS.txt" | awk '{print $1}' | head -n 1 || true)"
  if [ -z "$expected" ]; then
    echo "ERROR: ${archive_name} is missing from SHA256SUMS.txt." >&2
    echo "       Refusing to install an unverified binary (set PA2PA_SKIP_CHECKSUM=1 to override)." >&2
    return 1
  fi

  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive_path" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$archive_path" | awk '{print $1}')"
  else
    echo "ERROR: Neither 'sha256sum' nor 'shasum' is available to verify ${archive_name}." >&2
    echo "       Install coreutils/shasum or set PA2PA_SKIP_CHECKSUM=1 to override." >&2
    return 1
  fi

  if [ -z "$actual" ] || [ "$expected" != "$actual" ]; then
    echo "ERROR: SHA256 checksum mismatch for ${archive_name}!" >&2
    echo "  Expected: $expected" >&2
    echo "  Actual  : ${actual:-<empty>}" >&2
    return 1
  fi

  echo "==> Verified SHA256 checksum (${actual:0:12}...)"
  return 0
}

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
    if ! verify_sha256 "$tmp/$archive" "$archive" "$sums_url" "$tmp"; then
      rm -rf "$tmp"
      exit 1
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
    echo "==> Building pa2pa from source (Linux) in ${src_dir}..."
    if pkg-config --exists webkit2gtk-4.1 2>/dev/null || pkg-config --exists webkit2gtk-4.0 2>/dev/null; then
      cargo build --manifest-path "$src_dir/Cargo.toml" --release --bin pa2pa --bin pa2pa-pill
      install -m 755 "$src_dir/target/release/pa2pa" "$INSTALL_DIR/pa2pa"
      install -m 755 "$src_dir/target/release/pa2pa-pill" "$INSTALL_DIR/pa2pa-pill"
    else
      echo "Note: WebKitGTK dev headers not found; installing headless 'pa2pa' CLI/daemon/MCP/relay."
      echo "      To enable the native desktop tray + floating pill on Linux, install:"
      echo "      sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev libxdo-dev"
      cargo build --manifest-path "$src_dir/Cargo.toml" --release --bin pa2pa
      install -m 755 "$src_dir/target/release/pa2pa" "$INSTALL_DIR/pa2pa"
    fi
  else
    echo "==> Building pa2pa + pa2pa-pill (macOS) from source in ${src_dir}..."
    cargo build --manifest-path "$src_dir/Cargo.toml" --release --bin pa2pa --bin pa2pa-pill
    install -m 755 "$src_dir/target/release/pa2pa" "$INSTALL_DIR/pa2pa"
    install -m 755 "$src_dir/target/release/pa2pa-pill" "$INSTALL_DIR/pa2pa-pill"
  fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" 2>/dev/null && pwd || true)"
LOCAL_ROOT="$(cd "$SCRIPT_DIR/.." 2>/dev/null && pwd || true)"
IN_LOCAL_CLONE=0
if [ -f "${LOCAL_ROOT}/Cargo.toml" ] && grep -q 'pa2pa-cli' "${LOCAL_ROOT}/Cargo.toml" 2>/dev/null; then
  IN_LOCAL_CLONE=1
fi

INSTALLED=0
if [ "$FROM_SOURCE" != "1" ] && [ -n "$REPO" ] && [ "$TARGET" != "source" ]; then
  if [ "$IN_LOCAL_CLONE" -eq 1 ]; then
    echo "Note: Downloading latest GitHub release (pass --from-source to build from this local checkout instead)."
  fi
  if install_from_release "$REPO"; then
    INSTALLED=1
  fi
fi

if [ "$INSTALLED" -eq 0 ]; then
  if [ "$IN_LOCAL_CLONE" -eq 1 ]; then
    build_from_source "$LOCAL_ROOT"
  elif [ -n "$REPO" ]; then
    TMP_SRC="$(mktemp -d)"
    trap 'rm -rf "$TMP_SRC"' EXIT
    git clone --depth 1 "https://github.com/${REPO}.git" "$TMP_SRC/pa2pa"
    build_from_source "$TMP_SRC/pa2pa"
  else
    echo "Run this script from inside a cloned pa2pa repo, or set PA2PA_REPO=<owner/repo>." >&2
    exit 1
  fi
fi

NEW_VERSION="$("$INSTALL_DIR/pa2pa" --version 2>/dev/null | head -n 1 || echo "pa2pa")"

echo ""
if [ -n "$OLD_VERSION" ]; then
  if [ "$OLD_VERSION" != "$NEW_VERSION" ]; then
    echo "Upgraded: ${OLD_VERSION} -> ${NEW_VERSION} (in ${INSTALL_DIR})"
  else
    echo "Reinstalled: ${NEW_VERSION} (in ${INSTALL_DIR})"
  fi
else
  echo "Installed: ${NEW_VERSION} (in ${INSTALL_DIR})"
fi

# Restart installed background services automatically so upgrades take effect immediately.
if [ "$OS" = "Darwin" ]; then
  if [ -f "$HOME/Library/LaunchAgents/dev.pa2pa.daemon.plist" ] || [ -f "$HOME/Library/LaunchAgents/dev.pa2pa.pill.plist" ]; then
    echo "==> Existing macOS LaunchAgent(s) detected; restarting to pick up ${NEW_VERSION}..."
    if ! "$INSTALL_DIR/pa2pa" service restart; then
      echo "Warning: Could not restart LaunchAgents automatically; run 'pa2pa service restart'." >&2
    fi
  fi
elif [ "$OS" = "Linux" ]; then
  if command -v systemctl >/dev/null 2>&1; then
    if systemctl --user is-active --quiet pa2pa-daemon.service 2>/dev/null || systemctl --user is-active --quiet pa2pa-pill.service 2>/dev/null; then
      echo "==> Existing systemd user service(s) detected; restarting to pick up ${NEW_VERSION}..."
      if ! "$INSTALL_DIR/pa2pa" service restart; then
        echo "Warning: Could not restart systemd services automatically; run 'pa2pa service restart'." >&2
      fi
    fi
  fi
fi

if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
  echo ""
  echo "Add this to your shell profile (~/.zshrc or ~/.bashrc):"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo ""
echo "Next steps:"
echo "  pa2pa init --name <your-name>            # zero-server P2P by default (add --relay wss://... optionally)"
echo "  pa2pa setup-mcp --apply                  # register pa2pa in detected AI agents"
echo "  pa2pa service install                    # start daemon + menu bar/tray icon + floating pill"
echo "  pa2pa doctor                             # verify identity, MCP registrations, and services"
