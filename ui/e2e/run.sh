#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Browser-Tests der Admin-UI gegen ein echtes `aigw serve` (UI eingebettet, CSP aus crates/aigw-admin):
# CSP-Prüfung der Overlays/Diagramme, Verwaltungsseiten gegen die echte Admin-API (Bootstrap-Token, ohne OIDC)
# und Screenshots (Musterseiten im Mock-Modus, echte Seiten mit per API angelegten Daten).
#
# Voraussetzung: ui/dist gebaut (pnpm build) – das aigw-Binary bettet es beim Übersetzen ein.
#   AIGW_BIN   fertiges Binary (Standard: target/debug/aigw, wird sonst gebaut)
#   E2E_OUT    Ergebnisordner (Standard: ui/e2e/results; Screenshots unter screens/)
#   E2E_GREP   nur passende Tests (Playwright --grep)
set -euo pipefail

E2E=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$E2E/../.." && pwd)
OUT=${E2E_OUT:-$E2E/results}
PORT=${AIGW_E2E_PORT:-4899}
BIN=${AIGW_BIN:-$ROOT/target/debug/aigw}

[[ -f "$ROOT/ui/dist/index.html" ]] || { echo "FEHLER: ui/dist fehlt – zuerst 'pnpm build' in ui/" >&2; exit 1; }
if [[ -z "${AIGW_BIN:-}" ]]; then (cd "$ROOT" && cargo build -q -p aigw); fi

rm -rf "$OUT" && mkdir -p "$OUT/screens"
RUN=$(mktemp -d)
printf 'server:\n  data:\n    listen: "127.0.0.1:%s"\n  control:\n    listen: "127.0.0.1:%s"\nstore:\n  url: "sqlite://%s/aigw.db?mode=rwc"\n' \
  "$PORT" "$PORT" "$RUN" > "$RUN/aigw.yaml"
# AIGW-77: Link-Vorlage zum Tracing-System (Log-Explorer).
printf 'admin:\n  ui:\n    trace_url: "https://tracing.example.org/trace/{traceId}"\n' >> "$RUN/aigw.yaml"
# Zweiter Server mit OIDC (Einladungen, F90): Issuer wird erst beim Login abgefragt, der Test meldet sich per
# Bootstrap-Token an (sessionStorage) und erzeugt nur Einladungslinks.
OIDC_PORT=$((PORT + 1))
mkdir -p "$RUN/oidc"
printf 'server:\n  data:\n    listen: "127.0.0.1:%s"\n  control:\n    listen: "127.0.0.1:%s"\nstore:\n  url: "sqlite://%s/oidc/aigw.db?mode=rwc"\nadmin:\n  auth:\n    oidc:\n      issuer: "http://127.0.0.1:9/realms/e2e"\n      client_id: aigw-e2e\n      redirect_url: "http://127.0.0.1:%s/admin/auth/callback"\n' \
  "$OIDC_PORT" "$OIDC_PORT" "$RUN" "$OIDC_PORT" > "$RUN/oidc/aigw.yaml"
# Wegwerf-Master-Key, -Bootstrap-Token und -Datenbank nur für diesen Lauf (ohne Key startet `aigw serve` nicht).
TOKEN="aigw-admin-e2e-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
(cd "$RUN" && AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n') AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
  exec "$BIN" serve --config "$RUN/aigw.yaml") > "$OUT/server.log" 2>&1 &
PID=$!
(cd "$RUN/oidc" && AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n') AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
  exec "$BIN" serve --config "$RUN/oidc/aigw.yaml") > "$OUT/server-oidc.log" 2>&1 &
PID2=$!
trap 'kill $PID $PID2 2>/dev/null || true; rm -rf "$RUN"' EXIT
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$PORT/admin/" && break; sleep 0.2; done
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$OIDC_PORT/admin/api/v1/version" && break; sleep 0.2; done
curl -fsS -D "$OUT/headers-admin.txt" -o /dev/null "http://127.0.0.1:$PORT/admin/"
grep -qi '^content-security-policy:' "$OUT/headers-admin.txt"

cd "$E2E"
if [[ -f pnpm-lock.yaml ]]; then pnpm install --frozen-lockfile; else pnpm install; fi
if ! ls "${PLAYWRIGHT_BROWSERS_PATH:-$HOME/.cache/ms-playwright}" 2>/dev/null | grep -q chromium; then
  pnpm exec playwright install --with-deps chromium > "$OUT/playwright-install.log" 2>&1
fi
AIGW_OIDC_URL="http://127.0.0.1:$OIDC_PORT" AIGW_URL="http://127.0.0.1:$PORT" AIGW_TOKEN="$TOKEN" E2E_OUT="$OUT" pnpm exec playwright test ${E2E_GREP:+--grep "$E2E_GREP"}
