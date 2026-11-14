# Changelog

Alle nennenswerten Änderungen an ai-gw. Format nach [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
Versionierung nach [Semantic Versioning](https://semver.org/lang/de/). Bis 1.0 können sich
Konfiguration, Manifest-Format (`aigw/v1alpha1`) und Admin-API zwischen Minor-Versionen
inkompatibel ändern; solche Änderungen stehen hier unter „Geändert“ bzw. „Entfernt“.

## [Unreleased]

### Hinzugefügt

- **WASM-Plugins (AIGW-63):** Plugin-Art `wasm` führt hochgeladene WebAssembly-Komponenten
  (Component Model, WIT-Welt `aigw:plugin/plugin` in `crates/aigw-wasm/wit/plugin.wit`) in der
  Plugin-Kette aus – Anfrage, Antwort und Stream, Ergebnis weiter/ändern/blockieren/ablehnen,
  JSON-IR und Meta. Neues Crate `aigw-wasm` (wasmtime 50: Cranelift, Pooling-Allocator, Cache
  kompilierter Module nach SHA-256, Instanz je Aufruf). Sandbox: WASI 0.2 ohne Dateisystem,
  Netz und Umgebung; Fuel (CPU), Epoch-Interrupt (Zeit) und Speichergrenze je Aufruf, Fehler →
  `failMode` (Standard `closed`). Module in der Tabelle `wasm_module` (Migration 0009, SQLite +
  Postgres), Teil des Snapshots (inkrementell, Platten-Cache, Hot Reload); Admin-API
  `/admin/api/v1/wasm-modules` (Hochladen mit Pflicht-SHA-256 und `If-Match`, Liste,
  Download, Löschen mit Schutz bei verwendeten Modulen; Schreiben nur globaler `admin`),
  Startkonfiguration `wasm.*` inkl. `wasm.modules` (Dateien), GitOps-Verweis per Name +
  optional SHA-256. Metriken `aigw_wasm_*`. Admin-UI: Reiter „WASM-Module“ unter Guardrails &
  Plugins, Plugin-Art `wasm` im Bindungsformular. Beispiel-Plugin `plugins/keyword-guard`
  (Stichwort-Blocker, Header-Setzer), Test-Plugin `plugins/testkit`,
  `scripts/build-wasm-plugins.sh`. Cargo-Feature `wasm` (Standard an). Konzept:
  `docs/konzept/17-wasm-plugins.md`.
- **Plugin-Fabriken mit Snapshot (AIGW-63):** `PluginFactory::build_in` (Snapshot + wirksames
  Zeitlimit) und `PluginFactory::fingerprint` (Neubau einer Bindung, wenn sich ein
  referenziertes Snapshot-Objekt ändert).
- **tiktoken-Rangtabellen optional (AIGW-95):** Cargo-Feature `embedded-tokenizers`
  (Crates `aigw`, `aigw-tokenizer`; Standard an – Standard-Builds und Images unverändert).
  Ohne das Feature enthält das Binary die Tabellen `o200k_base`/`cl100k_base` nicht; neuer
  Konfig-Abschnitt `tokenizer` mit `ranks_dir` (Originaldateien `*.tiktoken`, SHA-256-Prüfung
  gegen eingebaute Summen) und `download_ranks` (einmaliger Download von
  `openaipublic.blob.core.windows.net` über den gemeinsamen Client mit SSRF-Regeln, Standard
  aus). Fehlen die Tabellen, gilt die Heuristik mit Startwarnung. `scripts/check.sh` baut und
  testet auch die Variante ohne Feature. Lizenzfrage der Tabellen weiter offen
  (docs/konzept/99-offene-fragen.md, THIRD-PARTY.md).

- **RUSTSEC-2026-0459 erneut geprüft (AIGW-64):** weiterhin nicht ohne Ausnahme lösbar –
  sqlx 0.9.0 (neuestes Release) pinnt libsqlite3-sys <0.38, gepatcht ist erst 0.39; Ausnahme
  in `deny.toml` bleibt (Stand dort vermerkt).

- **Entra ID: verschlüsselte Zertifikatsschlüssel, `x5c`, Azure Arc (AIGW-94):**
  `clientCertificate` akzeptiert `ENCRYPTED PRIVATE KEY` (PKCS#8, PBES2/PBKDF2/AES-CBC) mit
  dem Passwort im neuen Zugangsdaten-Feld `clientCertificatePassword` (Wert oder
  `valueFrom.file`/`env`); Fehlermeldungen ohne Geheimnisse. `spec.entra.sendX5c` bzw.
  `settings.entraSendX5c` schickt die Zertifikatskette als `x5c` mit (Subject Name + Issuer).
  Managed Identity unter **Azure Arc** (`IDENTITY_ENDPOINT` + `IMDS_ENDPOINT`, Challenge mit
  Schlüsseldatei nur aus dem Arc-Verzeichnis, S-35); unterliegt
  `upstream.entra.ambient_identity`. Admin-UI: Passwortfeld und Schalter „x5c“ im
  Zugangsdaten-Formular.

- **content-safety mit Entra ID (AIGW-89):** `provider: azure` mit `auth: entraId` und
  `credential` = ProviderCredential mit `spec.entra` (Managed Identity, Workload Identity,
  Client Credentials mit Secret/Zertifikat) – `Authorization: Bearer` statt
  `Ocp-Apim-Subscription-Key`, gleiche Token-Quelle (Cache, Erneuerung) wie Deployments mit
  `auth: entraId`; Endpunkt nur auf Azure-Hosts. `PluginHost::entra_token`. Admin-UI: Zugang
  „Entra ID“ im Plugin-Formular und beim Anwenden des Guardrail-Profils „Streng“
  (`contentSafety.auth`).
- **content-safety prüft Tool-Call-Argumente im Stream (AIGW-89, S-10):** Satzmodus mit
  eigenem Puffer je Tool-Block, Prüfung am Blockende.
- **Kosten und Obergrenze des Prüfmodells (AIGW-89, S-13):** Aufrufe von `content-safety`
  mit `provider: model` zählen für Kosten, Budgets und Token-Limits des Keys
  (`RequestRecord.plugin_usage`/`plugin_cost`, Anteil in `cost`); neue Einstellung
  `maxChecks` (Standard 100 Prüfungen je Anfrage).

- **Preisvorschläge (AIGW-85):** `GET /admin/api/v1/price-suggestions` vergleicht je Deployment
  den aktuellen Preis mit einem Vorschlag aus der Azure Retail Prices API (Azure OpenAI/Foundry,
  je Region und Bereitstellungsart, USD, Zwischenspeicher, fehlertolerant) bzw. dem
  mitgelieferten Preiskatalog (`crates/aigw-billing/catalog/prices.yaml`: OpenAI, Anthropic,
  Mistral, Gemini; optional per URL aktualisierbar, auch im LiteLLM-Format);
  `POST …/price-suggestions:accept` legt neue Preisversionen mit Labels und Audit an, manuelle
  Preise nur mit `overrideManual`. Admin-UI: Abschnitt „Preisvorschläge“ auf der Preise-Seite
  mit Auswahl, Übernahme und Hinweis auf Deployments ohne Preis. Konfiguration
  `billing.price_suggestions`, Deployment-Einstellung `settings.pricing`
  (`region`, `deploymentType`, `model`, `meter`, `source`), optionaler periodischer Abgleich
  mit Warnung (`check_interval_secs`, Standard aus); Pflegehilfe `scripts/price-catalog-check.py`.

- **Tool-Inventar und Tool-Richtlinien (AIGW-87):** Inventar der Tools, die Anwendungen in
  Anfragen mitschicken (OpenAI Chat/Responses, Anthropic Messages): Name, Art, Herkunft
  (`mcp__<server>__<tool>`, `<server>__<tool>`), Client, Organisation/Team/Key, Nutzung und
  Aufrufe durch das Modell, BLAKE3-Hashes von Beschreibung und Schema (Inhalte nur bei
  `payloadMode` `masked`/`full`), gepuffert geschrieben (`tools:`-Konfiguration, Migration
  0008). Änderungen bekannter Tools → Audit, Ereignis `tool.changed`, optional automatische
  Sperre (`tools.on_change: block`); Prompt-Injection-Prüfung der Beschreibungen
  (`tool.suspicious`). Neue Ressource `ToolPolicy` (GitOps, Audit, Snapshot): `allow`,
  `block-strip`, `block-reject`, `observe`, `require-approval` je Ebene, `locked`; Aufrufe
  gesperrter Tools in Antworten werden wie Ausgabe-Ablehnungen gefiltert (JSON und Stream).
  Admin-API `/tool-policies`, `/tools` (Freigeben/Sperren/Bestätigen), UI-Seite „Tools“,
  Metriken `aigw_tool_policy_actions_total`, `aigw_tool_inventory_events_total`. Doku
  `docs/konzept/16-tools.md`.
- **Claude in Azure AI Foundry:** Anthropic-Anbieter mit `settings.auth: entraId`
  (`Authorization: Bearer` statt `x-api-key`, alle Entra-ID-Arten inkl. Managed Identity,
  gemeinsame Token-Quelle mit Azure OpenAI), Zugangsdaten-Alternativen
  `tenantId`+`clientId`+`clientSecret`/`clientCertificate` für `anthropic`; Beispiel
  `config/examples/providers/resources-azure-foundry.yaml` (GPT + Claude, benutzerzugewiesene
  Managed Identity in Azure Container Apps).
- **Standard-Guardrail-Profile (AIGW-86):** „Basis“, „Streng“, „Offen“ versioniert im Binary
  (`crates/aigw-admin/profiles/*.yaml`); `GET /admin/api/v1/guardrail-profiles`,
  `POST /admin/api/v1/guardrail-profiles/{name}:apply` mit Ziel-Scope, Dry-Run-Diff, idempotent,
  Profilwechsel ersetzt Bindungen des alten Profils, Audit je Objekt, Rechte wie
  Plugin-Bindungen; Payload-Logging-Obergrenze an Organisation/Team. UI: Reiter „Profile“ auf
  `/guardrails`, Hinweis ohne aktive Guardrails in Übersicht und Guardrails-Seite.
- Antwort-Header `x-aigw-cache-reason` (`temperature` | `client`) bei `x-aigw-cache: bypass`.
- **Entra ID an den Zugangsdaten (AIGW-84):** `ProviderCredential.spec.entra`
  (`managedIdentity`, `workloadIdentity`, `clientSecret`, `clientCertificate` mit `tenantId`,
  `clientId`, `authority`, `scope`; nur Secret/Zertifikat verschlüsselt in `data`) für
  `azure-openai` und `anthropic`. Deployments mit `credential` übernehmen die Anmeldung,
  eigene Einstellungen haben Vorrang, Inline-Einstellungen bleiben gültig; gemeinsame
  Validierung Admin-API/GitOps, Migration `0007_credential_auth` (beide Dialekte).
  Beispiel `resources-azure-foundry.yaml` nutzt jetzt diese Variante.
- **Anbieter-Übersicht (AIGW-84):** `GET /admin/api/v1/providers` und Admin-UI „Anbieter“:
  je Anbieter + Endpunkt Anmeldeart (API-Key, Entra ID mit Art, …), Deployments,
  Modellgruppen und Zustand dieser Instanz (Circuit Breaker, Health Checks, nicht nutzbare
  Deployments mit Grund) – auch ohne Zugangsdaten. Formular „Zugangsdaten“ mit Anmeldeart
  „Entra ID“.
- Doku und Tests: Managed Identity in Azure Container Apps (`IDENTITY_ENDPOINT`/
  `IDENTITY_HEADER`, benutzerzugewiesen), Postgres mit TLS (`sslmode=require`/`verify-full`,
  eingebettete Wurzeln für Azure Database for PostgreSQL).

- **Container-Image für linux/arm64 (AIGW-91):** Das Dockerfile kompiliert das Binary auf der
  Build-Plattform quer (Cross-GCC aus Debian, `rustup target add`), arm64 braucht daher kein
  QEMU für den Compiler; aws-lc-sys, mimalloc und SQLite bauen ohne Anpassung mit.
  `scripts/build-images.sh` baut amd64 + arm64 (`--load`, optional Tarballs `SAVE_DIR`,
  Smoke `SMOKE=1` mit QEMU/binfmt, Multi-Arch-Manifest `PUSH=…`). arm64-Image unter QEMU mit
  dem vollständigen `container-smoke.sh` und nativ auf einem arm64-Host geprüft.
- `scripts/k8s-check.sh`: `kubectl kustomize` + kubeconform (strikt, Kubernetes 1.34) für
  `deploy/kubernetes`, optional `kubectl apply --dry-run=server`.
- CI: Lizenzübersicht `scripts/third-party.sh --check` und Manifest-Prüfung in jedem Lauf;
  Job „Container-Images (amd64 + arm64)“ bei Tags `v*` und manuell (Tarballs als Artefakt).

### Geändert

- `PluginHost::chat` bekommt die Sammelstelle `&PluginCalls` (aus `CallInfo::plugin_calls`)
  für die Kostenzurechnung; `CallInfo` hat das neue Feld `plugin_calls` (AIGW-89).

- OpenAI-Antworten ohne Text, Ablehnung und Tool-Aufrufe (z. B. Claude hat `max_tokens`
  komplett für Thinking verbraucht, `finish_reason: length`) haben `content: ""` statt `null`
  – wie OpenAI selbst (AIGW-86).
- **Entra Workload/Managed Identity nur mit Freigabe (AIGW-90, inkompatibel):** Zugangsdaten
  bzw. Deployments mit `managedIdentity`/`workloadIdentity` nutzen die Identität des
  Gateway-Prozesses nur noch mit `upstream.entra.ambient_identity: true` (ENV
  `AIGW_UPSTREAM__ENTRA__AMBIENT_IDENTITY=true`, Standard `false`). Ohne Freigabe sind sie
  „nicht nutzbar“ (Meldung nennt den Schlüssel), beim Start warnt das Log mit den betroffenen
  Objekten. **Bestehende Installationen mit Managed Identity müssen den Schalter setzen.**
- **Rolle `gitops` nicht mehr admin-gleichwertig (S-05, AIGW-90, inkompatibel):**
  `config:apply` mit einem gitops-Key lehnt das Anlegen, Ändern und Löschen von `User` mit
  Rollen oder OIDC-Verknüpfung mit `422` ab (auch `dryRun`, je Nutzer ein Eintrag in `issues`),
  außer mit `gitops.manage_users: true`. Globaler `admin` und CLI unverändert.
- **MCP-Server prüfen ihren Geheimnis-Verweis (AIGW-90):** Admin-API (Anlegen/Ändern) und
  GitOps verlangen, dass `spec.secret` bzw. `spec.credential` existiert und das Token-Feld
  (`settings.credentialField`, Standard `apiKey`, sonst `value`) enthält – sonst `422` mit
  `issues`.

### Behoben

- **Redis-Ausfalltests stabil unter Last (AIGW-94):** `falls_back_and_recovers` und
  `deferred_settles_replayed_after_outage` hingen an Wanduhrzeit: feste `CLIENT PAUSE`-Dauer
  (600/700 ms), 100 ms Zeitlimit und ein 300-ms-Zeitraum. Unter Last (Load > 30, nachgestellt)
  lief das Nachtragen in die Zeitüberschreitung und wurde erneut gebucht (`k2` ≠ 2) bzw. der
  kurze Zeitraum endete schon vor dem Abgleich (`pending` < 2). Jetzt hält ein TCP-Proxy je Test
  den Server an, bis der Test ihn freigibt; Zeitlimit 2 s, Zeitraumende relativ zum Ausfall.
  Aussagen unverändert (alt 2 Fehler in 85 Läufen unter Last, neu 0 in 115).

## [0.1.0] – MVP – Datum offen

Erste Version: AI-Gateway, Admin-API und Admin-UI in einem Binary.

### Hinzugefügt

- **Datenebene** (`:4000`): OpenAI-kompatibel (`/v1/chat/completions`, `/v1/responses`,
  `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/*`, `/v1/models`) und Anthropic
  Messages (`/v1/messages`); Übersetzung immer über eine gemeinsame Zwischendarstellung (IR),
  Streaming mit Backpressure, Fehler im Eingangsformat.
- **Anbieter:** OpenAI, Azure OpenAI (API-Key oder Entra ID: Client-Secret,
  Client-Zertifikat, Workload Identity, Managed Identity; Token-Cache je Credential mit
  Erneuerung vor Ablauf, Sovereign Clouds über `entraAuthority`/`entraScope`), Anthropic, AWS Bedrock (SigV4
  oder API-Key), Google Gemini API und Vertex AI (Service-Account), Mistral, Ollama, vLLM,
  generisch OpenAI-kompatibel, eingebauter Mock-Anbieter.
- **Routing:** Modellgruppen mit Aliasen, Strategien (gewichtet, geringste Latenz, geringste
  Last, Kosten), Prioritäten, Retries, Fallbacks zwischen Gruppen, Circuit Breaker, Health
  Checks, Deployment-Kapazität (RPM/TPM), Hot Reload.
- **Mandanten und Zugriff:** Organisationen, Teams, Nutzer, API-Keys (aktiv/gesperrt/
  widerrufen, Ablauf, Rotation), Modellfreigaben, Payload-Richtlinien je Ebene.
- **Limits und Budgets:** Anfrage-/Token-Limits in gleitenden Fenstern und Kalenderzeiträumen
  je Key/Team/Organisation/Modellgruppe; Zähler-Backends `local`, `postgres-sync`, `redis`;
  Kostenrechnung mit Preisen je Modell und Zeitraum, Budgets (strikt mit Reservierung oder
  weich), Warnungen per Webhook; Tokenizer (tiktoken-kompatibel, Hugging Face).
- **Plugins und Guardrails:** Plugin-Kette mit Vererbung/Sperre je Ebene; eingebaut
  `header`, `request-transform`, `pii` (Maskieren, Pseudonymisieren, Ablehnen),
  `prompt-filter` (Wortlisten, Regex, Prompt-Injection), `content-safety` (Azure, OpenAI,
  eigenes Modell), `webhook`, `cache`.
- **Antwort-Cache:** exakt (RAM, optional Redis) und semantisch (eigener HNSW-Index, pgvector
  oder Redis Search).
- **MCP-Gateway** (`/mcp`): MCP-Server hinter dem Gateway, Werkzeugfreigaben je Team/Key.
- **Admin-API** (`:4001`, OpenAPI): CRUD aller Ressourcen mit ETag/If-Match, Cursor-Pagination,
  RFC-9457-Fehler; OIDC-Anmeldung (generisch, Microsoft Entra ID), Sessions, CSRF-Schutz, RBAC
  (`admin`, `org-admin`, `team-admin`, `viewer`, `auditor`, `gitops`), Admin-API-Keys,
  Bootstrap-Token, Einladungen; Audit-Log mit Hash-Kette.
- **Admin-UI** (Svelte 5, eingebettet): Ressourcen-Editoren, Analytics, Log-Explorer,
  Playground, YAML-Import/-Export.
- **GitOps:** `aigw config export|diff|apply|validate`, Modus „nur YAML“.
- **Beobachtung:** Request-Log mit Rollups und Aufbewahrung, Prometheus-Metriken (`/metrics`),
  OpenTelemetry-Traces und -Logs (OTLP/HTTP), JSON-Lines-Export mit Rotation.
- **Datenhaltung:** SQLite (Standard) oder Postgres mit eigenem Migrationssatz je Dialekt,
  verschlüsselte Zugangsdaten (AES-256-GCM, Master-Key), verschlüsselter Snapshot-Cache für
  den Kaltstart ohne Datenbank, `/readyz` mit Zustand `degraded`.
- **Sicherheit:** SSRF-Schutz für alle ausgehenden Verbindungen (nach DNS-Auflösung),
  Body-Limits, rustls mit aws-lc-rs; Sicherheitsreview und Härtungshinweise.
- **Betrieb (AIGW-59):** Container-Image (distroless, nicht-root, schreibgeschütztes Root-FS,
  `HEALTHCHECK`, OCI-Labels, buildx-fähig für amd64/arm64), Docker-Compose-Beispiele
  (SQLite; Postgres + Redis + OpenTelemetry-Collector), Kubernetes-Grundgerüst,
  Betriebshandbuch `docs/betrieb/`, geprüfte Beispielkonfigurationen `config/examples/`,
  Befehle `aigw healthcheck`, `aigw config check` und `aigw config rekey`
  (Master-Key-Rotation), Lizenzübersicht `THIRD-PARTY.md`.
- **Leistung:** mimalloc als Standard-Allokator (abschaltbar mit `--no-default-features`),
  größere Accept-Warteschlange; Messungen in `docs/betrieb/leistung.md`.
- **Verbindungsschutz der Listener** (S-32): Höchstzahl gleichzeitiger Verbindungen je
  Listener (`server.*.max_connections`, darüber sofort `503`), Lese-Timeouts gegen Slowloris
  (Anfragekopf, Pause und Mindestrate des Anfragekörpers), Keep-Alive-Leerlauf, HTTP/2-Grenzen
  (`server.http`); laufende Streams bleiben unberührt. Metriken `aigw_server_*`.

### Bekannte Einschränkungen

- GitOps-Manifeste: höchstens 1 024 YAML-Dokumente je Datei.
- Container-Image nur für linux/amd64 gebaut und getestet; arm64 vorbereitet
  (behoben in Unreleased, AIGW-91).
- Lizenz der eingebetteten tiktoken-Rangtabellen (MIT) vor einem öffentlichen Release
  rechtlich bestätigen.

[Unreleased]: https://github.com/mupf-dev/ai-gw/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mupf-dev/ai-gw/releases/tag/v0.1.0
