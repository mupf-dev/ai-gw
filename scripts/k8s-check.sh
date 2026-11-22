#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Prüft die Kubernetes-Manifeste (AIGW-91): `kubectl kustomize deploy/kubernetes` rendert das
# kustomize-Grundgerüst, kubeconform validiert jedes Objekt streng gegen die OpenAPI-Schemas der
# Kubernetes-Version KUBE_VERSION (offline nach dem ersten Schema-Download, kein Cluster nötig).
# Helm-Chart deploy/helm/ai-gw (AIGW-119): `helm lint --strict` und `helm template` je
# Prüfvariante (deploy/helm/ai-gw/ci/*-values.yaml), kubeconform (CRDs wie ServiceMonitor aus
# dem datreeio/CRDs-catalog), erwartete Objekte und Ablehnung ungültiger Werte.
# Ist ein Cluster erreichbar (KUBE_SERVER_DRY_RUN=1), zusätzlich `kubectl apply --dry-run=server`.
# Fehlende Werkzeuge werden geprüft (SHA-256) nach ${XDG_CACHE_HOME:-~/.cache}/aigw-k8s geladen.
#
#   scripts/k8s-check.sh
set -euo pipefail
cd "$(dirname "$0")/.."

KUBE_VERSION=${KUBE_VERSION:-1.34.0}
KUBECTL_VERSION=${KUBECTL_VERSION:-v1.34.2}
KUBECONFORM_VERSION=v0.8.0
HELM_VERSION=v4.3.2
declare -A HELM_SHA256=(
  [amd64]=d1c0278d46beab79582d7e50f5b3dc206944ba8e577cde618513fd07daa53983
  [arm64]=df31cb61680ef728f82c770e5280dd565617bee6b039d37e85c677288be4bbb0
)
declare -A KUBECONFORM_SHA256=(
  [amd64]=9bc2bffbf71f261128533edaf912153948b7ff238f9a531ae6d34466ec287883
  [arm64]=1f53fc8e81258197a35e8603054162a5af1de8c5af13746c71ab680d9534ed87
)
BIN=${XDG_CACHE_HOME:-$HOME/.cache}/aigw-k8s
mkdir -p "$BIN"
export PATH="$BIN:$PATH"
case "$(uname -m)" in
  x86_64) ARCH=amd64 ;;
  aarch64 | arm64) ARCH=arm64 ;;
  *) echo "nicht unterstützte Architektur $(uname -m)" >&2; exit 1 ;;
esac

if ! command -v kubectl >/dev/null 2>&1; then
  base="https://dl.k8s.io/release/$KUBECTL_VERSION/bin/linux/$ARCH/kubectl"
  curl -fsSLo "$BIN/kubectl" "$base"
  echo "$(curl -fsSL "$base.sha256")  $BIN/kubectl" | sha256sum -c --quiet
  chmod +x "$BIN/kubectl"
fi
if ! command -v kubeconform >/dev/null 2>&1; then
  tgz="$BIN/kubeconform.tar.gz"
  curl -fsSLo "$tgz" \
    "https://github.com/yannh/kubeconform/releases/download/$KUBECONFORM_VERSION/kubeconform-linux-$ARCH.tar.gz"
  echo "${KUBECONFORM_SHA256[$ARCH]}  $tgz" | sha256sum -c --quiet
  tar -xzf "$tgz" -C "$BIN" kubeconform
  rm -f "$tgz"
fi

if ! command -v helm >/dev/null 2>&1; then
  tgz="$BIN/helm.tar.gz"
  curl -fsSLo "$tgz" "https://get.helm.sh/helm-$HELM_VERSION-linux-$ARCH.tar.gz"
  echo "${HELM_SHA256[$ARCH]}  $tgz" | sha256sum -c --quiet
  tar -xzf "$tgz" -C "$BIN" --strip-components=1 "linux-$ARCH/helm"
  rm -f "$tgz"
fi

OUT=$(mktemp)
trap 'rm -f "$OUT" "$OUT".*' EXIT
echo "--- kubectl kustomize deploy/kubernetes ($(kubectl version --client -o json | grep -m1 gitVersion | tr -d ' ,'))"
kubectl kustomize deploy/kubernetes >"$OUT"
grep -E '^kind:' "$OUT" | sort | uniq -c
echo "--- kubeconform (Kubernetes $KUBE_VERSION, strikt)"
# Über stdin: bei Dateipfaden filtert kubeconform nach Endung (.yaml/.json).
kubeconform -strict -summary -kubernetes-version "$KUBE_VERSION" -cache "$BIN" - <"$OUT" | tee "$OUT.sum"
grep -Eq 'Summary: [1-9][0-9]* resources? found.*Invalid: 0, Errors: 0, Skipped: 0' "$OUT.sum"
# Bild-Referenz aus kustomization.yaml übernommen?
grep -q 'image: registry.example.org/ai-gw:' "$OUT"
# Optionale Komponente tiktoken-fetch (AIGW-92): Grundgerüst + Init-Container.
COMP=$(mktemp -d)
trap 'rm -rf "$OUT" "$OUT.sum" "$COMP"' EXIT
cat >"$COMP/kustomization.yaml" <<YAML
resources: [$PWD/deploy/kubernetes]
components: [$PWD/deploy/kubernetes/components/tiktoken-fetch]
images: [{name: ai-gw, newName: registry.example.org/ai-gw, newTag: 0.1.0}]
YAML
echo "--- Komponente tiktoken-fetch"
kubectl kustomize --load-restrictor LoadRestrictionsNone "$COMP" >"$COMP/out.yaml"
grep -q 'name: tiktoken-fetch' "$COMP/out.yaml"
# Bild-Referenz auch im Init-Container (images: der eigenen kustomization.yaml)
[[ $(grep -c 'image: registry.example.org/ai-gw:' "$COMP/out.yaml") == 2 ]]
kubeconform -strict -summary -kubernetes-version "$KUBE_VERSION" -cache "$BIN" - <"$COMP/out.yaml"
if [[ "${KUBE_SERVER_DRY_RUN:-0}" == 1 ]]; then
  echo "--- kubectl apply --dry-run=server"
  kubectl apply --dry-run=server -f "$OUT"
fi

CHART=deploy/helm/ai-gw
CRDS='https://raw.githubusercontent.com/datreeio/CRDs-catalog/main/{{.Group}}/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json'
echo "--- Helm-Chart $CHART ($(helm version --short))"
for values in "$CHART"/ci/*-values.yaml; do
  variant=$(basename "$values" -values.yaml)
  echo "--- helm lint/template: $variant"
  helm lint --strict "$CHART" -f "$values" --quiet
  helm template aigw "$CHART" -n aigw -f "$values" --kube-version "$KUBE_VERSION" >"$OUT.$variant"
  grep -E '^kind:' "$OUT.$variant" | sort | uniq -c
  kubeconform -strict -summary -kubernetes-version "$KUBE_VERSION" -cache "$BIN" \
    -schema-location default -schema-location "$CRDS" - <"$OUT.$variant" | tee "$OUT.sum"
  grep -Eq 'Summary: [1-9][0-9]* resources? found.*Invalid: 0, Errors: 0, Skipped: 0' "$OUT.sum"
done
# Erwartete Objekte je Variante.
has() { grep -q "^kind: $2\$" "$OUT.$1" || { echo "FEHLER: $1 ohne $2" >&2; exit 1; }; }
hasnt() { ! grep -q "^kind: $2\$" "$OUT.$1" || { echo "FEHLER: $1 mit $2" >&2; exit 1; }; }
has sqlite PersistentVolumeClaim; hasnt sqlite PodDisruptionBudget; hasnt sqlite Ingress
grep -q 'type: Recreate' "$OUT.sqlite"
grep -q 'image: "ghcr.io/mupf-dev/ai-gw:0.1.0"' "$OUT.sqlite"
has production PodDisruptionBudget; has production Ingress; has production ServiceMonitor
hasnt production PersistentVolumeClaim
grep -q 'type: RollingUpdate' "$OUT.production"
grep -q 'name: AIGW_STORE__URL' "$OUT.production"
grep -q 'image: "ghcr.io/mupf-dev/ai-gw@sha256:' "$OUT.production"
# Ungültige Werte müssen mit verständlicher Meldung abbrechen.
reject() { # <erwartete Meldung> <helm-Argumente…>
  local msg=$1; shift
  if helm template aigw "$CHART" "$@" >"$OUT.neg" 2>&1; then
    echo "FEHLER: helm template $* wurde nicht abgelehnt" >&2; exit 1
  fi
  grep -q "$msg" "$OUT.neg" || { cat "$OUT.neg" >&2; exit 1; }
}
reject 'masterKey.existingSecret fehlt'
reject 'SQLite erlaubt nur replicaCount: 1' --set masterKey.existingSecret=k --set replicaCount=2
echo "OK: Kubernetes-Manifeste und Helm-Chart gültig"
