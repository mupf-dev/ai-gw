#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Browser-Tests der Admin-UI gegen ein echtes `aigw serve` (UI eingebettet, CSP aus crates/aigw-admin):
# CSP-Prüfung der Overlays/Diagramme und Screenshots der Musterseiten (hell/dunkel × Desktop/schmal).
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
printf 'server:\n  data:\n    listen: "127.0.0.1:%s"\n  control:\n    listen: "127.0.0.1:%s"\n' "$PORT" "$PORT" > "$RUN/aigw.yaml"
(cd "$RUN" && exec "$BIN" serve --config "$RUN/aigw.yaml") > "$OUT/server.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true; rm -rf "$RUN"' EXIT
for _ in $(seq 1 50); do curl -fsS -o /dev/null "http://127.0.0.1:$PORT/admin/" && break; sleep 0.2; done
curl -fsS -D "$OUT/headers-admin.txt" -o /dev/null "http://127.0.0.1:$PORT/admin/"
grep -qi '^content-security-policy:' "$OUT/headers-admin.txt"

cd "$E2E"
if [[ -f pnpm-lock.yaml ]]; then pnpm install --frozen-lockfile; else pnpm install; fi
if ! ls "${PLAYWRIGHT_BROWSERS_PATH:-$HOME/.cache/ms-playwright}" 2>/dev/null | grep -q chromium; then
  pnpm exec playwright install --with-deps chromium > "$OUT/playwright-install.log" 2>&1
fi
AIGW_URL="http://127.0.0.1:$PORT" E2E_OUT="$OUT" pnpm exec playwright test ${E2E_GREP:+--grep "$E2E_GREP"}
