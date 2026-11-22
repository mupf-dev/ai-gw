<!--
SPDX-FileCopyrightText: 2026 mupf-dev
SPDX-License-Identifier: CC-BY-4.0
-->
# WASM-Plugins (Beispiele)

Komponenten für die Plugin-Art `wasm` (AIGW-63, Konzept:
[docs/konzept/17-wasm-plugins.md](../docs/konzept/17-wasm-plugins.md)). Schnittstelle:
[crates/aigw-wasm/wit/plugin.wit](../crates/aigw-wasm/wit/plugin.wit).

| Plugin | Zweck |
|---|---|
| `keyword-guard` | Beispiel: Stichwörter blockieren/maskieren, Header setzen |
| `testkit` | Testmodi für die Tests und die Latenzmessung von `aigw-wasm` |

Bauen (Rust ≥ 1.82 mit Ziel `wasm32-wasip2`, erzeugt direkt eine Komponente):

```sh
rustup target add wasm32-wasip2
cd plugins/keyword-guard && cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/aigw_plugin_keyword_guard.wasm
sha256sum target/wasm32-wasip2/release/aigw_plugin_keyword_guard.wasm
```

`scripts/build-wasm-plugins.sh` baut beide und legt sie als Testdateien unter
`crates/aigw-wasm/tests/data/` ab.

Hochladen (globaler `admin`):

```sh
curl -X PUT "$AIGW/admin/api/v1/wasm-modules/keyword-guard?sha256=$SHA" \
  -H "authorization: Bearer $TOKEN" -H "if-match: *" \
  -H 'content-type: application/wasm' --data-binary @aigw_plugin_keyword_guard.wasm
```
