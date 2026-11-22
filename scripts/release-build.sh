#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Release-Artefakte bauen (AIGW-119) – derselbe Einstieg im Workflow .github/workflows/release.yml
# und auf der Buildbox (Probelauf ohne Veröffentlichung):
#
#   VERSION=0.1.0 scripts/release-build.sh               # Binaries, Tarballs, SBOM, SHA256SUMS → dist/
#   VERSION=0.1.0 SMOKE=1 scripts/release-build.sh       # zusätzlich Images je Architektur laden und
#                                                        # scripts/container-smoke.sh (arm64 unter QEMU)
#   VERSION=0.1.0 PUSH=ghcr.io/mupf-dev/ai-gw scripts/release-build.sh
#                                                        # zusätzlich Multi-Arch-Image pushen (nur CI!)
#
# Umgebung: VERSION (Pflicht, = Cargo-Version; führendes "v" erlaubt), OUT (dist), PLATFORMS
# (linux/amd64,linux/arm64), SMOKE_PLATFORMS (= PLATFORMS), REVISION (git HEAD), BUILDER
# (aigw-release), ALLOW_PLACEHOLDER=1 (CHANGELOG-Datum darf offen sein, Probelauf).
#
# Binaries: glibc (wie das Image, distroless/cc) aus der Dockerfile-Stufe `build`, quer kompiliert
# ohne QEMU (AIGW-91); Ziel `binary` exportiert nur das Binary. Kein musl: aws-lc-rs, mimalloc
# und SQLite sind mit glibc geprüft und gemessen (docs/betrieb/leistung.md), musl bräuchte eine
# zweite Cross-Toolchain und eigene Messungen.
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=${VERSION:?VERSION fehlt (z. B. VERSION=0.1.0)}
VERSION=${VERSION#v}
OUT=${OUT:-dist}
PLATFORMS=${PLATFORMS:-linux/amd64,linux/arm64}
SMOKE_PLATFORMS=${SMOKE_PLATFORMS:-$PLATFORMS}
BUILDER=${BUILDER:-aigw-release}
REVISION=${REVISION:-$(git rev-parse HEAD 2>/dev/null || echo unknown)}
# Reproduzierbare Zeitstempel: Commit-Zeit (ohne git: jetzt).
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || date +%s)}
CREATED=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)
export SOURCE_DATE_EPOCH
# Gepinnte Hilfs-Images (Digest = unveränderlich).
BUILDKIT_IMAGE=moby/buildkit:v0.35.1@sha256:baf6138eb8c37644800c7c913fba051e840ebb9ba13eaf92d92ef9a1712d0b4c
SYFT_IMAGE=anchore/syft:v1.57.2@sha256:e97e20b6e6d1699c60296b66ab337941724f9bd06ac279d893eda89601fc4b2a
BINFMT_IMAGE=tonistiigi/binfmt:qemu-v10.0.4@sha256:8f58e6214f4cc9dc83ce8f5acad1ece508eb6b20e696a8c1e9f274481982c541

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }

step "Vorprüfung Version $VERSION (Cargo.toml, Cargo.lock, ui/package.json, CHANGELOG)"
scripts/release-notes.sh "$VERSION" >/dev/null

# Prerelease (1.2.3-rc.1) bekommt nur den Versions-Tag, sonst zusätzlich <major>.<minor> und latest.
TAGS=("$VERSION")
if [[ "$VERSION" != *-* ]]; then
  TAGS+=("$(cut -d. -f1-2 <<<"$VERSION")" latest)
fi

ARGS=(--build-arg "VERSION=$VERSION" --build-arg "REVISION=$REVISION" --build-arg "CREATED=$CREATED"
  --build-arg "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH")

# Eigener BuildKit-Builder (docker-container): Multi-Arch-Push und gemeinsamer Cache für
# Binary-Export, geladene Smoke-Images und Push.
if ! docker buildx inspect "$BUILDER" >/dev/null 2>&1; then
  step "BuildKit-Builder $BUILDER anlegen"
  docker buildx create --name "$BUILDER" --driver docker-container \
    --driver-opt "image=$BUILDKIT_IMAGE" --bootstrap >/dev/null
fi
BX=(docker buildx build --builder "$BUILDER")

rm -rf "$OUT"
mkdir -p "$OUT"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
IFS=, read -ra plats <<<"$PLATFORMS"

for plat in "${plats[@]}"; do
  arch=${plat#linux/}
  step "Binary $plat"
  start=$(date +%s)
  "${BX[@]}" --platform "$plat" --target binary "${ARGS[@]}" --output "type=local,dest=$TMP/bin-$arch" .
  echo "Bauzeit $plat: $(($(date +%s) - start)) s"
  bin="$TMP/bin-$arch/aigw"
  machine=$(od -An -tx1 -j18 -N2 "$bin" | tr -d ' ')
  case "$arch:$machine" in
    amd64:3e00 | arm64:b700) ;;
    *) echo "FEHLER: $bin hat ELF e_machine $machine, erwartet $arch" >&2; exit 1 ;;
  esac
  glibc=$(grep -ao 'GLIBC_2\.[0-9]*' "$bin" | sort -uV | tail -1)
  echo "Binary $arch: $(($(wc -c <"$bin") / 1048576)) MiB, benötigt mindestens $glibc"
  if [[ "$(uname -m)" == "$( [[ $arch == amd64 ]] && echo x86_64 || echo aarch64)" ]]; then
    "$bin" --version | tee "$TMP/version"
    grep -qx "aigw $VERSION" "$TMP/version"
  fi

  name="ai-gw-$VERSION-linux-$arch"
  stage="$TMP/$name"
  mkdir -p "$stage/config"
  install -m 0755 "$bin" "$stage/aigw"
  cp -r LICENSE LICENSES README.md CHANGELOG.md THIRD-PARTY.md "$stage/"
  cp config/aigw.example.yaml "$stage/config/"
  # Reproduzierbares Archiv: feste Reihenfolge, Zeitstempel und Besitzer, gzip ohne Name/Zeit.
  tar -C "$TMP" --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
    --format=posix --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
    -cf - "$name" | gzip -n -9 >"$OUT/$name.tar.gz"
  echo "Archiv: $OUT/$name.tar.gz ($(du -h "$OUT/$name.tar.gz" | cut -f1))"
done

step "SBOM (CycloneDX, syft) aus Cargo.lock und ui/pnpm-lock.yaml"
# Nur die Lockfiles des Produkts (ohne plugins/*/Cargo.lock und Test-Fixtures). Cargo.lock
# enthält auch Dev- und plattformfremde Abhängigkeiten (Obermenge des Binaries).
mkdir -p "$TMP/sbom/ui"
cp Cargo.toml Cargo.lock "$TMP/sbom/"
cp ui/package.json ui/pnpm-lock.yaml "$TMP/sbom/ui/"
sbom="$OUT/ai-gw-$VERSION.cdx.json"
docker run --rm -v "$TMP/sbom:/src:ro" "$SYFT_IMAGE" scan dir:/src -q \
  --source-name ai-gw --source-version "$VERSION" -o cyclonedx-json >"$sbom"
# Produkt-Metadaten (Lizenz FSL-1.1-ALv2, Hersteller mupf-dev) in metadata.component ergänzen.
python3 - "$sbom" "$VERSION" <<'PY'
import json, sys
path, version = sys.argv[1], sys.argv[2]
bom = json.load(open(path))
meta = bom.setdefault("metadata", {})
comp = meta.setdefault("component", {"type": "application", "name": "ai-gw"})
comp.update({
    "type": "application", "name": "ai-gw", "version": version,
    "licenses": [{"license": {"id": "FSL-1.1-ALv2"}}],
    "supplier": {"name": "mupf-dev", "url": ["https://github.com/mupf-dev"]},
    "authors": [{"name": "mupf-dev"}],
    "purl": f"pkg:github/mupf-dev/ai-gw@v{version}",
    "externalReferences": [{"type": "vcs", "url": "https://github.com/mupf-dev/ai-gw"}],
})
meta["supplier"] = {"name": "mupf-dev", "url": ["https://github.com/mupf-dev"]}
json.dump(bom, open(path, "w"), indent=2)
PY
grep -q '"FSL-1.1-ALv2"' "$sbom"
n=$(grep -o '"bom-ref"' "$sbom" | wc -l)
grep -q '"bomFormat": *"CycloneDX"' "$sbom"
[[ "$n" -gt 100 ]] || { echo "FEHLER: SBOM mit nur $n Komponenten" >&2; exit 1; }
echo "SBOM: $sbom ($n Komponenten)"

step "SHA256SUMS"
(cd "$OUT" && sha256sum -- *.tar.gz *.cdx.json >SHA256SUMS && cat SHA256SUMS)

if [[ "${SMOKE:-0}" == 1 ]]; then
  native=$(docker version --format '{{.Server.Os}}/{{.Server.Arch}}')
  IFS=, read -ra splats <<<"$SMOKE_PLATFORMS"
  for plat in "${splats[@]}"; do
    arch=${plat#linux/}
    ref="ai-gw:release-$arch"
    step "Image $plat laden und testen ($ref)"
    "${BX[@]}" --platform "$plat" "${ARGS[@]}" -t "$ref" --load .
    test "$(docker image inspect "$ref" --format '{{.Os}}/{{.Architecture}}')" = "$plat"
    test "$(docker image inspect "$ref" --format '{{index .Config.Labels "org.opencontainers.image.version"}}')" = "$VERSION"
    if [[ "$plat" != "$native" ]] && ! docker run --rm --platform "$plat" "$ref" --version >/dev/null 2>&1; then
      echo "--- QEMU/binfmt für $arch registrieren"
      docker run --privileged --rm "$BINFMT_IMAGE" --install "$arch"
    fi
    docker run --rm --platform "$plat" "$ref" --version | grep -qx "aigw $VERSION"
    IMAGE="$ref" SKIP_BUILD=1 scripts/container-smoke.sh
  done
fi

if [[ -n "${PUSH:-}" ]]; then
  step "Multi-Arch-Image $PUSH pushen (${TAGS[*]})"
  tagargs=()
  for t in "${TAGS[@]}"; do tagargs+=(-t "$PUSH:$t"); done
  # Labels stehen im Dockerfile (Image-Konfiguration je Architektur); Annotationen am Index
  # und an den Manifesten, damit Registries (GHCR) Beschreibung/Lizenz/Quelle anzeigen.
  ann=()
  for kv in \
    "org.opencontainers.image.title=ai-gw" \
    "org.opencontainers.image.authors=mupf-dev" \
    "org.opencontainers.image.description=AI-Gateway (OpenAI-/Anthropic-kompatibel) mit Admin-API und Admin-UI in einem Binary" \
    "org.opencontainers.image.licenses=FSL-1.1-ALv2" \
    "org.opencontainers.image.source=https://github.com/mupf-dev/ai-gw" \
    "org.opencontainers.image.url=https://github.com/mupf-dev/ai-gw" \
    "org.opencontainers.image.documentation=https://github.com/mupf-dev/ai-gw/tree/v$VERSION/docs/betrieb" \
    "org.opencontainers.image.vendor=mupf-dev" \
    "org.opencontainers.image.version=$VERSION" \
    "org.opencontainers.image.revision=$REVISION" \
    "org.opencontainers.image.created=$CREATED"; do
    ann+=(--annotation "index,manifest:$kv")
  done
  # Herkunft/SBOM attestiert der Workflow signiert (actions/attest); BuildKit-eigene,
  # unsignierte Attestierungen würden nur zusätzliche unknown/unknown-Manifeste erzeugen.
  "${BX[@]}" --platform "$PLATFORMS" "${ARGS[@]}" "${tagargs[@]}" "${ann[@]}" \
    --provenance=false --sbom=false --metadata-file "$TMP/meta.json" --push .
  digest=$(grep -o '"containerimage.digest": *"sha256:[0-9a-f]*"' "$TMP/meta.json" | grep -o 'sha256:[0-9a-f]*')
  [[ -n "$digest" ]] || { echo "FEHLER: kein Digest in den Build-Metadaten" >&2; exit 1; }
  docker buildx imagetools inspect "$PUSH@$digest"
  echo "Digest: $digest"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    echo "digest=$digest" >>"$GITHUB_OUTPUT"
  fi
fi

step "Fertig: $OUT/"
ls -l "$OUT"
