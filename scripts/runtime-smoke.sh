#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Laufzeitprüfung: baut das Release-Binary (mit eingebetteter UI aus ui/dist, falls gebaut),
# startet `aigw serve` (Daten- und Steuerebene getrennt) und prüft /healthz, die Admin-API
# (Bootstrap-Token, Key anlegen), /v1/* mit und ohne Key, Routing über Snapshot-Routen
# (Mock-Deployments, Retry auf anderes Deployment, Hot Reload), Responses, Embeddings, Bilder,
# Audio (AIGW-21, inkl. Fähigkeiten der Modellgruppe), Routing-Ressourcen per REST
# (Zugangsdaten ohne Geheimwert in der Antwort, Modellgruppe/Deployment/Preis, Alias-Kollision,
# SSRF-Ablehnung, Freigabe per Key), /metrics, OTLP-Export (gegen einen lokalen Fake-Collector,
# falls python3 vorhanden), /admin/, /readyz und den Kaltstart ohne Datenbank aus dem
# verschlüsselten Snapshot-Cache (AIGW-75, F14/F15).
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
printf 'server:\n  data:\n    listen: "127.0.0.1:%s"\n  control:\n    listen: "127.0.0.1:%s"\nstore:\n  url: "sqlite://%s/aigw.db?mode=rwc"\nsnapshot:\n  cache_file: "%s/snapshot.cache"\n  cache_write_interval_secs: 1\n' \
  "$PORT" "$CPORT" "$TMP" "$TMP" >"$TMP/aigw.yaml"
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
---
# AIGW-21: Gruppe für Embeddings, Bilder und Audio (Chat ist hier nicht freigegeben).
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: mock-media}
spec:
  capabilities: [embeddings, images, audio-transcription, audio-speech]
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: mock-media-echo}
spec: {modelGroup: mock-media, provider: mock, providerModel: mock}
YAML
# Wegwerf-Master-Key nur für diesen Lauf (ohne Key startet `aigw serve` nicht).
TOKEN="aigw-admin-smoke-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
# Upstream-Ausnahmen per ENV (wie für einen lokalen Ollama): http + privat nur für localhost.
AIGW_MASTER_KEY=$MASTER_KEY AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
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
echo "--- GET /readyz (Daten, Steuerung) → ready"
curl -fsS "$BASE/readyz" | tee "$TMP/readyz.json"; echo
grep -q '"status":"ready"' "$TMP/readyz.json"
curl -fsS "$CBASE/readyz" | grep -q '"source":"database"'
echo "--- Admin-API ohne Token → 401"
test "$(curl -s -o /dev/null -w '%{http_code}' "$CBASE/admin/api/v1/api-keys")" = 401
echo "--- POST /admin/api/v1/organizations (Keys brauchen eine Organisation, F3)"
curl -fsS "$CBASE/admin/api/v1/organizations" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke-org"},"spec":{}}' >/dev/null
echo "--- POST /admin/api/v1/api-keys ohne Organisation → 422"
test "$(curl -s -o /dev/null -w '%{http_code}' "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke-noorg"},"spec":{}}')" = 422
echo "--- POST /admin/api/v1/api-keys"
KEY=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{"organization":"smoke-org"}}' |
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
echo "--- Key löschen und sofort gleichnamig neu anlegen → neuer Key gilt nach dem Hot Reload, alter 401"
curl -fsS -X DELETE "$CBASE/admin/api/v1/api-keys/smoke" -H "authorization: Bearer $TOKEN" -H 'if-match: *'
OLD_KEY=$KEY
KEY=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{"organization":"smoke-org"}}' |
  grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
test -n "$KEY" && test "$KEY" != "$OLD_KEY"
for _ in $(seq 1 50); do
  curl -fsS "$BASE/v1/models" -H "authorization: Bearer $KEY" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -fsS -o /dev/null "$BASE/v1/models" -H "authorization: Bearer $KEY"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/models" -H "authorization: Bearer $OLD_KEY")" = 401
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
grep -qi '^x-aigw-model: ' "$TMP/h1.txt"
grep -q 'Echo: Hallo' "$TMP/b1.json"
echo "--- POST /v1/messages (Stream) über Modellgruppe mock"
curl -fsS -N "$BASE/v1/messages" -H "x-api-key: $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"Hallo"}]}' \
  >"$TMP/s1.txt"
grep -q 'message_stop' "$TMP/s1.txt"
echo "--- POST /v1/responses (nicht gestreamt und gestreamt) über Modellgruppe mock"
curl -fsS "$BASE/v1/responses" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock","input":"Hallo"}' >"$TMP/r1.json"
grep -q '"object":"response"' "$TMP/r1.json"
grep -q 'Echo: Hallo' "$TMP/r1.json"
curl -fsS -N "$BASE/v1/responses" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock","input":"Hallo","stream":true}' >"$TMP/r2.txt"
grep -q '^event: response.completed' "$TMP/r2.txt"
echo "--- POST /v1/responses mit previous_response_id → 400 (zustandslos)"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/responses" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"model":"mock","input":"x","previous_response_id":"resp_1"}')" = 400
echo "--- POST /v1/embeddings (mock-media); Chat-Gruppe ohne Fähigkeit → 400"
curl -fsS "$BASE/v1/embeddings" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock-media","input":["Hallo","Welt"],"dimensions":4}' >"$TMP/e1.json"
grep -q '"object":"embedding"' "$TMP/e1.json"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/embeddings" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"model":"mock","input":"x"}')" = 400
echo "--- POST /v1/images/generations (mock-media)"
curl -fsS "$BASE/v1/images/generations" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"model":"mock-media","prompt":"Katze"}' | grep -q '"b64_json"'
echo "--- POST /v1/audio/transcriptions (multipart, mock-media)"
head -c 32000 /dev/zero >"$TMP/probe.wav"
curl -fsS "$BASE/v1/audio/transcriptions" -H "authorization: Bearer $KEY" \
  -F model=mock-media -F "file=@$TMP/probe.wav;type=audio/wav" | grep -q 'Mock-Transkript (32000 Bytes)'
echo "--- POST /v1/audio/speech (gestreamte Audio-Bytes, mock-media)"
curl -fsS "$BASE/v1/audio/speech" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock-media","input":"Hallo","voice":"alloy","response_format":"wav"}' -o "$TMP/speech.wav"
test "$(head -c 4 "$TMP/speech.wav")" = RIFF
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
  -d '{"metadata":{"name":"smoke-limited"},"spec":{"organization":"smoke-org","allowedModels":["rest-model"]}}' |
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
# Eingeschränkter Key, unbekanntes Modell: trotzdem präzise 404 (F39).
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $LIMITED" -H 'content-type: application/json' \
  -d '{"model":"gibt-es-nicht","messages":[{"role":"user","content":"Hallo"}]}')" = 404
curl -fsS "$BASE/v1/models" -H "authorization: Bearer $LIMITED" | grep -q '"rest-model"'
echo "--- Plugins (AIGW-36): header auf Modellgruppe mock-2, request-transform global, per Apply"
cat >"$TMP/plugins.yaml" <<'YAML'
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: smoke-header}
spec:
  scope: {level: model-group, target: mock-2}
  plugin: header
  config: {response: {set: {x-smoke-plugin: "on"}}}
---
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: smoke-cap}
spec:
  scope: {level: global}
  plugin: request-transform
  order: 10
  config: {caps: {maxTokens: 100}, rejectOverCap: true}
---
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: smoke-cache}
spec:
  scope: {level: model-group, target: mock-2}
  plugin: cache
  config: {ttlSecs: 600}
YAML
apply "$TMP/plugins.yaml"
FOUND=0
for _ in $(seq 1 50); do
  curl -fsS -D "$TMP/p2.txt" -o /dev/null "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
    -d '{"model":"mock-2","messages":[{"role":"user","content":"Hallo"}]}'
  if grep -qi '^x-smoke-plugin: on' "$TMP/p2.txt"; then FOUND=1; break; fi
  sleep 0.2
done
test "$FOUND" = 1
echo "--- Plugin-Bindungen per REST (AIGW-69): Prüfung (422), wirksamer failMode, Ausgabe-Guardrail"
CODE=$(curl -s -o "$TMP/pb1.json" -w '%{http_code}' "$CBASE/admin/api/v1/plugin-bindings" \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"metadata":{"name":"smoke-bad"},"spec":{"scope":{"level":"global"},"plugin":"pii","config":{"acton":"mask"}}}')
cat "$TMP/pb1.json"; echo
test "$CODE" = 422
grep -q 'Plugin-Bindung (pii)' "$TMP/pb1.json"
curl -fsS "$CBASE/admin/api/v1/plugin-bindings" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"metadata":{"name":"smoke-pii"},"spec":{"scope":{"level":"model-group","target":"mock-2"},"plugin":"pii","order":5,"config":{"categories":{"creditCard":{"action":"reject","direction":"output"}}}}}' |
  tee "$TMP/pb2.json" | grep -q '"failMode":"closed","failModeSource":"plugin-default"'
echo
# Ausgabe enthält eine Kartennummer (Echo) → 200 mit finish_reason content_filter (F160).
FOUND=0
for _ in $(seq 1 50); do
  curl -fsS -D "$TMP/pb3.txt" -o "$TMP/pb3.json" "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
    -d '{"model":"mock-2","messages":[{"role":"user","content":"Karte 4111 1111 1111 1111"}]}'
  if grep -qi '^x-aigw-guardrail: pii' "$TMP/pb3.txt"; then FOUND=1; break; fi
  sleep 0.2
done
test "$FOUND" = 1
cat "$TMP/pb3.json"; echo
grep -q '"finish_reason":"content_filter"' "$TMP/pb3.json"
if grep -q '4111' "$TMP/pb3.json"; then exit 1; fi
echo "--- DELETE ohne If-Match → 428 (F38), mit ETag aus GET → 204"
test "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$CBASE/admin/api/v1/plugin-bindings/smoke-pii" \
  -H "authorization: Bearer $TOKEN")" = 428
ETAG=$(curl -fsS -D - -o /dev/null "$CBASE/admin/api/v1/plugin-bindings/smoke-pii" \
  -H "authorization: Bearer $TOKEN" | tr -d '\r' | sed -n 's/^[Ee][Tt][Aa][Gg]: //p')
test -n "$ETAG"
curl -fsS -X DELETE "$CBASE/admin/api/v1/plugin-bindings/smoke-pii" -H "authorization: Bearer $TOKEN" \
  -H "if-match: $ETAG"
if [[ -f crates/aigw-wasm/tests/data/keyword-guard.wasm ]]; then
  echo "--- WASM-Plugin (AIGW-63): Modul hochladen (SHA-256), Bindung, Stichwort blockiert, Header gesetzt"
  WASM=crates/aigw-wasm/tests/data/keyword-guard.wasm
  SHA=$(sha256sum "$WASM" | cut -d' ' -f1)
  test "$(curl -s -o /dev/null -w '%{http_code}' -X PUT "$CBASE/admin/api/v1/wasm-modules/smoke-guard?sha256=$(printf '0%.0s' $(seq 64))" \
    -H "authorization: Bearer $TOKEN" -H 'content-type: application/wasm' --data-binary @"$WASM")" = 422
  curl -fsS -X PUT "$CBASE/admin/api/v1/wasm-modules/smoke-guard?sha256=$SHA" -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/wasm' --data-binary @"$WASM" -o "$TMP/wu.json"
  grep -q "\"sha256\":\"$SHA\"" "$TMP/wu.json"
  curl -fsS "$CBASE/admin/api/v1/plugin-bindings" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -d "{\"metadata\":{\"name\":\"smoke-wasm\"},\"spec\":{\"scope\":{\"level\":\"model-group\",\"target\":\"mock-2\"},\"plugin\":\"wasm\",\"config\":{\"module\":\"smoke-guard\",\"sha256\":\"$SHA\",\"config\":{\"block\":[\"geheimprojekt\"],\"responseHeaders\":{\"x-smoke-wasm\":\"geprueft\"}}}}}" >/dev/null
  FOUND=0
  for _ in $(seq 1 50); do
    CODE=$(curl -s -D "$TMP/w1.txt" -o "$TMP/w1.json" -w '%{http_code}' "$BASE/v1/chat/completions" \
      -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
      -d '{"model":"mock-2","messages":[{"role":"user","content":"Zum Geheimprojekt bitte"}]}')
    if [[ "$CODE" == 400 ]] && grep -q keyword_blocked "$TMP/w1.json"; then FOUND=1; break; fi
    sleep 0.2
  done
  test "$FOUND" = 1
  grep -qi '^x-aigw-guardrail: wasm' "$TMP/w1.txt"
  curl -fsS -D "$TMP/w2.txt" -o /dev/null "$BASE/v1/chat/completions" -H "authorization: Bearer $KEY" \
    -H 'content-type: application/json' -d '{"model":"mock-2","messages":[{"role":"user","content":"Hallo"}]}'
  grep -qi '^x-smoke-wasm: geprueft' "$TMP/w2.txt"
  curl -fsS -o "$TMP/wm.txt" "$CBASE/metrics"
  grep -q 'aigw_wasm_calls_total{module="smoke-guard",phase="request",outcome="block"}' "$TMP/wm.txt"
  test "$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$CBASE/admin/api/v1/wasm-modules/smoke-guard" \
    -H "authorization: Bearer $TOKEN" -H 'if-match: *')" = 409
  curl -fsS -X DELETE "$CBASE/admin/api/v1/plugin-bindings/smoke-wasm" -H "authorization: Bearer $TOKEN" -H 'if-match: *'
  curl -fsS -X DELETE "$CBASE/admin/api/v1/wasm-modules/smoke-guard" -H "authorization: Bearer $TOKEN" -H 'if-match: *'
fi
echo "--- Antwort-Cache (AIGW-39): zweite gleiche Anfrage mit temperature 0 = hit-exact"
for i in 1 2; do
  curl -fsS -D "$TMP/c$i.txt" -o "$TMP/c$i.json" "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
    -d '{"model":"mock-2","temperature":0,"messages":[{"role":"user","content":"Cache-Test"}]}'
done
grep -i '^x-aigw-cache' "$TMP/c1.txt" "$TMP/c2.txt"
grep -qi '^x-aigw-cache: miss' "$TMP/c1.txt"
grep -qi '^x-aigw-cache: hit-exact' "$TMP/c2.txt"
if grep -qi '^x-aigw-deployment' "$TMP/c2.txt"; then exit 1; fi
# Andere Modellgruppe: kein Header; globaler Deckel → 400 (OpenAI-Eingang; der Zähler der
# Anthropic-4xx unten erwartet genau einen Fehler).
curl -fsS -D "$TMP/p3.txt" -o /dev/null "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}'
if grep -qi '^x-smoke-plugin' "$TMP/p3.txt"; then exit 1; fi
CODE=$(curl -s -o "$TMP/p4.json" -w '%{http_code}' "$BASE/v1/chat/completions" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"mock","max_tokens":5000,"messages":[{"role":"user","content":"Hallo"}]}')
cat "$TMP/p4.json"; echo
test "$CODE" = 400
grep -q '"invalid_request_error"' "$TMP/p4.json"
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
grep -q '^aigw_cache_lookups_total{result="hit-exact"} [1-9]' "$TMP/metrics.txt"
grep -q '^aigw_snapshot_stale_seconds 0$' "$TMP/metrics.txt"
grep -q '^# TYPE aigw_plugin_failmode_triggered_total counter' "$TMP/metrics.txt"
# Limits nach der Metrik-Prüfung (erzeugt weitere 4xx).
echo "--- Limits (AIGW-30): Key mit 1 Anfrage/Minute → zweite Anfrage 429 mit Rate-Limit-Headern"
RL=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"metadata":{"name":"smoke-rl"},"spec":{"organization":"smoke-org","limits":[{"metric":"requests","limit":1,"window":"minute"}],"budget":{"amount":"5","period":"month","warnAtPercent":[80]}}}' |
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
echo "--- Kalender-Limit (F25): 1 Anfrage je Kalendertag → 429 bis Mitternacht; Limit-Hinweise (F29)"
CAL=$(curl -fsS "$CBASE/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"metadata":{"name":"smoke-cal"},"spec":{"organization":"smoke-org","limits":[{"metric":"requests","limit":1,"window":"calendar-day"}]}}' |
  grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
test -n "$CAL"
CODE=000
for _ in $(seq 1 50); do # Hot Reload abwarten
  CODE=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $CAL" -H 'content-type: application/json' \
    -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}')
  [[ "$CODE" != 401 ]] && break
  sleep 0.1
done
test "$CODE" = 200
CODE=$(curl -s -D "$TMP/h5.txt" -o /dev/null -w '%{http_code}' "$BASE/v1/chat/completions" \
  -H "authorization: Bearer $CAL" -H 'content-type: application/json' \
  -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}')
test "$CODE" = 429
grep -qi '^retry-after: [0-9]' "$TMP/h5.txt"
grep -qi '^x-ratelimit-reset-requests: ' "$TMP/h5.txt"
curl -fsS "$CBASE/admin/api/v1/usage/limit-warnings" -H "authorization: Bearer $TOKEN" |
  tee "$TMP/lw.json"; echo
grep -q '"counterBackend":"local"' "$TMP/lw.json"
echo "--- MCP-Gateway (AIGW-41): Server per REST (SSRF → 422), /mcp initialize + tools/list"
adm POST /mcp-servers '{"metadata":{"name":"smoke-meta"},"spec":{"url":"http://169.254.169.254/mcp"}}' >/dev/null
expect 422
adm POST /mcp-servers '{"metadata":{"name":"smoke-mcp"},"spec":{"url":"https://mcp.example.org/mcp","enabled":false}}'; echo
expect 201
curl -fsS -D "$TMP/m1.txt" -o "$TMP/m1.json" "$BASE/mcp" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}'
cat "$TMP/m1.json"; echo
grep -q '"protocolVersion":"2025-06-18"' "$TMP/m1.json"
MCP_SID=$(grep -i '^mcp-session-id:' "$TMP/m1.txt" | cut -d' ' -f2 | tr -d '\r')
test -n "$MCP_SID"
curl -fsS "$BASE/mcp" -H "authorization: Bearer $KEY" -H "mcp-session-id: $MCP_SID" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -H 'mcp-protocol-version: 2025-06-18' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | tee "$TMP/m2.json"; echo
grep -q '"tools":\[\]' "$TMP/m2.json"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/mcp/unknown" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}')" = 404
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/mcp" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}')" = 401
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
echo "--- Secret (F149): Antwort ohne Wert, Webhook-/MCP-Verweis; MCP-Upstream-Status (AIGW-41-Folge)"
adm POST /secrets '{"metadata":{"name":"smoke-hmac"},"spec":{"description":"Smoke","data":{"value":{"value":"hmac-smoke-nicht-echt"}}}}' |
  tee "$TMP/secret.json"; echo
expect 201
if grep -q 'hmac-smoke-nicht-echt' "$TMP/secret.json"; then echo 'Geheimnis in Antwort' >&2; exit 1; fi
grep -q '"fields":\["value"\]' "$TMP/secret.json"
adm GET /mcp-servers:status | tee "$TMP/mcpstatus.json"; echo
expect 200
grep -q '"gatewayEnabled"' "$TMP/mcpstatus.json"
echo "--- Audit-Log (F115/F116): Arten kebab-case, Hash-Kette unversehrt (API und CLI)"
adm GET '/audit-log?kind=secret&limit=1' | grep -q '"kind":"secret"'
expect 200
adm GET /audit-log/verify | tee "$TMP/verify.json"; echo
expect 200
grep -q '"ok":true' "$TMP/verify.json"
AIGW_MASTER_KEY=$MASTER_KEY target/release/aigw audit verify --config "$TMP/aigw.yaml"
echo "--- GET /admin/api/v1/version";   curl -fsS "$CBASE/admin/api/v1/version" | tee "$TMP/version.json"; echo
grep -q '"snapshot":{' "$TMP/version.json"
grep -q '"source":"database"' "$TMP/version.json"
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

echo "--- Snapshot-Cache geschrieben (F14)"
for _ in $(seq 1 50); do
  [[ -s "$TMP/snapshot.cache" ]] && break
  sleep 0.1
done
test -s "$TMP/snapshot.cache"
head -c 8 "$TMP/snapshot.cache" | grep -q AIGWSNAP
if grep -q "$KEY" "$TMP/snapshot.cache"; then echo "FEHLER: Klartext im Cache" >&2; exit 1; fi
# Warten, bis der letzte Stand (inkl. aller Änderungen oben) im Cache steht.
sleep 2
kill "$PID"; wait "$PID" 2>/dev/null || true

echo "--- Kaltstart ohne Datenbank aus dem Cache → /readyz degraded, Anfragen laufen (F14/F15)"
sed "s#sqlite://$TMP/aigw.db#sqlite://$TMP/later/aigw.db#" "$TMP/aigw.yaml" >"$TMP/aigw-cold.yaml"
AIGW_MASTER_KEY=$MASTER_KEY AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
  target/release/aigw serve --config "$TMP/aigw-cold.yaml" &
PID=$!
for _ in $(seq 1 50); do
  curl -fsS "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 0.1
done
curl -fsS "$BASE/readyz" | tee "$TMP/readyz.json"; echo
grep -q '"status":"degraded"' "$TMP/readyz.json"
grep -q '"source":"cache"' "$TMP/readyz.json"
curl -fsS "$CBASE/admin/api/v1/version" | grep -q '"source":"cache"'
curl -fsS "$BASE/v1/models" -H "authorization: Bearer $KEY" | grep -q '"mock"'
echo "--- Datenbank wieder erreichbar → normaler Betrieb"
mkdir -p "$TMP/later"
for _ in $(seq 1 300); do
  curl -fsS "$BASE/readyz" 2>/dev/null | grep -q '"status":"ready"' && break
  sleep 0.1
done
curl -fsS "$BASE/readyz" | tee "$TMP/readyz.json"; echo
grep -q '"status":"ready"' "$TMP/readyz.json"
grep -q '"source":"database"' "$TMP/readyz.json"

echo "Laufzeitprüfung bestanden"
