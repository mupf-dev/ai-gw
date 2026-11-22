#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Kaltstart-Test (AIGW-57): Datenbank mit vielen Ressourcen füllen (Standard 10 000 API-Keys,
# 1 000 Modellgruppen mit je 2 Deployments, 10 Teams) und messen, wie lange `aigw serve` bis
# `/readyz` = 200 braucht und wie lange das Laden des Snapshots dauert. Postgres läuft als
# Wegwerf-Container (Docker); zum Vergleich dasselbe mit SQLite.
# Doku und Messwerte: docs/betrieb/leistung.md.
#
# Umgebung: KEYS, MODEL_GROUPS, RUNS (Starts je Datenbank, Standard 3), DBS ("postgres sqlite"),
# OUT (Standard target/bench/coldstart.txt).
set -euo pipefail
cd "$(dirname "$0")/.."

KEYS=${KEYS:-10000}
MODEL_GROUPS=${MODEL_GROUPS:-1000}
RUNS=${RUNS:-3}
DBS=${DBS:-"postgres sqlite"}
OUT=${OUT:-target/bench/coldstart.txt}
PORT=9300
CPORT=9301

cargo build --release -p aigw
AIGW=target/release/aigw
mkdir -p "$(dirname "$OUT")"
TMP=$(mktemp -d)
PG=""
GW=""
cleanup() {
  [[ -n "$GW" ]] && { kill "$GW" 2>/dev/null || true; wait "$GW" 2>/dev/null || true; }
  [[ -n "$PG" ]] && docker rm -f "$PG" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT
export AIGW_MASTER_KEY
AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')

# Manifeste erzeugen (Key-Klartexte als Dateien; Format sk-aigw-<12>-<43>). Ein Manifest darf
# höchstens 1024 YAML-Dokumente enthalten (Grenze des YAML-Parsers) – daher Teildateien zu je
# 1000 Dokumenten, nacheinander angewendet (Abhängigkeiten zuerst).
mkdir -p "$TMP/keys" "$TMP/manifests"
awk -v keys="$KEYS" -v groups="$MODEL_GROUPS" -v dir="$TMP/keys" -v out="$TMP/manifests" '
function emit(text) {
  f = sprintf("%s/%05d.yaml", out, int(n / 1000))
  if (f != cur) { if (cur != "") close(cur); cur = f }
  printf "---\n%s", text > f
  n++
}
BEGIN {
  emit("apiVersion: aigw/v1alpha1\nkind: Organization\nmetadata: {name: org}\nspec:\n  budget: {amount: \"100000\", period: month}\n")
  for (t = 0; t < 10; t++)
    emit(sprintf("apiVersion: aigw/v1alpha1\nkind: Team\nmetadata: {name: team-%d}\nspec:\n  organization: org\n  limits:\n    - {metric: tokens, limit: 1000000, window: minute}\n  budget: {amount: \"1000\", period: month}\n", t))
  for (g = 0; g < groups; g++) {
    emit(sprintf("apiVersion: aigw/v1alpha1\nkind: ModelGroup\nmetadata: {name: model-%d}\nspec:\n  aliases: [model-%d-latest]\n", g, g))
    for (d = 0; d < 2; d++)
      emit(sprintf("apiVersion: aigw/v1alpha1\nkind: Deployment\nmetadata: {name: model-%d-dep-%d}\nspec:\n  modelGroup: model-%d\n  provider: openai-compatible\n  endpoint: \"http://127.0.0.1:9100/v1\"\n  providerModel: m%d\n  capacity: {rpm: 1000}\n", g, d, g, g))
  }
  for (k = 0; k < keys; k++) {
    kf = sprintf("%s/%d", dir, k)
    printf "sk-aigw-c%011d-%032d%011d", k, 0, k > kf
    close(kf)
    emit(sprintf("apiVersion: aigw/v1alpha1\nkind: ApiKey\nmetadata: {name: key-%d}\nspec:\n  organization: org\n  team: team-%d\n  allowedModels: [model-%d, model-%d]\n  limits:\n    - {metric: requests, limit: 600, window: minute}\n  secret:\n    valueFrom: {file: \"%s\"}\n", k, k % 10, k % groups, (k + 1) % groups, kf))
  }
}'

write_config() { # write_config <store-url>
  cat >"$TMP/aigw.yaml" <<YAML
server:
  data: {listen: "127.0.0.1:$PORT"}
  control: {listen: "127.0.0.1:$CPORT"}
store:
  url: "$1"
log:
  level: info
  format: json
admin:
  auth: {bootstrap: "off"}
  allow_insecure_upstreams: [127.0.0.1]
  allow_private_upstreams: [127.0.0.1]
YAML
}

# Millisekunden seit der Epoche (bash-eigen; `date +%3N` ist nicht überall Millisekunden).
ms() { echo $((${EPOCHREALTIME/./} / 1000)); }

measure() { # measure <name>
  local name=$1
  local t0
  t0=$(ms)
  local m
  for m in "$TMP"/manifests/*.yaml; do
    "$AIGW" config apply -c "$TMP/aigw.yaml" --api-keys include "$m" >/dev/null
  done
  echo "== $name: Apply in $(ls "$TMP"/manifests | wc -l) Teilen (${KEYS} Keys, ${MODEL_GROUPS} Modellgruppen, $((2 * MODEL_GROUPS)) Deployments): $(($(ms) - t0)) ms"
  for run in $(seq 1 "$RUNS"); do
    t0=$(ms)
    "$AIGW" serve --config "$TMP/aigw.yaml" >"$TMP/log.$run" 2>&1 &
    GW=$!
    until curl -fsS "http://127.0.0.1:$CPORT/readyz" >/dev/null 2>&1; do
      kill -0 "$GW" 2>/dev/null || { cat "$TMP/log.$run"; exit 1; }
      sleep 0.01
    done
    local ready=$(($(ms) - t0))
    local snap
    snap=$(grep 'Konfigurations-Snapshot geladen' "$TMP/log.$run" | head -1 |
      grep -oE '"?(keys|model_groups|load_ms)"?[=:] ?[0-9]+' | tr -d '"' | tr '\n' ' ' || true)
    local rss
    rss=$(awk '/^VmRSS/{printf "%.1f", $2/1024}' "/proc/$GW/status")
    echo "== $name: Start $run: /readyz nach $ready ms, RSS $rss MiB; Snapshot: $snap"
    kill "$GW"
    wait "$GW" 2>/dev/null || true
    GW=""
  done
}

exec > >(tee "$OUT") 2>&1
echo "# $(date -u +%FT%TZ) $(nproc) vCPU, $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), Last: $(cut -d" " -f1-3 /proc/loadavg)"
for db in $DBS; do
  case $db in
    postgres)
      PG=aigw-bench-pg-$$
      docker run -d --rm --name "$PG" -e POSTGRES_PASSWORD=bench -e POSTGRES_DB=aigw \
        -p 127.0.0.1:15432:5432 postgres:17-alpine >/dev/null
      until docker exec "$PG" pg_isready -U postgres -d aigw >/dev/null 2>&1; do sleep 0.2; done
      sleep 1
      write_config "postgres://postgres:bench@127.0.0.1:15432/aigw"
      measure postgres
      docker rm -f "$PG" >/dev/null
      PG=""
      ;;
    sqlite)
      write_config "sqlite://$TMP/aigw.db?mode=rwc"
      measure sqlite
      ;;
  esac
done
