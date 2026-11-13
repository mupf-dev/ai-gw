#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Laufzeitprüfung: baut das Release-Binary (mit eingebetteter UI aus ui/dist, falls gebaut),
# startet `aigw serve` auf einem freien Port und prüft /healthz, die Admin-API und /admin/.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT=${AIGW_SMOKE_PORT:-14000}
LOCKED=()
[[ -f Cargo.lock ]] && LOCKED=(--locked)
cargo build --release "${LOCKED[@]}" -p aigw
ls -l target/release/aigw

TMP=$(mktemp -d)
trap 'kill "${PID:-}" 2>/dev/null || true; rm -rf "$TMP"' EXIT
printf 'server:\n  listen: "127.0.0.1:%s"\n' "$PORT" >"$TMP/aigw.yaml"
target/release/aigw serve --config "$TMP/aigw.yaml" &
PID=$!

BASE="http://127.0.0.1:$PORT"
for _ in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done

echo "--- GET /healthz";               curl -fsS "$BASE/healthz"; echo
echo "--- GET /admin/api/v1/version";   curl -fsS "$BASE/admin/api/v1/version"; echo
echo "--- GET /admin/api/openapi.json"; curl -fsS "$BASE/admin/api/openapi.json" | head -c 200; echo
echo "--- GET /admin/ (Kopfzeilen)";    curl -fsS -D - -o "$TMP/index.html" "$BASE/admin/"
grep -o '<title>[^<]*</title>' "$TMP/index.html"
ASSET=$(grep -o '/admin/assets/[^"]*\.js' "$TMP/index.html" | head -1 || true)
if [[ -n "$ASSET" ]]; then
  echo "--- GET $ASSET"; curl -fsS -o /dev/null -w '%{http_code} %{content_type}\n' "$BASE$ASSET"
else
  echo "Hinweis: keine gebaute UI eingebettet (Fallback-Seite)"
fi
echo "Laufzeitprüfung bestanden"
