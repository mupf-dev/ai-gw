// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Eigener HNSW-Index (Hierarchical Navigable Small World, Malkov & Yashunin 2016) für die
//! Kosinus-Ähnlichkeit.
//!
//! - Vektoren werden beim Einfügen normiert; Distanz = `1 − ⟨a, b⟩` (Kosinus).
//! - Ebene eines Knotens: `⌊−ln(U)·mL⌋` mit `mL = 1/ln(M)`; Ebene 0 hat bis zu `2·M`
//!   Nachbarn, höhere bis zu `M`.
//! - Nachbarwahl mit der Heuristik (Algorithmus 4 des Papers, wie hnswlib): ein Kandidat wird
//!   nur verbunden, wenn er näher an der Anfrage liegt als an allen schon gewählten Nachbarn –
//!   das hält Cluster untereinander verbunden und verbessert den Recall deutlich.
//! - Löschen erledigt der Aufrufer über `accept` bei der Suche (Grabsteine) und baut bei
//!   vielen Grabsteinen neu auf (siehe [`crate::vector::memory`]); gelöschte Knoten bleiben
//!   bis dahin als Wegpunkte erhalten, damit der Graph zusammenhängend bleibt.
//!
//! Der Index ist nicht nebenläufig veränderbar (`&mut self` beim Einfügen); Suchen laufen
//! über `&self` parallel (Besuchslisten aus einem kleinen Pool).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Mutex, PoisonError};

/// Parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// Nachbarn je Knoten auf Ebenen ≥ 1 (Ebene 0: `2·m`).
    pub m: usize,
    /// Kandidatenliste beim Einfügen.
    pub ef_construction: usize,
    /// Kandidatenliste bei der Suche (mindestens `k`).
    pub ef_search: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            ef_search: 64,
        }
    }
}

const MAX_LEVEL: usize = 16;

/// Distanz + Knoten, geordnet nach Distanz (`total_cmp`).
#[derive(Debug, Clone, Copy)]
struct Cand(f32, u32);

impl PartialEq for Cand {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Cand {}
impl PartialOrd for Cand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cand {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(self.1.cmp(&other.1))
    }
}

/// Besuchsliste mit Generationszähler (kein Löschen je Suche).
#[derive(Debug, Default)]
struct Visited {
    marks: Vec<u32>,
    generation: u32,
}

impl Visited {
    fn reset(&mut self, n: usize) {
        if self.marks.len() < n {
            self.marks.resize(n, 0);
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.marks.iter_mut().for_each(|m| *m = 0);
            self.generation = 1;
        }
    }

    /// `true`, wenn `i` neu ist (und markiert ihn).
    fn insert(&mut self, i: u32) -> bool {
        let slot = &mut self.marks[i as usize];
        if *slot == self.generation {
            false
        } else {
            *slot = self.generation;
            true
        }
    }
}

/// Skalarprodukt (acht Akkumulatoren, damit der Compiler vektorisiert).
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (a, b) = (&a[..n], &b[..n]);
    let mut acc = [0f32; 8];
    let (ca, ra) = a.as_chunks::<8>();
    let (cb, rb) = b.as_chunks::<8>();
    for (x, y) in ca.iter().zip(cb) {
        for ((s, x), y) in acc.iter_mut().zip(x).zip(y) {
            *s += x * y;
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// Normiert `v` auf Länge 1 (Nullvektor bleibt null). `false` bei NaN/∞ oder Nullvektor.
pub fn normalize(v: &mut [f32]) -> bool {
    let norm = dot(v, v).sqrt();
    if !norm.is_finite() || norm == 0.0 {
        return false;
    }
    v.iter_mut().for_each(|x| *x /= norm);
    true
}

/// HNSW-Graph.
pub struct Hnsw {
    dim: usize,
    params: HnswParams,
    m0: usize,
    ml: f64,
    data: Vec<f32>,
    /// Je Knoten je Ebene die Nachbarn.
    links: Vec<Vec<Vec<u32>>>,
    entry: Option<u32>,
    max_level: usize,
    rng: u64,
    pool: Mutex<Vec<Visited>>,
    scratch: Visited,
}

impl std::fmt::Debug for Hnsw {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hnsw")
            .field("dim", &self.dim)
            .field("len", &self.len())
            .field("params", &self.params)
            .field("max_level", &self.max_level)
            .finish()
    }
}

impl Hnsw {
    /// Leerer Index für Vektoren der Länge `dim`.
    pub fn new(dim: usize, params: HnswParams) -> Self {
        let m = params.m.max(2);
        Self {
            dim,
            params: HnswParams { m, ..params },
            m0: 2 * m,
            ml: 1.0 / (m as f64).ln(),
            data: Vec::new(),
            links: Vec::new(),
            entry: None,
            max_level: 0,
            rng: 0x9E37_79B9_7F4A_7C15,
            pool: Mutex::new(Vec::new()),
            scratch: Visited::default(),
        }
    }

    /// Dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Parameter.
    pub fn params(&self) -> HnswParams {
        self.params
    }

    /// Anzahl Knoten (inkl. vom Aufrufer gelöschter).
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Leer?
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// Normierter Vektor eines Knotens.
    pub fn vector(&self, i: u32) -> &[f32] {
        let s = i as usize * self.dim;
        &self.data[s..s + self.dim]
    }

    /// Geschätzter Speicherbedarf eines Knotens (Bytes).
    pub fn node_bytes(&self) -> usize {
        self.dim * 4 + (self.m0 + self.params.m / 2) * 4 + 64
    }

    #[inline]
    fn dist(&self, q: &[f32], i: u32) -> f32 {
        1.0 - dot(q, self.vector(i))
    }

    fn next_rand(&mut self) -> u64 {
        // splitmix64
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn random_level(&mut self) -> usize {
        // U in (0, 1]
        let u = ((self.next_rand() >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        let l = (-u.ln() * self.ml).floor();
        if l.is_finite() && l > 0.0 {
            (l as usize).min(MAX_LEVEL)
        } else {
            0
        }
    }

    fn greedy(&self, q: &[f32], mut ep: Cand, from: usize, to_excl: usize) -> Cand {
        for l in (to_excl..=from).rev() {
            loop {
                let mut changed = false;
                for &nb in &self.links[ep.1 as usize][l] {
                    let d = self.dist(q, nb);
                    if d < ep.0 {
                        ep = Cand(d, nb);
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
        }
        ep
    }

    /// Suche auf einer Ebene; Ergebnis aufsteigend nach Distanz.
    fn search_layer(
        &self,
        q: &[f32],
        eps: &[Cand],
        ef: usize,
        layer: usize,
        visited: &mut Visited,
    ) -> Vec<Cand> {
        visited.reset(self.links.len());
        let mut cands: BinaryHeap<std::cmp::Reverse<Cand>> = BinaryHeap::with_capacity(ef * 2);
        let mut best: BinaryHeap<Cand> = BinaryHeap::with_capacity(ef + 1);
        for &e in eps {
            if visited.insert(e.1) {
                cands.push(std::cmp::Reverse(e));
                best.push(e);
            }
        }
        while best.len() > ef {
            best.pop();
        }
        while let Some(std::cmp::Reverse(c)) = cands.pop() {
            let worst = best.peek().map_or(f32::INFINITY, |b| b.0);
            if c.0 > worst && best.len() >= ef {
                break;
            }
            for &nb in &self.links[c.1 as usize][layer] {
                if !visited.insert(nb) {
                    continue;
                }
                let d = self.dist(q, nb);
                let worst = best.peek().map_or(f32::INFINITY, |b| b.0);
                if best.len() < ef || d < worst {
                    cands.push(std::cmp::Reverse(Cand(d, nb)));
                    best.push(Cand(d, nb));
                    if best.len() > ef {
                        best.pop();
                    }
                }
            }
        }
        best.into_sorted_vec()
    }

    /// Heuristik (Algorithmus 4): `cands` aufsteigend nach Distanz zur Basis.
    fn select(&self, cands: &[Cand], m: usize) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::with_capacity(m);
        for &c in cands {
            if out.len() >= m {
                break;
            }
            let v = self.vector(c.1);
            let good = out.iter().all(|&r| 1.0 - dot(v, self.vector(r)) > c.0);
            if good {
                out.push(c.1);
            }
        }
        out
    }

    /// Fügt einen Vektor ein (wird normiert); liefert die Knotennummer.
    /// `None` bei falscher Dimension oder ungültigem Vektor.
    pub fn insert(&mut self, v: &[f32]) -> Option<u32> {
        if v.len() != self.dim {
            return None;
        }
        let mut q = v.to_vec();
        if !normalize(&mut q) {
            return None;
        }
        let id = u32::try_from(self.links.len()).ok()?;
        let level = self.random_level();
        self.data.extend_from_slice(&q);
        self.links.push(vec![Vec::new(); level + 1]);
        let Some(entry) = self.entry else {
            self.entry = Some(id);
            self.max_level = level;
            return Some(id);
        };
        let mut ep = Cand(self.dist(&q, entry), entry);
        if self.max_level > level {
            ep = self.greedy(&q, ep, self.max_level, level + 1);
        }
        let mut visited = std::mem::take(&mut self.scratch);
        let mut eps = vec![ep];
        for l in (0..=level.min(self.max_level)).rev() {
            let w = self.search_layer(&q, &eps, self.params.ef_construction, l, &mut visited);
            let neigh = self.select(&w, self.params.m);
            let mmax = if l == 0 { self.m0 } else { self.params.m };
            for &nb in &neigh {
                let list = &mut self.links[nb as usize][l];
                list.push(id);
                if list.len() > mmax {
                    let base = nb;
                    let bv = self.vector(base).to_vec();
                    let mut cs: Vec<Cand> = self.links[base as usize][l]
                        .iter()
                        .map(|&x| Cand(1.0 - dot(&bv, self.vector(x)), x))
                        .collect();
                    cs.sort_unstable();
                    let kept = self.select(&cs, mmax);
                    self.links[base as usize][l] = kept;
                }
            }
            self.links[id as usize][l] = neigh;
            eps = w;
        }
        self.scratch = visited;
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(id);
        }
        Some(id)
    }

    /// Die `k` nächsten Knoten zu `q` (bereits normiert oder nicht), die `accept` erfüllen,
    /// als (Knoten, Kosinus-Ähnlichkeit) absteigend nach Ähnlichkeit. `ef` = Kandidatenliste
    /// (`None` = `ef_search`).
    pub fn search(
        &self,
        q: &[f32],
        k: usize,
        ef: Option<usize>,
        accept: impl Fn(u32) -> bool,
    ) -> Vec<(u32, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if q.len() != self.dim || k == 0 {
            return Vec::new();
        }
        let mut qn = q.to_vec();
        if !normalize(&mut qn) {
            return Vec::new();
        }
        let ep = Cand(self.dist(&qn, entry), entry);
        let ep = if self.max_level > 0 {
            self.greedy(&qn, ep, self.max_level, 1)
        } else {
            ep
        };
        let ef = ef.unwrap_or(self.params.ef_search).max(k);
        let mut visited = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_default();
        let w = self.search_layer(&qn, &[ep], ef, 0, &mut visited);
        {
            let mut pool = self.pool.lock().unwrap_or_else(PoisonError::into_inner);
            if pool.len() < 64 {
                pool.push(visited);
            }
        }
        w.into_iter()
            .filter(|c| accept(c.1))
            .take(k)
            .map(|c| (c.1, 1.0 - c.0))
            .collect()
    }
}

/// Exakte Suche (Brute-Force, Kosinus) – Referenz für Recall-Tests.
pub fn brute_force(data: &[Vec<f32>], q: &[f32], k: usize) -> Vec<(u32, f32)> {
    let qn = dot(q, q).sqrt().max(f32::MIN_POSITIVE);
    let mut all: Vec<Cand> = data
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let vn = dot(v, v).sqrt().max(f32::MIN_POSITIVE);
            Cand(
                1.0 - dot(q, v) / (qn * vn),
                u32::try_from(i).unwrap_or(u32::MAX),
            )
        })
        .collect();
    let k = k.min(all.len());
    if k == 0 {
        return Vec::new();
    }
    all.select_nth_unstable(k - 1);
    all.truncate(k);
    all.sort_unstable();
    all.into_iter().map(|c| (c.1, 1.0 - c.0)).collect()
}

/// Einfacher, reproduzierbarer Zufallsgenerator (splitmix64) für Tests und Benchmarks.
#[derive(Debug, Clone)]
pub struct TestRng(u64);

impl TestRng {
    /// Mit Startwert.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Nächste Zufallszahl.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Gleichverteilt in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standardnormalverteilt (Box-Muller).
    pub fn normal(&mut self) -> f32 {
        let u1 = self.uniform().max(1e-12);
        let u2 = self.uniform();
        ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
    }

    /// Geclusterte Vektoren (Gauß-Mischung um `clusters` zufällige Zentren, Streuung `spread`)
    /// – ähnlich realen Embeddings, deren Ähnlichkeiten nicht gleichverteilt sind.
    pub fn clustered(
        &mut self,
        n: usize,
        dim: usize,
        clusters: usize,
        spread: f32,
    ) -> Vec<Vec<f32>> {
        let centers: Vec<Vec<f32>> = (0..clusters.max(1))
            .map(|_| (0..dim).map(|_| self.normal()).collect())
            .collect();
        (0..n)
            .map(|_| {
                let c = &centers[(self.next_u64() % centers.len() as u64) as usize];
                c.iter().map(|x| x + spread * self.normal()).collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recall(n: usize, dim: usize, params: HnswParams, queries: usize, k: usize) -> f64 {
        let mut rng = TestRng::new(42);
        let data = rng.clustered(n, dim, 50, 0.6);
        let mut h = Hnsw::new(dim, params);
        for v in &data {
            assert!(h.insert(v).is_some());
        }
        let qs = rng.clustered(queries, dim, 50, 0.6);
        let mut hits = 0usize;
        for q in &qs {
            let truth: Vec<u32> = brute_force(&data, q, k).into_iter().map(|x| x.0).collect();
            let got = h.search(q, k, None, |_| true);
            hits += got.iter().filter(|(i, _)| truth.contains(i)).count();
        }
        hits as f64 / (queries * k) as f64
    }

    #[test]
    fn recall_small_against_brute_force() {
        let r = recall(
            3000,
            64,
            HnswParams {
                m: 16,
                ef_construction: 100,
                ef_search: 64,
            },
            100,
            10,
        );
        assert!(r >= 0.95, "Recall {r}");
    }

    #[test]
    fn exact_vector_is_found_with_similarity_one_and_accept_filters() {
        let mut rng = TestRng::new(7);
        let data = rng.clustered(500, 16, 5, 0.3);
        let mut h = Hnsw::new(16, HnswParams::default());
        for v in &data {
            h.insert(v);
        }
        let got = h.search(&data[123], 1, None, |_| true);
        assert_eq!(got[0].0, 123);
        assert!((got[0].1 - 1.0).abs() < 1e-4);
        let got = h.search(&data[123], 1, None, |i| i != 123);
        assert_ne!(got[0].0, 123);
        assert!(h.insert(&[1.0; 3]).is_none()); // falsche Dimension
        assert!(h.insert(&[0.0; 16]).is_none()); // Nullvektor
        assert_eq!(h.search(&[1.0; 3], 1, None, |_| true), vec![]);
    }

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..37).map(|i| i as f32 * 0.5).collect();
        let b: Vec<f32> = (0..37).map(|i| 1.0 - i as f32 * 0.1).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-2);
    }
}
