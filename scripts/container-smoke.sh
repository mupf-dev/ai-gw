#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Container-Smoke-Test (AIGW-59): baut das Image (außer SKIP_BUILD=1) und prüft
#   1. Einzelcontainer mit SQLite: schreibgeschütztes Root-FS, ohne Capabilities, nicht-root;
#      HEALTHCHECK (`aigw healthcheck`) wird healthy, /readyz, /healthz, Admin-UI eingebettet,
#      `aigw config check`, Key anlegen, Mock-Chat über die Datenebene, Neustart mit
#      erhaltener Datenbank;
#   2. deploy/compose/docker-compose.yaml mit Postgres + Redis + OpenTelemetry-Collector
#      (Profil otel): Start, healthy, Postgres als Datenbank, Redis als Zähler-Backend, Chat,
#      Traces kommen im Collector an.
# Nur Wegwerf-Schlüssel und -Passwörter; alles wird am Ende entfernt.
#
#   scripts/container-smoke.sh                 # Image ai-gw:smoke
#   IMAGE=ai-gw:dev SKIP_BUILD=1 scripts/container-smoke.sh
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=${IMAGE:-ai-gw:smoke}
PORT=${AIGW_SMOKE_PORT:-24000}
CPORT=${AIGW_SMOKE_CONTROL_PORT:-24001}
NAME="aigw-smoke-$$"
PROJECT="aigw-smoke-$$"
TMP=$(mktemp -d)
cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker volume rm -f "$NAME-data" >/dev/null 2>&1 || true
  if [[ -f "$TMP/compose/docker-compose.yaml" ]]; then
    (cd "$TMP/compose" && docker compose -p "$PROJECT" --profile otel down -v >/dev/null 2>&1) || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  docker build -t "$IMAGE" --build-arg VERSION=0.1.0-smoke --build-arg REVISION=smoke .
fi
echo "--- Image"
docker image inspect "$IMAGE" --format 'Größe: {{.Size}} Bytes, Benutzer {{.Config.User}}, Healthcheck {{json .Config.Healthcheck.Test}}'
docker image inspect "$IMAGE" --format '{{index .Config.Labels "org.opencontainers.image.licenses"}}' | grep -qx 'FSL-1.1-ALv2'
docker history "$IMAGE" --format '{{.Size}}\t{{.CreatedBy}}' | grep -E 'aigw|distroless' | head -3 || true
test "$(docker image inspect "$IMAGE" --format '{{.Config.User}}')" = "65532:65532"

wait_healthy() { # <container> <sekunden>
  local s
  for _ in $(seq 1 "$2"); do
    s=$(docker inspect --format '{{.State.Health.Status}}' "$1" 2>/dev/null || echo missing)
    [[ "$s" == healthy ]] && { echo "$1: healthy"; return 0; }
    sleep 1
  done
  echo "$1: nicht healthy ($s)" >&2
  docker logs "$1" 2>&1 | tail -30 >&2
  return 1
}

TOKEN="aigw-admin-smoke-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
MOCK_YAML='apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: mock}
spec: {}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: mock-echo}
spec: {modelGroup: mock, provider: mock, providerModel: mock}'

# Organisation, Key, Mock-Modell anlegen und einen Chat senden. <data-url> <control-url>
exercise() {
  local base=$1 cbase=$2 key
  curl -fsS "$cbase/admin/api/v1/organizations" -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{}}' >/dev/null
  key=$(curl -fsS "$cbase/admin/api/v1/api-keys" -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{"organization":"smoke"}}' |
    grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
  test -n "$key"
  printf '%s\n' "$MOCK_YAML" | curl -fsS "$cbase/admin/api/v1/config:apply" \
    -H "authorization: Bearer $TOKEN" -H 'content-type: application/yaml' --data-binary @- >/dev/null
  for _ in $(seq 1 50); do
    [[ "$(curl -s -o /dev/null -w '%{http_code}' "$base/v1/chat/completions" \
      -H "authorization: Bearer $key" -H 'content-type: application/json' \
      -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}')" == 200 ]] && break
    sleep 0.2
  done
  curl -fsS "$base/v1/chat/completions" -H "authorization: Bearer $key" \
    -H 'content-type: application/json' \
    -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}' | tee "$TMP/chat.json"
  echo
  grep -q 'Echo: Hallo' "$TMP/chat.json"
  test "$(curl -s -o /dev/null -w '%{http_code}' "$base/v1/models")" = 401
}

# --- 1. Einzelcontainer, SQLite ----------------------------------------------------------------
echo "=== 1. Einzelcontainer (SQLite, read-only)"
docker run --rm "$IMAGE" --version
docker run --rm "$IMAGE" keygen >"$TMP/master-key"
test "$(tr -d '\n' <"$TMP/master-key" | wc -c)" -eq 64
run_single() {
  docker run -d --name "$NAME" --read-only --cap-drop ALL --security-opt no-new-privileges \
    -p "127.0.0.1:$PORT:4000" -p "127.0.0.1:$CPORT:4001" \
    -e AIGW_MASTER_KEY="$(cat "$TMP/master-key")" -e AIGW_BOOTSTRAP_TOKEN="$TOKEN" \
    -e AIGW_LOG_FORMAT=json \
    -v "$NAME-data:/data" "$IMAGE" >/dev/null
}
run_single
wait_healthy "$NAME" 60
BASE="http://127.0.0.1:$PORT"
CBASE="http://127.0.0.1:$CPORT"
curl -fsS "$BASE/healthz" >/dev/null
curl -fsS "$CBASE/readyz" | tee "$TMP/readyz.json"; echo
grep -q '"status":"ready"' "$TMP/readyz.json"
docker exec "$NAME" aigw healthcheck
docker exec "$NAME" aigw config check
# Admin-UI eingebettet (nicht die Platzhalterseite ohne ui/dist)
curl -fsS "$CBASE/admin/" | grep -qi '<script' || { echo "Admin-UI fehlt" >&2; exit 1; }
exercise "$BASE" "$CBASE"
echo "--- Neustart: Datenbank im Volume bleibt erhalten"
docker rm -f "$NAME" >/dev/null
run_single
wait_healthy "$NAME" 60
curl -fsS "$CBASE/admin/api/v1/organizations/smoke" -H "authorization: Bearer $TOKEN" | grep -q '"smoke"'
echo "--- Prozess läuft als 65532, Root-FS schreibgeschützt"
docker inspect --format '{{.HostConfig.ReadonlyRootfs}} {{.Config.User}}' "$NAME" | grep -qx 'true 65532:65532'
docker rm -f "$NAME" >/dev/null
docker volume rm -f "$NAME-data" >/dev/null

# --- 2. Compose mit Postgres, Redis, OTel-Collector ---------------------------------------------
echo "=== 2. docker compose: Postgres + Redis + OTel-Collector"
mkdir -p "$TMP/compose/secrets" "$TMP/config/examples"
cp deploy/compose/docker-compose.yaml deploy/compose/otel-collector.yaml "$TMP/compose/"
cp -r config/examples/ha-postgres-redis "$TMP/config/examples/"
cp "$TMP/master-key" "$TMP/compose/secrets/aigw-master-key"
# Docker Compose übernimmt Besitzer/Rechte der Host-Datei (siehe docs/betrieb/installation.md).
chown 65532:65532 "$TMP/compose/secrets/aigw-master-key" 2>/dev/null ||
  chmod 0644 "$TMP/compose/secrets/aigw-master-key"
chmod 0400 "$TMP/compose/secrets/aigw-master-key" 2>/dev/null || true
# Der Compose-Pfad zur Startkonfiguration ist relativ (../../config/examples/…).
mkdir -p "$TMP/a/b"
mv "$TMP/compose" "$TMP/a/b/compose"
mv "$TMP/config" "$TMP/a/config"
COMPOSE_DIR="$TMP/a/b/compose"
ln -s "$COMPOSE_DIR" "$TMP/compose"
cat >"$COMPOSE_DIR/.env" <<EOF
POSTGRES_PASSWORD=smoke-$(od -An -tx1 -N12 /dev/urandom | tr -d ' \n')
AIGW_IMAGE=$IMAGE
AIGW_BOOTSTRAP_TOKEN=$TOKEN
AIGW_OTLP_ENDPOINT=http://otel-collector:4318
AIGW_DATA_PORT=$PORT
AIGW_CONTROL_PORT=$CPORT
EOF
# Nur für den Test: Mock-Anbieter erlauben (im Beispiel aus), Traces schnell exportieren.
cat >"$COMPOSE_DIR/docker-compose.override.yaml" <<'EOF'
services:
  aigw:
    environment:
      AIGW_ADMIN__ALLOW_MOCK: "true"
      AIGW_OBSERVE__OTLP__TRACES__FLUSH_INTERVAL_MS: "200"
EOF
(cd "$COMPOSE_DIR" && docker compose -p "$PROJECT" --profile otel up -d --quiet-pull)
CID=$(cd "$COMPOSE_DIR" && docker compose -p "$PROJECT" ps -q aigw)
wait_healthy "$CID" 120
curl -fsS "$CBASE/readyz" | tee "$TMP/readyz2.json"; echo
grep -q '"status":"ready"' "$TMP/readyz2.json"
docker logs "$CID" 2>&1 | grep -m1 'ai-gw gestartet'
docker exec "$CID" aigw config check | tee "$TMP/check.txt"
grep -q 'Datenbank: postgres' "$TMP/check.txt"
grep -qi 'Zähler: Redis' "$TMP/check.txt"
exercise "$BASE" "$CBASE"
echo "--- Daten in Postgres"
(cd "$COMPOSE_DIR" && docker compose -p "$PROJECT" exec -T postgres \
  psql -U aigw -d aigw -tAc "select count(*) from api_key") | grep -qx 1
echo "--- Redis erreichbar (Schlüssel ohne Limits: 0)"
(cd "$COMPOSE_DIR" && docker compose -p "$PROJECT" exec -T redis redis-cli dbsize)
echo "--- /metrics"
curl -fsS "$CBASE/metrics" | grep -m3 '^aigw_requests_total'
echo "--- Traces im Collector"
for _ in $(seq 1 30); do
  (cd "$COMPOSE_DIR" && docker compose -p "$PROJECT" logs otel-collector 2>&1) | grep -q 'Traces' && break
  sleep 1
done
(cd "$COMPOSE_DIR" && docker compose -p "$PROJECT" logs otel-collector 2>&1) | grep -m2 'Traces'
echo "--- Lesezugriff aufs Root-FS / Benutzer"
docker inspect --format '{{.HostConfig.ReadonlyRootfs}} {{.Config.User}}' "$CID" | grep -qx 'true 65532:65532'
echo "OK: Container-Smoke bestanden"
