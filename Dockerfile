# syntax=docker/dockerfile:1
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Container-Image von ai-gw (AIGW-59), Doku: docs/betrieb/installation.md
#
# Mehrstufig: Admin-UI (Node/pnpm) → Binary (Rust, UI eingebettet) → distroless/cc, nicht-root.
# Laufzeit-Image ohne Shell und Paketmanager; tauglich für ein schreibgeschütztes Root-FS
# (beschreibbar nur das Volume /data).
#
#   docker build -t ai-gw:dev .
#   docker run --rm -p 4000:4000 -p 4001:4001 -e AIGW_MASTER_KEY=… -v aigw-data:/data ai-gw:dev
#
# Mehrere Architekturen (buildx; arm64 wird auf amd64 quer kompiliert, kein QEMU nötig;
# Skript mit Tarballs/Smoke: scripts/build-images.sh):
#   docker buildx build --platform linux/amd64,linux/arm64 \
#     --build-arg VERSION=0.1.0 --build-arg REVISION=$(git rev-parse HEAD) -t ai-gw:0.1.0 .

ARG NODE_IMAGE=node:24-bookworm-slim
ARG RUST_IMAGE=rust:1-slim-bookworm
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot

# --- 1. Admin-UI ---------------------------------------------------------------------------
# Plattformunabhängige Ausgabe (HTML/JS/CSS) → immer auf der Build-Plattform bauen.
FROM --platform=$BUILDPLATFORM ${NODE_IMAGE} AS ui
RUN npm install -g pnpm@12.18.0
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ./
RUN --mount=type=cache,id=aigw-pnpm,target=/root/.local/share/pnpm/store \
    pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm run build

# --- 2. Rust-Binary ------------------------------------------------------------------------
# Läuft auf der Build-Plattform und kompiliert bei Bedarf quer (AIGW-91): linux/arm64 auf einem
# amd64-Builder (und umgekehrt) mit dem Debian-Cross-GCC als Linker/C-Compiler – ohne
# QEMU-Emulation des Compilers. C-Anteile (aws-lc-sys, mimalloc, SQLite) baut das `cc`-Crate
# mit demselben Cross-GCC; aws-lc-sys bringt vorgenerierte Bindings mit (kein bindgen/cmake).
FROM --platform=$BUILDPLATFORM ${RUST_IMAGE} AS build
ARG TARGETPLATFORM
ARG TARGETARCH
ARG BUILDARCH
# Cargo-Features des Binaries; leer = Standard (mimalloc). `--build-arg CARGO_FEATURES=
# --no-default-features` baut mit dem System-Allokator.
ARG CARGO_FEATURES=""
# Rust-Ziel und GNU-Präfix je Zielarchitektur; Cross-GCC nur, wenn Ziel ≠ Build-Plattform.
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  gnu=x86_64-linux-gnu;  pkg=gcc-x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; pkg=gcc-aarch64-linux-gnu ;; \
      *) echo "nicht unterstützte Zielarchitektur: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update && apt-get install -y --no-install-recommends "$pkg" "libc6-dev-${TARGETARCH}-cross" \
      && rm -rf /var/lib/apt/lists/*; \
    fi; \
    printf '%s\n' "$triple" >/rust-target; printf '%s\n' "$gnu" >/gnu-prefix
WORKDIR /src
# Toolchain aus rust-toolchain.toml vorab installieren (eigene, gut cachebare Schicht).
COPY rust-toolchain.toml ./
RUN rustup toolchain install && rustup target add "$(cat /rust-target)"
COPY . .
COPY --from=ui /src/ui/dist /src/ui/dist
ENV AIGW_UI_DIST=/src/ui/dist
# Cache je Zielplattform getrennt (sonst mischen sich amd64- und arm64-Artefakte).
RUN --mount=type=cache,id=aigw-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=aigw-target-${TARGETPLATFORM},target=/src/target \
    set -eu; triple=$(cat /rust-target); gnu=$(cat /gnu-prefix); \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      t=$(echo "$triple" | tr '-' '_'); env=$(echo "$t" | tr 'a-z' 'A-Z'); \
      export "CARGO_TARGET_${env}_LINKER=${gnu}-gcc" "CC_${t}=${gnu}-gcc" "AR_${t}=${gnu}-ar"; \
    fi; \
    cargo build --release --locked -p aigw --target "$triple" ${CARGO_FEATURES} \
    && install -D "target/$triple/release/aigw" /out/usr/local/bin/aigw \
    && if [ "$TARGETARCH" = "$BUILDARCH" ]; then /out/usr/local/bin/aigw --version; \
       else file_arch=$(od -An -tx1 -j18 -N2 /out/usr/local/bin/aigw | tr -d ' '); \
            echo "Cross-Build $triple (ELF e_machine $file_arch)"; fi
# Datenverzeichnis für UID/GID 65532 (nonroot) vorbereiten; distroless hat kein mkdir/chown.
RUN install -d -m 0750 /out/data \
    && install -D -m 0644 config/aigw.container.yaml /out/etc/aigw/aigw.yaml

# --- 3. Nur das Binary (Release, AIGW-119) -------------------------------------------------
# `docker buildx build --target binary --platform linux/amd64,linux/arm64 --output type=local,dest=out .`
# legt out/linux_<arch>/aigw ab (scripts/release-build.sh). Dieselbe Build-Stufe wie das Image.
FROM scratch AS binary
COPY --from=build /out/usr/local/bin/aigw /aigw

# --- 4. Laufzeit ---------------------------------------------------------------------------
# distroless/cc: glibc + libgcc + CA-Zertifikate, keine Shell, kein Paketmanager;
# Benutzer nonroot (65532:65532).
FROM ${RUNTIME_IMAGE}
ARG VERSION=0.1.0-dev
ARG REVISION=unknown
ARG CREATED=unknown
LABEL org.opencontainers.image.title="ai-gw" \
      org.opencontainers.image.description="AI-Gateway (OpenAI-/Anthropic-kompatibel) mit Admin-API und Admin-UI in einem Binary" \
      org.opencontainers.image.licenses="FSL-1.1-ALv2" \
      org.opencontainers.image.source="https://github.com/mupf-dev/ai-gw" \
      org.opencontainers.image.url="https://github.com/mupf-dev/ai-gw" \
      org.opencontainers.image.documentation="https://github.com/mupf-dev/ai-gw/tree/main/docs/betrieb" \
      org.opencontainers.image.vendor="mupf-dev" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"
COPY --from=build /out/usr/local/bin/aigw /usr/local/bin/aigw
COPY --from=build /out/etc/aigw/aigw.yaml /etc/aigw/aigw.yaml
COPY --from=build --chown=65532:65532 /out/data /data
# Konfigurationspfad für `serve`, `healthcheck` und `config …` (eigene Datei einhängen oder
# AIGW_CONFIG ändern; einzelne Werte per AIGW_<ABSCHNITT>__<FELD>).
ENV AIGW_CONFIG=/etc/aigw/aigw.yaml
# tiktoken-Rangtabellen (o200k_base/cl100k_base) sind NICHT im Image (Lizenz ungeklärt,
# AIGW-92). Gesucht werden sie im Volume; bereitstellen mit
#   docker run --rm -v aigw-data:/data ai-gw:dev tokenizer fetch
# oder Download beim Start erlauben (AIGW_TOKENIZER__DOWNLOAD_RANKS=true). Ohne Tabellen
# schätzt das Gateway Tokens (Heuristik).
ENV AIGW_TOKENIZER__RANKS_DIR=/data/tiktoken
USER 65532:65532
WORKDIR /data
VOLUME ["/data"]
EXPOSE 4000 4001
# Bereitschaft über GET /readyz der Steuerebene (Adresse aus der Konfiguration), ohne curl.
HEALTHCHECK --interval=15s --timeout=5s --start-period=20s --retries=3 \
    CMD ["/usr/local/bin/aigw", "healthcheck"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/aigw"]
CMD ["serve"]
