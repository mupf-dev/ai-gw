#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Prüft die Kubernetes-Manifeste (AIGW-91): `kubectl kustomize deploy/kubernetes` rendert das
# kustomize-Grundgerüst, kubeconform validiert jedes Objekt streng gegen die OpenAPI-Schemas der
# Kubernetes-Version KUBE_VERSION (offline nach dem ersten Schema-Download, kein Cluster nötig).
# Ist ein Cluster erreichbar (KUBE_SERVER_DRY_RUN=1), zusätzlich `kubectl apply --dry-run=server`.
# Fehlende Werkzeuge werden geprüft (SHA-256) nach ${XDG_CACHE_HOME:-~/.cache}/aigw-k8s geladen.
#
#   scripts/k8s-check.sh
set -euo pipefail
cd "$(dirname "$0")/.."

KUBE_VERSION=${KUBE_VERSION:-1.34.0}
KUBECTL_VERSION=${KUBECTL_VERSION:-v1.34.2}
KUBECONFORM_VERSION=v0.8.0
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

OUT=$(mktemp)
trap 'rm -f "$OUT" "$OUT.sum"' EXIT
echo "--- kubectl kustomize deploy/kubernetes ($(kubectl version --client -o json | grep -m1 gitVersion | tr -d ' ,'))"
kubectl kustomize deploy/kubernetes >"$OUT"
grep -E '^kind:' "$OUT" | sort | uniq -c
echo "--- kubeconform (Kubernetes $KUBE_VERSION, strikt)"
# Über stdin: bei Dateipfaden filtert kubeconform nach Endung (.yaml/.json).
kubeconform -strict -summary -kubernetes-version "$KUBE_VERSION" -cache "$BIN" - <"$OUT" | tee "$OUT.sum"
grep -Eq 'Summary: [1-9][0-9]* resources? found.*Invalid: 0, Errors: 0, Skipped: 0' "$OUT.sum"
# Bild-Referenz aus kustomization.yaml übernommen?
grep -q 'image: registry.example.org/ai-gw:' "$OUT"
if [[ "${KUBE_SERVER_DRY_RUN:-0}" == 1 ]]; then
  echo "--- kubectl apply --dry-run=server"
  kubectl apply --dry-run=server -f "$OUT"
fi
echo "OK: Kubernetes-Manifeste gültig"
