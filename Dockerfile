# syntax=docker/dockerfile:1
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Mehrstufiger Build: UI (Node/pnpm) → Binary (Rust, UI eingebettet) → distroless, nicht-root.
#   docker build -t ai-gw .
#   docker run --rm -p 4000:4000 ai-gw

# --- 1. Admin-UI ---------------------------------------------------------------------------
FROM node:24-bookworm-slim AS ui
RUN npm install -g pnpm@12.18.0
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm run build

# --- 2. Rust-Binary ------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS build
WORKDIR /src
# Toolchain aus rust-toolchain.toml vorab installieren (eigene, gut cachebare Schicht).
COPY rust-toolchain.toml ./
RUN rustup toolchain install
COPY . .
COPY --from=ui /src/ui/dist /src/ui/dist
ENV AIGW_UI_DIST=/src/ui/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p aigw \
    && install -D target/release/aigw /out/aigw

# --- 3. Laufzeit ---------------------------------------------------------------------------
# distroless/cc: glibc + libgcc, keine Shell, kein Paketmanager; Benutzer nonroot (65532).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/aigw /usr/local/bin/aigw
COPY config/aigw.container.toml /etc/aigw/aigw.toml
USER nonroot:nonroot
EXPOSE 4000
ENTRYPOINT ["/usr/local/bin/aigw"]
CMD ["serve", "--config", "/etc/aigw/aigw.toml"]
