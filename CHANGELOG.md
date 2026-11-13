# Changelog

Alle nennenswerten Änderungen an ai-gw. Format nach [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
Versionierung nach [Semantic Versioning](https://semver.org/lang/de/). Bis 1.0 können sich
Konfiguration, Manifest-Format (`aigw/v1alpha1`) und Admin-API zwischen Minor-Versionen
inkompatibel ändern; solche Änderungen stehen hier unter „Geändert“ bzw. „Entfernt“.

## [Unreleased]

## [0.1.0] – MVP – Datum offen

Erste Version: AI-Gateway, Admin-API und Admin-UI in einem Binary.

### Hinzugefügt

- **Datenebene** (`:4000`): OpenAI-kompatibel (`/v1/chat/completions`, `/v1/responses`,
  `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/*`, `/v1/models`) und Anthropic
  Messages (`/v1/messages`); Übersetzung immer über eine gemeinsame Zwischendarstellung (IR),
  Streaming mit Backpressure, Fehler im Eingangsformat.
- **Anbieter:** OpenAI, Azure OpenAI (API-Key), Anthropic, AWS Bedrock (SigV4
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

### Bekannte Einschränkungen

- GitOps-Manifeste: höchstens 1 024 YAML-Dokumente je Datei.
- Azure OpenAI mit Entra ID (`settings.auth: entraId`) ist noch nicht umgesetzt.
- Container-Image nur für linux/amd64 gebaut und getestet; arm64 vorbereitet.
- Keine eigene Verbindungsgrenze/Header-Timeouts am Listener (Reverse Proxy davor; Arbeit in
  AIGW-80).
- Lizenz der eingebetteten tiktoken-Rangtabellen (MIT) vor einem öffentlichen Release
  rechtlich bestätigen.

[Unreleased]: https://github.com/mupf-dev/ai-gw/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mupf-dev/ai-gw/releases/tag/v0.1.0
