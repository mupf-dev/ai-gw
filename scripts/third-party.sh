#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 mupf-dev
# SPDX-License-Identifier: FSL-1.1-ALv2
#
# Erzeugt THIRD-PARTY.md: Lizenzen aller Rust-Abhängigkeiten, die in das Binary `aigw`
# eingehen (normale Abhängigkeiten, alle Zielplattformen; aus `cargo metadata`), und der
# Laufzeit-Abhängigkeiten der Admin-UI (aus `pnpm licenses list --prod`). Zulässige Lizenzen
# prüft `cargo deny check licenses` (deny.toml) in scripts/check.sh.
#
#   scripts/third-party.sh            # schreibt THIRD-PARTY.md
#   scripts/third-party.sh --check    # Exit 1, wenn THIRD-PARTY.md veraltet ist
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=THIRD-PARTY.md
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

cargo metadata --format-version 1 --locked >"$TMP/cargo.json"
if command -v pnpm >/dev/null 2>&1 && [[ -d ui/node_modules ]]; then
  (cd ui && pnpm licenses list --prod --json) >"$TMP/pnpm.json"
else
  echo "{}" >"$TMP/pnpm.json"
  echo "Hinweis: pnpm bzw. ui/node_modules fehlt – UI-Abschnitt bleibt leer" >&2
fi

python3 -I - "$TMP/cargo.json" "$TMP/pnpm.json" >"$TMP/out.md" <<'PY'
import json, sys
from collections import defaultdict

meta = json.load(open(sys.argv[1]))
pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
root = next(p["id"] for p in meta["packages"] if p["name"] == "aigw" and p["source"] is None)

# Nur normale Abhängigkeiten (keine dev-/build-Abhängigkeiten) ab dem Binary.
seen, stack = set(), [root]
while stack:
    cur = stack.pop()
    if cur in seen:
        continue
    seen.add(cur)
    for d in nodes[cur]["deps"]:
        if any(k["kind"] is None for k in d["dep_kinds"]):
            stack.append(d["pkg"])

by_license = defaultdict(list)
for pid in seen:
    p = pkgs[pid]
    if p["source"] is None:  # eigene Workspace-Crates
        continue
    by_license[p.get("license") or "UNBEKANNT"].append(f'{p["name"]} {p["version"]}')

print("<!-- Erzeugt mit scripts/third-party.sh – nicht von Hand bearbeiten. -->")
print("# Drittanbieter-Lizenzen\n")
print("ai-gw selbst steht unter FSL-1.1-ALv2 (Code) bzw. CC-BY-4.0 (Dokumentation).")
print("Diese Übersicht listet die Lizenzen der Komponenten, die in das Binary `aigw` und die")
print("eingebettete Admin-UI eingehen. Die zulässigen Lizenzen legt `deny.toml` fest")
print("(geprüft mit `cargo deny check licenses`). Lizenztexte: jeweilige Quellpakete.\n")
print("## Eingebettete Daten\n")
print("| Datei | Herkunft | Lizenz |")
print("|---|---|---|")
print("| Laufzeit: tiktoken-Rangtabellen `cl100k_base.tiktoken`, `o200k_base.tiktoken` | Von OpenAI"
      " veröffentlicht (openaipublic.blob.core.windows.net/encodings, referenziert vom Projekt "
      "tiktoken). **Nicht im Repository, im Binary oder im Image** (AIGW-92); der Betreiber stellt "
      "sie selbst bereit (`aigw tokenizer fetch`, `tokenizer.ranks_dir`, optional "
      "`tokenizer.download_ranks`), SHA-256 geprüft; ohne sie Heuristik | Die Dateien tragen keine "
      "Lizenzangabe (das Repo tiktoken steht unter MIT); Bezug und Nutzung verantwortet der "
      "Betreiber (docs/konzept/99-offene-fragen.md) |")
print("| `crates/aigw-billing/catalog/prices.yaml` | Eigene, gepflegte Datei (AIGW-85): Listenpreise "
      "(Fakten) aus den öffentlichen Preisseiten von OpenAI, Anthropic, Mistral und Google (Gemini API); "
      "Werte abgeglichen mit LiteLLM `model_prices_and_context_window.json` (BerriAI, MIT) – keine "
      "Übernahme der fremden Datei | FSL-1.1-ALv2 (wie der Code) |")
print("| Laufzeit: Azure Retail Prices API (`prices.azure.com`) | Öffentliche Preisabfrage von Microsoft "
      "ohne Anmeldung, nur zur Anzeige von Vorschlägen; nichts wird mitgeliefert | – |\n")
total = sum(len(v) for v in by_license.values())
print(f"## Rust-Abhängigkeiten des Binaries ({total} Crates)\n")
print("| Lizenz (SPDX-Ausdruck) | Anzahl | Crates |")
print("|---|---|---|")
for lic in sorted(by_license, key=lambda l: (-len(by_license[l]), l)):
    names = sorted(by_license[lic])
    print(f"| `{lic}` | {len(names)} | {', '.join(names)} |")

ui = json.load(open(sys.argv[2]))
print("\n## Admin-UI (npm, Laufzeit-Abhängigkeiten im Bundle)\n")
if not ui:
    print("_nicht erzeugt (pnpm/node_modules fehlte)_")
else:
    print("| Lizenz | Pakete |")
    print("|---|---|")
    for lic in sorted(ui):
        names = sorted(f'{p["name"]} {",".join(p.get("versions") or [p.get("version", "")])}' for p in ui[lic])
        print(f"| `{lic}` | {', '.join(names)} |")
print("\n## Container-Image\n")
print("Das Laufzeit-Image basiert auf `gcr.io/distroless/cc-debian12` (Debian-Pakete u. a. glibc")
print("[LGPL-2.1-or-later], libgcc [GPL-3.0-or-later WITH GCC-exception-3.1], ca-certificates")
print("[MPL-2.0]); deren Lizenzen liegen im Image unter `/usr/share/doc/*/copyright`.")
PY

if [[ "${1:-}" == "--check" ]]; then
  diff -u "$OUT" "$TMP/out.md" && echo "THIRD-PARTY.md aktuell"
else
  mv "$TMP/out.md" "$OUT"
  echo "geschrieben: $OUT"
fi
