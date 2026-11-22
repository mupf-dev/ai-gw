#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Lädt die tiktoken-Rangtabellen von OpenAI (cl100k_base, o200k_base) und prüft die SHA-256-
# Summen (dieselben wie in tiktoken und in crates/aigw-tokenizer/src/ranks.rs). ai-gw enthält
# die Dateien nicht (AIGW-92, Lizenz ungeklärt). Für Entwicklung/Tests ohne gebautes Binary;
# im Betrieb gleichwertig: `aigw tokenizer fetch --dir <verzeichnis>`.
#
#   scripts/fetch-tokenizers.sh [verzeichnis]     # Standard: ~/.cache/aigw/tiktoken
#   export AIGW_TOKENIZER_RANKS_DIR=$(scripts/fetch-tokenizers.sh)   # Referenztests aktivieren
#
# Ausgabe auf stdout: nur das Verzeichnis (Meldungen auf stderr). Exit-Code ≠ 0 bei Fehlern.
set -euo pipefail

dir="${1:-${XDG_CACHE_HOME:-$HOME/.cache}/aigw/tiktoken}"
base="${AIGW_TOKENIZER_BASE_URL:-https://openaipublic.blob.core.windows.net/encodings/}"
declare -A SHA=(
  [cl100k_base]=223921b76ee99bde995b7ff738513eef100fb51d18c93597a113bcffe865b2a7
  [o200k_base]=446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d
)

mkdir -p "$dir"
for name in cl100k_base o200k_base; do
  file="$dir/$name.tiktoken"
  if [[ -f "$file" ]] && echo "${SHA[$name]}  $file" | sha256sum -c --status; then
    echo "vorhanden: $file" >&2
    continue
  fi
  tmp="$file.tmp.$$"
  curl -fsS --proto '=https' --max-time 120 -o "$tmp" "$base$name.tiktoken"
  if ! echo "${SHA[$name]}  $tmp" | sha256sum -c --status; then
    rm -f "$tmp"
    echo "FEHLER: $name.tiktoken: SHA-256 stimmt nicht" >&2
    exit 1
  fi
  mv "$tmp" "$file"
  echo "geladen: $file" >&2
done
echo "$dir"
