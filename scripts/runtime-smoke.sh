#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Laufzeitprüfung: baut das Release-Binary (mit eingebetteter UI aus ui/dist, falls gebaut),
# startet `aigw serve` (Daten- und Steuerebene getrennt) und prüft /healthz, die Admin-API
# (Bootstrap-Token, Key anlegen), /v1/* mit und ohne Key, Routing über Snapshot-Routen
# (Mock-Deployments, Retry auf anderes Deployment, Hot Reload), Routing-Ressourcen per REST
# (Zugangsdaten ohne Geheimwert in der Antwort, Modellgruppe/Deployment/Preis, Alias-Kollision,
# SSRF-Ablehnung, Freigabe per Key), /metrics, OTLP-Export (gegen einen lokalen Fake-Collector,
# falls python3 vorhanden) und /admin/.
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
# Erst beenden und warten, dann löschen – sonst schreibt `aigw` beim Herunterfahren noch in die
# bereits gelöschte SQLite-Datei („disk I/O error“).
trap 'kill "${PID:-}" "${OTLP_PID:-}" 2>/dev/null || true; wait "${PID:-}" 2>/dev/null || true; rm -rf "$TMP"' EXIT
printf 'server:\n  data:\n    listen: "127.0.0.1:%s"\n  control:\n    listen: "127.0.0.1:%s"\nstore:\n  url: "sqlite://%s/aigw.db?mode=rwc"\n' \
  "$PORT" "$CPORT" "$TMP" >"$TMP/aigw.yaml"
# Fake-OTLP-Collector (nur lokal): speichert jeden POST-Body als Datei je Pfad.
OTLP_PORT=${AIGW_SMOKE_OTLP_PORT:-14318}
if command -v python3 >/dev/null 2>&1; then
  mkdir -p "$TMP/otlp"
  python3 -I -c '
import http.server, sys, os, itertools
out = sys.argv[1]; n = itertools.count()
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["content-length"]))
        name = self.path.strip("/").replace("/", "_")
        ok = self.headers.get("content-type") == "application/x-protobuf"
        with open(os.path.join(out, "%s.%d%s" % (name, next(n), "" if ok else ".badtype")), "wb") as f:
            f.write(body)
        self.send_response(200); self.send_header("content-length", "0"); self.end_headers()
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
' "$TMP/otlp" "$OTLP_PORT" &
  OTLP_PID=$!
  printf 'observe:\n  flush_interval_ms: 100\n  otlp:\n    endpoint: "http://127.0.0.1:%s"\n    traces: {enabled: true, flush_interval_ms: 100}\n    logs: {enabled: true}\n' \
    "$OTLP_PORT" >>"$TMP/aigw.yaml"
fi
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
# Upstream-Ausnahmen per ENV (wie für einen lokalen Ollama): http + privat nur für localhost.
AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n') AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
  AIGW_ADMIN__ALLOW_INSECURE_UPSTREAMS=localhost AIGW_ADMIN__ALLOW_PRIVATE_UPSTREAMS=localhost \
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

# Routing-Ressourcen per REST (AIGW-65), ergänzend zum GitOps-Apply oben.
adm() { # adm <Methode> <Pfad> [JSON] → Status in $TMP/status, Körper auf stdout
  curl -sS -o "$TMP/body" -w '%{http_code}' -X "$1" "$CBASE/admin/api/v1$2" \
    -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' ${3:+-d "$3"} \
    >"$TMP/status"
  cat "$TMP/body"
}
expect() { test "$(cat "$TMP/status")" = "$1" || { echo "erwartet $1, erhalten $(cat "$TMP/status")" >&2; exit 1; }; }
echo "--- POST /admin/api/v1/provider-credentials (Antwort ohne Geheimwert)"
adm POST /provider-credentials \
  '{"metadata":{"name":"smoke-cred"},"spec":{"provider":"openai-compatible","data":{"apiKey":{"value":"sk-smoke-nicht-echt"}}}}' |
  tee "$TMP/cred.json"; echo
expect 201
if grep -q 'sk-smoke-nicht-echt' "$TMP/cred.json"; then echo 'Geheimnis in Antwort' >&2; exit 1; fi
grep -q '"hasSecret":true' "$TMP/cred.json"
echo "--- POST /admin/api/v1/model-groups"
adm POST /model-groups '{"metadata":{"name":"rest-model"},"spec":{"aliases":["rest-alias"]}}'; echo
expect 201
echo "--- Alias-Kollision mit GitOps-Gruppe (mock-alias) → 409"
adm POST /model-groups '{"metadata":{"name":"rest-other"},"spec":{"aliases":["mock-alias"]}}' >/dev/null
expect 409
echo "--- POST /admin/api/v1/deployments (Mock-Anbieter)"
adm POST /deployments '{"metadata":{"name":"rest-echo"},"spec":{"modelGroup":"rest-model","provider":"mock","providerModel":"mock","capacity":{"rpm":600}}}'; echo
expect 201
echo "--- SSRF: Cloud-Metadaten-Adresse als Endpunkt → 422"
adm POST /deployments '{"metadata":{"name":"rest-evil"},"spec":{"modelGroup":"rest-model","provider":"openai-compatible","endpoint":"http://169.254.169.254/latest","providerModel":"x"}}' >/dev/null
expect 422
echo "--- lokaler Endpunkt nach Freischaltung (AIGW_ADMIN__…) → 201 (deaktiviert)"
adm POST /deployments '{"metadata":{"name":"rest-local"},"spec":{"modelGroup":"rest-model","provider":"ollama","endpoint":"http://localhost:11434/v1","providerModel":"llama3.1","enabled":false}}' >/dev/null
expect 201
echo "--- POST /admin/api/v1/prices (Name vom Server)"
adm POST /prices '{"spec":{"provider":"openai","model":"gpt-4o-mini","validFrom":"2024-07-18T00:00:00Z","perMillionTokens":{"input":"0.15","output":"0.60"}}}'; echo
expect 201
echo "--- Key mit Freigabe nur für rest-model: Chat über Alias → 200, mock → 403"
LIMITED=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"metadata":{"name":"smoke-limited"},"spec":{"allowedModels":["rest-model"]}}' |
  grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
CODE=000
for _ in $(seq 1 50); do
  CODE=$(curl -s -D "$TMP/h2.txt" -o "$TMP/b2.json" -w '%{http_code}' "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $LIMITED" -H 'content-type: application/json' \
    -d '{"model":"rest-alias","messages":[{"role":"user","content":"Hallo"}]}')
  [[ "$CODE" == 200 ]] && break
  sleep 0.1
done
test "$CODE" = 200
grep -qi '^x-aigw-deployment: rest-echo' "$TMP/h2.txt"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $LIMITED" -H 'content-type: application/json' \
  -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}')" = 403
curl -fsS "$BASE/v1/models" -H "authorization: Bearer $LIMITED" | grep -q '"rest-model"'
echo "--- Admin nicht auf Datenebene"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/api/v1/version")" = 404
echo "--- GET /metrics (Steuerebene)"
curl -fsS "$CBASE/metrics" | tee "$TMP/metrics.txt" | grep -E '^aigw_(log_|requests_total|upstream_errors|snapshot_|inflight)'
grep -q '^aigw_log_dropped_total 0$' "$TMP/metrics.txt"
grep -q '^aigw_requests_total{.*ingress="anthropic-messages",status_class="4xx"} 1$' "$TMP/metrics.txt"
grep -q '^aigw_requests_total{model_group="mock",.*status_class="2xx"} [1-9]' "$TMP/metrics.txt"
grep -q '^aigw_upstream_errors_total{class="[a-z_]*"} [1-9]' "$TMP/metrics.txt"
grep -q '^aigw_inflight_requests 0$' "$TMP/metrics.txt"
grep -q '^aigw_snapshot_version [1-9]' "$TMP/metrics.txt"
grep -q '^aigw_snapshot_stale_seconds 0$' "$TMP/metrics.txt"
# Limits nach der Metrik-Prüfung (erzeugt weitere 4xx).
echo "--- Limits (AIGW-30): Key mit 1 Anfrage/Minute → zweite Anfrage 429 mit Rate-Limit-Headern"
RL=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"metadata":{"name":"smoke-rl"},"spec":{"limits":[{"metric":"requests","limit":1,"window":"minute"}],"budget":{"amount":"5","period":"month","warnAtPercent":[80]}}}' |
  grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
test -n "$RL"
CODE=000
for _ in $(seq 1 50); do # Hot Reload abwarten
  CODE=$(curl -s -D "$TMP/h3.txt" -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $RL" -H 'content-type: application/json' \
    -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}')
  [[ "$CODE" != 401 ]] && break
  sleep 0.1
done
test "$CODE" = 200
grep -qi '^x-ratelimit-limit-requests: 1' "$TMP/h3.txt"
CODE=$(curl -s -D "$TMP/h4.txt" -o "$TMP/b4.json" -w '%{http_code}' "$BASE/v1/messages" \
  -H "x-api-key: $RL" -H 'content-type: application/json' \
  -d '{"model":"mock","max_tokens":5,"messages":[{"role":"user","content":"Hallo"}]}')
cat "$TMP/b4.json"; echo
test "$CODE" = 429
grep -qi '^retry-after:' "$TMP/h4.txt"
grep -qi '^anthropic-ratelimit-requests-remaining: 0' "$TMP/h4.txt"
grep -q '"rate_limit_error"' "$TMP/b4.json"
echo "--- GET /admin/api/v1/usage/budgets (AIGW-31)"
curl -fsS "$CBASE/admin/api/v1/usage/budgets?level=api-key" -H "authorization: Bearer $TOKEN" |
  tee "$TMP/budgets.json"; echo
grep -q '"targetName":"smoke-rl"' "$TMP/budgets.json"
if [[ -n "${OTLP_PID:-}" ]]; then
  echo "--- OTLP-Export an den Fake-Collector (Traces + Logs)"
  for _ in $(seq 1 50); do
    compgen -G "$TMP/otlp/v1_traces.*" >/dev/null && compgen -G "$TMP/otlp/v1_logs.*" >/dev/null && break
    sleep 0.1
  done
  ls -l "$TMP/otlp"
  compgen -G "$TMP/otlp/v1_traces.*" >/dev/null
  compgen -G "$TMP/otlp/v1_logs.*" >/dev/null
  if compgen -G "$TMP/otlp/*.badtype" >/dev/null; then
    echo "FEHLER: OTLP-Export ohne content-type application/x-protobuf" >&2
    exit 1
  fi
  grep -q 'gen_ai.operation.name' "$TMP"/otlp/v1_traces.*
  grep -q 'aigw.request' "$TMP"/otlp/v1_logs.*
  curl -fsS "$CBASE/metrics" | grep '^aigw_otlp_'
fi
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/metrics")" = 404
echo "--- GET /admin/api/v1/version";   curl -fsS "$CBASE/admin/api/v1/version"; echo
echo "--- GET /admin/api/openapi.json"
curl -fsS -o "$TMP/openapi.json" "$CBASE/admin/api/openapi.json"; head -c 200 "$TMP/openapi.json"; echo
grep -q "/admin/api/v1/provider-credentials" "$TMP/openapi.json"
echo "--- GET /admin/ (Kopfzeilen)";    curl -fsS -D - -o "$TMP/index.html" "$CBASE/admin/"
grep -o '<title>[^<]*</title>' "$TMP/index.html"
ASSET=$(grep -o '/admin/assets/[^"]*\.js' "$TMP/index.html" | head -1 || true)
if [[ -n "$ASSET" ]]; then
  echo "--- GET $ASSET"; curl -fsS -o /dev/null -w '%{http_code} %{content_type}\n' "$CBASE$ASSET"
else
  echo "Hinweis: keine gebaute UI eingebettet (Fallback-Seite)"
fi
echo "Laufzeitprüfung bestanden"
