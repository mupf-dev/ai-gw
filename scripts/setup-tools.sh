#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Richtet die Werkzeuge auf einer frischen Buildbox bzw. im CI-Runner ein (idempotent):
# rustup mit der Toolchain aus rust-toolchain.toml, cargo-deny und reuse.
# Node/pnpm bringt die Box bereits mit (im CI: setup-node/pnpm-Action).
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO_DENY_VERSION=0.20.2
REUSE_VERSION=6.2.0

if ! command -v rustup >/dev/null 2>&1 && [[ ! -x "$HOME/.cargo/bin/rustup" ]]; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install

if ! cargo deny --version 2>/dev/null | grep -q "$CARGO_DENY_VERSION"; then
  cargo install --locked "cargo-deny@$CARGO_DENY_VERSION"
fi

if ! command -v reuse >/dev/null 2>&1; then
  python3 -m venv "$HOME/.venvs/reuse"
  "$HOME/.venvs/reuse/bin/pip" install -q "reuse==$REUSE_VERSION"
  mkdir -p "$HOME/.local/bin"
  ln -sf "$HOME/.venvs/reuse/bin/reuse" "$HOME/.local/bin/reuse"
fi

echo "Werkzeuge bereit: $(cargo --version), $(cargo deny --version), $(reuse --version | head -1)"
