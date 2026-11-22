#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# End-to-End-Last- und Latenztest mit dem echten `aigw`-Binary (AIGW-57, Ziele Q1/Q4):
# Fake-Upstream (Beispiel `latency upstream`), `aigw serve` mit SQLite, Key-Prüfung,
# Limits/Budgets (Org, Team, Key), Preis, Request-Log an, ohne Plugins, und Lastgenerator
# (`latency load`) als getrennte Prozesse auf getrennten Kernen (mindestens 8 vCPU).
# Doku und Messwerte: docs/betrieb/leistung.md.
#
#   scripts/bench-e2e.sh                 # alle Teile (Dauer ≈ 10 Min.)
#   PARTS="latency" scripts/bench-e2e.sh # nur Latenz; weitere Teile: concurrency, redis
#   PROFILE=1 PARTS=profile scripts/bench-e2e.sh  # perf-Profil unter Last (braucht perf)
#
# Umgebung: ALLOC=system (ohne mimalloc), RT=1 (SCHED_FIFO für alle Prozesse, auf geteilten Maschinen empfohlen),
# COUNTERS=local|redis (Zähler-Backend; redis braucht REDIS_URL oder Docker),
# CONNS (Standard 200), OUT (Ergebnisdatei, Standard target/bench/e2e.txt).
set -euo pipefail
cd "$(dirname "$0")/.."

PARTS=${PARTS:-"latency concurrency redis"}
CONNS=${CONNS:-200}
OUT=${OUT:-target/bench/e2e.txt}
UP_ADDR=127.0.0.1:9100
GW_PORT=9200
CTL_PORT=9201

# PROFILE=1: eigenes Zielverzeichnis mit Frame-Pointern und Zeilentabellen (für perf).
TD=target
if [[ "${PROFILE:-0}" == 1 ]]; then
  TD=target/prof
  export CARGO_TARGET_DIR=$TD RUSTFLAGS="-C force-frame-pointers=yes"
  export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only CARGO_PROFILE_RELEASE_STRIP=none
fi
# ALLOC=system: Binary ohne mimalloc (Vergleich des Allokators, AIGW-57).
if [[ "${ALLOC:-mimalloc}" == system ]]; then
  cargo build --release -p aigw --no-default-features
else
  cargo build --release -p aigw
fi
cargo build --release -p aigw-proxy --example latency
B=$TD/release/examples/latency
AIGW=$TD/release/aigw
RTP=()
if [[ "${RT:-0}" == 1 ]]; then
  RTP=(chrt -f 20)
fi
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
mkdir -p "$(dirname "$OUT")"
TMP=$(mktemp -d)
UP=""
GW=""
REDIS_CTR=""
cleanup() {
  kill $UP $GW 2>/dev/null || true
  wait $UP $GW 2>/dev/null || true
  [[ -n "$REDIS_CTR" ]] && docker rm -f "$REDIS_CTR" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

# Fake-Upstream starten (Umgebung CHUNKS/CHUNK_DELAY_MS siehe Beispiel).
start_upstream() {
  [[ -n "$UP" ]] && { kill "$UP"; wait "$UP" 2>/dev/null || true; }
  env THREADS=2 "$@" "${RTP[@]}" taskset -c 0,1 "$B" upstream "$UP_ADDR" &
  UP=$!
  sleep 0.5
}

# Konfiguration und Manifest (Org/Team/Key mit Limits und Budgets, Preis, ohne Plugins).
KEY="sk-aigw-bench0000001-$(printf 'B%.0s' $(seq 1 43))"
printf '%s' "$KEY" >"$TMP/key"
write_config() {
  local counters=$1
  cat >"$TMP/aigw.yaml" <<YAML
server:
  data: {listen: "127.0.0.1:$GW_PORT"}
  control: {listen: "127.0.0.1:$CTL_PORT"}
store:
  url: "sqlite://$TMP/aigw.db?mode=rwc"
counters:
  backend: $counters
${REDIS_URL:+  redis_url: "$REDIS_URL"}
log:
  level: warn
observe:
  request_log: true
admin:
  auth: {bootstrap: "off"}
  allow_insecure_upstreams: [127.0.0.1]
  allow_private_upstreams: [127.0.0.1]
YAML
}
cat >"$TMP/manifest.yaml" <<YAML
apiVersion: aigw/v1alpha1
kind: Organization
metadata: {name: bench}
spec:
  budget: {amount: "1000000", period: month}
  limits:
    - {metric: requests, limit: 100000000, window: minute}
---
apiVersion: aigw/v1alpha1
kind: Team
metadata: {name: bench}
spec:
  organization: bench
  limits:
    - {metric: tokens, limit: 1000000000, window: minute}
    - {metric: requests, limit: 10000000, window: second, modelGroup: bench}
  budget: {amount: "100000", period: month}
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: bench}
spec: {}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: bench}
spec:
  modelGroup: bench
  provider: openai-compatible
  endpoint: "http://$UP_ADDR/v1"
  providerModel: bench
---
apiVersion: aigw/v1alpha1
kind: Price
metadata: {name: bench}
spec:
  provider: openai-compatible
  model: bench
  validFrom: 2024-01-01T00:00:00Z
  perMillionTokens: {input: "1", output: "2"}
---
apiVersion: aigw/v1alpha1
kind: ApiKey
metadata: {name: bench}
spec:
  organization: bench
  team: bench
  allowedModels: [bench]
  limits:
    - {metric: requests, limit: 100000000, window: minute}
    - {metric: concurrency, limit: 100000}
  budget: {amount: "10000", period: day}
  secret:
    valueFrom: {file: "$TMP/key"}
YAML

export AIGW_MASTER_KEY
AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')

# Gateway starten (Kerne 2–3) und auf /readyz warten.
start_gateway() {
  [[ -n "$GW" ]] && { kill "$GW" 2>/dev/null; wait "$GW" 2>/dev/null || true; }
  rm -f "$TMP"/aigw.db*
  "$AIGW" config apply -c "$TMP/aigw.yaml" --api-keys include "$TMP/manifest.yaml" >/dev/null
  "${RTP[@]}" taskset -c 2,3 "$AIGW" serve --config "$TMP/aigw.yaml" &
  GW=$!
  for _ in $(seq 1 100); do
    curl -fsS "http://127.0.0.1:$CTL_PORT/readyz" >/dev/null 2>&1 && break
    sleep 0.1
  done
  curl -fsS "http://127.0.0.1:$GW_PORT/v1/models" -H "authorization: Bearer $KEY" >/dev/null
}

# Speicher des Gateways (RSS aktuell / Höchststand) in MiB.
rss() {
  awk '/^VmRSS/{r=$2} /^VmHWM/{h=$2} END{printf "RSS=%.1f MiB, Höchststand=%.1f MiB", r/1024, h/1024}' "/proc/$GW/status"
}

# CPU-Zeit des Gateways (user+system) in Sekunden.
cpu() {
  awk -v t="$(getconf CLK_TCK)" '{printf "%.1f", ($14 + $15) / t}' "/proc/$GW/stat"
}

load() { # load <Ziel> <Verbindungen> <Anfragen> <json|stream> [ENV=… …]
  local target=$1 conns=$2 n=$3 mode=$4
  shift 4
  env THREADS=4 "$@" "${RTP[@]}" taskset -c 4-7 "$B" load "$target" "$conns" "$n" "$mode"
}
GWA=127.0.0.1:$GW_PORT

part_latency() {
  start_upstream
  start_gateway
  echo "## Latenz (Zähler: ${COUNTERS:-local}), $CONNS Verbindungen"
  for mode in json stream; do
    for fmt in openai anthropic; do
      echo "== Gateway ($mode, $fmt, $CONNS Verbindungen, Sättigung)"
      load "$GWA" "$CONNS" 40000 "$mode" AUTH="$KEY" FORMAT=$fmt
    done
    echo "== direkt ($mode, $CONNS Verbindungen, Sättigung)"
    load "$UP_ADDR" "$CONNS" 40000 "$mode"
  done
  for rate in 2000 10000; do
    for mode in json stream; do
      echo "== direkt ($mode, $CONNS Verbindungen, $rate req/s)"
      load "$UP_ADDR" "$CONNS" 20000 "$mode" RATE=$rate
      for fmt in openai anthropic; do
        echo "== Gateway ($mode, $fmt, $CONNS Verbindungen, $rate req/s)"
        load "$GWA" "$CONNS" 20000 "$mode" RATE=$rate AUTH="$KEY" FORMAT=$fmt
      done
    done
  done
  for mode in json stream; do
    echo "== direkt ($mode, 1 Verbindung)"
    env THREADS=1 "${RTP[@]}" taskset -c 4 "$B" load "$UP_ADDR" 1 5000 "$mode"
    for fmt in openai anthropic; do
      echo "== Gateway ($mode, $fmt, 1 Verbindung)"
      env THREADS=1 AUTH="$KEY" FORMAT=$fmt "${RTP[@]}" taskset -c 4 "$B" load "$GWA" 1 5000 "$mode"
    done
  done
  echo "== Speicher nach dem Latenzteil: $(rss)"
  # Vergleich: Beispiel-Gateway (nur Proxy-Kern: kein Key, keine Limits, kein Request-Log)
  # auf denselben Kernen, um den Anteil der vollen Kette zu sehen.
  kill "$GW"; wait "$GW" 2>/dev/null || true
  env THREADS=2 "${RTP[@]}" taskset -c 2,3 "$B" gateway "$GWA" "$UP_ADDR" &
  GW=$!
  sleep 0.5
  for mode in json stream; do
    echo "== Beispiel-Gateway ($mode, $CONNS Verbindungen, Sättigung)"
    load "$GWA" "$CONNS" 40000 "$mode"
    echo "== Beispiel-Gateway ($mode, $CONNS Verbindungen, 2000 req/s)"
    load "$GWA" "$CONNS" 20000 "$mode" RATE=2000
    echo "== Beispiel-Gateway ($mode, 1 Verbindung)"
    env THREADS=1 "${RTP[@]}" taskset -c 4 "$B" load "$GWA" 1 5000 "$mode"
  done
  kill "$GW"; wait "$GW" 2>/dev/null || true
  GW=""
}

part_concurrency() {
  # Lange Streams: 200 Chunks im Abstand von 5 ms (≈ 1 s je Antwort), 1000 bzw. 4000
  # gleichzeitige Verbindungen; Speicher nach jeder Runde (Leck-Suche: RSS muss sich nach
  # der ersten Runde stabilisieren).
  start_upstream CHUNKS=200 CHUNK_DELAY_MS=5
  start_gateway
  echo "## Hohe Parallelität, lange Streams (200 Chunks × 5 ms)"
  echo "== direkt: 1000 Verbindungen, 10000 Streams (Grenze des Fake-Upstreams)"
  load "$UP_ADDR" 1000 10000 stream WARMUP=0
  echo "== Speicher vor dem Test: $(rss)"
  for round in 1 2 3; do
    echo "== Runde $round: 1000 Verbindungen, 10000 Streams"
    local c0
    c0=$(cpu)
    load "$GWA" 1000 10000 stream AUTH="$KEY" WARMUP=0
    echo "== Speicher nach Runde $round: $(rss); Gateway-CPU: $(awk -v a="$c0" -v b="$(cpu)" 'BEGIN{print b - a}') s"
  done
  echo "== 4000 Verbindungen, 8000 Streams"
  c0=$(cpu)
  load "$GWA" 4000 8000 stream AUTH="$KEY" WARMUP=0
  echo "== Speicher danach: $(rss); Gateway-CPU: $(awk -v a="$c0" -v b="$(cpu)" 'BEGIN{print b - a}') s"
  echo "== 1000 Verbindungen, JSON-Sättigung, 200000 Anfragen (kurze Antworten)"
  start_upstream
  load "$GWA" 1000 200000 json AUTH="$KEY" WARMUP=0
  echo "== Speicher danach: $(rss)"
  sleep 2
  echo "== Speicher nach 2 s Leerlauf: $(rss)"
}

# perf-Profil des Gateways je Modus (10 000 req/s, 200 Verbindungen, 8 s); Berichte nach
# $(dirname $OUT)/perf-<modus>.txt (eigene Zeit je Funktion und inklusive Zeit).
part_profile() {
  start_upstream
  start_gateway
  local dir
  dir=$(dirname "$OUT")
  for mode in json stream; do
    echo "== Profil ($mode, openai, 10000 req/s)"
    load "$GWA" "$CONNS" 120000 "$mode" RATE=10000 AUTH="$KEY" WARMUP=5 &
    local lp=$!
    sleep 1
    perf record -q -F 1999 -g -p "$GW" -o "$TMP/perf.data" -- sleep 8 2>/dev/null
    wait $lp
    {
      echo "### eigene Zeit"
      perf report -i "$TMP/perf.data" --no-children --sort dso,symbol --stdio -g none --percent-limit 0.3 2>/dev/null | grep -v '^#' | grep -v '^$'
      echo "### inklusive Zeit"
      perf report -i "$TMP/perf.data" --children --sort symbol --stdio -g none --percent-limit 1.5 2>/dev/null | grep -v '^#' | grep -v '^$'
    } >"$dir/perf-$mode.txt"
    echo "Profil: $dir/perf-$mode.txt"
  done
}

part_redis() {
  if [[ -z "${REDIS_URL:-}" ]]; then
    REDIS_CTR=aigw-bench-redis-$$
    # Host-Netz (kein docker-proxy dazwischen), eigener Kern (5) wie ein separater Redis-Host.
    docker run -d --rm --name "$REDIS_CTR" --network host --cpuset-cpus 5 redis:7-alpine \
      redis-server --port 16379 --bind 127.0.0.1 --save "" --appendonly no >/dev/null
    REDIS_URL=redis://127.0.0.1:16379/0
    for _ in $(seq 1 50); do
      docker exec "$REDIS_CTR" redis-cli ping 2>/dev/null | grep -q PONG && break
      sleep 0.2
    done
  fi
  write_config redis
  start_upstream
  start_gateway
  echo "## Zähler-Backend redis ($REDIS_URL)"
  for mode in json stream; do
    echo "== Gateway ($mode, openai, $CONNS Verbindungen, Sättigung, redis)"
    load "$GWA" "$CONNS" 40000 "$mode" AUTH="$KEY"
    echo "== Gateway ($mode, openai, $CONNS Verbindungen, 2000 req/s, redis)"
    load "$GWA" "$CONNS" 20000 "$mode" RATE=2000 AUTH="$KEY"
  done
  echo "== Gateway (json, openai, 1 Verbindung, redis)"
  env THREADS=1 AUTH="$KEY" "${RTP[@]}" taskset -c 4 "$B" load "$GWA" 1 5000 json
  write_config "${COUNTERS:-local}"
}

write_config "${COUNTERS:-local}"
# Ausgabe zusätzlich in $OUT (ohne Subshell, damit cleanup die Prozesse kennt).
exec > >(tee "$OUT") 2>&1
echo "# $(date -u +%FT%TZ) $(nproc) vCPU, $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"
echo "# Upstream: Kerne 0-1, Gateway (aigw serve, Allokator ${ALLOC:-mimalloc}): Kerne 2-3, Last: Kerne 4-7, RT=${RT:-0}, Last vorher: $(cut -d" " -f1-3 /proc/loadavg)"
for p in $PARTS; do
  "part_$p"
done
