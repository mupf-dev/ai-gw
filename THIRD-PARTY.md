<!-- Erzeugt mit scripts/third-party.sh – nicht von Hand bearbeiten. -->
# Drittanbieter-Lizenzen

ai-gw selbst steht unter AGPL-3.0-or-later (Code) bzw. CC-BY-4.0 (Dokumentation).
Diese Übersicht listet die Lizenzen der Komponenten, die in das Binary `aigw` und die
eingebettete Admin-UI eingehen. Die zulässigen Lizenzen legt `deny.toml` fest
(geprüft mit `cargo deny check licenses`). Lizenztexte: jeweilige Quellpakete.

## Eingebettete Daten

| Datei | Herkunft | Lizenz |
|---|---|---|
| `crates/aigw-tokenizer/data/cl100k_base.bin`, `o200k_base.bin` | BPE-Rangtabellen aus den von OpenAI veröffentlichten `*.tiktoken`-Dateien (Projekt tiktoken), umkodiert | MIT (`LICENSES/MIT.txt`; **vor einem öffentlichen Release rechtlich bestätigen**) |
| `crates/aigw-billing/catalog/prices.yaml` | Eigene, gepflegte Datei (AIGW-85): Listenpreise (Fakten) aus den öffentlichen Preisseiten von OpenAI, Anthropic, Mistral und Google (Gemini API); Werte abgeglichen mit LiteLLM `model_prices_and_context_window.json` (BerriAI, MIT) – keine Übernahme der fremden Datei | AGPL-3.0-or-later (wie der Code) |
| Laufzeit: Azure Retail Prices API (`prices.azure.com`) | Öffentliche Preisabfrage von Microsoft ohne Anmeldung, nur zur Anzeige von Vorschlägen; nichts wird mitgeliefert | – |

## Rust-Abhängigkeiten des Binaries (285 Crates)

| Lizenz (SPDX-Ausdruck) | Anzahl | Crates |
|---|---|---|
| `MIT OR Apache-2.0` | 147 | allocator-api2 0.2.21, annotate-snippets 0.12.18, anstyle 1.0.14, arc-swap 1.9.5, arrayvec 0.7.8, async-trait 0.1.92, base64 0.22.1, base64 0.23.1, bitflags 2.13.2, block-buffer 0.10.4, block-buffer 0.12.1, bumpalo 3.20.3, cfg-if 1.0.5, chacha20 0.10.2, clap 4.6.7, clap_builder 4.6.7, clap_derive 4.6.7, clap_lex 1.1.1, core-foundation 0.10.1, core-foundation-sys 0.8.7, cpufeatures 0.2.17, cpufeatures 0.3.1, crc 3.4.0, crc-catalog 2.5.0, crc32fast 1.5.5, crossbeam-queue 0.3.14, crossbeam-utils 0.8.23, crypto-common 0.1.6, crypto-common 0.2.2, deranged 0.5.9, digest 0.10.7, digest 0.11.3, dirs 6.0.0, dirs-sys 0.5.0, displaydoc 0.2.7, either 1.19.0, encoding_rs_io 0.1.8, errno 0.3.15, etcetera 0.11.0, flate2 1.1.11, form_urlencoded 1.2.2, futures-channel 0.3.34, futures-core 0.3.34, futures-executor 0.3.34, futures-intrusive 0.5.0, futures-io 0.3.34, futures-sink 0.3.34, futures-task 0.3.34, futures-util 0.3.34, getrandom 0.2.17, getrandom 0.4.3, granit-parser 2.1.0, hashbrown 0.14.5, hashbrown 0.16.1, hashbrown 0.17.1, hashlink 0.11.1, heck 0.5.0, hex 0.4.3, hkdf 0.13.0, hmac 0.13.0, http 1.5.0, httparse 1.10.1, httpdate 1.0.3, hybrid-array 0.4.15, idna 1.1.0, itoa 1.0.19, js-sys 0.3.107, lazy_static 1.5.1, libc 0.2.191, lock_api 0.4.14, log 0.4.35, md-5 0.11.0, mime 0.3.18, num-conv 0.2.2, num-traits 0.2.19, once_cell 1.21.4, openssl-probe 0.2.2, parking_lot 0.12.5, parking_lot_core 0.9.12, percent-encoding 2.3.2, powerfmt 0.2.1, proc-macro2 1.0.107, quote 1.0.47, rand 0.10.3, rand_core 0.10.1, regex 1.13.1, regex-automata 0.4.18, regex-syntax 0.8.11, rustls-pki-types 1.16.0, rustversion 1.0.23, scopeguard 1.2.0, security-framework 3.7.0, security-framework-sys 2.17.0, serde 1.0.229, serde-saphyr 2.1.1, serde_core 1.0.229, serde_derive 1.0.229, serde_json 1.0.154, serde_path_to_error 0.1.20, sha1 0.11.0, sha2 0.10.9, sha2 0.11.1, signal-hook-registry 1.4.8, simdutf8 0.1.5, smallvec 1.16.5, socket2 0.6.5, sqlx 0.9.0, sqlx-core 0.9.0, sqlx-macros 0.9.0, sqlx-macros-core 0.9.0, sqlx-mysql 0.9.0, sqlx-postgres 0.9.0, sqlx-sqlite 0.9.0, stable_deref_trait 1.2.1, syn 2.0.119, syn 3.0.10, terminal_size 0.4.4, thiserror 2.0.21, thiserror-impl 2.0.21, thread_local 1.1.10, time 0.3.55, time-core 0.1.9, time-macros 0.2.32, tokio-rustls 0.26.6, typenum 1.20.1, unicase 2.10.0, unicode-bidi 0.3.18, unicode-width 0.2.3, url 2.5.8, utoipa 6.0.0, utoipa-gen 6.0.1, wasm-bindgen 0.2.130, wasm-bindgen-macro 0.2.130, wasm-bindgen-macro-support 0.2.130, wasm-bindgen-shared 0.2.130, windows-link 0.2.1, windows-sys 0.52.0, windows-sys 0.61.2, windows-targets 0.52.6, windows_aarch64_gnullvm 0.52.6, windows_aarch64_msvc 0.52.6, windows_i686_gnu 0.52.6, windows_i686_gnullvm 0.52.6, windows_i686_msvc 0.52.6, windows_x86_64_gnu 0.52.6, windows_x86_64_gnullvm 0.52.6, windows_x86_64_msvc 0.52.6 |
| `MIT` | 48 | atoi 2.0.0, axum 0.8.9, axum-core 0.5.6, bytes 1.12.1, combine 4.6.9, dashmap 6.2.1, dotenvy 0.15.7, generic-array 0.14.9, h2 0.4.20, http-body 1.1.0, http-body-util 0.1.5, hyper 1.12.0, hyper-util 0.1.21, libmimalloc-sys 0.1.49, libredox 0.1.25, libsqlite3-sys 0.37.0, matchers 0.2.0, mimalloc 0.1.52, mime_guess 2.0.5, mio 1.2.4, nu-ansi-term 0.50.3, redox_syscall 0.5.18, redox_users 0.5.3, rust-embed 8.14.0, rust-embed-impl 8.14.0, rust-embed-utils 8.14.0, schannel 0.1.30, sharded-slab 0.1.7, simd-adler32 0.3.10, slab 0.4.12, spin 0.9.9, synstructure 0.14.0, tokio 1.54.1, tokio-macros 2.7.2, tokio-stream 0.1.19, tokio-util 0.7.20, tower 0.5.3, tower-layer 0.3.3, tower-service 0.3.3, tracing 0.1.44, tracing-attributes 0.1.31, tracing-core 0.1.36, tracing-serde 0.2.0, tracing-subscriber 0.3.23, try-lock 0.2.5, valuable 0.1.1, want 0.3.2, zmij 1.0.23 |
| `Apache-2.0 OR MIT` | 21 | async-lock 3.5.0, atomic-waker 1.1.2, cmov 0.5.4, const-oid 0.10.3, ctutils 0.4.8, equivalent 1.0.3, event-listener 5.4.2, event-listener-strategy 0.5.4, fastrand 2.5.0, idna_adapter 1.2.2, indexmap 2.14.2, multiversion_no_op 1.0.0, nohash-hasher 0.2.0, parking 2.2.1, pin-project-lite 0.2.17, portable-atomic 1.15.0, portable-atomic-util 0.2.8, utf8_iter 1.1.0, utf_types 1.0.0, uuid 1.29.2, zeroize 1.9.1 |
| `Unicode-3.0` | 18 | icu_collections 2.4.0, icu_locale_core 2.4.0, icu_normalizer 2.4.0, icu_normalizer_data 2.4.0, icu_properties 2.4.0, icu_properties_data 2.4.0, icu_provider 2.4.0, litemap 0.8.3, potential_utf 0.1.6, tinystr 0.8.5, writeable 0.6.4, yoke 0.8.4, yoke-derive 0.8.4, zerofrom 0.1.8, zerofrom-derive 0.1.8, zerotrie 0.2.5, zerovec 0.11.9, zerovec-derive 0.11.7 |
| `Unlicense OR MIT` | 8 | aho-corasick 1.1.5, byteorder 1.5.0, jiff 0.2.43, jiff-core 0.1.5, jiff-static 0.2.43, jiff-tzdb 0.1.9, memchr 2.8.3, winapi-util 0.1.11 |
| `MIT/Apache-2.0` | 5 | arraydeque 0.5.1, core_detect 1.0.0, serde_urlencoded 0.7.1, shellexpand 3.1.2, stringprep 0.1.5 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 3 | unicode-ident 1.0.27, unicode-normalization 0.1.26, unicode-properties 0.1.5 |
| `Apache-2.0 OR ISC OR MIT` | 3 | hyper-rustls 0.27.10, rustls 0.23.46, rustls-native-certs 0.8.5 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 3 | linux-raw-sys 0.12.1, rustix 1.1.6, wasi 0.11.1+wasi-snapshot-preview1 |
| `BSD-3-Clause` | 3 | redis 1.8.1, sha1_smol 1.0.1, subtle 2.6.1 |
| `Apache-2.0` | 2 | backon 1.6.0, sync_wrapper 1.0.2 |
| `ISC` | 2 | rustls-webpki 0.103.17, untrusted 0.9.0 |
| `Unlicense/MIT` | 2 | same-file 1.0.6, walkdir 2.5.0 |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | 1 | encoding_rs 0.8.44 |
| `0BSD OR MIT OR Apache-2.0` | 1 | adler2 2.0.1 |
| `Apache-2.0 / MIT` | 1 | fnv 1.0.7 |
| `Apache-2.0 AND ISC` | 1 | ring 0.17.14 |
| `Apache-2.0 OR BSL-1.0` | 1 | ryu 1.0.23 |
| `Apache-2.0 OR BSL-1.0 OR MIT` | 1 | whoami 2.1.3 |
| `Apache-2.0 OR MIT OR Zlib` | 1 | arcstr 1.2.0 |
| `Apache-2.0/MIT` | 1 | flume 0.12.0 |
| `BSL-1.0` | 1 | xxhash-rust 0.8.19 |
| `CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception` | 1 | blake3 1.8.7 |
| `CC0-1.0 OR MIT-0 OR Apache-2.0` | 1 | constant_time_eq 0.4.2 |
| `CDLA-Permissive-2.0` | 1 | webpki-roots 1.0.12 |
| `ISC AND (Apache-2.0 OR ISC)` | 1 | aws-lc-rs 1.19.0 |
| `ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)` | 1 | aws-lc-sys 0.47.0 |
| `MIT AND BSD-3-Clause` | 1 | matchit 0.8.4 |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | 1 | r-efi 6.0.0 |
| `MIT OR Zlib OR Apache-2.0` | 1 | miniz_oxide 0.9.1 |
| `MPL-2.0` | 1 | option-ext 0.2.0 |
| `Zlib` | 1 | foldhash 0.2.0 |
| `Zlib OR Apache-2.0 OR MIT` | 1 | tinyvec 1.13.3 |

## Admin-UI (npm, Laufzeit-Abhängigkeiten im Bundle)

| Lizenz | Pakete |
|---|---|
| `Apache-2.0` | @internationalized/date 3.12.4, @swc/helpers 0.5.24, aria-query 5.3.1, axobject-query 4.1.0 |
| `BSD-3-Clause` | d3-array 2.12.1, d3-interpolate-path 2.3.0, d3-path 1.0.9, d3-sankey 0.12.3, d3-shape 1.3.7, d3-tile 1.0.0, rw 1.3.3 |
| `ISC` | d3-array 3.2.5, d3-chord 3.0.2, d3-color 3.1.1, d3-contour 4.0.3, d3-delaunay 6.0.6, d3-dispatch 3.0.2, d3-dsv 3.0.2, d3-force 3.0.1, d3-format 3.1.3, d3-geo 3.1.2, d3-geo-polygon 2.0.1, d3-geo-projection 4.0.0, d3-geo-voronoi 2.1.0, d3-hierarchy 3.1.2,3.1.3, d3-interpolate 3.0.2, d3-path 3.1.1, d3-polygon 3.0.2, d3-quadtree 3.0.2, d3-random 3.0.2, d3-scale 4.0.3, d3-scale-chromatic 3.1.1, d3-shape 3.2.1, d3-time 3.1.1, d3-time-format 4.1.1, d3-timer 3.0.2, d3-tricontour 1.1.0, delaunator 5.1.0, internmap 1.0.1,2.0.3 |
| `MIT` | @dagrejs/dagre 2.0.4, @dagrejs/graphlib 3.0.4, @floating-ui/core 1.8.0, @floating-ui/dom 1.8.0, @floating-ui/utils 0.2.12, @jridgewell/gen-mapping 0.3.13, @jridgewell/remapping 2.3.5, @jridgewell/resolve-uri 3.1.2, @jridgewell/sourcemap-codec 1.6.0, @jridgewell/trace-mapping 0.3.31, @layerstack/svelte-actions 1.0.1-next.20, @layerstack/svelte-state 0.1.0-next.25, @layerstack/tailwind 2.0.0-next.21, @layerstack/utils 2.0.0-next.19, @sveltejs/acorn-typescript 1.0.13, @types/d3-array 3.2.2, @types/d3-contour 3.0.6, @types/estree 1.0.10, @types/geojson 7946.0.16, acorn 8.19.0, bits-ui 2.20.6, clsx 2.1.1, commander 7.2.0, dequal 2.0.3, devalue 5.9.4, esm-env 1.2.2, esrap 2.4.0, iconv-lite 0.6.3, inline-style-parser 0.2.7, is-reference 3.0.3, layerchart 2.9.2, locate-character 3.0.0, lz-string 1.5.0, magic-string 0.30.21, memoize 10.2.0, mimic-function 5.0.1, openapi-fetch 0.17.0, openapi-typescript-helpers 0.1.0, runed 0.35.1,0.37.1, safer-buffer 2.1.2, style-to-object 1.0.14, svelte 5.57.3, svelte-toolbelt 0.10.7, tabbable 6.5.1, tailwind-merge 3.7.0, uplot 1.6.32, zimmerframe 1.1.5 |
| `Unlicense` | robust-predicates 3.0.3 |

## Container-Image

Das Laufzeit-Image basiert auf `gcr.io/distroless/cc-debian12` (Debian-Pakete u. a. glibc
[LGPL-2.1-or-later], libgcc [GPL-3.0-or-later WITH GCC-exception-3.1], ca-certificates
[MPL-2.0]); deren Lizenzen liegen im Image unter `/usr/share/doc/*/copyright`.
