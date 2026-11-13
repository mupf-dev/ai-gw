# syntax=docker/dockerfile:1
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
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
# Mehrere Architekturen (buildx; arm64 auf amd64 per QEMU – langsam – oder mit nativem Builder):
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
FROM ${RUST_IMAGE} AS build
ARG TARGETPLATFORM
# Cargo-Features des Binaries; leer = Standard (mimalloc). `--build-arg CARGO_FEATURES=
# --no-default-features` baut mit dem System-Allokator.
ARG CARGO_FEATURES=""
WORKDIR /src
# Toolchain aus rust-toolchain.toml vorab installieren (eigene, gut cachebare Schicht).
COPY rust-toolchain.toml ./
RUN rustup toolchain install
COPY . .
COPY --from=ui /src/ui/dist /src/ui/dist
ENV AIGW_UI_DIST=/src/ui/dist
# Cache je Zielplattform getrennt (sonst mischen sich amd64- und arm64-Artefakte).
RUN --mount=type=cache,id=aigw-cargo-registry-${TARGETPLATFORM},target=/usr/local/cargo/registry \
    --mount=type=cache,id=aigw-target-${TARGETPLATFORM},target=/src/target \
    cargo build --release --locked -p aigw ${CARGO_FEATURES} \
    && install -D target/release/aigw /out/usr/local/bin/aigw \
    && /out/usr/local/bin/aigw --version
# Datenverzeichnis für UID/GID 65532 (nonroot) vorbereiten; distroless hat kein mkdir/chown.
RUN install -d -m 0750 /out/data \
    && install -D -m 0644 config/aigw.container.yaml /out/etc/aigw/aigw.yaml

# --- 3. Laufzeit ---------------------------------------------------------------------------
# distroless/cc: glibc + libgcc + CA-Zertifikate, keine Shell, kein Paketmanager;
# Benutzer nonroot (65532:65532).
FROM ${RUNTIME_IMAGE}
ARG VERSION=0.1.0-dev
ARG REVISION=unknown
ARG CREATED=unknown
LABEL org.opencontainers.image.title="ai-gw" \
      org.opencontainers.image.description="AI-Gateway (OpenAI-/Anthropic-kompatibel) mit Admin-API und Admin-UI in einem Binary" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      org.opencontainers.image.source="https://github.com/mupf-dev/ai-gw" \
      org.opencontainers.image.url="https://github.com/mupf-dev/ai-gw" \
      org.opencontainers.image.documentation="https://github.com/mupf-dev/ai-gw/tree/main/docs/betrieb" \
      org.opencontainers.image.vendor="ai-gw contributors" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.created="${CREATED}"
COPY --from=build /out/usr/local/bin/aigw /usr/local/bin/aigw
COPY --from=build /out/etc/aigw/aigw.yaml /etc/aigw/aigw.yaml
COPY --from=build --chown=65532:65532 /out/data /data
# Konfigurationspfad für `serve`, `healthcheck` und `config …` (eigene Datei einhängen oder
# AIGW_CONFIG ändern; einzelne Werte per AIGW_<ABSCHNITT>__<FELD>).
ENV AIGW_CONFIG=/etc/aigw/aigw.yaml
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
