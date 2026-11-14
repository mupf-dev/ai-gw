# Messwerte WASM-Plugins (AIGW-63)

`cargo bench -p aigw-wasm --bench wasm` (divan, Release, Buildbox x86_64 geteilt, 2026-11-14,
100 Proben; Streuung durch parallele Last anderer Builds):

| Benchmark | Median | Schnellster | Bemerkung |
|---|---|---|---|
| `empty_plugin_request` | 42,6 µs | 32,5 µs | Leer-Plugin (`testkit`, `mode: pass`), Anfrage-Phase: IR → JSON, frische Instanz (Pooling), Aufruf, `spawn_blocking`, Ergebnis anwenden |
| `empty_plugin_chunk` | 22,1 µs | 21,7 µs | Leer-Plugin, ein Stream-Ereignis (synchron, `block_in_place`) |
| `keyword_guard_request` | 79,4 µs | 40,7 µs | Beispiel `keyword-guard` (Konfiguration parsen, Wörter suchen) |

Einordnung: deutlich unter dem Budget Q1 (p50 < 200 µs Gateway-Overhead) und ≈ 3–4
Größenordnungen unter einem Webhook-Aufruf; im Stream-Pfad kostet ein WASM-Plugin ≈ 20 µs je
Ereignis (bei ~ 50 Ereignissen/s eines Streams vernachlässigbar, bei sehr vielen Stream-Plugins
spürbar – dann `phases: [request]` bzw. Antwort gepuffert prüfen).

Modulgrößen (Release, `opt-level = "s"`, LTO, strip): `keyword-guard.wasm` 127 814 Byte,
`testkit.wasm` 73 314 Byte. Kompilieren einer Komponente: einmal je SHA-256 (Cache).

## Binärgröße und Bauzeit (Feature `wasm`)

Release-Profil (thin-LTO, `codegen-units = 1`, `strip`), Buildbox x86_64 mit 8 Kernen, 2026-11-14:

| Build | Binary `aigw` |
|---|---|
| Standard (`mimalloc`, `embedded-tokenizers`, `wasm`) | 54,8 MB (54 789 312 Byte) |
| ohne `wasm` (`--no-default-features --features mimalloc,embedded-tokenizers`) | 41,1 MB (41 071 208 Byte) |

- Mehrgröße durch `wasm`: **+13,7 MB (+33 %)**.
- Zusätzliche Bauzeit: 58 Crates werden nur für `aigw-wasm` gebraucht (wasmtime, Cranelift,
  wasmtime-wasi, wasmparser …). Neubau nur dieser Crates plus Neulinken: 367 s Wandzeit, ≈ 554 s
  CPU-Zeit (`cargo build --release --timings`). Der Build ohne `wasm` brauchte nach dem
  Featurewechsel 437 s (thin-LTO linkt und optimiert das Binary vollständig neu).
- Bewertung: deutlicher, aber vertretbarer Aufwand – das Feature bleibt standardmäßig an.
  Für schlanke Images: Build ohne `wasm`.
- MSRV: wasmtime 50 braucht Rust ≥ 1.97 (`aigw-wasm` setzt `rust-version = "1.97"`); der
  Workspace-MSRV 1.95 gilt für den Build ohne Feature `wasm`.
