#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Baut die WASM-Plugins unter plugins/ (Ziel wasm32-wasip2, AIGW-63) und legt sie als
# Testdateien unter crates/aigw-wasm/tests/data/ ab (eingecheckt, damit `cargo test` ohne
# WASM-Ziel läuft). Danach: SHA-256 und Größe ausgeben.
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
