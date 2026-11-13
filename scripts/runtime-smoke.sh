#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Laufzeitprüfung: baut das Release-Binary (mit eingebetteter UI aus ui/dist, falls gebaut),
# startet `aigw serve` (Daten- und Steuerebene getrennt) und prüft /healthz, die Admin-API
# (Bootstrap-Token, Key anlegen), /v1/* mit und ohne Key, Routing über Snapshot-Routen
# (Mock-Deployments, Retry auf anderes Deployment, Hot Reload), /metrics und /admin/.
#
# Modellgruppen und Mock-Deployments kommen per GitOps-Apply (`POST
# /admin/api/v1/config:apply`) in die laufende Instanz.
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
# Modellgruppe „mock“: Priorität 0 = absichtlich fehlerhaftes Mock-Deployment, Priorität 1 =
# Echo → jede Anfrage braucht einen Retry auf das andere Deployment.
cat >"$TMP/models.yaml" <<'YAML'
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: mock}
spec:
  aliases: [mock-alias]
  settings:
    retry: {maxAttempts: 2, initialBackoffMs: 10}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: mock-flaky}
spec: {modelGroup: mock, provider: mock, providerModel: mock-error, priority: 0}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: mock-echo}
spec: {modelGroup: mock, provider: mock, providerModel: mock, priority: 1}
YAML
# Wegwerf-Master-Key nur für diesen Lauf (ohne Key startet `aigw serve` nicht).
TOKEN="aigw-admin-smoke-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n') AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
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
echo "--- Admin-API ohne Token → 401"
test "$(curl -s -o /dev/null -w '%{http_code}' "$CBASE/admin/api/v1/api-keys")" = 401
echo "--- POST /admin/api/v1/api-keys"
KEY=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{}}' |
  grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
test -n "$KEY"
echo "--- GET /v1/models ohne Key → 401"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/models")" = 401
echo "--- GET /v1/models mit Key (Hot Reload abwarten)"
for _ in $(seq 1 50); do
  curl -fsS "$BASE/v1/models" -H "authorization: Bearer $KEY" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -fsS "$BASE/v1/models" -H "authorization: Bearer $KEY"; echo
echo "--- POST /admin/api/v1/config:apply (Modellgruppe mock mit zwei Mock-Deployments)"
apply() {
  curl -fsS "$CBASE/admin/api/v1/config:apply" -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/yaml' --data-binary "@$1" | head -c 300
  echo
}
apply "$TMP/models.yaml"
echo "--- POST /v1/messages (x-api-key), unbekanntes Modell → 404"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/messages" -H "x-api-key: $KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"unbekannt","max_tokens":64,"messages":[{"role":"user","content":"Hallo"}]}')" = 404
echo "--- POST /v1/chat/completions über Alias: Retry auf anderes Deployment"
for _ in $(seq 1 50); do
  [[ "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
    -d '{"model":"mock-alias","messages":[{"role":"user","content":"Hallo"}]}')" == 200 ]] && break
  sleep 0.1
done
curl -fsS -D "$TMP/h1.txt" -o "$TMP/b1.json" "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock-alias","messages":[{"role":"user","content":"Hallo"}]}'
cat "$TMP/b1.json"; echo
grep -i '^x-aigw-' "$TMP/h1.txt"
grep -qi '^x-aigw-deployment: mock-echo' "$TMP/h1.txt"
grep -qi '^x-aigw-attempts: 2' "$TMP/h1.txt"
grep -q 'Echo: Hallo' "$TMP/b1.json"
echo "--- POST /v1/messages (Stream) über Modellgruppe mock"
curl -fsS -N "$BASE/v1/messages" -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"Hallo"}]}' \
  >"$TMP/s1.txt"
grep -q 'message_stop' "$TMP/s1.txt"
echo "--- Hot Reload: neue Modellgruppe mock-2 per Apply, danach erreichbar"
cat >"$TMP/models2.yaml" <<'YAML'
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: mock-2}
spec: {routing: least-busy}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: mock-2-echo}
spec: {modelGroup: mock-2, provider: mock, providerModel: mock}
YAML
apply "$TMP/models2.yaml"
CODE=000
for _ in $(seq 1 50); do
  CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
    -d '{"model":"mock-2","messages":[{"role":"user","content":"Hallo"}]}')
  [[ "$CODE" == 200 ]] && break
  sleep 0.2
done
test "$CODE" = 200
echo "--- Admin nicht auf Datenebene"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/api/v1/version")" = 404
echo "--- GET /metrics (Steuerebene)"
curl -fsS "$CBASE/metrics" | tee "$TMP/metrics.txt" | grep '^aigw_log_'
grep -q '^aigw_log_dropped_total 0$' "$TMP/metrics.txt"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/metrics")" = 404
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
