# ai-gw – Architekturkonzept (AIGW-7)

Stand: 2026-11-12 · Status: Entwurf zur Durchsicht
Grundlage: [00-entscheidungen.md](00-entscheidungen.md) (E1–E6)

## 1. Ziele und Qualitätsanforderungen

| ID | Anforderung | Zielwert (MVP) |
|---|---|---|
| Q1 | Zusätzliche Latenz durch das Gateway (ohne Plugins, ohne Cache) | p99 < 1 ms, p50 < 200 µs |
| Q2 | Gleichzeitige Streaming-Verbindungen | ≥ 10 000 je Instanz mit 2 vCPU / 1 GB RAM |
| Q3 | Speicherbedarf je offenem Stream | < 32 KiB (ohne Payload-Logging) |
| Q4 | Time-to-first-token-Aufschlag | < 1 ms (Chunks werden sofort weitergereicht, kein Puffern) |
| Q5 | Kaltstart des Binarys bis betriebsbereit | < 500 ms (SQLite) |
| Q6 | Wirksamwerden von Konfigurationsänderungen | < 1 s auf allen Instanzen |
| Q7 | Kein Datenbankzugriff im Hot Path | Auth, Routing, Limits und Policies kommen aus dem RAM |
| Q8 | Ausfall von DB oder Redis | Der Proxy läuft mit dem letzten Snapshot weiter; Zähler fallen auf `local` zurück, Logs werden gepuffert bzw. verworfen und gezählt |

## 2. Prozess- und Crate-Struktur

Ein Binary `aigw` mit zwei Listenern (Standard: getrennte Ports; per Konfiguration auf einen Port zusammenlegbar):

- **Datenebene** (`:4000`): OpenAI-kompatible API, Anthropic-Messages-API, MCP-Gateway
- **Steuerebene** (`:4001`): Admin-API `/admin/api/v1`, Admin-UI `/admin/`, `/metrics`, `/healthz`, `/readyz`

```
crates/
  aigw-core       Typen, Fehler, kanonisches Anfragemodell (IR), Konfigurations-Snapshot
  aigw-proxy      Hot Path: Listener, Auth, Pipeline, Routing, Streaming
  aigw-providers  Anbieter-Adapter (Trait + Implementierungen)
  aigw-store      Persistenz (sqlx: SQLite/Postgres), Migrationen, Snapshot-Aufbau, LISTEN/NOTIFY
  aigw-counters   Zähler-Trait + Backends local / postgres-sync / redis
  aigw-plugins    Plugin-Kette + eingebaute Plugins (Guardrails, PII, Cache …)
  aigw-admin      Admin-API (axum + utoipa → OpenAPI), OIDC, RBAC
  aigw            Binary: CLI, Start, eingebettete UI
ui/               Svelte-5-SPA (wird in das Binary eingebettet)
```

Laufzeit: tokio Multi-Thread, hyper 1 (HTTP/1.1 + HTTP/2) und rustls. Upstream-Verbindungen laufen über einen eigenen Connection-Pool pro Anbieter-Endpunkt mit HTTP/2 und Keep-Alive.

## 3. Lebenszyklus einer Anfrage (Hot Path)

```
Client ──► [1 Annahme] ──► [2 Auth] ──► [3 Kontext] ──► [4 Limits: Reservierung]
                                                            │
        ┌───────────────────────────────────────────────────┘
        ▼
 [5 Pre-Plugins] ──► [6 Cache-Lookup] ──Treffer──► Antwort
        │ kein Treffer
        ▼
 [7 Routing: Deployment wählen] ──► [8 Adapter: IR → Anbieterformat] ──► Upstream
        ▲          │ Fehler vor dem ersten Byte
        └─Fallback/Retry
                                         Upstream-Stream
                                               ▼
 [9 Adapter: Anbieterformat → Client-Format, Chunk für Chunk] ──► [10 Stream-Plugins] ──► Client
                                               ▼ (Ende/Abbruch)
 [11 Usage abrechnen: Zähler, Budget, Kosten] ──► [12 Log-Ereignis in Ring-Puffer] ──► Batch-Writer/Exporter
```

1. **Annahme:** Header lesen. Den Body nur so weit parsen, wie nötig (siehe §4).
2. **Auth:** API-Key aus `Authorization: Bearer` oder `api-key`/`x-api-key` lesen, SHA-256-Hash bilden und in der HashMap des Snapshots nachschlagen (O(1), vergleicht nur Hashes). Ergebnis ist ein `KeyCtx` (Key, Team, Organisation, Rechte, Limits, Policies).
3. **Kontext:** Das Modell-Alias wird zur Modellgruppe aufgelöst und die Freigabe für den Key geprüft. Die wirksame Policy (Org → Team → Key → Route) wird beim Snapshot-Aufbau vorberechnet, nicht pro Anfrage.
4. **Limits:** Zähler werden *reserviert* (siehe §8). Bei Überschreitung folgt sofort `429` mit `Retry-After` und `x-ratelimit-*`-Headern.
5. **Pre-Plugins:** Guardrails auf dem Prompt, PII-Maskierung, Header-Manipulation (siehe §9).
6. **Cache:** zuerst exakt, dann semantisch, falls für die Route aktiviert (siehe §10).
7. **Routing:** Ein Deployment aus der Modellgruppe auswählen (siehe §6).
8. bis 10. **Übersetzung und Streaming:** siehe §5 und §7.
11. **Abrechnung:** Usage aus der Antwort (oder geschätzt bei Abbruch) gleicht die Reservierung aus, schreibt Kosten fort und prüft das Budget.
12. **Log:** Ein Ereignis wird nicht-blockierend in einen begrenzten Kanal geschrieben (siehe §12).

## 4. Kanonisches Anfragemodell und Formate

- **Eingangsformate:** OpenAI (Chat Completions, Responses, Embeddings, Images, Audio, Models) und **Anthropic Messages** (`/v1/messages`). Damit funktionieren Werkzeuge wie Claude Code oder Anthropic-SDKs direkt.
- **Kanonische Zwischenform (IR)** in `aigw-core`: Nachrichten, Content-Teile (Text, Bild, Audio, Datei), Tools und Tool-Aufrufe, Sampling-Parameter, Reasoning-Einstellungen, Antwortformat (JSON-Schema). Unbekannte Felder werden in `extra` mitgeführt, damit nichts verloren geht.
- **Immer über die IR (Entscheidung 2026-11-13):** Jede Anfrage wird vollständig in die IR geparst, validiert und für den Anbieter neu serialisiert, auch wenn Eingangs- und Anbieterformat gleich sind. Das ergibt einheitliches Verhalten, Validierung und eine einzige Codebasis für Plugins.
  - Damit neue Anbieter-Features nicht verloren gehen, werden unbekannte Felder in `extra` mitgeführt und bei **gleichem** Zielformat unverändert wieder ausgegeben (Best Effort); bei anderem Zielformat werden sie verworfen und als Warnung geloggt.
  - Effizienz: Parsen mit geliehenen Strings (`Cow<'_, str>`) aus dem `Bytes`-Puffer, wenig Kopieren; Ziel Q1 wird durch Benchmarks in AIGW-57 abgesichert.
- **Fehlerformat:** immer im Fehlerformat des Eingangs (OpenAI `{"error":{…}}` bzw. Anthropic `{"type":"error",…}`), mit Header `x-aigw-request-id`.

## 5. Anbieter-Adapter

```rust
pub trait Provider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> &Capabilities;          // Chat, Tools, Vision, Embeddings …
    fn native_format(&self) -> WireFormat;             // Zielformat (für `extra`-Weitergabe)
    fn build_request(&self, req: &IrRequest, dep: &Deployment) -> Result<http::Request<Body>>;
    fn parse_response(&self, resp: Bytes) -> Result<IrResponse>;
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>; // inkrementell: Bytes → IR-Events
    fn classify_error(&self, status: StatusCode, body: &[u8]) -> UpstreamError; // retrybar? Quota?
}
```

| Anbieter | Format | Authentifizierung | Bemerkung |
|---|---|---|---|
| OpenAI, generisch kompatibel, vLLM, Ollama, Mistral | OpenAI | Bearer | Mistral und Ollama haben Sonderfelder |
| Azure OpenAI / Azure AI Foundry | OpenAI (Deployment-URL) | `api-key` oder Entra-ID-Token | Prioritätsgruppen für PTU/PAYG |
| Anthropic | Messages | `x-api-key` | Prompt-Caching-Felder durchreichen |
| AWS Bedrock | Converse / InvokeModel | **SigV4, eigene Implementierung** | Event-Stream-Binärformat dekodieren |
| Google Vertex / Gemini | generateContent | Service-Account-JWT → OAuth-Token, gecacht | AI-Studio-Key alternativ |

Zugangsdaten der Anbieter werden **verschlüsselt** gespeichert (AES-256-GCM, Master-Key aus Datei oder Umgebungsvariable, Rotation über Key-ID). Im RAM liegen sie entschlüsselt; die Admin-API gibt sie nie aus.

## 6. Routing und Resilienz

- **Modellgruppe** (öffentlicher Name, z. B. `gpt-4o`, `claude-sonnet`) → n **Deployments** (Anbieter + Endpunkt + Anbieter-Modell + Zugangsdaten + Gewicht + Priorität + eigene RPM/TPM-Kapazität).
- **Strategien:** `weighted` (Standard), `least-latency` (EWMA der TTFT bzw. Latenz), `least-busy` (offene Anfragen), `cost` (günstigstes zuerst).
- **Prioritätsgruppen:** Gruppe 0 vor Gruppe 1 (z. B. PTU vor PAYG). Eine Gruppe ist gesperrt, solange alle Deployments darin offen (Circuit Breaker) oder an der Kapazitätsgrenze sind.
- **Retries:** nur **vor dem ersten Byte an den Client**, mit exponentiellem Backoff und Jitter. Respektiert `Retry-After`. Retrybar sind 408, 429, 5xx und Verbindungsfehler; 400 und Content-Filter nicht.
- **Fallback-Ketten** über Modellgruppen hinweg (z. B. `gpt-4o` → `claude-sonnet`), optional nur bei bestimmten Fehlerklassen (Kontextlänge → Modell mit größerem Kontext).
- **Circuit Breaker** je Deployment: Fehlerquote im gleitenden Fenster, halboffener Zustand mit Probe. Optional aktive Health Checks.
- **Timeouts:** Verbindungsaufbau, Time-to-first-byte, Pause zwischen Chunks und Gesamtzeit, jeweils einzeln konfigurierbar.

## 7. Streaming

- Upstream-Bytes → inkrementeller SSE-/Event-Stream-Decoder → bei Übersetzung IR-Events → Encoder im Client-Format. Auch Stream-Chunks laufen immer durch Decoder → IR-Event → Encoder.
- **Backpressure:** Gelesen wird nur, wenn der Client Daten abnimmt; es gibt keinen unbegrenzten Puffer.
- **Client-Abbruch:** Die Upstream-Anfrage wird sofort abgebrochen. Ohne Usage-Event wird die Usage geschätzt (bisher gesendete Tokens) und markiert (`usage_estimated=true`).
- **Usage:** Bei OpenAI-Streams wird `stream_options.include_usage` serverseitig erzwungen; der Usage-Chunk wird entfernt, wenn der Client ihn nicht angefordert hat.
- **Stream-Plugins** (z. B. Ausgabe-Guardrails) arbeiten auf einem Gleitfenster. Wer die ganze Antwort braucht, schaltet für die Route auf „gepuffert“ um; das wird im UI als Latenzkosten angezeigt.

## 8. Limits, Zähler und Budgets

**Zähler-Trait** (E4):

```rust
#[async_trait]
pub trait CounterBackend: Send + Sync {
    /// Reserviert `amount` im Fenster; gibt Restkontingent oder Ablehnung zurück.
    async fn reserve(&self, key: &CounterKey, amount: u64, limit: u64, window: Window) -> Reservation;
    /// Gleicht eine Reservierung mit dem tatsächlichen Verbrauch ab (kann ± sein).
    async fn settle(&self, r: Reservation, actual: u64);
    async fn get(&self, key: &CounterKey, window: Window) -> u64;
}
```

| Backend | Genauigkeit | Funktionsweise |
|---|---|---|
| `local` | exakt je Instanz | Atomics und Sliding-Window (zwei Buckets mit Gewichtung) |
| `postgres-sync` | ±1–2 s im Cluster | lokal wie `local`; alle 1–2 s werden Deltas per `UPSERT … RETURNING` geschrieben und die Summe aller Instanzen zurückgelesen |
| `redis` | exakt im Cluster | Lua-Skript `reserve`/`settle` atomar, Pipelining; bei Redis-Ausfall Rückfall auf `local` mit Warnung |

- **Token-Limits (TPM)** analog zu Azure `llm-token-limit`: Die Reservierung beträgt *geschätzte Prompt-Tokens + `max_tokens`* (ohne `max_tokens`: konfigurierbarer Standard). Abgeglichen wird mit der tatsächlichen Usage. Die Prompt-Tokens werden über die Zeichenzahl geschätzt; ein genauer Tokenizer ist je Modellfamilie optional und kostet Latenz.
- **Ebenen:** Key, Team, Organisation, Modellgruppe (global) und Deployment (Kapazität, wird für das Routing genutzt).
- **Budgets:** Betrag je Zeitraum (Tag, Woche, Monat, frei) auf Key, Team oder Organisation, mit Warnschwellen (Ereignis/Webhook) und harter Grenze (`429` bzw. `402`-artiger Fehlercode `budget_exceeded`). Die Kosten stammen aus einer **Preistabelle** je Modell (Input, Output, Cached Input, Reasoning, Bild/Audio). Die Tabelle ist versioniert; Kosten werden mit dem zum Anfragezeitpunkt gültigen Preis berechnet.
- Umsetzung (AIGW-30/31): [11-limits-und-budgets.md](11-limits-und-budgets.md).

## 9. Plugin-Kette

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn on_request(&self, ctx: &mut ReqCtx) -> Flow;      // Continue | Respond(..) | Reject(..)
    async fn on_response(&self, ctx: &mut RespCtx) -> Flow;    // nicht-streamend
    fn on_chunk(&self, ctx: &mut ChunkCtx, ev: &mut IrEvent) -> Flow; // synchron, kein await im Chunk-Pfad
}
```

- **Konfiguration** je Ebene: global, Organisation, Team, Key, Modellgruppe, Route. Die Kette wird beim Snapshot-Aufbau zusammengesetzt und als `Arc<[Box<dyn Plugin>]>` abgelegt. Umsetzung (AIGW-36, Trait-Details, Vererbung mit `override`/`disable`/`locked`, Fehlermodi, gepufferter Modus): [12-plugins.md](12-plugins.md).
- **Eingebaute Plugins (MVP):** `pii` (Erkennung per Regex und Prüfsumme: E-Mail, IBAN, Kreditkarte mit Luhn-Prüfung, Telefon, Steuer-ID usw.; Modi maskieren, ablehnen oder pseudonymisieren mit Rückübersetzung in der Antwort), `prompt-filter` (Listen, Regex, Prompt-Injection-Heuristiken), `content-safety` (externe Dienste: Azure Content Safety, OpenAI Moderation, Llama Guard über ein konfiguriertes Modell), `cache`, `header` (Header setzen oder entfernen), `request-transform` (Standardparameter setzen oder überschreiben, z. B. `max_tokens`-Deckel). Guardrails umgesetzt (AIGW-37/38): [13-guardrails.md](13-guardrails.md).
- **Eigene Plugins von Nutzern (Entscheidung 2026-11-13):**
  - **MVP: Webhook-Plugin** `webhook`: ruft einen externen HTTP-Dienst in den Phasen `on_request` / `on_response` auf (JSON mit IR-Anfrage/-Antwort + Kontext ohne Geheimnisse). Antwort: `continue` / `modify` (geänderte IR) / `reject` (Status + Meldung). Konfigurierbar: Timeout (Standard 200 ms), Verhalten bei Fehler (`fail-open` / `fail-closed`), HMAC-Signatur der Aufrufe, Keep-Alive-Pool, SSRF-Schutz. Nicht im Chunk-Pfad (zu teuer pro Token); für Ausgaben nur bei gepufferter Route.
  - **Nach dem MVP: WASM-Plugins** (wasmtime, Component Model, WIT-Schnittstelle aus dem Plugin-Trait abgeleitet), auch im Chunk-Pfad nutzbar, mit Speicher-/Zeitgrenzen je Aufruf. Die Trait-Schnittstelle wird im MVP so entworfen, dass sie sich ohne Umbau darauf abbilden lässt.

## 10. Cache

- **Exakt:** Schlüssel = BLAKE3 über (Scope, Modellgruppe, normalisierter Request ohne `user`/`stream`). Ablage im RAM als LRU mit Größengrenze in Bytes, optional zusätzlich in Redis. Gestreamte Antworten werden aus dem Cache als Stream wiedergegeben.
- **Semantisch (Entscheidung 2026-11-13):** Embedding der letzten Nutzernachricht (plus Hash des Systemprompts als harter Filter) über eine konfigurierte Embedding-Modellgruppe. Vektor-Backend als Trait `VectorIndex` (`insert`, `search(k, threshold)`, `evict`), analog zum Zähler-Trait:
  - `memory` (Standard): **eigener HNSW-Index im RAM** je Scope, Kosinus-Schwelle konfigurierbar, TTL; persistente Ablage in der DB für den Warmstart.
  - `pgvector`: gemeinsamer Cache über alle Instanzen in Postgres (Erweiterung `vector`).
  - `redis`: Redis Stack / Valkey Search (`FT.SEARCH` KNN) für gemeinsamen Cache.
  Bei Ausfall eines externen Backends: Cache-Miss, keine Fehlerweitergabe.
- **Scope** (Standard: Team) verhindert, dass Antworten zwischen Mandanten durchsickern. Nie mandantenübergreifend.
- Antwort-Header `x-aigw-cache: hit-exact | hit-semantic | miss | bypass`; ein Cache-Treffer verbraucht keine Upstream-Tokens, wird aber im Log gezählt.
- Umsetzung (AIGW-39/40): [14-cache.md](14-cache.md) – Pipeline-Schritt 6 mit Richtlinie aus dem Plugin `cache`, Crate `aigw-cache`.

## 11. MCP-Gateway

- ai-gw ist **MCP-Server** (Streamable HTTP) für Clients und **MCP-Client** zu registrierten Upstream-MCP-Servern (Streamable HTTP und SSE, optional stdio als verwalteter Prozess).
- Tools aller freigegebenen Server werden unter einem Namensraum zusammengeführt (`<server>__<tool>`). Die Freigabe läuft je Key und Team bis auf Tool-Ebene.
- Zugangsdaten zu Upstream-MCP-Servern liegen verschlüsselt wie die der Anbieter. OAuth-Weiterleitung ist optional.
- Jeder Tool-Aufruf wird protokolliert (Argumente maskierbar). Limits und Plugins (z. B. PII) greifen auch hier.
- **SSRF-Schutz:** Upstream-URLs werden gegen eine Allowlist geprüft; private Adressbereiche sind standardmäßig gesperrt.
- Umsetzung (AIGW-41): Crate `aigw-mcp`, Endpunkte `/mcp` und `/mcp/{server}`, Details in
  [15-mcp-gateway.md](15-mcp-gateway.md).

## 12. Beobachtung

- **Request-Log:** begrenzter MPSC-Kanal → Batch-Writer (alle 500 ms oder 1000 Einträge). Ist der Kanal voll, wird verworfen und der Verlust als Metrik `aigw_log_dropped_total` gezählt. Der Proxy wartet nie auf Logs. Der Payload wird nur gespeichert, wenn es konfiguriert ist; dann ist er maskiert und separat befristet.
- **Aggregation:** Minuten-Rollups (Requests, Tokens, Kosten, Latenz-Histogramme, Fehler, Cache) je Key, Team, Modell und Deployment. Die Dashboards lesen nur die Rollups; Rohlogs werden nach einer konfigurierbaren Frist gelöscht.
- **Prometheus** `/metrics` und **OpenTelemetry** (OTLP-Traces mit Spans für Auth, Plugins, Upstream und TTFT; GenAI-Semantic-Conventions).
- **Export** der Logs: OTLP-Logs, JSON-Lines-Datei und Webhook.

## 13. Datenmodell (Kern)

| Entität | Wichtige Felder |
|---|---|
| `organization` | id, name, Einstellungen |
| `team` | id, org_id, name, Limits, Budget, erlaubte Modellgruppen |
| `user` | id, OIDC-Subjekt/Issuer, E-Mail, Rollen je Org/Team |
| `api_key` | id, Präfix (sichtbar), SHA-256-Hash, team_id, user_id?, Ablauf, Status, Limits, Budget, erlaubte Modelle, Metadaten |
| `provider_credential` | id, Typ, verschlüsselte Daten, key_id |
| `model_group` | id, öffentlicher Name, Aliasse, Routing-Strategie, Fallbacks |
| `deployment` | id, model_group_id, Anbieter, Endpunkt, Anbieter-Modell, credential_id, Gewicht, Priorität, Kapazität |
| `price` | model_ref, gültig_ab, Preise je Token-Art |
| `policy` / `plugin_binding` | Ebene + Ziel-ID, Plugin, Reihenfolge, Konfiguration (JSON) |
| `mcp_server`, `mcp_grant` | Upstream, Transport, Zugang; Freigaben je Key/Team/Tool |
| `counter_state` | nur `postgres-sync`: Schlüssel, Fenster, Wert |
| `budget_usage` | Ebene, Zeitraum, Betrag |
| `request_log`, `usage_rollup` | siehe §12 |
| `audit_log` | jede Admin-Änderung: wer, was, vorher/nachher |
| `config_version` | monoton steigende Version für Snapshot und Verteilung |

- Migrationen über sqlx; das Schema ist so geschrieben, dass es auf SQLite und Postgres gleich läuft (keine Postgres-spezifischen Typen in Kerntabellen).
- **API-Keys:** Format `sk-aigw-<12 Zeichen Präfix>-<32 Byte Zufall, Base62>`. Der Klartext wird nur einmal bei der Erzeugung angezeigt. Wegen der hohen Entropie genügt SHA-256 als Hash (kein Argon2 nötig; das hält den Hot Path schnell).

## 14. Konfigurations-Snapshot und Verteilung

- Jede Admin-Änderung läuft als DB-Transaktion, erhöht `config_version` und schreibt das `audit_log`.
- Jede Instanz hält `ArcSwap<Snapshot>`. Ein Snapshot ist unveränderlich und enthält vorberechnete Indizes (Key-Hash → KeyCtx, Alias → Modellgruppe, zusammengesetzte Plugin-Ketten). Laufende Anfragen behalten ihren Snapshot. Die Routing-Tabelle wird je `config_version` aus dem Snapshot gebaut; die Route einer Anfrage wird über den kanonischen Namen der **freigegebenen** Gruppe in der Tabelle derselben Version gesucht (S-31: ggf. sofortiger Neuaufbau bzw. die vorige Tabelle), nie über den Alias in einer anderen Version.
- **Aktualisierung:** SQLite in einer Instanz → direkt nach dem Commit. Postgres → `NOTIFY aigw_config, '<version>'` an alle Instanzen, die neu aufbauen (inkrementell, siehe unten). Zusätzlich wird alle 30 s die Version abgefragt, falls eine Benachrichtigung verloren geht.
- **Umsetzung (AIGW-10):** `aigw_core::snapshot::SnapshotHandle` (`load()` lock-frei, Versions-Abo per `tokio::sync::watch`), `aigw_store::reload::Reloader`. Auslöser werden 100 ms zusammengefasst (Debounce). Schlägt der Neubau fehl oder ist die Datenbank nicht erreichbar, bleibt der letzte Snapshot aktiv (Q8); erneuter Versuch mit Backoff (0,25 s → 30 s), LISTEN-Verbindung mit Reconnect. Abfrageintervall: `store.reload_poll_secs` (0 = aus).
- **Inkrementeller Neubau (AIGW-75, F16):** Änderungsjournal ist das `audit_log` (jede Änderung
  schreibt in derselben Transaktion `config_version`, Art und Objekt-ID; eine eigene
  `change_log`-Tabelle wäre eine Doppelung). Der Reloader liest die Einträge seit der aktiven
  Version, lädt genau diese Objekte (plus Abhängigkeiten ohne eigenen Eintrag: Keys eines geänderten
  Teams, nach Löschungen alle Plugin-Bindungen/MCP-Freigaben) und arbeitet sie per
  `Snapshot::apply_patch` ein; abgeleitete Indizes (Key-Kontexte, Modellgruppen samt Aliassen und
  Deployments, aufgelöste Zugangsdaten) bleiben konsistent. Vollständiger Neubau als Rückfall bei
  Lücke im Journal, mehr als `snapshot.max_incremental_changes` Einträgen, Version rückwärts,
  Dauerkonflikt und immer nach einem Kaltstart aus dem Cache. **Konsistenzprüfung** alle
  `snapshot.verify_interval_secs` (Standard 600): vollständiger Neubau im Hintergrund und
  inhaltlicher Vergleich (`Snapshot: PartialEq`); bei Abweichung ersetzt der Neubau den Stand,
  Metrik `aigw_snapshot_reload_total{result="verify_mismatch"}`, Fehler im Log. Messwerte:
  [messwerte/snapshot-inkrementell-2026-11-13.txt](messwerte/snapshot-inkrementell-2026-11-13.txt).
- **Snapshot-Cache auf Platte (AIGW-75, F14):** `snapshot.cache_file` (Standard aus). Jeder aus
  der Datenbank gebaute Snapshot wird höchstens alle `snapshot.cache_write_interval_secs` als
  Ganzes mit AES-256-GCM und dem Master-Key verschlüsselt geschrieben (Kopf mit Format und Key-ID
  als AAD; atomar über Temp-Datei, `fsync`, `rename`; Modus 0600). Ist die Datenbank beim Start
  nicht erreichbar, startet der Gateway mit diesem Stand (Readiness `degraded`), migriert die
  Datenbank, sobald sie erreichbar ist, und ersetzt den Stand durch einen vollständigen Neubau.
  Ungültiger Cache (Format, Integrität, fremder Key) wird verworfen; ohne gültigen Cache bricht der
  Start wie bisher ab.
- **Readiness (AIGW-75, F15):** `GET /readyz` auf beiden Listenern: `200 {"status":"ready"}`
  bzw. `200 {"status":"degraded"}`, wenn der Snapshot aus dem Cache stammt oder länger als
  `snapshot.stale_after_secs` (Standard 300) nicht mit der Datenbank abgeglichen werden konnte;
  Body enthält `snapshot` (`version`, `ageSeconds`, `source`, `stale`, `failing`). Dasselbe Feld
  `snapshot` liefert `GET /admin/api/v1/version` (UI-Banner).
- **GitOps:** `aigw config export > aigw.yaml` und `aigw config apply aigw.yaml` (Diff, `--dry-run`, `--prune`). Geheimnisse stehen im YAML nur als Verweis (`env:`, `file:`), nie im Klartext. Der Modus „nur YAML“ (keine Admin-Schreibrechte, UI schreibgeschützt) ist möglich. **Umsetzung (AIGW-33):** [09-gitops.md](09-gitops.md) (`aigw config export|diff|apply|validate`, `config:export`/`config:apply` in der Admin-API, `gitops.read_only_admin`).

## 15. Admin-API, Authentifizierung, Rollen

- REST unter `/admin/api/v1`, OpenAPI 3.1 per utoipa. Aus dieser Spezifikation werden die UI-Typen generiert, und **später auch ein MCP-Server für die Administration** (gleiches Prinzip wie bei cloud-project: API, Portal und MCP gleichwertig).
- **Login:** OIDC (Authorization Code + PKCE) gegen einen beliebigen IdP (Entra ID, Keycloak, Kanidm, später der OIDC-AS von cloud-project). Danach gilt ein Session-Cookie (`HttpOnly`, `SameSite=Strict`, CSRF-Token). Für die Erstinstallation gibt es einen lokalen Bootstrap-Admin über ein einmaliges Token in der Startausgabe.
- **Automatisierung:** Admin-API-Keys mit Rollen und Ablauf.
- **Rollen:** `admin` (alles), `org-admin`, `team-admin` (Keys, Budgets und Limits im eigenen Team, im Rahmen der Team-Grenzen), `viewer` (lesen, Dashboards), `auditor` (Audit-Log). Rollen können IdP-Gruppen zugeordnet werden.
- Nutzer mit eigenem Key können über die UI ihre eigenen Keys und ihren Verbrauch sehen (Self-Service, wie bei LiteLLM).

## 16. Sicherheit (Grundsätze)

- rustls only, TLS 1.2/1.3; optional mTLS zur Datenebene.
- Geheimnisse: Master-Key nie in der DB; Zugangsdaten verschlüsselt; Logs ohne Header `Authorization`/`api-key`.
- SSRF-Schutz für alle frei konfigurierbaren Upstream-URLs (Anbieter-Endpunkte, MCP, Webhooks):
  URL-Prüfung beim Schreiben und Adressprüfung bei jedem Verbindungsaufbau (nach der
  Namensauflösung, gegen DNS-Rebinding) im gemeinsamen Outbound-Baustein `aigw-outbound`
  (AIGW-72); keine Weiterleitungen, Größen- und Zeitlimits.
- Grenzen für Body-Größe, Header und Anzahl gleichzeitiger Verbindungen je Key (Schutz vor Missbrauch).
- `cargo deny` und `cargo audit` in CI; kein `unsafe` in eigenen Crates außer begründet.

## 17. Vorschläge zur Bestätigung

Diese Punkte sind im Konzept als Vorschlag gesetzt und werden einzeln mit dem User abgestimmt:

1. ✅ **Angenommen (2026-11-13):** Anthropic-Messages-API zusätzlich als **Eingangsformat** (nicht nur als Anbieter).
2. ❌ **Abgelehnt (2026-11-13):** Durchreiche-Pfad – stattdessen immer Übersetzung über die IR (§4).
3. ✅ **Angenommen mit Ergänzung (2026-11-13):** Webhook-Plugins im MVP, WASM-Plugins nach dem MVP (§9).
4. ✅ **Angenommen erweitert (2026-11-13):** Eigener HNSW-Index im RAM als Standard, zusätzlich optional pgvector oder Redis als Vektor-Backend (§10).
5. ✅ **Angenommen (2026-11-13):** Getrennte Listener als Standard (`:4000` Daten, `:4001` Steuerung), per Konfiguration auf einen Port zusammenlegbar (§2).
