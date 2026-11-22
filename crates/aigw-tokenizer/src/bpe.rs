// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Byte-Pair-Encoding mit Rangtabelle (Verfahren von tiktoken).
//!
//! Ein Stück (Ergebnis des Pre-Tokenizers) beginnt als Folge einzelner Bytes. Wiederholt wird
//! das benachbarte Paar mit dem kleinsten Rang (Rang = Token-ID des zusammengesetzten
//! Byte-Strings) verschmolzen, bei Gleichstand das linke. Kurze Stücke laufen über eine
//! einfache Liste (wie tiktoken), lange (z. B. CJK-Sätze ohne Leerzeichen) über einen Heap
//! mit verketteter Liste (O(n log n) statt O(n²)). Ganze Stücke, die selbst ein Token sind,
//! kosten nur einen Tabellenzugriff.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::hash::{BuildHasherDefault, Hasher};

/// Schneller, nicht kryptografischer Hash für Byte-Strings (FxHash-Variante). Die Schlüssel der
/// Tabelle sind fest (keine Einfügungen aus Anfragen), Kollisionsangriffe sind daher nicht
/// möglich – Nachschlagen kostet höchstens die feste Sondierungslänge der Tabelle.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FxHasher(u64);

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut a = [0u8; 8];
            if let Some(dst) = a.get_mut(..rest.len()) {
                dst.copy_from_slice(rest);
            }
            self.add(u64::from_le_bytes(a));
        }
    }

    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        // Obere Bits gut durchmischen (hashbrown nutzt die obersten 7 Bits als Steuerbyte).
        let x = self.0;
        (x ^ (x >> 29)).wrapping_mul(0xbf58_476d_1ce4_e5b9) ^ (x >> 32)
    }
}

pub(crate) type FxBuild = BuildHasherDefault<FxHasher>;

/// Rang „kein Token“.
const NONE: u32 = u32::MAX;
/// Ab dieser Länge (Bytes) arbeitet die Verschmelzung mit Heap.
const HEAP_THRESHOLD: usize = 96;

/// Rangtabelle: Byte-String → Rang (= Token-ID).
#[derive(Debug)]
pub(crate) struct Ranks {
    map: HashMap<&'static [u8], u32, FxBuild>,
}

/// Fehler beim Lesen einer eingebetteten Tabelle.
#[derive(Debug, thiserror::Error)]
#[error("Rangtabelle ungültig: {0}")]
pub struct TableError(pub(crate) &'static str);

impl Ranks {
    /// Liest das Binärformat (`AGTK`, u32 Anzahl, je Rang: u8 Länge + Bytes).
    pub(crate) fn from_blob(blob: &'static [u8]) -> Result<Self, TableError> {
        let (magic, rest) = blob.split_first_chunk::<4>().ok_or(TableError("zu kurz"))?;
        if magic != b"AGTK" {
            return Err(TableError("Kennung fehlt"));
        }
        let (n, mut rest) = rest.split_first_chunk::<4>().ok_or(TableError("zu kurz"))?;
        let n = u32::from_le_bytes(*n);
        let mut map = HashMap::with_capacity_and_hasher(n as usize, FxBuild::default());
        for rank in 0..n {
            let (&len, r) = rest.split_first().ok_or(TableError("abgeschnitten"))?;
            let (tok, r) = r
                .split_at_checked(usize::from(len))
                .ok_or(TableError("abgeschnitten"))?;
            if len == 0 || map.insert(tok, rank).is_some() {
                return Err(TableError("doppeltes oder leeres Token"));
            }
            rest = r;
        }
        if !rest.is_empty() {
            return Err(TableError("überzählige Bytes"));
        }
        for b in 0..=255u8 {
            if !map.contains_key([b].as_slice()) {
                return Err(TableError("Einzelbyte fehlt"));
            }
        }
        Ok(Self { map })
    }

    /// Anzahl Tokens.
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    #[inline]
    fn rank(&self, b: &[u8]) -> u32 {
        self.map.get(b).copied().unwrap_or(NONE)
    }

    /// Anzahl Tokens eines Stücks.
    #[inline]
    pub(crate) fn count_piece(&self, piece: &[u8]) -> usize {
        match piece.len() {
            0 => 0,
            1 => 1,
            _ if self.map.contains_key(piece) => 1,
            n if n < HEAP_THRESHOLD => merge_small(self, piece, |_, _| ()),
            _ => merge_heap(self, piece, |_, _| ()),
        }
    }

    /// Token-IDs eines Stücks.
    pub(crate) fn encode_piece(&self, piece: &[u8], out: &mut Vec<u32>) {
        match piece.len() {
            0 => {}
            _ if self.map.contains_key(piece) => out.push(self.rank(piece)),
            n if n < HEAP_THRESHOLD => {
                merge_small(self, piece, |a, b| out.push(self.rank(&piece[a..b])));
            }
            _ => {
                merge_heap(self, piece, |a, b| out.push(self.rank(&piece[a..b])));
            }
        }
    }
}

/// tiktoken `_byte_pair_merge`: Liste (Start, Rang des Paars ab hier). `emit` erhält die
/// Byte-Bereiche der Tokens; Rückgabe = Anzahl.
fn merge_small(t: &Ranks, piece: &[u8], mut emit: impl FnMut(usize, usize)) -> usize {
    let n = piece.len();
    let mut parts: Vec<(usize, u32)> = Vec::with_capacity(n + 1);
    let mut min = (NONE, usize::MAX);
    for i in 0..n - 1 {
        let r = t.rank(&piece[i..i + 2]);
        if r < min.0 {
            min = (r, i);
        }
        parts.push((i, r));
    }
    parts.push((n - 1, NONE));
    parts.push((n, NONE));
    let rank_at = |parts: &Vec<(usize, u32)>, i: usize| -> u32 {
        if i + 3 < parts.len() {
            t.rank(&piece[parts[i].0..parts[i + 3].0])
        } else {
            NONE
        }
    };
    while min.0 != NONE {
        let i = min.1;
        if i > 0 {
            parts[i - 1].1 = rank_at(&parts, i - 1);
        }
        parts[i].1 = rank_at(&parts, i);
        parts.remove(i + 1);
        min = (NONE, usize::MAX);
        for (j, &(_, r)) in parts[..parts.len() - 1].iter().enumerate() {
            if r < min.0 {
                min = (r, j);
            }
        }
    }
    for w in parts.windows(2) {
        emit(w[0].0, w[1].0);
    }
    parts.len() - 1
}

/// Gleiche Verschmelzungsreihenfolge mit Heap (Rang, Startposition) und verketteter Liste;
/// veraltete Heap-Einträge werden beim Entnehmen verworfen.
fn merge_heap(t: &Ranks, piece: &[u8], mut emit: impl FnMut(usize, usize)) -> usize {
    let n = piece.len();
    // Teile über Byte-Startpositionen; next[p] = Start des folgenden Teils (n = Ende).
    let mut next: Vec<usize> = (1..=n).collect();
    let mut prev: Vec<usize> = (0..n).map(|i| i.wrapping_sub(1)).collect();
    let mut alive = vec![true; n];
    let mut heap: BinaryHeap<Reverse<(u32, usize, usize)>> = BinaryHeap::with_capacity(n);
    for i in 0..n - 1 {
        let r = t.rank(&piece[i..i + 2]);
        if r != NONE {
            heap.push(Reverse((r, i, i + 2)));
        }
    }
    let mut count = n;
    while let Some(Reverse((_, p, end))) = heap.pop() {
        // Gültig, wenn p noch ein Teil ist und das Paar (p, next[p]) noch bei `end` endet.
        if !alive[p] {
            continue;
        }
        let q = next[p];
        if q >= n || next[q] != end {
            continue;
        }
        // p und q verschmelzen.
        alive[q] = false;
        next[p] = end;
        if end < n {
            prev[end] = p;
        }
        count -= 1;
        // Neue Paare links (prev, p) und rechts (p, next).
        let pp = prev[p];
        if pp != usize::MAX {
            let r = t.rank(&piece[pp..end]);
            if r != NONE {
                heap.push(Reverse((r, pp, end)));
            }
        }
        if end < n {
            let e2 = next[end];
            let r = t.rank(&piece[p..e2]);
            if r != NONE {
                heap.push(Reverse((r, p, e2)));
            }
        }
    }
    let mut p = 0;
    while p < n {
        emit(p, next[p]);
        p = next[p];
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(tokens: &[&[u8]]) -> Ranks {
        let mut blob = b"AGTK".to_vec();
        let mut all: Vec<Vec<u8>> = (0..=255u8).map(|b| vec![b]).collect();
        all.extend(tokens.iter().map(|t| t.to_vec()));
        blob.extend_from_slice(&u32::try_from(all.len()).unwrap().to_le_bytes());
        for t in &all {
            blob.push(u8::try_from(t.len()).unwrap());
            blob.extend_from_slice(t);
        }
        Ranks::from_blob(Box::leak(blob.into_boxed_slice())).unwrap()
    }

    #[test]
    fn small_and_heap_agree() {
        let t = table(&[b"ab", b"bc", b"abc", b"aa", b"aaaa", b"cd", b"abcd"]);
        let mut rnd = 12345u64;
        for len in [2usize, 3, 5, 17, 95, 96, 97, 300] {
            for _ in 0..50 {
                let s: Vec<u8> = (0..len)
                    .map(|_| {
                        rnd = rnd.wrapping_mul(6364136223846793005).wrapping_add(1);
                        b"abcd"[(rnd >> 33) as usize % 4]
                    })
                    .collect();
                let mut a = Vec::new();
                let mut b = Vec::new();
                let na = merge_small(&t, &s, |x, y| a.push((x, y)));
                let nb = merge_heap(&t, &s, |x, y| b.push((x, y)));
                assert_eq!(a, b, "{:?}", String::from_utf8_lossy(&s));
                assert_eq!(na, nb);
            }
        }
    }

    #[test]
    fn rejects_bad_blobs() {
        assert!(Ranks::from_blob(b"XXXX\0\0\0\0").is_err());
        assert!(Ranks::from_blob(b"AGTK\x01\0\0\0\x02a").is_err());
    }
}
