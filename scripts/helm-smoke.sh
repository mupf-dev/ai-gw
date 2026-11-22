#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Smoke-Test des Helm-Charts deploy/helm/ai-gw in einem Wegwerf-Cluster (kind, AIGW-119):
#   Image bauen (außer SKIP_BUILD=1) und in kind laden → Secrets (Master-Key, Bootstrap-Token) →
#   `helm install` (Standard: SQLite auf PVC) → Rollout, /readyz, /healthz, Admin-UI eingebettet,
#   Organisation + Key + Mock-Modell, Chat über die Datenebene → Pod neu starten, Datenbank im
#   PVC erhalten → `helm upgrade` mit geänderter Konfiguration rollt neu aus → `helm uninstall`
#   behält das PVC (resource-policy keep). Der Cluster wird am Ende gelöscht.
# Braucht Docker; kind, kubectl und helm werden geprüft (SHA-256) nach
# ${XDG_CACHE_HOME:-~/.cache}/aigw-k8s geladen (kubectl/helm über scripts/k8s-check.sh).
#
#   scripts/helm-smoke.sh
#   IMAGE=ai-gw:release-amd64 SKIP_BUILD=1 scripts/helm-smoke.sh
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=${IMAGE:-ai-gw:helm-smoke}
KIND_VERSION=v0.33.0
declare -A KIND_SHA256=(
  [amd64]=aee6151561422756b764a4ae28e7f44cda5af5a9eead3cc9985112b1de8d8e0d
  [arm64]=20022bee6cfcd5086cb7234d218e3454e6090022f2a8f55d1fa7fcf42c3867a2
)
BIN=${XDG_CACHE_HOME:-$HOME/.cache}/aigw-k8s
export PATH="$BIN:$PATH"
case "$(uname -m)" in
  x86_64) ARCH=amd64 ;;
  aarch64 | arm64) ARCH=arm64 ;;
  *) echo "nicht unterstützte Architektur $(uname -m)" >&2; exit 1 ;;
esac
CLUSTER="aigw-helm-$$"
NS=aigw
REL=ai-gw
TMP=$(mktemp -d)
PF_PIDS=()
cleanup() {
  for p in "${PF_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

# Werkzeuge: kubectl/helm/kubeconform über k8s-check.sh (prüft dabei das Chart), kind hier.
scripts/k8s-check.sh >/dev/null
if ! command -v kind >/dev/null 2>&1; then
  curl -fsSLo "$BIN/kind" "https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-linux-$ARCH"
  echo "${KIND_SHA256[$ARCH]}  $BIN/kind" | sha256sum -c --quiet
  chmod +x "$BIN/kind"
fi
if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  docker build -t "$IMAGE" --build-arg VERSION=0.1.0-smoke --build-arg REVISION=smoke .
fi

echo "=== kind-Cluster $CLUSTER ($(kind version))"
kind create cluster --name "$CLUSTER" --wait 180s
kubectl cluster-info --context "kind-$CLUSTER" >/dev/null
kind load docker-image "$IMAGE" --name "$CLUSTER"
repo=${IMAGE%:*}
tag=${IMAGE##*:}

kubectl create namespace "$NS"
docker run --rm "$IMAGE" keygen >"$TMP/master-key"
TOKEN="aigw-admin-smoke-$(od -An -tx1 -N16 /dev/urandom | tr -d ' \n')"
kubectl -n "$NS" create secret generic aigw-master-key --from-file=master-key="$TMP/master-key"
kubectl -n "$NS" create secret generic aigw-bootstrap --from-literal=token="$TOKEN"
cat >"$TMP/values.yaml" <<EOF
image: {repository: "$repo", tag: "$tag", pullPolicy: Never}
masterKey: {existingSecret: aigw-master-key}
config:
  admin: {allow_mock: true}
extraEnv:
  - name: AIGW_BOOTSTRAP_TOKEN
    valueFrom: {secretKeyRef: {name: aigw-bootstrap, key: token}}
resources:
  requests: {cpu: 100m, memory: 128Mi}
EOF

echo "=== helm install (SQLite auf PVC)"
helm install "$REL" deploy/helm/ai-gw -n "$NS" -f "$TMP/values.yaml" --wait --timeout 5m
kubectl -n "$NS" get pods,svc,pvc

port_forward() { # <lokaler Port> <Service> <Port>
  kubectl -n "$NS" port-forward "svc/$2" "$1:$3" >"$TMP/pf-$1.log" 2>&1 &
  PF_PIDS+=($!)
  for _ in $(seq 1 50); do
    curl -s -o /dev/null "http://127.0.0.1:$1/healthz" && return 0
    sleep 0.2
  done
  cat "$TMP/pf-$1.log" >&2
  return 1
}
stop_forwards() {
  for p in "${PF_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  PF_PIDS=()
}
BASE=http://127.0.0.1:24100
CBASE=http://127.0.0.1:24101
forward() {
  port_forward 24100 "$REL-data" 4000
  port_forward 24101 "$REL-control" 4001
}
forward
curl -fsS "$BASE/healthz" >/dev/null
curl -fsS "$CBASE/readyz" | tee "$TMP/readyz.json"; echo
grep -q '"status":"ready"' "$TMP/readyz.json"
curl -fsS "$CBASE/admin/" | grep -qi '<script' || { echo "Admin-UI fehlt" >&2; exit 1; }

auth=(-H "authorization: Bearer $TOKEN")
curl -fsS "$CBASE/admin/api/v1/organizations" "${auth[@]}" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{}}' >/dev/null
key=$(curl -fsS "$CBASE/admin/api/v1/api-keys" "${auth[@]}" \
  -H 'content-type: application/json' -d '{"metadata":{"name":"smoke"},"spec":{"organization":"smoke"}}' |
  grep -o '"secret":"sk-aigw-[^"]*"' | cut -d'"' -f4)
test -n "$key"
printf '%s\n' 'apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: mock}
spec: {}
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: mock-echo}
spec: {modelGroup: mock, provider: mock, providerModel: mock}' |
  curl -fsS "$CBASE/admin/api/v1/config:apply" "${auth[@]}" -H 'content-type: application/yaml' \
    --data-binary @- >/dev/null
chat() {
  curl -s -o "$TMP/chat.json" -w '%{http_code}' "$BASE/v1/chat/completions" \
    -H "authorization: Bearer $key" -H 'content-type: application/json' \
    -d '{"model":"mock","messages":[{"role":"user","content":"Hallo"}]}'
}
for _ in $(seq 1 50); do [[ "$(chat)" == 200 ]] && break; sleep 0.2; done
grep -q 'Echo: Hallo' "$TMP/chat.json" || { cat "$TMP/chat.json" >&2; exit 1; }
echo "Chat über die Datenebene: ok"
test "$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/models")" = 401

echo "=== Pod neu starten: Datenbank im PVC bleibt erhalten"
stop_forwards
kubectl -n "$NS" rollout restart "deploy/$REL"
kubectl -n "$NS" rollout status "deploy/$REL" --timeout 3m
forward
for _ in $(seq 1 50); do [[ "$(chat)" == 200 ]] && break; sleep 0.2; done
grep -q 'Echo: Hallo' "$TMP/chat.json"
echo "Key und Modell nach Neustart: ok"

echo "=== helm upgrade mit geänderter Konfiguration rollt neu aus"
old=$(kubectl -n "$NS" get pods -l app.kubernetes.io/instance="$REL" -o jsonpath='{.items[0].metadata.name}')
stop_forwards
helm upgrade "$REL" deploy/helm/ai-gw -n "$NS" -f "$TMP/values.yaml" --set config.log.format=text \
  --wait --timeout 5m
kubectl -n "$NS" rollout status "deploy/$REL" --timeout 3m
new=$(kubectl -n "$NS" get pods -l app.kubernetes.io/instance="$REL" \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')
[[ "$new" != "$old" ]] || { echo "FEHLER: kein neuer Pod nach Konfigurationsänderung" >&2; exit 1; }
kubectl -n "$NS" get configmap "$REL" -o jsonpath='{.data.aigw\.yaml}' | grep -q 'format: text'
# Sicherheitskontext im laufenden Pod
kubectl -n "$NS" get pod "$new" -o jsonpath='{.spec.securityContext.runAsUser} {.spec.containers[0].securityContext.readOnlyRootFilesystem}' |
  grep -qx '65532 true'

echo "=== helm uninstall behält das PVC"
helm uninstall "$REL" -n "$NS" --wait
kubectl -n "$NS" get pvc "$REL-data" >/dev/null
echo "OK: Helm-Smoke-Test bestanden"
