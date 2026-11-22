#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT_DIR/target/debug/pa2pa"

if [ ! -x "$BIN" ]; then
  echo "Building pa2pa..."
  cargo build --manifest-path "$ROOT_DIR/Cargo.toml" --bin pa2pa
fi

TMP_DIR="$(mktemp -d -t pa2pa-demo-XXXXXX)"
RELAY_PORT=18787
FLO_HOME="$TMP_DIR/flo"
BEN_HOME="$TMP_DIR/benjamin"

cleanup() {
  if [ -n "${RELAY_PID:-}" ]; then
    kill "$RELAY_PID" 2>/dev/null || true
    wait "$RELAY_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

echo "==> 1. Starting local zero-knowledge E2E relay on 127.0.0.1:$RELAY_PORT"
"$BIN" relay --listen "127.0.0.1:$RELAY_PORT" --db "$TMP_DIR/relay.db" >/dev/null 2>&1 &
RELAY_PID=$!
sleep 0.7

echo "==> 2. Initializing Flo and Benjamin profiles (dedicated Ed25519 SSH keys)"
"$BIN" --home "$FLO_HOME" init --name flo --display-name "Flo" --relay "ws://127.0.0.1:$RELAY_PORT/ws" --dedicated-key --ui-bind 127.0.0.1:14789 >/dev/null
"$BIN" --home "$BEN_HOME" init --name benjamin --display-name "Benjamin (Lead)" --relay "ws://127.0.0.1:$RELAY_PORT/ws" --dedicated-key --ui-bind 127.0.0.1:14790 >/dev/null

FLO_INVITE="$("$BIN" --home "$FLO_HOME" invite)"
BEN_INVITE="$("$BIN" --home "$BEN_HOME" invite)"

echo "==> 3. Exchanging SSH invite URIs (mutual whitelist + owner policy prompts)"
"$BIN" --home "$FLO_HOME" contact add benjamin \
  --invite "$BEN_INVITE" \
  --trust ask \
  --prompt "Benjamin est un collègue de confiance. Réponds aux questions d'architecture, mais demande-moi validation avant toute écriture."

"$BIN" --home "$BEN_HOME" contact add flo \
  --invite "$FLO_INVITE" \
  --trust auto-read \
  --prompt "Flo est mon coéquipier. Tu peux répondre directement en lecture seule."

echo "==> 4. Benjamin's agent sends an E2E encrypted message to Flo's agent"
"$BIN" --home "$BEN_HOME" send flo "Salut agent de Flo ! Est-ce que la migration SQL v2 est prête pour la PR ?" --subject "Migration SQL v2"

echo "==> 5. Flo pulls & decrypts inbox (verified against Benjamin's SSH Ed25519 key)"
"$BIN" --home "$FLO_HOME" inbox

echo "==> Demo complete! (Temp directory cleaned up automatically)"
