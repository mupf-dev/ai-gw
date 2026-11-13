// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Lader für Hugging-Face-`tokenizer.json` (nur Token-Zählung).
//!
//! Unterstützt ist, was Llama 3, Mistral (7B v0.1–v0.3, Nemo/Tekken) und verwandte Modelle
//! brauchen:
//!
//! | Teil | unterstützt |
//! |---|---|
//! | `normalizer` | `Sequence`, `Prepend`, `Replace` (String/Regex), `Lowercase`, `null` |
//! | `pre_tokenizer` | `Sequence`, `Split` (Regex/String; Isolated, Removed, MergedWithPrevious, MergedWithNext, Contiguous), `ByteLevel`, `Metaspace`, `Digits`, `null` |
//! | `model` | `BPE` (Merges, `byte_fallback`, `fuse_unk`, `ignore_merges`), `Unigram` (Viterbi, `byte_fallback`) |
//!
//! Spezial-Tokens (`added_tokens`) und Post-Processor (BOS/EOS) zählen nicht: gezählt wird der
//! reine Text, wie `encode(text, add_special_tokens=False)`. Nicht Unterstütztes (z. B.
//! Unicode-Normalisierung NFC/NFKC, WordPiece) ist ein Ladefehler – der Aufrufer fällt dann auf
//! die Heuristik zurück.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;

use serde_json::Value;

use crate::regex_split::RegexSplit;

/// Fehler beim Laden einer `tokenizer.json`.
#[derive(Debug, thiserror::Error)]
pub enum HfError {
    /// Datei nicht lesbar.
    #[error("tokenizer.json nicht lesbar: {0}")]
    Io(#[from] std::io::Error),
    /// Kein gültiges JSON.
    #[error("tokenizer.json ungültig: {0}")]
    Json(#[from] serde_json::Error),
    /// Bestandteil wird nicht unterstützt.
    #[error("tokenizer.json: {0} wird nicht unterstützt")]
    Unsupported(String),
    /// Inhalt passt nicht zur Erwartung.
    #[error("tokenizer.json: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone)]
enum Normalizer {
    Prepend(String),
    Replace(Pattern, String),
    Lowercase,
}

#[derive(Debug, Clone)]
enum Pattern {
    Literal(String),
    Regex(RegexSplit),
}

impl Pattern {
    fn parse(v: &Value) -> Result<Self, HfError> {
        if let Some(s) = v.get("String").and_then(Value::as_str) {
            return Ok(Self::Literal(s.to_owned()));
        }
        if let Some(r) = v.get("Regex").and_then(Value::as_str) {
            return RegexSplit::new(r)
                .map(Self::Regex)
                .map_err(HfError::Invalid);
        }
        Err(HfError::Invalid(format!("Muster {v} unbekannt")))
    }

    fn matches(&self, s: &str, out: &mut Vec<(usize, usize)>) {
        match self {
            Self::Literal(l) if !l.is_empty() => {
                out.extend(s.match_indices(l.as_str()).map(|(i, m)| (i, i + m.len())));
            }
            Self::Literal(_) => {}
            Self::Regex(r) => r.matches(s, out),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Isolated,
    Removed,
    MergedWithPrevious,
    MergedWithNext,
    Contiguous,
}

impl Behavior {
    fn parse(s: &str) -> Result<Self, HfError> {
        Ok(match s {
            "Isolated" => Self::Isolated,
            "Removed" => Self::Removed,
            "MergedWithPrevious" => Self::MergedWithPrevious,
            "MergedWithNext" => Self::MergedWithNext,
            "Contiguous" => Self::Contiguous,
            other => return Err(HfError::Unsupported(format!("Split-Verhalten {other:?}"))),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prepend {
    Always,
    First,
    Never,
}

#[derive(Debug, Clone)]
enum PreTok {
    Split(Pattern, Behavior),
    ByteLevel {
        add_prefix_space: bool,
        use_regex: bool,
    },
    Metaspace {
        replacement: char,
        prepend: Prepend,
        split: bool,
    },
    Digits {
        individual: bool,
    },
}

/// Ein Stück nach der Vorverarbeitung; `first` = beginnt am Anfang des Originaltexts.
#[derive(Debug)]
struct Piece {
    text: String,
    first: bool,
}

/// Teilt `s` an den Trefferbereichen nach `behavior` (nicht leere Teile).
fn split_by(s: &str, ms: &[(usize, usize)], behavior: Behavior, out: &mut Vec<(usize, usize)>) {
    let push = |a: usize, b: usize, out: &mut Vec<(usize, usize)>| {
        if b > a {
            out.push((a, b));
        }
    };
    let mut pos = 0;
    match behavior {
        Behavior::Isolated | Behavior::Removed => {
            for &(a, b) in ms {
                push(pos, a, out);
                if behavior == Behavior::Isolated {
                    push(a, b, out);
                }
                pos = b;
            }
            push(pos, s.len(), out);
        }
        Behavior::MergedWithPrevious => {
            for &(_, b) in ms {
                push(pos, b, out);
                pos = b;
            }
            push(pos, s.len(), out);
        }
        Behavior::MergedWithNext => {
            for &(a, _) in ms {
                push(pos, a, out);
                pos = a;
            }
            push(pos, s.len(), out);
        }
        Behavior::Contiguous => {
            let mut i = 0;
            while i < ms.len() {
                let (a, mut b) = ms[i];
                push(pos, a, out);
                while i + 1 < ms.len() && ms[i + 1].0 == b {
                    i += 1;
                    b = ms[i].1;
                }
                push(a, b, out);
                pos = b;
                i += 1;
            }
            push(pos, s.len(), out);
        }
    }
}

fn split_pieces(
    pieces: Vec<Piece>,
    mut f: impl FnMut(&str, &mut Vec<(usize, usize)>),
) -> Vec<Piece> {
    let mut out = Vec::with_capacity(pieces.len());
    let mut ranges = Vec::new();
    for p in pieces {
        ranges.clear();
        f(&p.text, &mut ranges);
        for (k, &(a, b)) in ranges.iter().enumerate() {
            if let Some(t) = p.text.get(a..b) {
                out.push(Piece {
                    text: t.to_owned(),
                    first: p.first && k == 0 && a == 0,
                });
            }
        }
    }
    out
}

/// Abbildung Byte → Zeichen von GPT-2 (`ByteLevel`).
fn byte_chars() -> [char; 256] {
    let mut map = ['\0'; 256];
    let mut n = 0u32;
    for b in 0..=255u8 {
        let printable = matches!(b, b'!'..=b'~' | 0xA1..=0xAC | 0xAE..=0xFF);
        map[usize::from(b)] = if printable {
            char::from(b)
        } else {
            let c = char::from_u32(256 + n).unwrap_or('\u{FFFD}');
            n += 1;
            c
        };
    }
    map
}

const GPT2_PATTERN: &str =
    r"'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+";

impl PreTok {
    fn apply(&self, pieces: Vec<Piece>, ctx: &Ctx) -> Vec<Piece> {
        match self {
            Self::Split(p, behavior) => {
                let mut ms = Vec::new();
                split_pieces(pieces, |s, out| {
                    ms.clear();
                    p.matches(s, &mut ms);
                    split_by(s, &ms, *behavior, out);
                })
            }
            Self::ByteLevel {
                add_prefix_space,
                use_regex,
            } => {
                let mut pieces = pieces;
                if *add_prefix_space {
                    for p in &mut pieces {
                        if !p.text.starts_with(' ') {
                            p.text.insert(0, ' ');
                        }
                    }
                }
                if *use_regex && let Some(re) = &ctx.gpt2 {
                    let mut ms = Vec::new();
                    pieces = split_pieces(pieces, |s, out| {
                        ms.clear();
                        re.matches(s, &mut ms);
                        split_by(s, &ms, Behavior::Isolated, out);
                    });
                }
                for p in &mut pieces {
                    p.text = p.text.bytes().map(|b| ctx.bytes[usize::from(b)]).collect();
                }
                pieces
            }
            Self::Metaspace {
                replacement,
                prepend,
                split,
            } => {
                let rep = replacement.to_string();
                let mut pieces = pieces;
                for p in &mut pieces {
                    p.text = p.text.replace(' ', &rep);
                    let add = match prepend {
                        Prepend::Always => true,
                        Prepend::First => p.first,
                        Prepend::Never => false,
                    };
                    if add && !p.text.starts_with(*replacement) {
                        p.text.insert(0, *replacement);
                    }
                }
                if *split {
                    split_pieces(pieces, |s, out| {
                        let ms: Vec<(usize, usize)> = s
                            .match_indices(*replacement)
                            .map(|(i, m)| (i, i + m.len()))
                            .collect();
                        split_by(s, &ms, Behavior::MergedWithNext, out);
                    })
                } else {
                    pieces
                }
            }
            Self::Digits { individual } => split_pieces(pieces, |s, out| {
                let ms: Vec<(usize, usize)> = s
                    .char_indices()
                    .filter(|(_, c)| c.is_numeric())
                    .map(|(i, c)| (i, i + c.len_utf8()))
                    .collect();
                let b = if *individual {
                    Behavior::Isolated
                } else {
                    Behavior::Contiguous
                };
                split_by(s, &ms, b, out);
            }),
        }
    }
}

/// Vorberechnete Hilfen der Vorverarbeitung.
#[derive(Debug, Clone)]
struct Ctx {
    bytes: [char; 256],
    gpt2: Option<RegexSplit>,
}

/// Unbekanntes Zeichen (verschmilzt nie).
const UNK: u32 = u32::MAX;

#[derive(Debug, Clone)]
struct BpeModel {
    vocab: HashMap<String, u32>,
    merges: HashMap<(u32, u32), (u32, u32)>,
    byte_ids: Vec<Option<u32>>,
    has_unk: bool,
    byte_fallback: bool,
    fuse_unk: bool,
    ignore_merges: bool,
}

impl BpeModel {
    fn parse(m: &Value) -> Result<Self, HfError> {
        for k in ["continuing_subword_prefix", "end_of_word_suffix"] {
            if m.get(k)
                .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
            {
                return Err(HfError::Unsupported(format!("BPE mit {k}")));
            }
        }
        if m.get("dropout")
            .and_then(Value::as_f64)
            .is_some_and(|d| d > 0.0)
        {
            return Err(HfError::Unsupported("BPE mit dropout".into()));
        }
        let vocab: HashMap<String, u32> = m
            .get("vocab")
            .and_then(Value::as_object)
            .ok_or_else(|| HfError::Invalid("model.vocab fehlt".into()))?
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), u32::try_from(v.as_u64()?).ok()?)))
            .collect();
        let raw = m
            .get("merges")
            .and_then(Value::as_array)
            .ok_or_else(|| HfError::Invalid("model.merges fehlt".into()))?;
        let mut merges = HashMap::with_capacity(raw.len());
        for (rank, e) in raw.iter().enumerate() {
            let (a, b) = match e {
                Value::String(s) => match s.split_once(' ') {
                    Some((a, b)) => (a.to_owned(), b.to_owned()),
                    None => return Err(HfError::Invalid(format!("Merge {s:?} ungültig"))),
                },
                Value::Array(v) => match v.as_slice() {
                    [Value::String(a), Value::String(b)] => (a.clone(), b.clone()),
                    _ => return Err(HfError::Invalid(format!("Merge {e} ungültig"))),
                },
                _ => return Err(HfError::Invalid(format!("Merge {e} ungültig"))),
            };
            let (Some(&ia), Some(&ib), Some(&inew)) =
                (vocab.get(&a), vocab.get(&b), vocab.get(&format!("{a}{b}")))
            else {
                continue;
            };
            let rank = u32::try_from(rank).unwrap_or(u32::MAX);
            merges.entry((ia, ib)).or_insert((rank, inew));
        }
        let byte_ids = (0..=255u8)
            .map(|b| vocab.get(&format!("<0x{b:02X}>")).copied())
            .collect();
        let has_unk = m
            .get("unk_token")
            .and_then(Value::as_str)
            .is_some_and(|u| vocab.contains_key(u));
        let flag = |k: &str| m.get(k).and_then(Value::as_bool).unwrap_or(false);
        Ok(Self {
            vocab,
            merges,
            byte_ids,
            has_unk,
            byte_fallback: flag("byte_fallback"),
            fuse_unk: flag("fuse_unk"),
            ignore_merges: flag("ignore_merges"),
        })
    }

    fn count(&self, piece: &str) -> usize {
        if piece.is_empty() {
            return 0;
        }
        if self.ignore_merges && self.vocab.contains_key(piece) {
            return 1;
        }
        let mut sym: Vec<u32> = Vec::with_capacity(piece.len());
        let mut buf = [0u8; 4];
        for c in piece.chars() {
            let s = c.encode_utf8(&mut buf);
            if let Some(&id) = self.vocab.get(&*s) {
                sym.push(id);
                continue;
            }
            if self.byte_fallback {
                let ids: Option<Vec<u32>> = s
                    .bytes()
                    .map(|b| self.byte_ids.get(usize::from(b)).copied().flatten())
                    .collect();
                if let Some(ids) = ids {
                    sym.extend(ids);
                    continue;
                }
            }
            if self.has_unk && !(self.fuse_unk && sym.last() == Some(&UNK)) {
                sym.push(UNK);
            }
        }
        merge_count(&sym, &self.merges)
    }
}

/// Verschmilzt Symbole nach Merge-Rang (kleinster zuerst, bei Gleichstand das linke) und
/// liefert die verbleibende Anzahl.
fn merge_count(sym: &[u32], merges: &HashMap<(u32, u32), (u32, u32)>) -> usize {
    let n = sym.len();
    if n < 2 {
        return n;
    }
    let mut ids = sym.to_vec();
    let mut next: Vec<usize> = (1..=n).collect();
    let mut prev: Vec<usize> = (0..n).map(|i| i.wrapping_sub(1)).collect();
    let mut alive = vec![true; n];
    let mut heap: BinaryHeap<Reverse<(u32, usize, u32)>> = BinaryHeap::with_capacity(n);
    let pair = |ids: &[u32], a: usize, b: usize| merges.get(&(ids[a], ids[b])).copied();
    for i in 0..n - 1 {
        if let Some((rank, new)) = pair(&ids, i, i + 1) {
            heap.push(Reverse((rank, i, new)));
        }
    }
    let mut count = n;
    while let Some(Reverse((_, p, new))) = heap.pop() {
        if !alive[p] {
            continue;
        }
        let q = next[p];
        if q >= n || pair(&ids, p, q).map(|(_, id)| id) != Some(new) {
            continue;
        }
        ids[p] = new;
        alive[q] = false;
        let nq = next[q];
        next[p] = nq;
        if nq < n {
            prev[nq] = p;
        }
        count -= 1;
        let pp = prev[p];
        if pp != usize::MAX
            && let Some((rank, id)) = pair(&ids, pp, p)
        {
            heap.push(Reverse((rank, pp, id)));
        }
        if nq < n
            && let Some((rank, id)) = pair(&ids, p, nq)
        {
            heap.push(Reverse((rank, p, id)));
        }
    }
    count
}

#[derive(Debug, Clone)]
struct UnigramModel {
    pieces: HashMap<String, f64>,
    max_chars: usize,
    unk_score: f64,
    byte_fallback: bool,
    has_bytes: bool,
}

impl UnigramModel {
    fn parse(m: &Value) -> Result<Self, HfError> {
        let vocab = m
            .get("vocab")
            .and_then(Value::as_array)
            .ok_or_else(|| HfError::Invalid("model.vocab fehlt".into()))?;
        let unk_id = m
            .get("unk_id")
            .and_then(Value::as_u64)
            .and_then(|u| usize::try_from(u).ok());
        let mut pieces = HashMap::with_capacity(vocab.len());
        let mut min = f64::MAX;
        let mut max_chars = 1;
        for (i, e) in vocab.iter().enumerate() {
            let (Some(p), Some(score)) = (
                e.get(0).and_then(Value::as_str),
                e.get(1).and_then(Value::as_f64),
            ) else {
                return Err(HfError::Invalid(format!("Unigram-Eintrag {e} ungültig")));
            };
            min = min.min(score);
            if Some(i) == unk_id {
                continue;
            }
            max_chars = max_chars.max(p.chars().count());
            pieces.insert(p.to_owned(), score);
        }
        let has_bytes = (0..=255u8).all(|b| pieces.contains_key(&format!("<0x{b:02X}>")));
        Ok(Self {
            pieces,
            max_chars,
            unk_score: min - 10.0,
            byte_fallback: m.get("byte_fallback").and_then(Value::as_bool) == Some(true),
            has_bytes,
        })
    }

    /// Viterbi über Zeichenpositionen; aufeinanderfolgende unbekannte Zeichen ergeben ein
    /// Token (bzw. bei `byte_fallback` je Byte eines).
    fn count(&self, piece: &str) -> usize {
        let bounds: Vec<usize> = piece
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(piece.len()))
            .collect();
        let n = bounds.len() - 1;
        if n == 0 {
            return 0;
        }
        // best[j] = (Score, Startposition, unbekannt?)
        let mut best: Vec<(f64, usize, bool)> = vec![(f64::NEG_INFINITY, 0, false); n + 1];
        best[0].0 = 0.0;
        for i in 0..n {
            let base = best[i].0;
            if base.is_infinite() {
                continue;
            }
            let mut single = false;
            for j in i + 1..=n.min(i + self.max_chars) {
                let Some(sub) = piece.get(bounds[i]..bounds[j]) else {
                    continue;
                };
                if let Some(&score) = self.pieces.get(sub) {
                    if j == i + 1 {
                        single = true;
                    }
                    let s = base + score;
                    if s > best[j].0 {
                        best[j] = (s, i, false);
                    }
                }
            }
            if !single {
                let s = base + self.unk_score;
                if s > best[i + 1].0 {
                    best[i + 1] = (s, i, true);
                }
            }
        }
        let mut count = 0;
        let mut j = n;
        let mut prev_unk = false;
        while j > 0 {
            let (_, i, unk) = best[j];
            if unk && self.byte_fallback && self.has_bytes {
                count += piece.get(bounds[i]..bounds[j]).map_or(1, str::len);
                prev_unk = false;
            } else if unk {
                if !prev_unk {
                    count += 1;
                }
                prev_unk = true;
            } else {
                count += 1;
                prev_unk = false;
            }
            j = i;
        }
        count
    }
}

#[derive(Debug, Clone)]
enum Model {
    Bpe(BpeModel),
    Unigram(UnigramModel),
}

/// Tokenizer aus einer `tokenizer.json` (nur Zählen).
#[derive(Debug, Clone)]
pub struct HfTokenizer {
    normalizers: Vec<Normalizer>,
    pre: Vec<PreTok>,
    model: Model,
    ctx: Ctx,
}

fn parse_normalizer(v: &Value, out: &mut Vec<Normalizer>) -> Result<(), HfError> {
    if v.is_null() {
        return Ok(());
    }
    let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
    match ty {
        "Sequence" => {
            for n in v
                .get("normalizers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                parse_normalizer(n, out)?;
            }
        }
        "Prepend" => out.push(Normalizer::Prepend(
            v.get("prepend")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )),
        "Replace" => out.push(Normalizer::Replace(
            Pattern::parse(v.get("pattern").unwrap_or(&Value::Null))?,
            v.get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )),
        "Lowercase" => out.push(Normalizer::Lowercase),
        other => return Err(HfError::Unsupported(format!("Normalizer {other:?}"))),
    }
    Ok(())
}

fn parse_pre(v: &Value, out: &mut Vec<PreTok>) -> Result<(), HfError> {
    if v.is_null() {
        return Ok(());
    }
    let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
    let flag = |k: &str, d: bool| v.get(k).and_then(Value::as_bool).unwrap_or(d);
    match ty {
        "Sequence" => {
            for p in v
                .get("pretokenizers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                parse_pre(p, out)?;
            }
        }
        "Split" => {
            if flag("invert", false) {
                return Err(HfError::Unsupported("Split mit invert".into()));
            }
            out.push(PreTok::Split(
                Pattern::parse(v.get("pattern").unwrap_or(&Value::Null))?,
                Behavior::parse(
                    v.get("behavior")
                        .and_then(Value::as_str)
                        .unwrap_or("Isolated"),
                )?,
            ));
        }
        "ByteLevel" => out.push(PreTok::ByteLevel {
            add_prefix_space: flag("add_prefix_space", true),
            use_regex: flag("use_regex", true),
        }),
        "Metaspace" => {
            let replacement = v
                .get("replacement")
                .and_then(Value::as_str)
                .and_then(|s| s.chars().next())
                .unwrap_or('▁');
            let prepend = match v.get("prepend_scheme").and_then(Value::as_str) {
                Some("always") => Prepend::Always,
                Some("first") => Prepend::First,
                Some("never") => Prepend::Never,
                Some(other) => {
                    return Err(HfError::Unsupported(format!("prepend_scheme {other:?}")));
                }
                None if flag("add_prefix_space", true) => Prepend::Always,
                None => Prepend::Never,
            };
            out.push(PreTok::Metaspace {
                replacement,
                prepend,
                split: flag("split", true),
            });
        }
        "Digits" => out.push(PreTok::Digits {
            individual: flag("individual_digits", false),
        }),
        other => return Err(HfError::Unsupported(format!("Pre-Tokenizer {other:?}"))),
    }
    Ok(())
}

impl HfTokenizer {
    /// Liest eine `tokenizer.json` aus Bytes.
    pub fn from_json(json: &[u8]) -> Result<Self, HfError> {
        let v: Value = serde_json::from_slice(json)?;
        let mut normalizers = Vec::new();
        parse_normalizer(
            v.get("normalizer").unwrap_or(&Value::Null),
            &mut normalizers,
        )?;
        let mut pre = Vec::new();
        parse_pre(v.get("pre_tokenizer").unwrap_or(&Value::Null), &mut pre)?;
        let m = v
            .get("model")
            .ok_or_else(|| HfError::Invalid("model fehlt".into()))?;
        let model = match m.get("type").and_then(Value::as_str) {
            Some("BPE") => Model::Bpe(BpeModel::parse(m)?),
            Some("Unigram") => Model::Unigram(UnigramModel::parse(m)?),
            // Ältere Dateien ohne `type`: BPE an `merges` erkennen.
            None if m.get("merges").is_some() => Model::Bpe(BpeModel::parse(m)?),
            other => return Err(HfError::Unsupported(format!("Modell {other:?}"))),
        };
        let gpt2 = if pre.iter().any(|p| {
            matches!(
                p,
                PreTok::ByteLevel {
                    use_regex: true,
                    ..
                }
            )
        }) {
            Some(RegexSplit::new(GPT2_PATTERN).map_err(HfError::Invalid)?)
        } else {
            None
        };
        Ok(Self {
            normalizers,
            pre,
            model,
            ctx: Ctx {
                bytes: byte_chars(),
                gpt2,
            },
        })
    }

    /// Liest eine `tokenizer.json` aus einer Datei.
    pub fn from_file(path: &Path) -> Result<Self, HfError> {
        Self::from_json(&std::fs::read(path)?)
    }

    /// Anzahl Tokens von `text` (ohne Spezial-Tokens).
    pub fn count(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        let mut s = text.to_owned();
        for n in &self.normalizers {
            match n {
                Normalizer::Prepend(p) => s.insert_str(0, p),
                Normalizer::Replace(p, to) => {
                    let mut ms = Vec::new();
                    p.matches(&s, &mut ms);
                    if !ms.is_empty() {
                        let mut out = String::with_capacity(s.len());
                        let mut pos = 0;
                        for (a, b) in ms {
                            out.push_str(s.get(pos..a).unwrap_or_default());
                            out.push_str(to);
                            pos = b;
                        }
                        out.push_str(s.get(pos..).unwrap_or_default());
                        s = out;
                    }
                }
                Normalizer::Lowercase => s = s.to_lowercase(),
            }
        }
        let mut pieces = vec![Piece {
            text: s,
            first: true,
        }];
        for p in &self.pre {
            pieces = p.apply(pieces, &self.ctx);
        }
        pieces
            .iter()
            .map(|p| match &self.model {
                Model::Bpe(m) => m.count(&p.text),
                Model::Unigram(m) => m.count(&p.text),
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_behaviors() {
        let s = "a--b-c";
        let ms = [(1, 2), (2, 3), (4, 5)];
        let run = |b| {
            let mut out = Vec::new();
            split_by(s, &ms, b, &mut out);
            out.iter().map(|&(a, b)| &s[a..b]).collect::<Vec<_>>()
        };
        assert_eq!(run(Behavior::Isolated), ["a", "-", "-", "b", "-", "c"]);
        assert_eq!(run(Behavior::Removed), ["a", "b", "c"]);
        assert_eq!(run(Behavior::MergedWithPrevious), ["a-", "-", "b-", "c"]);
        assert_eq!(run(Behavior::MergedWithNext), ["a", "-", "-b", "-c"]);
        assert_eq!(run(Behavior::Contiguous), ["a", "--", "b", "-", "c"]);
    }

    #[test]
    fn byte_map_matches_gpt2() {
        let m = byte_chars();
        assert_eq!(m[usize::from(b' ')], 'Ġ');
        assert_eq!(m[usize::from(b'\n')], 'Ċ');
        assert_eq!(m[usize::from(b'a')], 'a');
    }

    #[test]
    fn rejects_unsupported() {
        let j = br#"{"normalizer": {"type": "NFC"}, "pre_tokenizer": null,
                     "model": {"type": "BPE", "vocab": {}, "merges": []}}"#;
        assert!(matches!(
            HfTokenizer::from_json(j),
            Err(HfError::Unsupported(_))
        ));
    }
}
