#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 ai-gw contributors
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Erzeugt die Referenz-Fixtures von aigw-tokenizer (einmalig von Hand, nicht zur Build-Zeit).

Benötigt `tiktoken` (MIT) und `tokenizers` (Apache-2.0) in einem venv:

    python -m pip install tiktoken tokenizers
    python -I gen_fixtures.py <ausgabe: tests/fixtures> [<verzeichnis mit echten tokenizer.json>]

Erzeugt:
- `tiktoken.jsonl`: Texte (Sprachen, Code, CJK, Emojis, Sonderfälle, Zufallstexte) mit den
  Token-IDs von tiktoken für cl100k_base und o200k_base.
- `hf_*.json` + `hf.jsonl`: kleine, hier trainierte tokenizer.json-Dateien (byte-level BPE mit
  Llama-3-/Tekken-Regex, SentencePiece-BPE mit byte_fallback, Metaspace, Unigram) und die
  Token-Anzahl laut `tokenizers` je Text.
- optional `hf_real.jsonl`: Token-Anzahl echter Tokenizer (Llama 3, Mistral 7B v0.3, Mistral
  Nemo, TinyLlama), falls deren tokenizer.json im angegebenen Verzeichnis liegen (die Dateien
  selbst werden wegen ihrer Lizenzen nicht eingecheckt; Test: AIGW_TOKENIZER_HF_DIR).
"""
import json
import random
import sys

import tiktoken
from tokenizers import Regex, Tokenizer, decoders, models, normalizers, pre_tokenizers, trainers

SAMPLES = [
    "Hello, world!",
    "The quick brown fox jumps over the lazy dog.",
    "I'm sure they'll say it's fine, but we've seen what you'd do. DON'T SHOUT! It'S ok.",
    "Die Bundesregierung hat am Mittwoch beschlossen, die Förderung für Wärmepumpen zu verlängern. "
    "Größere Unternehmen müssen außerdem Berichte über ihre Energieeffizienz vorlegen.",
    "Donaudampfschifffahrtsgesellschaftskapitänsmütze und Rindfleischetikettierungsüberwachungsaufgabenübertragungsgesetz",
    "fn main() {\n    let v: Vec<u32> = (0..10).map(|x| x * 2).collect();\n    println!(\"{:?}\", v);\n}\n",
    "def fib(n):\n\tif n < 2:\n\t\treturn n\n\treturn fib(n - 1) + fib(n - 2)\n\n\nprint([fib(i) for i in range(20)])  \n",
    '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hi"}], "max_tokens": 1024}',
    "SELECT id, name FROM users WHERE created_at >= '2024-01-01' ORDER BY id DESC LIMIT 100;",
    "<html>\n  <body class=\"main\">\n    <p>Text &amp; more</p>\n  </body>\n</html>",
    "日本語のテキストをトークン化します。東京は日本の首都です。",
    "我们今天去公园散步，天气非常好。人工智能正在改变世界。",
    "한국어 문장도 토큰화합니다. 서울은 대한민국의 수도입니다.",
    "Привет, как дела? Это тест на русском языке.",
    "مرحبا بالعالم، هذا اختبار.",
    "नमस्ते दुनिया, यह एक परीक्षण है।",
    "Emojis: 😀😃😄 👩‍👩‍👧‍👦 🇩🇪 ❤️ 👍🏽",
    "Combining: é ä ñ कि Z͓͑͒",
    "Numbers: 1234567890 3.14159 1,000,000 2024-11-13 0x1F 1e-9",
    "   leading spaces and trailing spaces   ",
    "line1\n\nline2\r\n\r\nline3\n   \n\tindented\n",
    "trailing newline and spaces \n  ",
    "tabs\t\tand nbsp em-space　ideographic",
    "a/b/c http://example.com/path?q=1&r=2#frag ../../etc/passwd",
    "CamelCaseIdentifier snake_case_identifier SCREAMING_CASE kebab-case-id",
    "ABCdef DEFghi GHIjklMNO XMLHttpRequest iPhone eBay McDonald's",
    "'s 't 're 've 'm 'll 'd 'S 'T 'RE 'VE 'M 'LL 'D ſ 'ſ",
    "!!!??? ... --- ___ *** ### $$$ %%% ^^^ &&& ((( ))) [[[ ]]] {{{ }}}",
    "\n\n\n\n",
    " ",
    "",
    "x" * 300,
    "ab " * 100,
    "Mixed: Straße 東京 Москва 123 abc!? 🙂\n",
    "Ǆemal ǅemal ǆemal ᾈ ᾉ Ǳ ǲ ǳ",  # Titlecase-Buchstaben (Lt)
    "ʰʲʷ ˠˤ ᵃᵇᶜ ꜱ",  # Modifier-Buchstaben (Lm)
]

# Bausteine für Zufallstexte (Sonderfälle der Pre-Tokenizer gezielt kombiniert).
POOL = (
    list("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ")
    + list("äöüßÄÖÜéèçñ")
    + list("0123456789")
    + list(" " * 12)
    + list("\n\r\t  　")
    + list(".,;:!?'\"()[]{}<>/\\|-_+=*&^%$#@~`")
    + ["'s", "'t", "'re", "'LL", "'d", "ſ"]
    + list("東京日本語中文한국어ひらがなカタカナ")
    + list("Привет")
    + ["́", "̈", "ि", "क", "ǅ", "ʰ", "ᵃ", "😀", "👍🏽", "🇩🇪"]
    + ["Hello", " world", "Straße", " the", " der", "  ", "\n\n", " \n", "\r\n"]
)


def random_texts(n: int) -> list[str]:
    rnd = random.Random(4711)
    out = []
    for _ in range(n):
        k = rnd.randint(1, 60)
        out.append("".join(rnd.choice(POOL) for _ in range(k)))
    return out


def tiktoken_fixture(path: str) -> list[str]:
    cl = tiktoken.get_encoding("cl100k_base")
    o2 = tiktoken.get_encoding("o200k_base")
    texts = SAMPLES + random_texts(400)
    with open(path, "w", encoding="utf-8") as f:
        for t in texts:
            row = {
                "text": t,
                "cl100k_base": cl.encode_ordinary(t),
                "o200k_base": o2.encode_ordinary(t),
            }
            f.write(json.dumps(row, ensure_ascii=False) + "\n")
    return texts


LLAMA3_PAT = (
    r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}{1,3}| ?[^\s\p{L}\p{N}]+[\r\n]*"
    r"|\s*[\r\n]+|\s+(?!\S)|\s+"
)
TEKKEN_PAT = (
    r"[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]*[\p{Ll}\p{Lm}\p{Lo}\p{M}]+"
    r"|[^\r\n\p{L}\p{N}]?[\p{Lu}\p{Lt}\p{Lm}\p{Lo}\p{M}]+[\p{Ll}\p{Lm}\p{Lo}\p{M}]*|\p{N}"
    r"| ?[^\s\p{L}\p{N}]+[\r\n/]*|\s*[\r\n]+|\s+(?!\S)|\s+"
)


def corpus() -> list[str]:
    return SAMPLES * 20 + random_texts(2000)


def byte_level(pattern: str) -> Tokenizer:
    tok = Tokenizer(models.BPE(ignore_merges=True))
    tok.pre_tokenizer = pre_tokenizers.Sequence(
        [
            pre_tokenizers.Split(Regex(pattern), behavior="isolated", invert=False),
            pre_tokenizers.ByteLevel(add_prefix_space=False, use_regex=False),
        ]
    )
    tok.decoder = decoders.ByteLevel()
    tr = trainers.BpeTrainer(
        vocab_size=1200, initial_alphabet=pre_tokenizers.ByteLevel.alphabet(), show_progress=False
    )
    tok.train_from_iterator(corpus(), tr)
    return tok


def gpt2_style() -> Tokenizer:
    tok = Tokenizer(models.BPE())
    tok.pre_tokenizer = pre_tokenizers.ByteLevel(add_prefix_space=True, use_regex=True)
    tr = trainers.BpeTrainer(
        vocab_size=900, initial_alphabet=pre_tokenizers.ByteLevel.alphabet(), show_progress=False
    )
    tok.train_from_iterator(corpus(), tr)
    return tok


def sentencepiece_bpe() -> Tokenizer:
    # Wie Llama 2 / Mistral 7B v0.1 (TinyLlama): Normalizer Prepend+Replace, byte_fallback.
    tok = Tokenizer(models.BPE(unk_token="<unk>", byte_fallback=True, fuse_unk=True))
    tok.normalizer = normalizers.Sequence(
        [normalizers.Prepend("▁"), normalizers.Replace(" ", "▁")]
    )
    byte_tokens = [f"<0x{b:02X}>" for b in range(256)]
    tr = trainers.BpeTrainer(
        vocab_size=900, special_tokens=["<unk>"] + byte_tokens, show_progress=False,
        limit_alphabet=60,
    )
    tok.train_from_iterator(corpus(), tr)
    return tok


def metaspace_bpe() -> Tokenizer:
    # Wie Mistral 7B v0.3: Pre-Tokenizer Metaspace (prepend_scheme first, split false).
    tok = Tokenizer(models.BPE(unk_token="<unk>", byte_fallback=True, fuse_unk=True))
    tok.pre_tokenizer = pre_tokenizers.Metaspace(replacement="▁", prepend_scheme="first", split=False)
    byte_tokens = [f"<0x{b:02X}>" for b in range(256)]
    tr = trainers.BpeTrainer(
        vocab_size=900, special_tokens=["<unk>"] + byte_tokens, show_progress=False,
        limit_alphabet=60,
    )
    tok.train_from_iterator(corpus(), tr)
    return tok


def unigram() -> Tokenizer:
    tok = Tokenizer(models.Unigram())
    tok.pre_tokenizer = pre_tokenizers.Metaspace(replacement="▁", prepend_scheme="always", split=True)
    tr = trainers.UnigramTrainer(
        vocab_size=700, special_tokens=["<unk>"], unk_token="<unk>", show_progress=False
    )
    tok.train_from_iterator(corpus(), tr)
    return tok


def hf_fixtures(out: str, texts: list[str]) -> None:
    toks = {
        "llama3_style": byte_level(LLAMA3_PAT),
        "tekken_style": byte_level(TEKKEN_PAT),
        "gpt2_style": gpt2_style(),
        "sp_bpe": sentencepiece_bpe(),
        "metaspace_bpe": metaspace_bpe(),
        "unigram": unigram(),
    }
    rows = []
    for name, tok in toks.items():
        tok.save(f"{out}/hf_{name}.json")
        tok = Tokenizer.from_file(f"{out}/hf_{name}.json")
        for t in texts:
            rows.append({"tokenizer": name, "text": t, "count": len(tok.encode(t, add_special_tokens=False).ids)})
    with open(f"{out}/hf.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def hf_real(src: str, out: str, texts: list[str]) -> None:
    rows = []
    for name in ["llama3", "mistral7b", "nemo", "tinyllama"]:
        try:
            tok = Tokenizer.from_file(f"{src}/{name}.json")
        except Exception as e:  # noqa: BLE001
            print("übersprungen:", name, e)
            continue
        for t in texts:
            rows.append({"tokenizer": name, "text": t, "count": len(tok.encode(t, add_special_tokens=False).ids)})
    with open(f"{out}/hf_real.jsonl", "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def main() -> None:
    out = sys.argv[1]
    texts = tiktoken_fixture(f"{out}/tiktoken.jsonl")
    hf_texts = SAMPLES + random_texts(400)[:150]
    hf_fixtures(out, hf_texts)
    if len(sys.argv) > 2:
        hf_real(sys.argv[2], out, texts)


if __name__ == "__main__":
    main()
