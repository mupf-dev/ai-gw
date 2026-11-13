# ai-gw – Grundsatzentscheidungen

Stand: 2026-11-12 · Status: angenommen

Ziel: eigenes AI-Gateway nach Vorbild von LiteLLM, Kong AI Gateway und Azure AI Gateway (APIM),
mit Fokus auf Effizienz im Hot Path und einer modernen Web-Administration.

| # | Thema | Entscheidung |
|---|---|---|
| E1 | Einordnung | Eigenständiges Produkt (eigenes Repo, eigenes PM-Projekt). So gebaut, dass Plattformen wie cloud-project es einbinden können (API-first, ein Binary, deklarative Konfiguration). cloud-project entscheidet später selbst, ob es ai-gw verwendet oder neu baut. |
| E2 | Sprache Gateway | Rust (tokio, hyper/axum, tower, rustls). Gateway und Admin-API laufen in einem Binary. |
| E3 | Frontend | Svelte 5 + Vite + Bits UI + Tailwind als statische SPA, eingebettet ins Binary. Design-System angelehnt an das cloud-project-Portal (Variante C). TS-Typen werden aus der OpenAPI-Spezifikation der Admin-API generiert. |
| E4 | Datenhaltung | Konfiguration liegt im RAM (keine DB-Abfrage pro Request). SQLite ist Standard, Postgres optional für HA (Änderungen per LISTEN/NOTIFY an alle Instanzen). YAML-Export und -Import (GitOps). Zähler-Backend als Trait: `local`, `postgres-sync` (Abgleich alle 1–2 s, unscharf) und `redis` (exakt, verteilt). Logs gepuffert und gebündelt in die DB, zusätzlich Export per OTel oder Datei; ClickHouse ggf. später. |
| E5 | MVP-Umfang | Vollausbau, siehe unten. |
| E6 | Lizenz | AGPL-3.0-or-later (wie cloud-project). |
| E7 | Konfigurationsformat | YAML für Startkonfiguration und GitOps-Ressourcen (gepflegter reiner Rust-Parser, Auswahl in AIGW-9); alle Startwerte zusätzlich per Umgebungsvariable `AIGW_…`. Das TOML-Gerüst aus AIGW-8 wird umgestellt. |
| E8 | RUSTSEC-2026-0459 (libsqlite3-sys über sqlx 0.9) | Ausnahme in `deny.toml` akzeptiert (nur lokal mit fremden DB/WAL-Dateien ausnutzbar); Prüftermin 2027-01-31, Entfernen sobald sqlx es zulässt. |
| E9 | Lizenz der Dokumentation | CC-BY-4.0 für `docs/**` und README (wie cloud-project); Code bleibt AGPL-3.0-or-later. |

## MVP-Umfang (E5)

**Proxy und API**
- OpenAI-kompatible Endpunkte: Chat Completions, Responses, Embeddings, Bilder, Audio, Models. Vollständiges SSE-Streaming.
- Anbieter: OpenAI, Anthropic, Azure OpenAI, AWS Bedrock, Google Vertex/Gemini, Mistral, Ollama, vLLM und generische OpenAI-kompatible Endpunkte.
- Übersetzung der Formate (Tools/Function Calling, Vision, Streaming-Events, Usage).

**Routing und Resilienz**
- Lastverteilung (gewichtet, Least-Latency, Least-Busy), Prioritätsgruppen (z. B. PTU vor Pay-as-you-go).
- Fallback-Ketten, Retries mit Backoff, Circuit Breaker, Timeouts, Health Checks.

**Zugriff und Mandanten**
- Organisationen, Teams und Nutzer; virtuelle API-Keys mit Modellfreigaben, Ablauf und Metadaten.
- Admin-UI mit OIDC-Login und Rollen (Admin, Team-Admin, Viewer).

**Limits und Kosten**
- Token- und Request-Limits pro Key, Team und Modell (vgl. Azure `llm-token-limit`).
- Budgets mit Zeitraum, Warnschwellen und harten Grenzen; Preistabelle pro Modell; Kostenabrechnung.

**Erweiterung und Schutz**
- Plugin-Kette (Pre-Request, Post-Response, Stream-Chunk) pro Route, Key oder Team.
- Guardrails: PII-Erkennung und -Maskierung, Prompt-Filter, Regex- und Listen-Regeln, externe Content-Safety-Dienste.
- Cache: exakt (Hash) und semantisch (Embeddings und Vektorsuche).
- MCP-Gateway: MCP-Server registrieren und mit Key- und Team-Rechten freigeben, Tool-Aufrufe protokollieren.

**Beobachtung**
- Request-Log (optional mit Payload, konfigurierbar und maskiert), Prometheus-Metriken, OpenTelemetry-Traces.
- Dashboards im UI: Requests, Tokens, Kosten, Latenz (p50/p95/p99), Fehler und Cache-Trefferquote.

**Admin-UI**
- Dashboards, Verwaltung von Keys, Teams, Modellen, Routen, Policies, Plugins und MCP-Servern.
- Log-Explorer, Playground und YAML-Import/-Export.
