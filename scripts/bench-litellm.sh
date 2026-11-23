#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Lastvergleich ai-gw vs. LiteLLM-Proxy unter gleichen Bedingungen (AIGW-102).
#
# Gleicher Fake-Upstream (Beispiel `latency upstream`, OpenAI-kompatibel, antwortet sofort) und
# gleicher Lastgenerator (`latency load`) wie scripts/bench-e2e.sh. Beide Gateways laufen als
# Container im Host-Netz auf denselben zwei Kernen (--cpuset-cpus "$GW_CPUS"), beide mit API-Key-Prüfung:
#
#   aigw-sqlite  ai-gw-Image (lokal gebaut), SQLite, Key mit Limits/Budgets, Preis, Request-Log an
#   aigw-pg      wie oben, Store Postgres 17
#   litellm      LiteLLM-Proxy, ohne DB, Prüfung gegen den Master-Key (einzige Key-Art ohne DB)
#   litellm-pg   LiteLLM-Proxy (litellm-database-Image) + Postgres 17, virtueller Key mit
#                Budget/RPM/TPM, Spend-Logs an (entspricht dem Request-Log von ai-gw)
#
# Ergänzung AIGW-107 (Mehrinstanz-Aufbau mit Redis):
#   aigw-pg-redis        wie aigw-pg, Zähler (Limits/Kapazität) in Redis 7 (counters.backend redis)
#   litellm-pg-w4        wie litellm-pg, aber 4 Worker auf 4 Kernen (2–5)
#   litellm-pg-redis-w4  wie litellm-pg-w4 + Redis 7 (router_settings redis_host/redis_port für
#                        RPM/TPM über Instanzen, use_redis_transaction_buffer für Spend-Updates)
#   Die *-w4-Varianten bekommen doppelt so viele Kerne wie ai-gw; die Last läuft dann nur auf
#   Kern 6 (für LiteLLM-Raten von wenigen hundert req/s ausreichend). Redis teilt sich Kern 7
#   mit Postgres.
#
# LiteLLM nach „Best Practices for Production“ (docs.litellm.ai/docs/proxy/prod): ein Container
# mit NUM_WORKERS = vCPU (hier 2), LITELLM_MODE=PRODUCTION, LITELLM_LOG=ERROR, set_verbose
# false, json_logs, proxy_batch_write_at 60, database_connection_pool_limit 10,
# disable_error_logs, request_timeout 600. Zusätzlich LITELLM_LOCAL_MODEL_COST_MAP=True (kein
# Nachladen der Preisliste aus dem Internet beim Start; ai-gw lädt ebenfalls nichts nach).
#
# Kerne: Upstream 0–1, Gateway 2–3, Last 4–6, Postgres 7 (≥ 8 vCPU, Buildbox).
# Gemessen je Variante: Kaltstart (erster Start bzw. Neustart bis Readiness), Speicher
# (docker stats: Leerlauf, Höchststand unter Last), Aufschlag bei geringer Last (1 Verbindung
# seriell, 10 Verbindungen mit 200 req/s; json und stream), Durchsatz/Fehler bei Sättigung mit
# 10/100/500 Verbindungen (json und stream), Gateway-CPU je Anfrage (cgroup), Image-Größe.
#
#   scripts/bench-litellm.sh                       # alles (≈ 30 Min., baut das ai-gw-Image)
#   VARIANTS="aigw-sqlite litellm" scripts/bench-litellm.sh
#   VARIANTS="aigw-pg-redis litellm-pg-w4 litellm-pg-redis-w4" OUT=target/bench/107-litellm.txt \
#     scripts/bench-litellm.sh                     # AIGW-107
#   BUILD_IMAGE=0 AIGW_IMAGE=ai-gw:dev-amd64 scripts/bench-litellm.sh
#
# Umgebung: LITELLM_TAG (fest gepinnt, Standard v1.108.8 = LTS), VARIANTS, CONCS ("10 100 500"),
# REQS_AIGW / REQS_LITELLM (Anfragen je Sättigungslauf), RT=1 (SCHED_FIFO für Upstream und Last),
# OUT (Standard target/bench/102-litellm.txt; Kurzfassung daneben *-summary.txt).
# Container heißen aigw102-*; sie werden am Ende (auch bei Abbruch) mit `docker rm -f` entfernt.
set -euo pipefail
cd "$(dirname "$0")/.."

LITELLM_TAG=${LITELLM_TAG:-v1.108.8}
LITELLM_IMAGE=ghcr.io/berriai/litellm:$LITELLM_TAG
LITELLM_DB_IMAGE=ghcr.io/berriai/litellm-database:$LITELLM_TAG
PG_IMAGE=postgres:17-alpine
REDIS_IMAGE=redis:7.4-alpine
AIGW_IMAGE=${AIGW_IMAGE:-aigw102-ai-gw:bench}
VARIANTS=${VARIANTS:-"aigw-sqlite aigw-pg litellm litellm-pg"}
CONCS=${CONCS:-"10 100 500"}
REQS_AIGW=${REQS_AIGW:-40000}
REQS_LITELLM=${REQS_LITELLM:-10000}
OUT=${OUT:-target/bench/102-litellm.txt}
SUM=${OUT%.txt}-summary.txt
WORKERS=2
UP_ADDR=127.0.0.1:9102
GW_PORT=9202
CTL_PORT=9203
PG_PORT=15532
REDIS_PORT=16379
GW_CTR=aigw102-gw
PG_CTR=aigw102-pg
REDIS_CTR=aigw102-redis
# Kerne je Variante (set_cores): Gateway-Container, Lastgenerator und dessen Threads.
GW_CPUS=2,3
LOAD_CPUS=4-6
LOAD_THREADS=3
GWA=127.0.0.1:$GW_PORT

cargo build --release -p aigw-proxy --example latency
B=target/release/examples/latency
RTP=()
if [[ "${RT:-0}" == 1 ]]; then RTP=(chrt -f 20); fi
ulimit -n 65536 2>/dev/null || ulimit -n "$(ulimit -Hn)"
mkdir -p "$(dirname "$OUT")"
TMP=$(mktemp -d)
chmod 0777 "$TMP"
UP=""
SAMPLER=""
cleanup() {
  if [[ -n "$SAMPLER" ]]; then kill "$SAMPLER" 2>/dev/null || true; fi
  if [[ -n "$UP" ]]; then kill "$UP" 2>/dev/null || true; fi
  wait 2>/dev/null || true
  docker rm -f "$GW_CTR" "$PG_CTR" "$REDIS_CTR" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

exec > >(tee "$OUT") 2>&1
: >"$SUM"
sum() { echo "$*" | tee -a "$SUM"; }

# --- Images -----------------------------------------------------------------------------
if [[ "${BUILD_IMAGE:-1}" == 1 ]]; then
  t=$(date +%s)
  docker build -q -t "$AIGW_IMAGE" . >/dev/null
  echo "# ai-gw-Image $AIGW_IMAGE gebaut in $(($(date +%s) - t)) s"
fi
for img in "$LITELLM_IMAGE" "$LITELLM_DB_IMAGE" "$PG_IMAGE" "$REDIS_IMAGE"; do
  docker pull -q "$img" >/dev/null
done
echo "# $(date -u +%FT%TZ) $(nproc) vCPU, $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), Last vorher: $(cut -d' ' -f1-3 /proc/loadavg)"
echo "# Upstream: Kerne 0-1, Gateway-Container: Kerne 2-3 (*-w4: 2-5), Last: Kerne 4-6 (*-w4: 6), Postgres/Redis: Kern 7, RT=${RT:-0}"
echo "# LiteLLM: $LITELLM_IMAGE / $LITELLM_DB_IMAGE ($(docker image inspect -f '{{index .RepoDigests 0}}' "$LITELLM_IMAGE" 2>/dev/null))"
echo "# ai-gw: $AIGW_IMAGE ($(docker image inspect -f '{{.Id}}' "$AIGW_IMAGE" | cut -c1-19))"
sum "## Image-Größen (entpackt laut docker image inspect / komprimiert docker save | gzip -1)"
for img in "$AIGW_IMAGE" "$LITELLM_IMAGE" "$LITELLM_DB_IMAGE"; do
  sum "image $img: $(($(docker image inspect -f '{{.Size}}' "$img") / 1048576)) MiB / $(($(docker save "$img" | gzip -1 | wc -c) / 1048576)) MiB"
done

# --- Hilfsfunktionen --------------------------------------------------------------------
# Speicher eines Containers in MiB wie `docker stats` (cgroup v2: memory.current ohne
# inactive_file); ohne cgroup-Datei Rückfall auf `docker stats`.
mem_mib() {
  local d
  d=$(cg_dir "$1")
  if [[ -r "$d/memory.current" ]]; then
    awk -v c="$(cat "$d/memory.current")" '/^inactive_file /{i=$2} END{printf "%.1f", (c - i) / 1048576}' "$d/memory.stat"
    return
  fi
  docker stats --no-stream --format '{{.MemUsage}}' "$1" | awk '{v=$1; u=v; gsub(/[0-9.]/,"",u); gsub(/[^0-9.]/,"",v)
    f=1; if(u=="GiB")f=1024; else if(u=="KiB"||u=="kB")f=1/1024; else if(u=="MB")f=0.9537; else if(u=="GB")f=953.7
    printf "%.1f", v*f}'
}
# RT=1: alle Prozesse/Threads des Containers mit SCHED_FIFO (wie Upstream und Last); später
# gestartete Prozesse (LiteLLM-Worker) erben die Klasse.
rt_container() {
  if [[ "${RT:-0}" == 1 ]]; then
    local pid
    while read -r pid; do chrt -a -f -p 20 "$pid" >/dev/null 2>&1 || true; done <"$(cg_dir "$1")/cgroup.procs"
  fi
}
cg_dir() { echo "/sys/fs/cgroup/system.slice/docker-$(docker inspect -f '{{.Id}}' "$1").scope"; }
cpu_usec() { awk '/^usage_usec/{print $2}' "$(cg_dir "$GW_CTR")/cpu.stat" 2>/dev/null || echo 0; }

# Lauf des Lastgenerators; Rohausgabe ins Protokoll, eine Kurzzeile in die Zusammenfassung.
# run_load <Variante> <Szenario> <Ziel> <Verbindungen> <Anfragen> <json|stream> [ENV=…]
run_load() {
  local v=$1 sc=$2 target=$3 c=$4 n=$5 mode=$6 o cores=$LOAD_CPUS th=$LOAD_THREADS c0 c1 s1="" peak=""
  shift 6
  [[ "$c" == 1 ]] && cores=${LOAD_CPUS%%[-,]*} th=1
  echo "== $v: $sc ($mode, $c Verbindungen, $n Anfragen)"
  c0=$(cpu_usec_if "$target")
  if [[ "${PEAK:-0}" == 1 ]]; then
    (while :; do mem_mib "$GW_CTR" >>"$TMP/peak"; echo >>"$TMP/peak"; sleep 0.1; done) &
    SAMPLER=$!
  fi
  o=$(env THREADS=$th WARMUP="${WARMUP:-5}" "$@" "${RTP[@]}" timeout 900 taskset -c "$cores" "$B" load "$target" "$c" "$n" "$mode" 2>&1) || o+=" (Abbruch/Timeout)"
  if [[ -n "$SAMPLER" ]]; then
    kill "$SAMPLER" 2>/dev/null; wait "$SAMPLER" 2>/dev/null || true; SAMPLER=""
    peak=" mem_peak=$(sort -n "$TMP/peak" | tail -1) MiB"
    rm -f "$TMP/peak"
  fi
  c1=$(cpu_usec_if "$target")
  echo "$o"
  local ok
  ok=$(grep -o '^[0-9]* Anfragen' <<<"$o" | cut -d' ' -f1)
  [[ "$c0" != 0 && -n "$ok" && "$ok" != 0 ]] && s1=" cpu=$(((c1 - c0) / ok)) µs/Anfrage"
  sum "$(printf '%-11s %-14s %-6s c=%-3s' "$v" "$sc" "$mode" "$c") $(grep -o '[0-9]* req/s' <<<"$o" | head -1) $(grep -o 'Fehler={[^}]*}' <<<"$o")$(grep -o ' (Abbruch/Timeout)' <<<"$o")" \
    "$(sed -E 's/= +/=/g' <<<"$o" | awk '/^ttfb/{printf "ttfb %s %s %s ", $2, $3, $4} /^gesamt/{printf "gesamt %s %s %s", $2, $3, $4}')$s1$peak"
}
cpu_usec_if() { if [[ "$1" == "$GWA" ]]; then cpu_usec; else echo 0; fi; }

wait_ready() { # wait_ready <URL> → Millisekunden seit $T0 oder Fehler nach 300 s
  local end=$((SECONDS + 300))
  until curl -fsS -o /dev/null "$1" 2>/dev/null; do
    ((SECONDS > end)) && return 1
    sleep 0.05
  done
  echo $((($(date +%s%N) - T0) / 1000000))
}

start_pg() {
  docker rm -f "$PG_CTR" >/dev/null 2>&1 || true
  docker run -d --name "$PG_CTR" --network host --cpuset-cpus 7 -e POSTGRES_PASSWORD=bench \
    -e POSTGRES_DB=aigw "$PG_IMAGE" -p "$PG_PORT" -c listen_addresses=127.0.0.1 >/dev/null
  until docker exec "$PG_CTR" pg_isready -h 127.0.0.1 -p "$PG_PORT" -U postgres -d aigw >/dev/null 2>&1; do sleep 0.2; done
  docker exec "$PG_CTR" createdb -h 127.0.0.1 -p "$PG_PORT" -U postgres litellm
}

start_redis() {
  docker rm -f "$REDIS_CTR" >/dev/null 2>&1 || true
  docker run -d --name "$REDIS_CTR" --network host --cpuset-cpus 7 "$REDIS_IMAGE" \
    redis-server --bind 127.0.0.1 --port "$REDIS_PORT" --save '' --appendonly no >/dev/null
  until docker exec "$REDIS_CTR" redis-cli -p "$REDIS_PORT" ping >/dev/null 2>&1; do sleep 0.2; done
}
# set_cores <Gateway-Kerne> <Last-Kerne> <Last-Threads> <LiteLLM-Worker>
set_cores() { GW_CPUS=$1 LOAD_CPUS=$2 LOAD_THREADS=$3 WORKERS=$4; }

# --- ai-gw ------------------------------------------------------------------------------
KEY="sk-aigw-bench0000001-$(printf 'B%.0s' $(seq 1 43))"
printf '%s' "$KEY" >"$TMP/key"
chmod 0644 "$TMP/key"
AIGW_MASTER_KEY=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
export AIGW_MASTER_KEY
aigw_config() { # aigw_config <store-url> [redis-url]
  local counters="backend: local"
  [[ -n "${2:-}" ]] && counters="backend: redis
  redis_url: \"$2\""
  cat >"$TMP/aigw.yaml" <<YAML
server:
  data: {listen: "127.0.0.1:$GW_PORT"}
  control: {listen: "127.0.0.1:$CTL_PORT"}
store:
  url: "$1"
counters:
  $counters
log:
  level: warn
observe:
  request_log: true
admin:
  auth: {bootstrap: "off"}
  allow_insecure_upstreams: [127.0.0.1]
  allow_private_upstreams: [127.0.0.1]
YAML
  # Gleiche Ressourcen wie scripts/bench-e2e.sh: Org/Team/Key mit Limits und Budgets, Preis.
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
    valueFrom: {file: "/bench/key"}
YAML
  docker run --rm --network host -v "$TMP:/bench" -e AIGW_MASTER_KEY "$AIGW_IMAGE" \
    config apply -c /bench/aigw.yaml --api-keys include /bench/manifest.yaml >/dev/null
}
aigw_run() {
  docker rm -f "$GW_CTR" >/dev/null 2>&1 || true
  T0=$(date +%s%N)
  docker run -d --name "$GW_CTR" --network host --cpuset-cpus "$GW_CPUS" -v "$TMP:/bench" \
    -e AIGW_MASTER_KEY "$AIGW_IMAGE" serve --config /bench/aigw.yaml >/dev/null
  wait_ready "http://127.0.0.1:$CTL_PORT/readyz"
}

# --- LiteLLM ----------------------------------------------------------------------------
LITELLM_MASTER_KEY="sk-aigw102-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
LITELLM_SALT_KEY="sk-salt-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
export LITELLM_MASTER_KEY LITELLM_SALT_KEY
cat >"$TMP/litellm.yaml" <<YAML
model_list:
  - model_name: bench
    litellm_params:
      model: openai/bench
      api_base: http://$UP_ADDR/v1
      api_key: fake-upstream-key
litellm_settings:
  request_timeout: 600
  set_verbose: false
  json_logs: true
general_settings:
  master_key: os.environ/LITELLM_MASTER_KEY
  proxy_batch_write_at: 60
  database_connection_pool_limit: 10
  disable_error_logs: true
YAML
# AIGW-107: mit Redis (Mehrinstanz-Betrieb nach „Best Practices for Production“).
sed -e 's/^general_settings:$/general_settings:\n  use_redis_transaction_buffer: true/' "$TMP/litellm.yaml" >"$TMP/litellm-redis.yaml"
cat >>"$TMP/litellm-redis.yaml" <<YAML
router_settings:
  redis_host: 127.0.0.1
  redis_port: $REDIS_PORT
YAML
chmod 0644 "$TMP/litellm.yaml" "$TMP/litellm-redis.yaml"
litellm_run() { # litellm_run <image> [DATABASE_URL] [Konfigdatei]
  local db=() cfg=${3:-$TMP/litellm.yaml}
  [[ -n "${2:-}" ]] && db=(-e "DATABASE_URL=$2")
  docker rm -f "$GW_CTR" >/dev/null 2>&1 || true
  T0=$(date +%s%N)
  docker run -d --name "$GW_CTR" --network host --cpuset-cpus "$GW_CPUS" \
    -v "$cfg:/app/config.yaml:ro" -e LITELLM_MASTER_KEY -e LITELLM_SALT_KEY \
    -e LITELLM_MODE=PRODUCTION -e LITELLM_LOG=ERROR -e LITELLM_LOCAL_MODEL_COST_MAP=True \
    "${db[@]}" "$1" --config /app/config.yaml --host 127.0.0.1 --port "$GW_PORT" \
    --num_workers "$WORKERS" >/dev/null
  wait_ready "http://$GWA/health/readiness"
}

# --- Ablauf -----------------------------------------------------------------------------
env THREADS=2 "${RTP[@]}" taskset -c 0,1 "$B" upstream "$UP_ADDR" &
UP=$!
sleep 0.5

sum "## direkt (Fake-Upstream ohne Gateway)"
for mode in json stream; do
  run_load direkt seriell "$UP_ADDR" 1 2000 "$mode"
  run_load direkt "200req/s" "$UP_ADDR" 10 2000 "$mode" RATE=200
done

for v in $VARIANTS; do
  sum "## $v"
  AUTH=""
  set_cores 2,3 4-6 3 2
  case $v in
    aigw-sqlite | aigw-pg | aigw-pg-redis)
      REQS=$REQS_AIGW
      if [[ $v == aigw-pg-redis ]]; then
        start_pg
        start_redis
        aigw_config "postgres://postgres:bench@127.0.0.1:$PG_PORT/aigw" "redis://127.0.0.1:$REDIS_PORT/0"
      elif [[ $v == aigw-pg ]]; then
        start_pg
        aigw_config "postgres://postgres:bench@127.0.0.1:$PG_PORT/aigw"
      else
        rm -f "$TMP"/aigw.db*
        aigw_config "sqlite:///bench/aigw.db?mode=rwc"
      fi
      start=(aigw_run)
      AUTH=$KEY
      ;;
    litellm)
      REQS=$REQS_LITELLM
      start=(litellm_run "$LITELLM_IMAGE")
      AUTH=$LITELLM_MASTER_KEY
      ;;
    litellm-pg | litellm-pg-w4)
      REQS=$REQS_LITELLM
      [[ $v == *-w4 ]] && set_cores 2-5 6 1 4
      start_pg
      start=(litellm_run "$LITELLM_DB_IMAGE" "postgresql://postgres:bench@127.0.0.1:$PG_PORT/litellm")
      ;;
    litellm-pg-redis-w4)
      REQS=$REQS_LITELLM
      set_cores 2-5 6 1 4
      start_pg
      start_redis
      start=(litellm_run "$LITELLM_DB_IMAGE" "postgresql://postgres:bench@127.0.0.1:$PG_PORT/litellm" "$TMP/litellm-redis.yaml")
      ;;
    *) echo "unbekannte Variante $v" >&2; exit 2 ;;
  esac
  sum "$v kerne: gateway=$GW_CPUS last=$LOAD_CPUS (Threads $LOAD_THREADS)$([[ $v == litellm* ]] && echo " worker=$WORKERS")"
  if ! ms=$("${start[@]}"); then
    sum "$v: Start fehlgeschlagen"; docker logs --tail 30 "$GW_CTR"; docker rm -f "$GW_CTR" "$PG_CTR" "$REDIS_CTR" >/dev/null 2>&1 || true; continue
  fi
  sum "$v kaltstart_erster_start=${ms} ms (mit leerer DB; LiteLLM inkl. Prisma-Migration)"
  if [[ $v == litellm-pg* ]]; then
    AUTH=$(curl -fsS "http://$GWA/key/generate" -H "authorization: Bearer $LITELLM_MASTER_KEY" \
      -H 'content-type: application/json' \
      -d '{"models":["bench"],"max_budget":10000,"rpm_limit":100000000,"tpm_limit":1000000000}' |
      grep -o '"key":"[^"]*"' | cut -d'"' -f4)
    if [[ -z "$AUTH" ]]; then sum "$v: /key/generate fehlgeschlagen"; docker rm -f "$GW_CTR" "$PG_CTR" "$REDIS_CTR" >/dev/null 2>&1 || true; continue; fi
  fi
  ms=$("${start[@]}") || ms="Timeout"
  sum "$v kaltstart_neustart=${ms} ms (DB vorhanden)"
  curl -fsS -o /dev/null "http://$GWA/v1/chat/completions" -H "authorization: Bearer $AUTH" \
    -H 'content-type: application/json' -d '{"model":"bench","messages":[{"role":"user","content":"hi"}]}'
  rt_container "$GW_CTR"
  sleep 2
  sum "$v mem_leerlauf=$(mem_mib "$GW_CTR") MiB"
  for mode in json stream; do
    run_load "$v" seriell "$GWA" 1 2000 "$mode" AUTH="$AUTH"
    run_load "$v" "200req/s" "$GWA" 10 2000 "$mode" RATE=200 AUTH="$AUTH"
  done
  for mode in json stream; do
    for c in $CONCS; do
      PEAK=1 run_load "$v" "Sättigung" "$GWA" "$c" "$REQS" "$mode" AUTH="$AUTH"
    done
  done
  sleep 2
  sum "$v mem_nach_last=$(mem_mib "$GW_CTR") MiB"
  if [[ $v == *redis* ]]; then
    sum "$v redis: $(docker exec "$REDIS_CTR" redis-cli -p "$REDIS_PORT" info stats | grep -E '^total_commands_processed' | tr -d '\r') keys=$(docker exec "$REDIS_CTR" redis-cli -p "$REDIS_PORT" dbsize | tr -d '\r')"
  fi
  docker rm -f "$GW_CTR" "$PG_CTR" "$REDIS_CTR" >/dev/null 2>&1 || true
done
echo "# Last danach: $(cut -d' ' -f1-3 /proc/loadavg)"
echo "# Zusammenfassung: $SUM"
