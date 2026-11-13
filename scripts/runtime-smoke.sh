#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Laufzeitprüfung: baut das Release-Binary (mit eingebetteter UI aus ui/dist, falls gebaut),
# startet `aigw serve` (Daten- und Steuerebene getrennt) und prüft /healthz, /v1/* (Mock-Anbieter),
# die Admin-API und /admin/.
set -euo pipefail
cd "$(dirname "$0")/.."

PORT=${AIGW_SMOKE_PORT:-14000}
CPORT=${AIGW_SMOKE_CONTROL_PORT:-14001}
LOCKED=()
[[ -f Cargo.lock ]] && LOCKED=(--locked)
cargo build --release "${LOCKED[@]}" -p aigw
ls -l target/release/aigw

TMP=$(mktemp -d)
trap 'kill "${PID:-}" 2>/dev/null || true; rm -rf "$TMP"' EXIT
printf 'server:\n  data:\n    listen: "127.0.0.1:%s"\n  control:\n    listen: "127.0.0.1:%s"\nstore:\n  url: "sqlite://%s/aigw.db?mode=rwc"\n' \
  "$PORT" "$CPORT" "$TMP" >"$TMP/aigw.yaml"
# Wegwerf-Master-Key nur für diesen Lauf (ohne Key startet `aigw serve` nicht).
AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n') \
  target/release/aigw serve --config "$TMP/aigw.yaml" &
PID=$!

BASE="http://127.0.0.1:$PORT"   # Datenebene
CBASE="http://127.0.0.1:$CPORT" # Steuerebene
for _ in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done

echo "--- GET /healthz (Daten)";       curl -fsS "$BASE/healthz"; echo
echo "--- GET /healthz (Steuerung)";    curl -fsS "$CBASE/healthz"; echo
echo "--- POST /v1/chat/completions";   curl -fsS "$BASE/v1/chat/completions" \
  -H 'content-type: application/json' \
  -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}'; echo
echo "--- POST /v1/messages (Stream)";   curl -fsSN "$BASE/v1/messages" \
  -H 'content-type: application/json' \
  -d '{"model":"mock","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"Hallo"}]}'
echo "--- Admin nicht auf Datenebene"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/api/v1/version")" = 404
echo "--- GET /admin/api/v1/version";   curl -fsS "$CBASE/admin/api/v1/version"; echo
echo "--- GET /admin/api/openapi.json"; curl -fsS "$CBASE/admin/api/openapi.json" | head -c 200; echo
echo "--- GET /admin/ (Kopfzeilen)";    curl -fsS -D - -o "$TMP/index.html" "$CBASE/admin/"
grep -o '<title>[^<]*</title>' "$TMP/index.html"
ASSET=$(grep -o '/admin/assets/[^"]*\.js' "$TMP/index.html" | head -1 || true)
if [[ -n "$ASSET" ]]; then
  echo "--- GET $ASSET"; curl -fsS -o /dev/null -w '%{http_code} %{content_type}\n' "$CBASE$ASSET"
else
  echo "Hinweis: keine gebaute UI eingebettet (Fallback-Seite)"
fi
echo "Laufzeitprüfung bestanden"
