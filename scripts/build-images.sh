#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Baut Container-Images je Architektur (AIGW-91). Das Rust-Binary wird auf der Build-Plattform
# quer kompiliert (Dockerfile, Stufe build) – arm64 braucht auf einem amd64-Builder kein QEMU.
#
#   scripts/build-images.sh                         # linux/amd64 + linux/arm64 → ai-gw:dev-<arch>
#   PLATFORMS=linux/arm64 TAG=0.1.0 scripts/build-images.sh
#   SAVE_DIR=dist scripts/build-images.sh           # zusätzlich dist/ai-gw-<tag>-<arch>.tar (docker save)
#   SMOKE=1 scripts/build-images.sh                 # danach je Image scripts/container-smoke.sh;
#                                                   # fremde Architekturen laufen dabei unter QEMU
#                                                   # (binfmt wird bei Bedarf registriert)
#   PUSH=registry.example.org/ai-gw scripts/build-images.sh
#                                                   # Multi-Arch-Manifest bauen und pushen
#
# Umgebung: IMAGE (Standard ai-gw), TAG (dev), VERSION/REVISION (Build-Args), PLATFORMS.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=${IMAGE:-ai-gw}
TAG=${TAG:-dev}
PLATFORMS=${PLATFORMS:-linux/amd64,linux/arm64}
VERSION=${VERSION:-0.1.0-dev}
REVISION=${REVISION:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}
CREATED=$(date -u +%Y-%m-%dT%H:%M:%SZ)
ARGS=(--build-arg "VERSION=$VERSION" --build-arg "REVISION=$REVISION" --build-arg "CREATED=$CREATED")

if [[ -n "${PUSH:-}" ]]; then
  # Multi-Arch-Manifest braucht einen Builder mit containerd-Image-Store oder docker-container-Treiber.
  docker buildx build --platform "$PLATFORMS" "${ARGS[@]}" -t "$PUSH:$TAG" --push .
  docker buildx imagetools inspect "$PUSH:$TAG"
  exit 0
fi

native=$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')
IFS=, read -ra plats <<<"$PLATFORMS"
for plat in "${plats[@]}"; do
  arch=${plat#linux/}
  ref="$IMAGE:$TAG-$arch"
  echo "=== $plat → $ref"
  start=$(date +%s)
  docker buildx build --platform "$plat" "${ARGS[@]}" -t "$ref" --load .
  echo "Bauzeit $plat: $(($(date +%s) - start)) s"
  test "$(docker image inspect "$ref" --format '{{.Os}}/{{.Architecture}}')" = "$plat"
  # .Size ist je nach Image-Store (containerd: komprimiert + entpackt) nicht vergleichbar;
  # daher Binary-Größe und komprimierte Größe (docker save | gzip) ausgeben.
  cid=$(docker create --platform "$plat" "$ref")
  bin=$(docker cp "$cid:/usr/local/bin/aigw" - | wc -c)
  docker rm "$cid" >/dev/null
  echo "Größe $plat: Binary ≈ $((bin / 1048576)) MiB, Image komprimiert $(($(docker save "$ref" | gzip -1 | wc -c) / 1048576)) MiB"
  if [[ -n "${SAVE_DIR:-}" ]]; then
    mkdir -p "$SAVE_DIR"
    docker save "$ref" | gzip -1 >"$SAVE_DIR/ai-gw-$TAG-$arch.tar.gz"
    echo "Tarball: $SAVE_DIR/ai-gw-$TAG-$arch.tar.gz ($(du -h "$SAVE_DIR/ai-gw-$TAG-$arch.tar.gz" | cut -f1))"
  fi
done

if [[ "${SMOKE:-0}" == 1 ]]; then
  for plat in "${plats[@]}"; do
    arch=${plat#linux/}
    if [[ "$plat" != "$native" ]] && ! docker run --rm --platform "$plat" "$IMAGE:$TAG-$arch" --version >/dev/null 2>&1; then
      echo "--- QEMU/binfmt für $arch registrieren"
      docker run --privileged --rm tonistiigi/binfmt:qemu-v10.0.4 --install "$arch"
    fi
    echo "=== Smoke $plat"
    docker run --rm --platform "$plat" "$IMAGE:$TAG-$arch" --version
    IMAGE="$IMAGE:$TAG-$arch" SKIP_BUILD=1 scripts/container-smoke.sh
  done
fi
