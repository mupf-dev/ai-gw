# Changelog

Alle nennenswerten Änderungen an ai-gw. Format nach [Keep a Changelog](https://keepachangelog.com/de/1.1.0/),
Versionierung nach [Semantic Versioning](https://semver.org/lang/de/). Bis 1.0 können sich
Konfiguration, Manifest-Format (`aigw/v1alpha1`) und Admin-API zwischen Minor-Versionen
inkompatibel ändern; solche Änderungen stehen hier unter „Geändert“ bzw. „Entfernt“.

## [Unreleased]

### Hinzugefügt

- **Budget je Deployment und Account-Rotation (AIGW-126):** `Deployment.spec.budget`
  (`amount`, `period`, `warnAtPercent`, `switchAtPercent`, `enforcement`; Migration 0015 für
  SQLite und Postgres). Erschöpfte Deployments überspringt das Routing ohne Fehler (`strict`
  reserviert die geschätzten Kosten je Versuch), abgerechnet wird nach der Antwort mit den
  tatsächlichen Kosten; sind alle erschöpft: `429 deployment_budget_exhausted`. Nachladen per
  höherem Betrag (Hot Reload). Neue Routing-Strategie **`budget-balanced`** (gleichmäßige
  Verteilung nach Budgetverbrauch in Prozent, „power of two choices“). Budget-Ebene
  `deployment` in `GET /usage/budgets`, `/costs/budgets` (Prognose), Webhook-Ereignissen und
  Metriken (`aigw_deployment_budget_used|amount|used_ratio`); UI: Budgetfelder und Verbrauch am
  Deployment, Strategie in der Modellgruppe, Deployment-Budgets auf der Seite „Kosten“. Rezept
  docs/betrieb/rezept-accounts-rotieren.md.

- **Lastvergleich LiteLLM mit Workern und Redis (AIGW-107):** `scripts/bench-litellm.sh` mit
  den Varianten `aigw-pg-redis`, `litellm-pg-w4`, `litellm-pg-redis-w4`; Ergebnisse in
  docs/betrieb/leistung.md §8.5 (u. a. ≈ 44 Redis-Befehle je Anfrage bei `counters.backend: redis`).
- **e2e:** `ui/e2e/run.sh` nimmt zusätzliche Playwright-Argumente über `E2E_ARGS` (z. B.
  `--repeat-each`); Tool-Inventar-Test (AIGW-87) unabhängig von Reihenfolge und Datenmenge
  (AIGW-121).
- **WASM-Module: Sigstore-Signatur ohne Schlüsselverwaltung und Vier-Augen-Freigabe
  (AIGW-106):** `wasm.sigstore` (Trust-Root `trusted_root.json` offline, erlaubte Identitäten
  mit Issuer/Subjekt als reguläre Ausdrücke) prüft Bundles aus `cosign sign-blob --bundle`
  (Fulcio-Kette, SCT, Rekor-SET und Inclusion-Proof mit Checkpoint) beim Upload, beim Import aus
  `wasm.modules` (`sigstore_bundle`), vor jedem Kompilieren und bei der Freigabe – ohne
  Netzaufrufe zur Laufzeit; eigenes schlankes Crate `aigw-sigstore` (aws-lc-rs,
  rustls-webpki), getestet mit der Sigstore-Konformitätssuite. Upload mit Bundle als
  JSON-Umschlag (`Content-Type: application/json`, `{content, sigstoreBundle}`), UI-Feld
  „Sigstore-Bundle“, CLI `aigw wasm verify-sigstore`. **Vier-Augen-Freigabe** `wasm.four_eyes`
  (Standard im neuen Profil `wasm.profile: strict`, das außerdem eine Signatur erzwingt):
  Uploads antworten mit `202` und warten auf Freigabe durch einen anderen globalen `admin`
  (`POST /admin/api/v1/wasm-modules/{name}/approve?sha256=…`, verwerfen mit `DELETE
  …/{name}/pending`, prüfen mit `GET …/{name}/pending/content`); Admin-Keys zählen als ihr
  Ersteller. UI: Bereich „Wartet auf Freigabe“ mit Buttons „Freigeben“/„Verwerfen“, Herkunft
  (hochgeladen/freigegeben von) je Modul. Audit-Art `wasm-module-approval` (Aktionen `submit`,
  `approve`, `reject`). Migration 0014 (`wasm_module_pending`; `wasm_module.sigstore_bundle`,
  `uploaded_by`, `approved_by`, `approved_at`). Neuer Signaturstatus `identity`, Metrikgründe
  `sigstore`/`identity`.

### Geändert

- **Erschöpftes Guthaben als `quota` (AIGW-125):** Anthropic „Your credit balance is too low“,
  „You have reached your specified (workspace) API usage limits“ (HTTP 400) und
  `enforced_spend_limit_reached` (HTTP 429) sowie OpenAI/Azure `billing_hard_limit_reached`,
  `billing_not_active` und „You exceeded your current quota“ gelten als `quota` statt
  `invalid_request`/`rate_limit`: kein Retry auf demselben Konto, Fallback auf das nächste
  Deployment, der Circuit Breaker öffnet sofort.

## [0.1.0] – JJJJ-MM-TT

Erste öffentliche Version: AI-Gateway, Admin-API und Admin-UI in einem Binary.

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
- **Kostenanalyse / FinOps (AIGW-117):** neue Seite „Kosten“ (Navigation, `/costs`,
  Befehlspalette): Kostenverlauf gestapelt nach Team, Organisation, Modellgruppe, Deployment,
  Anbieter oder API-Key (Top 5 + Übrige, Schrittweite wählbar, Tabellen-Alternative), Filter
  wie Analytics, Vergleich mit der Vorperiode (Delta absolut/%), Kosten je Anfrage und je 1000
  Ein-/Ausgabe-Tokens, Aufschlüsselung nach Eingabe/Cache lesen/Cache schreiben/Ausgabe/
  Plugins/Rest, Ersparnis durch Prompt-Caching des Anbieters (exakt) und den Antwort-Cache des
  Gateways (als Schätzung gekennzeichnet), Plugin-Kosten (content-safety, schema-guard)
  getrennt, Kostentreiber mit Drilldown (Filter, Log-Explorer), Budgets mit Prognose bis
  Periodenende (Durchschnitt der letzten N Tage bzw. linear), Warnung bei erwarteter
  Überschreitung und Burn-down, Chargeback-Export. Admin-API `GET /admin/api/v1/costs/{summary,
  breakdown,budgets,export}` mit denselben Sichtregeln wie Analytics; Export als CSV oder JSON
  gestreamt je Abschnitt (gesamt/Tag/Woche/Monat) mit Kostenstelle, Labels und Metadaten.
  Migration 0013 (Rollup-Spalten `cost_input`, `cost_cache_read`, `cost_cache_write`,
  `cost_output`, `cache_read_saved`, `plugin_cost`, `plugin_input_tokens`,
  `plugin_output_tokens`), `RequestRecord.cost_breakdown`. Neue Einstellungen
  `billing.currency` (Standard `USD`, nur Beschriftung) und `billing.cost_center_key`
  (Standard `costCenter`).
- **Workload-/Agent-Identitäten, Microsoft Entra Agent ID (AIGW-115):** Datenebene und
  MCP-Gateway nehmen neben API-Keys Access-Tokens (JWT) vertrauenswürdiger Aussteller an
  (`auth.jwt.issuers`, Profile `entra` und `generic`). Prüfung von Signatur (nur RS256/ES256),
  `iss`, `aud`, `exp`/`nbf`/`iat`, `tid`, optional App-Rollen/Scopes und Agent-Pflicht
  (`xms_act_fct`); JWKS über den Outbound-Baustein mit Cache und gedrosseltem Nachladen bei
  Schlüsselwechsel. Zuordnung über `spec.workloadIdentities` am API-Key (Admin-API, GitOps, UI;
  eindeutig über alle Keys, Rotation nimmt sie mit) – alle Key-Regeln gelten unverändert.
  Attribution (Agent, Art, Nutzer bei OBO, Blueprint) im Request-Log, Log-Explorer, Trace und
  OTLP-Logs; Token nie im Log und nie an Upstreams. Neues Crate `aigw-jwt` (gemeinsame
  JWS-Bausteine auch für den OIDC-Login). Migration 0012 (`api_key.workload_identities`,
  `request_log.workload`). Konzept `docs/konzept/18-workload-identitaeten.md`.
- **Unsichtbare Unicode-Zeichen im `prompt-filter` (AIGW-109):** `invisible: {action:
  strip|reject|flag, allowJoiners}` erkennt Tag-Zeichen (ASCII-Schmuggel), Zero-Width-, Bidi-,
  Formatzeichen und Folgen von Variantenselektoren auf den geprüften Rollen; legitime Fälle
  (Emoji-ZWJ, ZWNJ/ZWJ in persischer und indischen Schriften, Flaggen-Sequenzen, BOM am
  Anfang, einzelne Variantenselektoren) bleiben. Tag-Zeichen sind zusätzlich das
  Injection-Signal `unicode_tags` (auch im Tool-Inventar); Wörter, Regex und Signale prüfen den
  Text ohne unsichtbare Zeichen. Metrik-Kategorien `invisible_<art>`.
- **Wort-/Regex-Regeln auf der Ausgabe (AIGW-111):** `direction: request|response|both`
  (Standard `request`) im `prompt-filter` und je Regel; Antwort und Live-Stream (Puffer
  `streamWindowBytes` über Chunk-Grenzen), Ablehnung wie F160 (`content_filter`/`refusal`,
  `x-aigw-guardrail: prompt-filter`).
- **Plugin `prompt-leak` (AIGW-112):** Canary-Token im System-Prompt (`rotation: request`
  zufällig je Anfrage oder `stable` für Prompt-Caching), Erkennung in Antwort, Stream,
  Tool-Argumenten und Reasoning → blockieren (F160) oder zählen und entfernen; das Token
  erreicht nie Client oder Payload-Log, der Antwort-Cache rechnet ohne Token. Optional
  wörtliche Auszüge des System-Prompts über Wort-N-Gramme (`verbatim`). UI-Formular,
  Admin-/GitOps-Prüfung über die Registry, Beispiel in `config/examples/guardrails/`.
- **Guardrail-Profile v2:** „Basis“ entfernt unsichtbare Zeichen, „Streng“ lehnt sie ab und
  bindet `prompt-leak` (Canary, gesperrt, Order 90).
- **Guardrail `link-guard` (AIGW-110):** verhindert Datenabfluss über Links in Antworten
  (Prompt-Injection-Exfiltration). Erkennt Markdown-Bilder/-Links (auch verschachtelt),
  Referenz-Definitionen, HTML-Tags mit URL-Attributen (`img src/srcset`, `a href`, CSS `url()`,
  Meta-Refresh …) und nackte URLs; URL-Analyse wie im Browser (Entities, `\`, `user@host`,
  Prozent-Kodierung). Konfiguration `allowDomains` (mit `*.`-Wildcards), `images: block|allow`
  (Standard: fremde Bilder blockieren), `links`, `blockQueryParams`, `maxPathLength`, Aktion
  `strip|defang|reject|flag`, `direction` (Eingabe: Tool-Ergebnisse). Stream mit Puffer für
  über Chunk-Grenzen geteilte Links (`streamMaxHoldBytes`), Ablehnung wie F160. Im Profil
  „Streng“ (Version 3) als gesperrte Bindung.
- **Guardrail `language` (AIGW-114):** erlaubte/gesperrte Sprachen (`allow`/`deny`, ISO
  639-1) auf der Eingabe (Rollen wählbar), optional Ausgabe (Stream mit Stichprobe);
  `minChars`, `minConfidence`, Aktion `reject` (`400 language_not_allowed`) oder `flag`.
  Erkennung mit der Crate `whatlang` 0.16 (MIT, 69 Sprachen). Nicht in den Profilen.
- Beide Plugins: Registry, Prüfung in Admin-API/GitOps, Testlauf (`plugin-bindings:test`),
  strukturierte Formulare in der Admin-UI, Zähler/Metrik `aigw_guardrail_hits_total`.
- **Structured Output prüfen (AIGW-113):** neue Plugin-Art `schema-guard` prüft Antworten auf
  `response_format` `json_schema` (OpenAI) bzw. `output_config.format` (Anthropic) gegen das
  Schema, `json_object` auf JSON-Gültigkeit und Argumente erzwungener Tools gegen deren
  Parameterschema. Aktionen `flag` (Header `x-aigw-schema-guard: invalid`), `reject`
  (F160: `content_filter`/`refusal` bzw. `502 schema_validation_failed`) und `retry`
  (`maxRetries` 1–3; Wiederholung über das Gateway mit der endgültigen Anfrage nach allen
  Plugins, abgerechnet als Plugin-Modellaufruf; Ersatz-Antwort durchläuft innere
  Ausgabe-Plugins erneut). Stream: Prüfung am Ende (`live`) oder gepuffert (`buffer`).
  Schema-Schutz: Größe/Tiefe begrenzt, nur interne `$ref`, offline, lineare Regex;
  ungeeignete Schemas `400 response_schema_rejected`. Validator `jsonschema` (MIT) ohne
  Netz-/Dateizugriff. Admin-API/GitOps-Prüfung, UI-Formular, Metrik
  `aigw_guardrail_hits_total{plugin="schema-guard"}`. Doku `13-guardrails.md` §13.
- **Lastvergleich mit LiteLLM (AIGW-102):** `scripts/bench-litellm.sh` misst ai-gw (Image,
  SQLite/Postgres) und LiteLLM-Proxy `v1.108.8` (ohne DB/Postgres, Produktionseinstellungen)
  gegen denselben Fake-Upstream auf denselben Kernen: Aufschlag, Durchsatz/Fehler bei
  10/100/500 Verbindungen (json/stream), CPU je Anfrage, Speicher, Kaltstart, Image-Größe.
  Ergebnisse in `docs/betrieb/leistung.md` §8 und `messwerte/102-*.txt`.
- **Signierte WASM-Module (AIGW-101):** Ed25519-Signatur über
  `aigw-wasm-module-v1:<sha256>` (Schlüssel-ID + Base64). Konfiguration `wasm.trusted_keys`
  (öffentliche Schlüssel: Base64 roh/SPKI oder PEM) und `wasm.require_signature` (Standard:
  Pflicht, sobald Schlüssel gesetzt sind). Prüfung beim Upload (`PUT
  /admin/api/v1/wasm-modules/{name}?keyId=…&signature=…`, `422`), beim Import aus
  `wasm.modules` (`key_id`/`signature`) und vor dem Kompilieren in der Laufzeit (SHA-256 neu
  berechnet). Liste/Abfrage zeigen `signature {keyId, status, loadable}` sowie
  `signatureRequired`/`trustedKeys`; Admin-UI mit Spalte „Signatur“ und Feldern im
  Upload-Dialog. CLI `aigw wasm keygen|pubkey|sign|verify`. Migration 0011 (beide Dialekte),
  Metrik `aigw_wasm_signature_rejected_total`. Doku `17-wasm-plugins.md` §3a, S-37.
- **`concurrency`-Limits werden durchgesetzt (S-12, AIGW-100):** gleichzeitige Anfragen je
  Key, Team, Organisation (optional je Modellgruppe) und je Modellgruppe; atomar über alle
  Ebenen, Freigabe bei Abschluss, Client-Abbruch und Fehler; `429
  concurrency_limit_exceeded` mit `Retry-After: 1`. `local` je Instanz, `redis` im Cluster
  (Lease-Ablauf `counters.concurrency_lease_secs`, Standard 30 s), `postgres-sync` je Instanz.

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
  Job „Container-Images (amd64 + arm64)“ manuell bzw. aus dem Release-Workflow (Tarballs als Artefakt).
- **Release-Lieferung (AIGW-119):** Workflow `.github/workflows/release.yml` (Tag `v*`):
  Binaries `ai-gw-<version>-linux-{amd64,arm64}.tar.gz` (glibc, Admin-UI eingebettet),
  Multi-Arch-Image `ghcr.io/mupf-dev/ai-gw` (Tags `<version>`, `<major>.<minor>`, `latest`;
  OCI-Labels und -Annotationen), `SHA256SUMS`, SBOM (CycloneDX), SLSA-Build-Provenance und
  SBOM-Attestierung über Sigstore (`gh attestation verify`), GitHub-Release mit den Notizen
  aus diesem CHANGELOG. Probelauf ohne Veröffentlichung per `workflow_dispatch`; lokal bzw.
  auf der Buildbox `scripts/release-build.sh`, Vorprüfung `scripts/release-notes.sh`.
- **Helm-Chart `deploy/helm/ai-gw` (AIGW-119):** Image, Startkonfiguration als YAML,
  Master-Key und Datenbank-URL aus vorhandenen Secrets (`existingSecret`), Ressourcen,
  Probes (`/readyz`, `/healthz`), PodDisruptionBudget, optional Ingress, ServiceMonitor und
  PVC für SQLite, gehärteter securityContext. Prüfung mit `helm lint`, `helm template` und
  kubeconform in `scripts/k8s-check.sh`, Smoke-Test in kind `scripts/helm-smoke.sh`.
  Doku `docs/betrieb/helm.md`.

### Geändert

Gegenüber den Entwicklungsständen auf `main` vor dem ersten Release (für Betreiber von
Vorabständen):

- **Lizenz:** Der Code steht jetzt unter FSL-1.1-ALv2 (Functional Source License, nach zwei Jahren
  je Version Apache-2.0) statt unter AGPL-3.0-or-later. Plugin-Schnittstelle (WIT) und
  Beispiel-Plugins stehen unter Apache-2.0, Rechteinhaber ist mupf-dev. Frühere Stände bleiben unter AGPL.
- **tiktoken-Rangtabellen nicht mehr enthalten (AIGW-92, inkompatibel):** Die Tabellen
  `o200k_base`/`cl100k_base` von OpenAI (Lizenz ungeklärt) liegen nicht mehr im Repository,
  Binary oder Image; Cargo-Feature `embedded-tokenizers` und `LICENSES/MIT.txt` entfallen.
  ai-gw lädt sie aus `tokenizer.ranks_dir` (Image: `/data/tiktoken`); neu
  `aigw tokenizer fetch|verify` (Download/Prüfung mit SHA-256 über `aigw-outbound`, Spiegel per
  `--base-url`), `scripts/fetch-tokenizers.sh`, kustomize-Komponente
  `deploy/kubernetes/components/tiktoken-fetch` bzw. Helm-Wert `tokenizer.fetch.enabled`
  (Init-Container), Compose-Schalter
  `AIGW_TOKENIZER_DOWNLOAD`. Ohne Tabellen schätzt die Heuristik (wie bisher bei fehlenden
  Dateien). `tokenizer.download_ranks` installiert die Tabelle auch, wenn das Verzeichnis nicht
  beschreibbar ist. Tests laufen offline (synthetische Tabelle); exakte Referenztests mit
  `AIGW_TOKENIZER_RANKS_DIR` bzw. `check.sh` mit `AIGW_TOKENIZER_RANKS=fetch` (CI).
- **Admin-Cookies `__Host-` (S-23, AIGW-99, inkompatibel):** Session- und Login-Cookie heißen
  `__Host-aigw-session` bzw. `__Host-aigw-login` (`Path=/`, ohne `Domain`) – Schwester-
  Subdomains können sie nicht mehr setzen (Login-CSRF/Session-Fixierung). Alte
  `__Secure-`-Cookies werden nicht mehr angenommen und bei Login/Logout gelöscht: **alle
  Nutzer melden sich nach dem Update einmal neu an.** OpenAPI-Schema `session` entsprechend.
- **Private Upstreams nur für globale Bindungen (S-22, AIGW-99, inkompatibel):**
  `admin.allow_private_upstreams` gilt für Plugin-Bindungen (`webhook`, `content-safety`) nur
  noch auf den Ebenen `global`/`model-group`; Bindungen auf Organisation/Team/API-Key
  erreichen nur öffentliche Ziele (Prüfung beim Schreiben und nach der Namensauflösung).
  Bestehende Org-/Team-/Key-Bindungen mit privatem Ziel werden beim Kettenaufbau ungültig
  (Log `Plugin-Bindung ungültig`, je `failMode` übersprungen bzw. gesperrt) – auf eine globale
  bzw. Modellgruppen-Bindung umstellen. Namensregeln geben Link-Local (169.254.0.0/16,
  fe80::/10) nie mehr frei. `PluginHost::public_only`, `BuildCtx::level`,
  `OutboundClient::public_only`, `WebhookClient::public_only` neu.
- **Vertex-Token-Endpunkt geprüft (S-21, AIGW-99):** `settings.tokenUrl` und `token_uri` in
  `serviceAccountJson` nur noch `https://oauth2.googleapis.com/token` bzw. `https` unter
  `*.googleapis.com` – sonst `422` (Admin-API) bzw. `issues` (GitOps).
- **Plugins können Anfragen nicht mehr verteuern (S-30, AIGW-99):** Nach den Pre-Plugins
  werden `n` und `max_tokens` auf die bei der Admission zugelassenen Werte begrenzt (Webhook
  `modify`, WASM, `request-transform`); Begrenzung mit `warn`-Log.
- **Bootstrap-Token (S-29, AIGW-99):** Fehlversuche werden gezählt, ab 10 je Minute wird
  jeder weitere um 1 s gebremst (das richtige Token bleibt gültig). Die automatische Abschaltung gilt sofort im Prozess,
  auch wenn das Speichern scheitert (`error`-Log, Wiederholung); der Start holt einen
  fehlenden Abschaltvermerk nach (`aigw_admin::reconcile_bootstrap`).
- **MCP-Grenzen (S-28, AIGW-100, verhaltensändernd):** `initialize`, `tools/list`,
  `resources/*` und `prompts/*` zählen gegen die Limits des Keys (`requests`, `concurrency`;
  `ping` nicht) – Keys mit knappem RPM-Limit brauchen ggf. mehr Spielraum. Neu
  `mcp.max_sessions_per_key` (100), `mcp.max_sessions_per_team` (1000; darüber
  `429 too_many_sessions`) und `mcp.notification_buffer` (64; begrenzte Benachrichtigungskanäle,
  Überlauf wird verworfen und geloggt). `Sessions::create` bekommt das Team, `NoteSink` einen
  begrenzten `mpsc::Sender`.
- **Cache-AAD mit Partition (S-27, AIGW-100):** Warmstart-Einträge des semantischen Caches
  binden ID **und** Partition in die AAD; Migration 0010 (SQLite + Postgres) leert
  `cache_vector` einmalig (Cache wird neu aufgebaut).
- **Route aus derselben Konfigurationsversion wie die Freigabe (S-31, AIGW-100):**
  `ModelRoutes::route_for` (Standard: `route`); `SnapshotRoutes` sucht über den kanonischen
  Namen der freigegebenen Gruppe in der passenden Tabelle (älter → sofortiger Neuaufbau, neuer →
  vorige Tabelle).
- `CounterBackend` hat die neuen Pflichtmethoden `acquire_slots` und `in_flight` (S-12).
- Hinweis `concurrency-not-enforced` entfällt; bei `postgres-sync` meldet
  `GET /usage/limit-warnings` stattdessen `concurrency-per-instance`.

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

- Sicherheit: wasmtime 50.0.1 → 50.0.2 (RUSTSEC-2026-0566 bis -0573); `rust-version` auf 1.97 angehoben (Mindestversion von wasmtime 50.0.2).

- **Plugins vergrößern den Prompt nach der Admission (S-38, AIGW-103):** Nach den Pre-Plugins
  wird die Eingabe neu geschätzt; Wachstum wird auf Token-/Eingabe-Limits und harten Budgets
  nachreserviert oder die Anfrage abgelehnt (`429` bzw. Budget-Fehler).
- **Redis-Ausfalltests stabil unter Last (AIGW-94):** `falls_back_and_recovers` und
  `deferred_settles_replayed_after_outage` hingen an Wanduhrzeit: feste `CLIENT PAUSE`-Dauer
  (600/700 ms), 100 ms Zeitlimit und ein 300-ms-Zeitraum. Unter Last (Load > 30, nachgestellt)
  lief das Nachtragen in die Zeitüberschreitung und wurde erneut gebucht (`k2` ≠ 2) bzw. der
  kurze Zeitraum endete schon vor dem Abgleich (`pending` < 2). Jetzt hält ein TCP-Proxy je Test
  den Server an, bis der Test ihn freigibt; Zeitlimit 2 s, Zeitraumende relativ zum Ausfall.
  Aussagen unverändert (alt 2 Fehler in 85 Läufen unter Last, neu 0 in 115).

### Bekannte Einschränkungen

- GitOps-Manifeste: höchstens 1 024 YAML-Dokumente je Datei.
- Lizenz der eingebetteten tiktoken-Rangtabellen (MIT) vor einem öffentlichen Release
  rechtlich bestätigen.

[Unreleased]: https://github.com/mupf-dev/ai-gw/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mupf-dev/ai-gw/releases/tag/v0.1.0
