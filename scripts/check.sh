#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Ein Einstieg für alle Qualitätsprüfungen (lokal mit Toolchain, auf der Buildbox, in CI):
#   Rust: fmt, clippy (-D warnings), Tests, cargo-deny
#   UI:   pnpm install, OpenAPI-Abgleich, TS-Typen, svelte-check, vite build
#   REUSE-Lint (falls `reuse` installiert ist)
#
# Umgebungsvariablen:
#   AIGW_SKIP_UI=1     UI-Schritte überspringen
#   AIGW_STRICT=1      fehlende Werkzeuge (cargo-deny, reuse) sind Fehler statt Warnung (CI)
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$(pwd)

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
missing() {
  if [[ "${AIGW_STRICT:-0}" == 1 ]]; then
    echo "FEHLER: $1 nicht installiert" >&2
    exit 1
  fi
  echo "WARNUNG: $1 nicht installiert – Schritt übersprungen" >&2
}

LOCKED=()
[[ -f Cargo.lock ]] && LOCKED=(--locked)

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy (-D warnings)"
cargo clippy --workspace --all-targets "${LOCKED[@]}" -- -D warnings

step "cargo test"
cargo test --workspace "${LOCKED[@]}"

step "cargo deny check"
if cargo deny --version >/dev/null 2>&1; then
  # Doppelte Versionen sind nur Warnungen; deren Abhängigkeitsbäume blähen die Ausgabe auf.
  cargo deny --all-features check --hide-inclusion-graph
else
  missing cargo-deny
fi

if [[ "${AIGW_SKIP_UI:-0}" != 1 ]]; then
  step "OpenAPI-Spezifikation aktuell? (ui/openapi.json)"
  mkdir -p target
  cargo run "${LOCKED[@]}" -q -p aigw -- openapi >target/openapi.json
  if ! cmp -s target/openapi.json ui/openapi.json; then
    diff -u ui/openapi.json target/openapi.json | head -40 || true
    echo "FEHLER: ui/openapi.json ist veraltet – scripts/gen-api.sh ausführen" >&2
    exit 1
  fi

  step "UI: pnpm install"
  cd "$ROOT/ui"
  if [[ -f pnpm-lock.yaml ]]; then
    pnpm install --frozen-lockfile
  else
    pnpm install
  fi

  step "UI: TS-Typen aus OpenAPI, svelte-check, build"
  pnpm run gen:api
  pnpm run check
  pnpm run build
  cd "$ROOT"
fi

step "REUSE-Lint"
if ! command -v reuse >/dev/null 2>&1; then
  missing reuse
elif git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  reuse lint
else
  # Ohne Git (z. B. `buildbox run --no-git`) kennt reuse die .gitignore nicht und würde
  # node_modules/ und target/ mitprüfen – daher eine saubere Kopie in einem Wegwerf-Repo prüfen.
  REUSE_TMP=$(mktemp -d)
  rsync -a --exclude target --exclude node_modules --exclude ui/dist --exclude '*.db*' ./ "$REUSE_TMP/"
  (cd "$REUSE_TMP" && rm -rf .git && git init -q && git add -A && reuse lint)
  rm -rf "$REUSE_TMP"
fi

step "Alle Prüfungen bestanden"
