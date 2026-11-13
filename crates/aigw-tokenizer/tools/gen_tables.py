#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Wandelt die BPE-Rangtabellen von OpenAI (`*.tiktoken`) in das kompakte Binärformat von
aigw-tokenizer um (einmalig von Hand, NICHT zur Build-Zeit).

Quelle: https://openaipublic.blob.core.windows.net/encodings/<name>.tiktoken (veröffentlicht
mit tiktoken, MIT). Die SHA-256-Summen werden wie in tiktoken geprüft.

Format (`data/<name>.bin`): b"AGTK" | u32 LE Anzahl | je Rang aufsteigend (0, 1, 2, …):
u8 Länge (1..=255) | Token-Bytes. Ränge müssen lückenlos sein.

Aufruf: python3 -I gen_tables.py <verzeichnis-mit-tiktoken-dateien> <ausgabe-verzeichnis>
"""
import base64
import hashlib
import struct
import sys

EXPECTED = {
    "cl100k_base": "223921b76ee99bde995b7ff738513eef100fb51d18c93597a113bcffe865b2a7",
    "o200k_base": "446a9538cb6c348e3516120d7c08b09f57c36495e2acfffe59a5bf8b0cfb1a2d",
}


def main(src: str, dst: str) -> None:
    for name, digest in EXPECTED.items():
        with open(f"{src}/{name}.tiktoken", "rb") as f:
            raw = f.read()
        if hashlib.sha256(raw).hexdigest() != digest:
            sys.exit(f"{name}: SHA-256 stimmt nicht")
        toks = {}
        for line in raw.splitlines():
            if not line:
                continue
            tok, rank = line.split()
            toks[int(rank)] = base64.b64decode(tok)
        if sorted(toks) != list(range(len(toks))):
            sys.exit(f"{name}: Ränge nicht lückenlos")
        out = bytearray(b"AGTK")
        out += struct.pack("<I", len(toks))
        for r in range(len(toks)):
            t = toks[r]
            if not 0 < len(t) < 256:
                sys.exit(f"{name}: Token {r} hat Länge {len(t)}")
            out.append(len(t))
            out += t
        with open(f"{dst}/{name}.bin", "wb") as f:
            f.write(out)
        print(name, len(toks), "Tokens,", len(out), "Bytes, sha256", hashlib.sha256(out).hexdigest())


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
