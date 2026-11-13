#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Latenz-Benchmark des Gateways (AIGW-14/15, Ziele Q1/Q4): lokaler Fake-Upstream, Gateway
# und Lastgenerator als getrennte Prozesse auf getrennten Kernen (mindestens 8 vCPU).
# Ergebnis: target/bench/latency.txt. Doku: docs/konzept/06-streaming-und-anbieter.md.
set -euo pipefail
cd "$(dirname "$0")/.."

CONNS=${CONNS:-200}
REQUESTS=${REQUESTS:-40000}

cargo build --release --example latency -p aigw-proxy
B=target/release/examples/latency
# RT=1: Echtzeit-Priorität (SCHED_FIFO) für alle drei Prozesse – nötig auf einer geteilten
# Maschine, damit fremde Last (z. B. Compiler) die Messung nicht verfälscht.
if [[ "${RT:-0}" == 1 ]]; then
  B="chrt -f 20 $B"
fi
mkdir -p target/bench

THREADS=2 taskset -c 0,1 $B upstream 127.0.0.1:9100 &
UP=$!
THREADS=2 taskset -c 2,3 $B gateway 127.0.0.1:9200 127.0.0.1:9100 &
GW=$!
trap 'kill $UP $GW 2>/dev/null || true' EXIT
sleep 1

{
  echo "# $(date -u +%FT%TZ) $(nproc) vCPU, $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"
  echo "# Upstream: Kerne 0-1, Gateway: Kerne 2-3, Last: Kerne 4-7, RT=${RT:-0}, Last vorher: $(cut -d" " -f1-3 /proc/loadavg)"
  for mode in json stream; do
    echo "== direkt ($mode, $CONNS Verbindungen, Sättigung)"
    THREADS=4 taskset -c 4-7 $B load 127.0.0.1:9100 "$CONNS" "$REQUESTS" "$mode"
    echo "== Gateway ($mode, $CONNS Verbindungen, Sättigung)"
    THREADS=4 taskset -c 4-7 $B load 127.0.0.1:9200 "$CONNS" "$REQUESTS" "$mode"
  done
  for rate in 2000 10000; do
    for mode in json stream; do
      echo "== direkt ($mode, $CONNS Verbindungen, $rate req/s)"
      RATE=$rate THREADS=4 taskset -c 4-7 $B load 127.0.0.1:9100 "$CONNS" 20000 "$mode"
      echo "== Gateway ($mode, $CONNS Verbindungen, $rate req/s)"
      RATE=$rate THREADS=4 taskset -c 4-7 $B load 127.0.0.1:9200 "$CONNS" 20000 "$mode"
    done
  done
  for mode in json stream; do
    echo "== direkt ($mode, 1 Verbindung)"
    THREADS=1 taskset -c 4 $B load 127.0.0.1:9100 1 5000 "$mode"
    echo "== Gateway ($mode, 1 Verbindung)"
    THREADS=1 taskset -c 4 $B load 127.0.0.1:9200 1 5000 "$mode"
  done
} 2>&1 | tee target/bench/latency.txt
