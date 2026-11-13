# ai-gw

Eigenes AI-Gateway nach Vorbild von LiteLLM, Kong AI Gateway und Azure AI Gateway.

- **Gateway + Admin-API:** Rust (tokio, hyper/axum, rustls), ein Binary
- **Admin-UI:** Svelte 5 + Vite + Bits UI + Tailwind, ins Binary eingebettet
- **Datenhaltung:** SQLite (Standard) oder Postgres; Zähler-Backends `local`, `postgres-sync`, `redis`

Entscheidungen: [docs/konzept/00-entscheidungen.md](docs/konzept/00-entscheidungen.md)

## Aufbau

| Pfad | Inhalt |
|---|---|
| `crates/aigw` | Binary: `aigw serve --config <pfad>`, `aigw openapi` |
| `crates/aigw-core` | gemeinsame Typen, Fehler, Konfigurationsmodell |
| `crates/aigw-proxy` | Hot Path (OpenAI-kompatible Endpunkte, `/healthz`) |
| `crates/aigw-providers` | Anbieter-Adapter (Trait `Provider`) |
| `crates/aigw-store` | Persistenz (sqlx: SQLite, Postgres) |
| `crates/aigw-counters` | Zähler-Backends (Trait `CounterBackend`, `local`) |
| `crates/aigw-plugins` | Plugin-Kette |
| `crates/aigw-admin` | Admin-API (axum + utoipa/OpenAPI) und eingebettete UI |
| `ui/` | Admin-UI (Svelte 5, Vite, TypeScript, Bits UI, Tailwind 4, pnpm) |
| `config/` | Beispielkonfigurationen |
| `scripts/` | `check.sh` (alle Prüfungen), `gen-api.sh`, `runtime-smoke.sh`, `setup-tools.sh` |

Endpunkte – Datenebene (`server.data.listen`, Standard `127.0.0.1:4000`):

- `POST /v1/chat/completions`, `POST /v1/messages`, `GET /v1/models` – OpenAI- und
  Anthropic-kompatibel (vorerst Mock-Anbieter `echo`, siehe `docs/konzept/03-ir-und-formate.md`)
- `GET /healthz` – Liveness

Steuerebene (`server.control.listen`, Standard `127.0.0.1:4001`; gleiche Adresse → ein Port):

- `GET /healthz` – Liveness
- `GET /admin/api/v1/version` – Version
- `GET /admin/api/openapi.json` – OpenAPI-Spezifikation der Admin-API
- `GET /admin/` – Admin-UI (SPA)

## Entwicklung

Der Entwicklungsserver ist klein und hat keine Rust-/Node-Toolchain. Gebaut und getestet wird
auf der Buildbox (siehe `/opt/buildfarm/README.md`). Ergebnisse, die eingecheckt werden
(`Cargo.lock`, `ui/pnpm-lock.yaml`, generierte Dateien), immer per `--fetch` zurückholen –
die Box wird vom Watchdog gelöscht.

```bash
export BUILDBOX_OWNER=ai-gw
buildbox status                       # läuft schon eine Box? Dann mitbenutzen
buildbox up                           # sonst starten (≈ 2–5 Min.)

# einmalig pro Box: rustup + Toolchain aus rust-toolchain.toml, cargo-deny, reuse
buildbox run --no-git ~/ai-gw "scripts/setup-tools.sh"

# alle Prüfungen: fmt, clippy -D warnings, Tests, cargo-deny, OpenAPI-Abgleich,
# UI (pnpm install, Typen, svelte-check, build), REUSE
buildbox run --no-git ~/ai-gw \
  "set -e; . ~/.cargo/env; export PATH=\$HOME/.local/bin:\$PATH; AIGW_STRICT=1 scripts/check.sh" \
  --fetch Cargo.lock --fetch ui/pnpm-lock.yaml

# Release-Binary mit eingebetteter UI bauen, starten, /healthz, Admin-API und /admin/ prüfen
buildbox run --no-git ~/ai-gw "set -e; . ~/.cargo/env; scripts/runtime-smoke.sh"

# Admin-API geändert → OpenAPI-Spezifikation und TS-Typen neu erzeugen und einchecken
buildbox run --no-git ~/ai-gw "set -e; . ~/.cargo/env; scripts/gen-api.sh" \
  --fetch ui/openapi.json --fetch ui/src/lib/api/schema.d.ts

# Container-Image (amd64) auf der Box bauen (dort mit BuildKit; der lokale Docker-Client
# über `--context buildbox` nutzt den Legacy-Builder ohne `RUN --mount`)
buildbox run --no-git ~/ai-gw "docker build -t ai-gw:dev ."
```

`target/` steht in `.buildboxignore` und bleibt als Cache auf der Box; `ui/dist` und
`ui/node_modules` ebenso. `--no-git` ist nötig, wenn aus einem Git-Worktree gearbeitet wird.

Hinweise:

- Ohne gebaute UI (`ui/dist`) baut `cargo build` trotzdem; dann wird eine Platzhalterseite
  eingebettet (`crates/aigw-admin/ui-fallback`). Pfad überschreibbar mit `AIGW_UI_DIST`.
- Logging: `AIGW_LOG` (Syntax wie `RUST_LOG`, Standard `info`), `--log-format json`.
- UI-Entwicklung: `pnpm -C ui dev` leitet `/admin/api` an ein laufendes `aigw` weiter
  (`AIGW_DEV_API`, Standard `http://127.0.0.1:4001`, Steuerebene).
- npm-Lieferkette: `ignoreScripts`, `minimumReleaseAge` 7 Tage, `--frozen-lockfile`
  (`ui/pnpm-workspace.yaml`).

### CI

`.github/workflows/ci.yml` führt `scripts/check.sh` aus. Weil das Repo privat ist und
Actions-Minuten kostet, läuft der Workflow **nur bei Pull Requests und manuell**
(`workflow_dispatch`), nicht bei jedem Push.

## Lizenz

Code: AGPL-3.0-or-later, siehe [LICENSE](LICENSE). Dokumentation (`docs/`, README): CC-BY-4.0, siehe [LICENSES/CC-BY-4.0.txt](LICENSES/CC-BY-4.0.txt).
