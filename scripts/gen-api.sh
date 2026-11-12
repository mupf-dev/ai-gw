#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Erzeugt ui/openapi.json aus der Admin-API (Rust/utoipa) und daraus die TS-Typen
# ui/src/lib/api/schema.d.ts (openapi-typescript). Beide Dateien werden eingecheckt;
# scripts/check.sh prüft, dass sie aktuell sind.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo run -q -p aigw -- openapi >ui/openapi.json
cd ui
[[ -d node_modules ]] || pnpm install --frozen-lockfile
pnpm run gen:api
