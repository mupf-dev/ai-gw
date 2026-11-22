#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Ein Einstieg für alle Qualitätsprüfungen (lokal mit Toolchain, auf der Buildbox, in CI):
#   Rust: fmt, clippy (-D warnings), Tests, cargo-deny
#   UI:   pnpm install, OpenAPI-Abgleich, TS-Typen, svelte-check, vite build
#   REUSE-Lint (fehlt `reuse`, wird es in ein venv unter ${XDG_CACHE_HOME:-~/.cache}/aigw-reuse-venv installiert)
#
# Umgebungsvariablen:
#   AIGW_SKIP_UI=1     UI-Schritte überspringen
#   AIGW_STRICT=1      fehlende Werkzeuge (cargo-deny, reuse) sind Fehler statt Warnung (CI)
#   AIGW_TEST_POSTGRES_URL=postgres://…   Store-Tests zusätzlich gegen Postgres (legt je Lauf
#                      eine Wegwerf-Datenbank an; der Benutzer braucht CREATEDB)
#   AIGW_TEST_POSTGRES=docker   startet dafür selbst einen Postgres-17-Container mit pgvector
#                      (Image AIGW_TEST_POSTGRES_IMAGE, Standard pgvector/pgvector:0.8.7-pg17) und setzt
#                      zusätzlich AIGW_TEST_PGVECTOR_URL (semantischer Cache, AIGW-40)
#   AIGW_TEST_PGVECTOR_URL=postgres://…   Cache-Tests gegen Postgres mit Erweiterung `vector`
#   AIGW_TEST_REDIS_URL=redis://…   Zähler-Tests (aigw-counters) zusätzlich gegen Redis/Valkey
#   AIGW_TEST_REDIS=docker      startet dafür selbst einen Redis-Stack-Container (Image
#                      AIGW_TEST_REDIS_IMAGE, Standard redis/redis-stack-server:7.4.0-v8) und setzt
#                      zusätzlich AIGW_TEST_REDIS_STACK_URL (Vektorsuche, AIGW-40)
#   AIGW_TEST_REDIS_STACK_URL=redis://…   Cache-Tests gegen Redis Stack / Valkey Search
#   AIGW_TEST_OTELCOL=docker    Gegentest des OTLP-Exports gegen den offiziellen OpenTelemetry
#                      Collector (Image AIGW_TEST_OTELCOL_IMAGE, Standard
#                      otel/opentelemetry-collector-contrib:0.165.0, file-Exporter); setzt
#                      AIGW_TEST_OTELCOL_URL und AIGW_TEST_OTELCOL_DIR (Test aigw-observe/tests/otelcol.rs)
#   AIGW_TEST_OTELCOL_URL=http://…, AIGW_TEST_OTELCOL_DIR=…   eigener Collector (OTLP/HTTP) mit
#                      file-Exportern nach $DIR/traces.json und $DIR/logs.json
#   AIGW_TOKENIZER_RANKS=fetch   tiktoken-Rangtabellen (nicht im Repo, AIGW-92) per
#                      scripts/fetch-tokenizers.sh laden (Cache ~/.cache/aigw/tiktoken) und die
#                      exakten Referenztests zur Pflicht machen (CI). Ohne: Tests laufen offline,
#                      die Vergleiche mit den echten Tabellen werden übersprungen.
#   AIGW_TOKENIZER_RANKS_DIR=…   Verzeichnis mit den Tabellen (statt Download); Referenztests Pflicht
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

if [[ "${AIGW_TEST_POSTGRES:-}" == docker && -z "${AIGW_TEST_POSTGRES_URL:-}" ]]; then
  # Feste Version (F173): reproduzierbare Läufe; Aktualisierung bewusst per Commit.
  PG_IMAGE="${AIGW_TEST_POSTGRES_IMAGE:-pgvector/pgvector:0.8.7-pg17}"
  step "Postgres 17 für Store-/Cache-Tests starten (Docker $PG_IMAGE, Wegwerf-Container)"
  PG_NAME="aigw-check-pg-$$"
  trap 'docker rm -f "$PG_NAME" >/dev/null 2>&1 || true' EXIT
  docker run -d --rm --name "$PG_NAME" -e POSTGRES_PASSWORD=aigw-test \
    -p 127.0.0.1::5432 "$PG_IMAGE" >/dev/null
  # Der Init-Server lauscht nur auf dem Socket; TCP-Bereitschaft = endgültiger Server.
  for _ in $(seq 1 60); do
    docker exec "$PG_NAME" pg_isready -q -h 127.0.0.1 -U postgres && break
    sleep 1
  done
  PG_PORT=$(docker port "$PG_NAME" 5432/tcp | head -1 | sed 's/.*://')
  export AIGW_TEST_POSTGRES_URL="postgres://postgres:aigw-test@127.0.0.1:${PG_PORT}/postgres"
  if [[ "$PG_IMAGE" == *pgvector* && -z "${AIGW_TEST_PGVECTOR_URL:-}" ]]; then
    export AIGW_TEST_PGVECTOR_URL="$AIGW_TEST_POSTGRES_URL"
  fi
fi

if [[ "${AIGW_TEST_REDIS:-}" == docker && -z "${AIGW_TEST_REDIS_URL:-}" ]]; then
  # Feste Version (F173).
  REDIS_IMAGE="${AIGW_TEST_REDIS_IMAGE:-redis/redis-stack-server:7.4.0-v8}"
  step "Redis für Zähler-/Cache-Tests starten (Docker $REDIS_IMAGE, Wegwerf-Container)"
  REDIS_NAME="aigw-check-redis-$$"
  # Bestehenden EXIT-Trap (Postgres) erhalten.
  trap 'docker rm -f "${PG_NAME:-}" "$REDIS_NAME" >/dev/null 2>&1 || true' EXIT
  docker run -d --rm --name "$REDIS_NAME" -p 127.0.0.1::6379 "$REDIS_IMAGE" >/dev/null
  for _ in $(seq 1 30); do
    docker exec "$REDIS_NAME" redis-cli ping 2>/dev/null | grep -q PONG && break
    sleep 1
  done
  REDIS_PORT=$(docker port "$REDIS_NAME" 6379/tcp | head -1 | sed 's/.*://')
  export AIGW_TEST_REDIS_URL="redis://127.0.0.1:${REDIS_PORT}/0"
  if [[ "$REDIS_IMAGE" == *redis-stack* && -z "${AIGW_TEST_REDIS_STACK_URL:-}" ]]; then
    export AIGW_TEST_REDIS_STACK_URL="$AIGW_TEST_REDIS_URL"
  fi
fi

if [[ "${AIGW_TEST_OTELCOL:-}" == docker && -z "${AIGW_TEST_OTELCOL_URL:-}" ]]; then
  # Gegentest gegen den offiziellen Collector (F103): OTLP/HTTP-Empfänger, file-Exporter (OTLP-JSON).
  OTELCOL_IMAGE="${AIGW_TEST_OTELCOL_IMAGE:-otel/opentelemetry-collector-contrib:0.165.0}"
  step "OpenTelemetry Collector für den OTLP-Gegentest starten (Docker $OTELCOL_IMAGE)"
  OTELCOL_NAME="aigw-check-otelcol-$$"
  OTELCOL_DIR=$(mktemp -d)
  # Der Collector läuft als UID 10001 und schreibt in das eingebundene Verzeichnis.
  chmod 0777 "$OTELCOL_DIR"
  cat >"$OTELCOL_DIR/config.yaml" <<'YAML'
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
exporters:
  file/traces:
    path: /out/traces.json
    flush_interval: 100ms
  file/logs:
    path: /out/logs.json
    flush_interval: 100ms
service:
  telemetry:
    logs:
      level: warn
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [file/traces]
    logs:
      receivers: [otlp]
      exporters: [file/logs]
YAML
  trap 'docker rm -f "${PG_NAME:-}" "${REDIS_NAME:-}" "$OTELCOL_NAME" >/dev/null 2>&1 || true; rm -rf "$OTELCOL_DIR"' EXIT
  docker run -d --rm --name "$OTELCOL_NAME" -p 127.0.0.1::4318 \
    -v "$OTELCOL_DIR:/out" "$OTELCOL_IMAGE" --config=/out/config.yaml >/dev/null
  OTELCOL_PORT=$(docker port "$OTELCOL_NAME" 4318/tcp | head -1 | sed 's/.*://')
  # Bereit, sobald der Empfänger antwortet (leerer Export → 200/400, nicht Verbindungsfehler).
  for _ in $(seq 1 30); do
    curl -s -o /dev/null -X POST -H 'content-type: application/x-protobuf' \
      "http://127.0.0.1:${OTELCOL_PORT}/v1/traces" && break
    sleep 1
  done
  export AIGW_TEST_OTELCOL_URL="http://127.0.0.1:${OTELCOL_PORT}"
  export AIGW_TEST_OTELCOL_DIR="$OTELCOL_DIR"
fi

if [[ "${AIGW_TOKENIZER_RANKS:-}" == fetch && -z "${AIGW_TOKENIZER_RANKS_DIR:-}" ]]; then
  step "tiktoken-Rangtabellen laden (AIGW-92)"
  AIGW_TOKENIZER_RANKS_DIR=$(scripts/fetch-tokenizers.sh)
  export AIGW_TOKENIZER_RANKS_DIR
fi
if [[ -n "${AIGW_TOKENIZER_RANKS_DIR:-}" ]]; then
  export AIGW_TOKENIZER_REQUIRE_RANKS=1
else
  echo "Hinweis: ohne tiktoken-Rangtabellen – exakte BPE-Referenztests werden übersprungen" \
    "(AIGW_TOKENIZER_RANKS=fetch)"
fi

step "cargo test${AIGW_TEST_POSTGRES_URL:+ (inkl. Postgres)}${AIGW_TEST_PGVECTOR_URL:+ (inkl. pgvector)}${AIGW_TEST_REDIS_URL:+ (inkl. Redis)}${AIGW_TEST_REDIS_STACK_URL:+ (inkl. Redis Stack)}${AIGW_TEST_OTELCOL_URL:+ (inkl. OTel-Collector)}"
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
  # Vorhandenes venv (Buildbox: /opt/reuse-venv) nutzen oder einmalig eines anlegen.
  REUSE_VENV="${XDG_CACHE_HOME:-$HOME/.cache}/aigw-reuse-venv"
  for v in /opt/reuse-venv "$REUSE_VENV"; do
    if [[ -x "$v/bin/reuse" ]]; then
      PATH="$v/bin:$PATH"
      break
    fi
  done
  if ! command -v reuse >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
    echo "reuse fehlt – installiere es in $REUSE_VENV" >&2
    if python3 -m venv "$REUSE_VENV" && "$REUSE_VENV/bin/pip" install -q reuse; then
      PATH="$REUSE_VENV/bin:$PATH"
    fi
  fi
fi
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
