#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Baut die WASM-Plugins unter plugins/ (Ziel wasm32-wasip2, AIGW-63) und legt sie als
# Testdateien unter crates/aigw-wasm/tests/data/ ab (eingecheckt, damit `cargo test` ohne
# WASM-Ziel läuft). Danach: SHA-256 und Größe ausgeben.
# Optional (AIGW-101): mit AIGW_WASM_SIGN_KEY (privater Ed25519-Schlüssel, PKCS#8) und
# AIGW_WASM_SIGN_KEY_ID je Modul eine Signatur `<modul>.wasm.sig` (Base64) daneben ablegen
# (`aigw wasm sign`; Doku 17-wasm-plugins.md §3a).
set -euo pipefail
cd "$(dirname "$0")/.."

rustup target add wasm32-wasip2 >/dev/null
OUT=crates/aigw-wasm/tests/data
mkdir -p "$OUT"
for dir in plugins/*/; do
  name=$(basename "$dir")
  (cd "$dir" && cargo build --release --target wasm32-wasip2 ${AIGW_LOCKED:+--locked})
  file="$dir/target/wasm32-wasip2/release/aigw_plugin_${name//-/_}.wasm"
  cp "$file" "$OUT/$name.wasm"
done
for f in "$OUT"/*.wasm; do
  printf '%s  %s  %s Byte\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$f" "$(stat -c %s "$f")"
done
if [[ -n "${AIGW_WASM_SIGN_KEY:-}" ]]; then
  : "${AIGW_WASM_SIGN_KEY_ID:?AIGW_WASM_SIGN_KEY_ID fehlt (Schlüssel-ID aus wasm.trusted_keys)}"
  for f in "$OUT"/*.wasm; do
    cargo run -q ${AIGW_LOCKED:+--locked} -p aigw -- wasm sign --key "$AIGW_WASM_SIGN_KEY" \
      --key-id "$AIGW_WASM_SIGN_KEY_ID" "$f" >"$f.sig"
    printf 'signiert: %s (%s)\n' "$f.sig" "$AIGW_WASM_SIGN_KEY_ID"
  done
fi
