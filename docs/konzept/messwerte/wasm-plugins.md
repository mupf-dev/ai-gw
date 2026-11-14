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

Binärgröße und Bauzeit mit/ohne Feature `wasm`: Messung ausstehend (Lauf wurde wegen
Kontingent-Pause abgebrochen) – siehe Bericht AIGW-63.
