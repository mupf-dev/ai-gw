// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Recall und Latenz des eigenen HNSW gegen Brute-Force (AIGW-40, Abnahme: Recall ≥ 0,95 bei
//! 100 000 Vektoren mit 768 Dimensionen, p99 im Bericht).
//!
//! ```text
//! cargo run --release -p aigw-cache --example hnsw_recall
//! N=100000 DIM=768 QUERIES=500 K=10 M=16 EFC=200 EF=32,64,128,256 DATA=clustered,uniform \
//!   cargo run --release -p aigw-cache --example hnsw_recall
//! ```
//!
//! Daten: `clustered` = Gauß-Mischung (200 Zentren, Streuung 1,0 – ähnlich realen Embeddings,
//! deren Ähnlichkeiten nicht gleichverteilt sind), `uniform` = isotrope Normalverteilung
//! (schwerster Fall für jeden ANN-Index). Anfragen stammen aus derselben Verteilung, sind aber
//! nicht im Index. Recall@k = Anteil der exakten k nächsten Nachbarn (Brute-Force, Kosinus)
//! in den k Treffern des HNSW. Latenz je Suche single-threaded.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::panic
)]

use std::time::Instant;

use aigw_cache::hnsw::{Hnsw, HnswParams, TestRng, brute_force};

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

fn main() {
    let n: usize = env("N", 100_000);
    let dim: usize = env("DIM", 768);
    let queries: usize = env("QUERIES", 500);
    let k: usize = env("K", 10);
    let m: usize = env("M", 16);
    let efc: usize = env("EFC", 200);
    let efs: Vec<usize> = std::env::var("EF")
        .unwrap_or_else(|_| "32,64,128,256".into())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let kinds = std::env::var("DATA").unwrap_or_else(|_| "clustered,uniform".into());
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    println!(
        "# HNSW-Recall (aigw-cache), N={n}, Dim={dim}, Anfragen={queries}, k={k}, M={m}, efConstruction={efc}, Threads Brute-Force={threads}"
    );
    for kind in kinds.split(',') {
        let mut rng = TestRng::new(20261113);
        let (data, qs) = match kind {
            "uniform" => (
                rng.clustered(n, dim, 1, 1e3),
                rng.clustered(queries, dim, 1, 1e3),
            ),
            _ => {
                let all = rng.clustered(n + queries, dim, 200, 1.0);
                let (d, q) = all.split_at(n);
                (d.to_vec(), q.to_vec())
            }
        };
        let t = Instant::now();
        let mut h = Hnsw::new(
            dim,
            HnswParams {
                m,
                ef_construction: efc,
                ef_search: 64,
            },
        );
        for v in &data {
            h.insert(v).expect("Einfügen");
        }
        let build = t.elapsed();
        // Exakte Nachbarn parallel.
        let t = Instant::now();
        let chunk = queries.div_ceil(threads);
        let truth: Vec<Vec<u32>> = std::thread::scope(|s| {
            let handles: Vec<_> = qs
                .chunks(chunk.max(1))
                .map(|part| {
                    let data = &data;
                    s.spawn(move || {
                        part.iter()
                            .map(|q| brute_force(data, q, k).into_iter().map(|x| x.0).collect())
                            .collect::<Vec<Vec<u32>>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().expect("Thread"))
                .collect()
        });
        let bf = t.elapsed();
        println!();
        println!(
            "## Daten {kind}: Aufbau {:.1} s ({:.0} µs/Vektor), Brute-Force {:.1} s",
            build.as_secs_f64(),
            build.as_secs_f64() * 1e6 / n as f64,
            bf.as_secs_f64()
        );
        println!();
        println!("| efSearch | Recall@{k} | Recall@1 | p50 µs | p99 µs | max µs |");
        println!("|---:|---:|---:|---:|---:|---:|");
        for &ef in &efs {
            let mut lat = Vec::with_capacity(queries);
            let mut hits = 0usize;
            let mut top1 = 0usize;
            for (q, tr) in qs.iter().zip(&truth) {
                let t = Instant::now();
                let got = h.search(q, k, Some(ef), |_| true);
                lat.push(t.elapsed().as_secs_f64() * 1e6);
                hits += got.iter().filter(|(i, _)| tr.contains(i)).count();
                top1 += usize::from(got.first().map(|x| x.0) == tr.first().copied());
            }
            lat.sort_by(f64::total_cmp);
            println!(
                "| {ef} | {:.4} | {:.4} | {:.0} | {:.0} | {:.0} |",
                hits as f64 / (queries * k) as f64,
                top1 as f64 / queries as f64,
                percentile(&lat, 0.5),
                percentile(&lat, 0.99),
                lat.last().copied().unwrap_or(0.0)
            );
        }
    }
}
