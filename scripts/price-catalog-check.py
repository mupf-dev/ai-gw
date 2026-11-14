#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Vergleicht crates/aigw-billing/catalog/prices.yaml mit LiteLLM model_prices_and_context_window.json.

Aufruf: scripts/price-catalog-check.py [PFAD|URL]   (Standard: aktuelle Datei von GitHub)
Gibt je Modell abweichende Preise aus (Exit 1 bei Abweichungen). Werte werden NICHT automatisch
übernommen – Änderungen bitte gegen die Preisseite des Anbieters prüfen und von Hand eintragen.
Nur Standardbibliothek; liest das Katalogformat zeilenweise (eine Zeile perMillionTokens je Modell).
"""
import json
import re
import sys
import urllib.request
from decimal import Decimal
from pathlib import Path

URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
CATALOG = Path(__file__).resolve().parent.parent / "crates/aigw-billing/catalog/prices.yaml"
FIELDS = {"input": "input_cost_per_token", "output": "output_cost_per_token",
          "cachedInput": "cache_read_input_token_cost", "cacheWrite": "cache_creation_input_token_cost"}
PREFIX = {"openai": [""], "anthropic": [""], "mistral": ["mistral/"], "vertex": ["gemini/", "vertex_ai/", ""]}


def per_million(v):
    return None if v is None else (Decimal(repr(v)) * 1_000_000).quantize(Decimal("0.000000001")).normalize()


def main():
    src = sys.argv[1] if len(sys.argv) > 1 else URL
    if re.match(r"^https?://", src):
        with urllib.request.urlopen(src, timeout=60) as r:  # noqa: S310 (feste bzw. angegebene URL)
            ll = json.load(r)
    else:
        ll = json.loads(Path(src).read_text())
    provider = model = None
    diffs = 0
    for line in CATALOG.read_text().splitlines():
        if m := re.match(r"\s*- provider: (\S+)", line):
            provider = m.group(1)
        elif m := re.match(r"\s*model: (\S+)", line):
            model = m.group(1)
        elif (m := re.match(r"\s*perMillionTokens: \{(.*)\}", line)) and provider and model:
            ours = {k: Decimal(v).normalize() for k, v in re.findall(r'(\w+): "([^"]+)"', m.group(1))}
            entry = next((ll[p + model] for p in PREFIX.get(provider, [""]) if p + model in ll), None)
            if entry is None:
                print(f"{provider}/{model}: nicht in LiteLLM")
                continue
            for k, f in FIELDS.items():
                theirs = per_million(entry.get(f))
                if ours.get(k, Decimal(0) if theirs == 0 else None) != theirs:
                    diffs += 1
                    print(f"{provider}/{model} {k}: Katalog {ours.get(k)} – LiteLLM {theirs}")
    print(f"{diffs} Abweichung(en)")
    return 1 if diffs else 0


if __name__ == "__main__":
    sys.exit(main())
