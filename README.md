# ai-gw

Eigenes AI-Gateway nach Vorbild von LiteLLM, Kong AI Gateway und Azure AI Gateway: ein
Binary für Gateway, Admin-API und Admin-UI.

- **Ein Endpunkt für viele Anbieter:** OpenAI- und Anthropic-kompatible API vor OpenAI, Azure
  OpenAI, Anthropic, AWS Bedrock, Gemini/Vertex AI, Mistral, Ollama, vLLM u. a.
- **Routing:** Modellgruppen, Lastverteilung, Retries, Fallbacks, Circuit Breaker
- **Mandanten:** Organisationen, Teams, API-Keys, Modellfreigaben, Limits, Budgets, Kosten
- **Workload-/Agent-Identitäten:** Anmeldung mit Entra-Access-Tokens (Microsoft Entra Agent ID) statt Key
- **Guardrails und Cache:** PII, Prompt-Filter, Content Safety, exakter/semantischer Cache, MCP-Gateway
- **Betrieb:** Admin-UI, GitOps (YAML), Prometheus, OpenTelemetry, SQLite oder Postgres
- **Technik:** Rust (tokio, hyper/axum, rustls), Admin-UI Svelte 5 (eingebettet)

Entscheidungen: [docs/konzept/00-entscheidungen.md](docs/konzept/00-entscheidungen.md) ·
Betriebshandbuch: [docs/betrieb/](docs/betrieb/README.md) · Änderungen: [CHANGELOG.md](CHANGELOG.md)

## Screenshots

Admin-Oberfläche (Svelte 5), Aufnahmen der echten UI. Weitere Bilder unter [docs/konzept/ux](docs/konzept/ux).

| | |
|---|---|
| <a href="docs/konzept/ux/echt-uebersicht-hell-desktop.png"><img src="docs/konzept/ux/echt-uebersicht-hell-desktop.png" alt="Übersicht" width="420"></a><br>Übersicht | <a href="docs/konzept/ux/echt-modelle-dunkel-desktop.png"><img src="docs/konzept/ux/echt-modelle-dunkel-desktop.png" alt="Modelle (dunkel)" width="420"></a><br>Modelle (dunkel) |
| <a href="docs/konzept/ux/echt-deployment-detail-hell-desktop.png"><img src="docs/konzept/ux/echt-deployment-detail-hell-desktop.png" alt="Deployment-Detail" width="420"></a><br>Deployment-Detail | <a href="docs/konzept/ux/echt-routing-einstellungen-hell-desktop.png"><img src="docs/konzept/ux/echt-routing-einstellungen-hell-desktop.png" alt="Routing-Einstellungen" width="420"></a><br>Routing-Einstellungen |
| <a href="docs/konzept/ux/echt-keys-hell-desktop.png"><img src="docs/konzept/ux/echt-keys-hell-desktop.png" alt="API-Keys" width="420"></a><br>API-Keys | <a href="docs/konzept/ux/echt-teams-hell-desktop.png"><img src="docs/konzept/ux/echt-teams-hell-desktop.png" alt="Teams" width="420"></a><br>Teams |
| <a href="docs/konzept/ux/echt-plugin-kette-hell-desktop.png"><img src="docs/konzept/ux/echt-plugin-kette-hell-desktop.png" alt="Plugin-Kette" width="420"></a><br>Plugin-Kette | <a href="docs/konzept/ux/echt-guardrails-pii-hell-desktop.png"><img src="docs/konzept/ux/echt-guardrails-pii-hell-desktop.png" alt="Guardrails / PII" width="420"></a><br>Guardrails / PII |
| <a href="docs/konzept/ux/echt-guardrail-profil-diff-hell-desktop.png"><img src="docs/konzept/ux/echt-guardrail-profil-diff-hell-desktop.png" alt="Guardrail-Profil (Diff)" width="420"></a><br>Guardrail-Profil (Diff) | <a href="docs/konzept/ux/echt-mcp-freigaben-hell-desktop.png"><img src="docs/konzept/ux/echt-mcp-freigaben-hell-desktop.png" alt="MCP-Freigaben" width="420"></a><br>MCP-Freigaben |
| <a href="docs/konzept/ux/echt-playground-hell-desktop.png"><img src="docs/konzept/ux/echt-playground-hell-desktop.png" alt="Playground" width="420"></a><br>Playground | <a href="docs/konzept/ux/echt-preise-hell-desktop.png"><img src="docs/konzept/ux/echt-preise-hell-desktop.png" alt="Preise" width="420"></a><br>Preise |
| <a href="docs/konzept/ux/echt-audit-hell-desktop.png"><img src="docs/konzept/ux/echt-audit-hell-desktop.png" alt="Audit-Log" width="420"></a><br>Audit-Log | <a href="docs/konzept/ux/echt-gitops-trockenlauf-hell-desktop.png"><img src="docs/konzept/ux/echt-gitops-trockenlauf-hell-desktop.png" alt="GitOps-Trockenlauf" width="420"></a><br>GitOps-Trockenlauf |
| <a href="docs/konzept/ux/echt-yaml-import-hell-desktop.png"><img src="docs/konzept/ux/echt-yaml-import-hell-desktop.png" alt="YAML-Import" width="420"></a><br>YAML-Import | <a href="docs/konzept/ux/befehlspalette-hell-desktop.png"><img src="docs/konzept/ux/befehlspalette-hell-desktop.png" alt="Befehlspalette" width="420"></a><br>Befehlspalette |

## Schnellstart mit Docker

```bash
docker build -t ai-gw:dev .                                   # Image bauen (BuildKit)
docker run --rm ai-gw:dev keygen > aigw-master-key            # Master-Key erzeugen
docker run -d --name aigw -p 127.0.0.1:4000:4000 -p 127.0.0.1:4001:4001 \
  -e AIGW_MASTER_KEY="$(cat aigw-master-key)" -v aigw-data:/data ai-gw:dev
docker logs aigw 2>&1 | grep Bootstrap-Token                  # einmaliges Admin-Token
```

Dann die Admin-UI unter <http://localhost:4001/admin/> mit dem Bootstrap-Token öffnen und
Organisation, Zugangsdaten, Modellgruppe und API-Key anlegen – oder Ressourcen als YAML
anwenden (Geheimnisse nur als Verweis, hier `AIGW_SECRET_OPENAI_API_KEY` in der Umgebung des
Containers, also beim `docker run` zusätzlich `-e AIGW_SECRET_OPENAI_API_KEY=…`):

```bash
curl -s localhost:4001/admin/api/v1/config:apply -H "authorization: Bearer <bootstrap-token>" \
  -H 'content-type: application/yaml' --data-binary @config/examples/minimal/resources.yaml
curl -s localhost:4000/v1/chat/completions -H "authorization: Bearer sk-aigw-…" \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"Hallo"}]}'
```

Mit Postgres, Redis und OpenTelemetry-Collector: [deploy/compose/](deploy/compose/docker-compose.yaml),
Kubernetes: [deploy/kubernetes/](deploy/kubernetes/), Details:
[docs/betrieb/installation.md](docs/betrieb/installation.md). Beispielkonfigurationen:
[config/examples/](config/examples/).

## Aufbau

| Pfad | Inhalt |
|---|---|
| `crates/aigw` | Binary: `aigw serve`, `config export\|diff\|apply\|validate\|check\|rekey`, `keygen`, `audit verify`, `healthcheck`, `openapi` |
| `crates/aigw-core` | gemeinsame Typen, IR, Formate (OpenAI/Anthropic), Konfigurations- und Ressourcenmodell, Validierung |
| `crates/aigw-proxy` | Hot Path: Endpunkte der Datenebene, Pipeline, Routing, Limits |
| `crates/aigw-providers` | Anbieter-Adapter (Trait `Provider`) |
| `crates/aigw-outbound` | ausgehender HTTP-Client mit SSRF-Schutz |
| `crates/aigw-jwt` | JWT-/JWS-Prüfung: Workload-/Agent-Tokens (Entra Agent ID), JWKS, OIDC-Bausteine |
| `crates/aigw-store` | Persistenz (SQLite, Postgres), Snapshot, GitOps, Verschlüsselung |
| `crates/aigw-counters` | Zähler-Backends `local`, `postgres-sync`, `redis` |
| `crates/aigw-billing` | Kosten und Budgets |
| `crates/aigw-plugins`, `aigw-pii` | Plugin-Kette, eingebaute Plugins, PII-Erkennung |
| `crates/aigw-wasm`, `plugins/` | WASM-Plugins (wasmtime, WIT-Welt), Beispiel- und Test-Plugins |
| `crates/aigw-cache` | Antwort-Cache (exakt, semantisch) |
| `crates/aigw-mcp` | MCP-Gateway |
| `crates/aigw-observe` | Request-Log, Prometheus, OpenTelemetry |
| `crates/aigw-tokenizer` | Token-Zählung (tiktoken-kompatibel, Hugging Face) |
| `crates/aigw-webhook` | Ereignis-Webhooks |
| `crates/aigw-admin` | Admin-API (axum + utoipa/OpenAPI), Anmeldung, eingebettete UI |
| `ui/` | Admin-UI (Svelte 5, Vite, TypeScript, Bits UI, Tailwind 4, pnpm) |
| `config/` | Startkonfigurationen (`aigw.example.yaml` mit allen Feldern) und `examples/` |
| `deploy/` | Docker Compose, Kubernetes |
| `docs/` | `konzept/` (Architektur), `betrieb/` (Betriebshandbuch) |
| `scripts/` | `check.sh` (alle Prüfungen), `runtime-smoke.sh`, `container-smoke.sh`, `build-images.sh`, `k8s-check.sh`, `gen-api.sh`, `third-party.sh`, Benchmarks |

Endpunkte – Datenebene (`server.data.listen`, Standard `127.0.0.1:4000`, im Container `0.0.0.0`):

- `POST /v1/chat/completions`, `POST /v1/messages`, `GET /v1/models` – OpenAI- und
  Anthropic-kompatibel (siehe `docs/konzept/03-ir-und-formate.md`)
- `POST /v1/responses`, `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/transcriptions`,
  `/v1/audio/translations`, `/v1/audio/speech` – OpenAI-kompatibel (Responses zustandslos;
  Modellgruppe braucht `settings.capabilities`, siehe `docs/konzept/03-ir-und-formate.md` §6)
- `/mcp`, `/mcp/{server}` – MCP-Gateway (wenn `mcp.enabled`)
- `GET /healthz` – Liveness
- `GET /readyz` – Readiness (`ready` bzw. `degraded` bei veraltetem Konfigurations-Snapshot)

Steuerebene (`server.control.listen`, Standard `127.0.0.1:4001`; gleiche Adresse → ein Port):

- `GET /healthz`, `GET /readyz` – Liveness, Readiness
- `GET /metrics` – Prometheus
- `GET /admin/api/v1/version` – Version (mit Zustand des Konfigurations-Snapshots)
- `GET /admin/api/openapi.json` – OpenAPI-Spezifikation der Admin-API
- `GET /admin/` – Admin-UI (SPA)

## Entwicklung

Voraussetzungen: Rust-Toolchain aus `rust-toolchain.toml` (über rustup), Node.js mit pnpm,
Docker für die Postgres-/Redis-Tests und Container-Builds. Rechenintensive Schritte lassen sich
auch auf einer entfernten Build-Maschine ausführen; generierte Dateien (`Cargo.lock`,
`ui/pnpm-lock.yaml`, `ui/openapi.json`, `ui/src/lib/api/schema.d.ts`) danach einchecken.

```bash
# einmalig: Toolchain aus rust-toolchain.toml, cargo-deny, reuse
scripts/setup-tools.sh

# alle Prüfungen: fmt, clippy -D warnings, Tests, cargo-deny, OpenAPI-Abgleich,
# UI (pnpm install, Typen, svelte-check, build), REUSE
AIGW_STRICT=1 scripts/check.sh
# zusätzlich gegen echte Postgres-/Redis-Container testen
AIGW_STRICT=1 AIGW_TEST_POSTGRES=docker AIGW_TEST_REDIS=docker scripts/check.sh

# Release-Binary mit eingebetteter UI bauen, starten, /healthz, Admin-API und /admin/ prüfen
scripts/runtime-smoke.sh

# UI-Ende-zu-Ende-Tests (Playwright)
cd ui/e2e && bash run.sh

# Admin-API geändert → OpenAPI-Spezifikation und TS-Typen neu erzeugen und einchecken
scripts/gen-api.sh

# Container-Image bauen und testen: Start mit SQLite (schreibgeschützt), Healthcheck,
# Compose mit Postgres + Redis + OTel
scripts/container-smoke.sh

# Lizenzübersicht THIRD-PARTY.md neu erzeugen (nach Änderungen an Abhängigkeiten)
scripts/third-party.sh
```

Hinweise:

- Ohne gebaute UI (`ui/dist`) baut `cargo build` trotzdem; dann wird eine Platzhalterseite
  eingebettet (`crates/aigw-admin/ui-fallback`). Pfad überschreibbar mit `AIGW_UI_DIST`.
- Logging: `AIGW_LOG` (Syntax wie `RUST_LOG`, Standard `info`), `--log-format json`.
- UI-Entwicklung: `pnpm -C ui dev` leitet `/admin/api` an ein laufendes `aigw` weiter
  (`AIGW_DEV_API`, Standard `http://127.0.0.1:4001`, Steuerebene).
- npm-Lieferkette: `ignoreScripts`, `minimumReleaseAge` 7 Tage, `--frozen-lockfile`
  (`ui/pnpm-workspace.yaml`).

### CI

`.github/workflows/ci.yml` führt `scripts/check.sh` und `scripts/third-party.sh --check`
(THIRD-PARTY.md aktuell) aus, prüft die Kubernetes-Manifeste (`scripts/k8s-check.sh`) und baut
bei Release-Tags oder manuell die Container-Images für amd64 und arm64
(`scripts/build-images.sh`, arm64 quer kompiliert, Smoke unter QEMU). Der Workflow läuft bei Pushes
auf `main`, bei Pull Requests, bei Tags `v*` und manuell (`workflow_dispatch`).

## Lizenz

Code: AGPL-3.0-or-later, siehe [LICENSE](LICENSE). Dokumentation (`docs/`, README, CHANGELOG):
CC-BY-4.0, siehe [LICENSES/CC-BY-4.0.txt](LICENSES/CC-BY-4.0.txt). Das Repository ist
[REUSE](https://reuse.software/)-konform (`REUSE.toml`).

Drittanbieter: [THIRD-PARTY.md](THIRD-PARTY.md) (erzeugt mit `scripts/third-party.sh`); erlaubte
Lizenzen der Abhängigkeiten prüft `cargo deny` (`deny.toml`). Die eingebetteten BPE-Rangtabellen
des Tokenizers stammen aus OpenAIs tiktoken (MIT) – vor einem öffentlichen Release rechtlich
bestätigen lassen.
