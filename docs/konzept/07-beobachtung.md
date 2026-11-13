# ai-gw – Beobachtung: Request-Log, Rollups, Payload-Logging, Metriken, Traces (AIGW-43/44/45)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §1 (Q7, Q8), §3 Schritt 12, §12;
[02-datenmodell.md](02-datenmodell.md) §4

## 1. Überblick

```
Proxy ──record()──► RequestLogger ──try_send──► begrenzter MPSC-Kanal ──► Writer-Task
       (nie wartend)     │ voll/geschlossen: verwerfen,             │ alle 500 ms oder 1000 Einträge
                         ▼ aigw_log_dropped_total + 1               ▼
                                         Aufbereitung (spawn_blocking): Zeilen, Payload
                                         (gefiltert/maskiert), Rollup-Deltas je Minute+Schlüssel
                                                                    ▼
                                  Store: eine Transaktion – request_log, request_payload,
                                  usage_rollup (additiver UPSERT)  ─► Exporter (JSON-Lines …)
Aufbewahrungs-Task: stündlich Rohlogs / Payloads / Rollups nach Frist löschen
```

| Baustein | Ort |
|---|---|
| Ereignis `RequestRecord`, Hook-Trait `RequestObserver`, `PayloadMode`, `RawPayload`, `CacheOutcome`, `SENSITIVE_HEADERS` | `aigw_core::observe` |
| Konfiguration `observe` | `aigw_core::config::ObserveConfig` |
| Logger, Writer, Aufbereitung, Maskierung, Exporter, Aufbewahrung, Zähler | Crate `aigw-observe` |
| Prometheus-Metriken (`GatewayMetrics`, `MetricsObserver`), §11 | `aigw_observe::prom` |
| OTLP-Export: `tracing`-Layer für Traces, `OtlpLogExporter`, §12 | `aigw_observe::otel` |
| Trace-Konventionen (`TRACE_TARGET`, `current_traceparent`, `gen_ai_system`) | `aigw_core::observe` |
| Tabellen, Batch-Schreiben, Löschen, Lesen | `aigw_store::observe` (Schema seit AIGW-73: `migrations/{sqlite,postgres}/0001_init.sql`) |
| Dashboards/Log-Explorer: Abfragen, Stunden-Rollups, §13 | `aigw_store::analytics`, `aigw_admin::analytics` |

**Eigene Crate statt Modul:** Der Proxy hängt nur vom Trait in `aigw-core` ab und bleibt frei von
Datenbank-, Regex- und (ab AIGW-45) OpenTelemetry-Abhängigkeiten. Das Request-Log ist isoliert
testbar und lässt sich austauschen (z. B. reiner Export ohne DB). `aigw-store` kennt das Ereignis
nicht – es bekommt fertige Zeilen und Rollup-Deltas.

## 2. Hot Path

- Der Proxy ruft am Ende jeder Anfrage (letztes Byte an den Client bzw. Abbruch) genau einmal
  `RequestObserver::record(RequestRecord)` auf.
- `RequestLogger::record` = `try_send` in einen begrenzten `tokio::mpsc`-Kanal. Kein Lock, keine
  Allokation, kein I/O; im Fehlerfall (voll/geschlossen) ein `Relaxed`-Inkrement von
  `aigw_log_dropped_total`. Gemessen < 1 µs (siehe §8).
- Payload: `RequestObserver::payload_mode()` sagt dem Proxy, ob er Header/Bodies mitgeben soll.
  `RawPayload` hält `Bytes` (referenzgezählt, keine Kopie). Filtern, Kürzen und Maskieren
  passieren erst im Writer.
- Speicher: Kanal-Kapazität `queue_capacity` (Standard 16 384) × Größe eines Eintrags; Tokio legt
  Blöcke erst bei Bedarf an.

### 2.1 Felder von `RequestRecord`

| Feld | Bedeutung | Spalte `request_log` |
|---|---|---|
| `request_id` | `x-aigw-request-id` (UUIDv7) | `id` |
| `started_at` | Annahme der Anfrage | `at` (Unix-ms) |
| `ingress` | Eingangsformat (`WireFormat`) | `endpoint` (`openai-chat`, `anthropic-messages`) |
| `model_alias`, `model_group` | angefordert bzw. aufgelöst | `model_requested`, `model_group` |
| `deployment_id`, `provider`, `provider_model` | gewähltes Deployment (nach Fallbacks das letzte) | gleichnamig |
| `status`, `error_kind` | HTTP-Status, Fehlerklasse (`ErrorKind`) | `status`, `error_code` |
| `usage`, `usage_estimated` | `ir::Usage` (inkl. Cache-Lesen/-Schreiben, Reasoning), geschätzt? | `*_tokens`, `usage_estimated` |
| `cost` | Platzhalter (`Amount`, bis Kostenberechnung) | `cost` (Nano) |
| `ttft`, `duration` | Zeit bis erstes Token, Gesamtdauer | `ttft_ms`, `latency_ms` |
| `stream`, `client_aborted` | Streaming, Client-Abbruch | gleichnamig |
| `cache` | `hit-exact` / `hit-semantic` / `miss` / – | `cache` |
| `api_key_id`, `team_id`, `organization_id` | sobald Auth (AIGW-14) verfügbar | gleichnamig |
| `client_ip` | Client-Adresse | `client_ip` |
| `payload` | `Option<Box<RawPayload>>` | → `request_payload` |

## 3. Batch-Writer

- Liest mit `recv_many` bis zur Batchgröße (`batch_size`, Standard 1000) und schreibt sofort,
  wenn sie erreicht ist; sonst spätestens nach `flush_interval_ms` (Standard 500).
- Aufbereitung (Zeilen, Payload, Rollups) per `spawn_blocking` – Maskierung blockiert keinen
  Runtime-Worker.
- **Eine Transaktion je Batch:** mehrzeilige `INSERT`s (bis 500 Zeilen je Statement) in
  `request_log` (`ON CONFLICT (id) DO NOTHING` – Wiederholungen sind idempotent) und
  `request_payload`, dann die Rollup-Deltas als `UPSERT`.
  - SQLite bekommt anonyme `?`-Platzhalter: Benannte Parameter (`$n`) sucht SQLite beim Parsen
    linear – bei ~13 500 Parametern je Statement quadratisch (gemessen 0,5 s statt 5 ms für
    500 Zeilen). Postgres nutzt `$n`.
- **Fehler der DB** (Q8): bis zu 4 Versuche mit 250 ms / 500 ms / 1 s Pause. Währenddessen liest
  der Writer nicht; der Kanal läuft voll, weitere Einträge werden gezählt verworfen. Danach ist
  der Batch verloren (`aigw_log_write_failed_total`).
- **Rollups exakt bei Wiederholungen (F34, AIGW-73):** Jeder Batch bekommt **eine** ID (UUIDv7),
  gleich für alle Versuche (`Store::write_request_batch_once`). Die Transaktion trägt sie zuerst
  in `observe_batch` ein (`ON CONFLICT DO NOTHING`); ist sie schon vorhanden – der vorige
  Versuch wurde trotz Fehlermeldung committet (z. B. Verbindung beim `COMMIT` abgerissen) –,
  wird nichts geschrieben und der Batch gilt als erledigt. So zählen die additiven Rollups nie
  doppelt. Die Retention löscht Einträge in `observe_batch` nach 7 Tagen.
- **Exporter** bekommen denselben aufbereiteten Batch nach dem DB-Schreiben (nacheinander).
  Fehler → `aigw_log_export_errors_total` + Warnung, kein Wiederholen.
- **Graceful Shutdown** (`ObserveHandle::shutdown`): Signal → Writer schließt den Kanal (neue
  `record()` zählen als verworfen), liest ihn leer, schreibt alles (ohne Wiederholungen),
  ruft `flush()` der Exporter. Begrenzt durch `shutdown_timeout_ms` (Standard 5 s).

## 4. Minuten-Rollups

Schlüssel: (`bucket_start` = Minutenbeginn, `api_key_id`, `team_id`, `organization_id`,
`model_group`, `deployment_id`), fehlende Werte als `''`. Bucket nach `started_at`.

Der Writer aggregiert je Batch im Speicher und schreibt je Schlüssel **ein** Delta:

```sql
INSERT INTO usage_rollup (bucket_start, …, requests, …, lat_inf) VALUES (…), (…)
ON CONFLICT (bucket_start, api_key_id, team_id, organization_id, model_group, deployment_id)
DO UPDATE SET requests = usage_rollup.requests + excluded.requests, …
```

Rein additiv → identisch auf SQLite (≥ 3.24) und Postgres, mehrere Instanzen ohne
Read-Modify-Write. Schlüssel werden sortiert geschrieben (gleiche Sperrreihenfolge).

| Spalte | Inhalt |
|---|---|
| `requests`, `errors` | Anfragen; Fehler = Status ≥ 400 oder Fehlerklasse gesetzt |
| `cache_hits` | Treffer im Antwort-Cache (exakt + semantisch) |
| `input_tokens`, `output_tokens`, `cached_tokens`, `cache_write_tokens`, `reasoning_tokens` | Tokens je Art |
| `cost` | Kosten (Nano); bis zur Kostenberechnung der Platzhalter aus `RequestRecord::cost` |
| `latency_ms_sum`, `ttft_ms_sum`, `ttft_count` | Summen für Mittelwerte |
| `estimated`, `aborted` | Anfragen mit geschätzter Usage, Client-Abbrüche |
| `lat_le_25` … `lat_le_60000`, `lat_inf` | Latenz-Histogramm: Anzahl je Bucket (nicht kumulativ), Grenzen 25, 50, 100, 250, 500, 1000, 2500, 5000, 10 000, 30 000, 60 000 ms, +Inf |

Das JSON-Feld `latency_hist` aus 0001 entfällt: JSON lässt sich nicht portabel additiv
aktualisieren.

## 5. Payload-Logging und Maskierung

`observe.payload` (global; je Key/Team folgt mit der Policy-Auflösung):

| Modus | Gespeichert |
|---|---|
| `off` (Standard) | nichts; der Proxy schneidet nichts mit |
| `metadata` | Größen von Request/Response, Header-**Namen** |
| `masked` | Bodies und Header-Werte maskiert, gekürzt auf `payload_max_bytes` (64 KiB) |
| `full` | Bodies und Header-Werte unverändert (gekürzt) |

- `Authorization`, `Proxy-Authorization`, `api-key`, `x-api-key`, `Cookie`, `Set-Cookie`
  erscheinen **in keinem Modus** – weder Wert noch Name (`aigw_core::observe::SENSITIVE_HEADERS`).
- Ablage in eigener Tabelle `request_payload` (ID = Request-ID, `at`, `mode`, JSON):
  `{"request":{"bytes","body","truncated?","binary?","headers"},"response":{…}}`. Nicht-UTF-8 →
  `binary: true` ohne Inhalt.
- **Maskierung** über den Trait `PayloadMasker` (austauschbar per `ObserveBuilder::masker`).
  Standard ist `PiiMasker` mit der PII-Engine `aigw-pii` – dieselbe Erkennung wie das
  Guardrail-Plugin `pii` (AIGW-37, [13-guardrails.md](13-guardrails.md) §2): E-Mail, Telefon,
  IBAN (Länderlänge + mod 97), Kreditkarte (BIN + Luhn), Steuer-ID, SVNR, Ausweis/Pass,
  IP-Adressen, Zugangsdaten in URLs und API-Keys werden zu `[EMAIL]`, `[PHONE]`, `[IBAN]`,
  `[CARD]`, `[TAX_ID]`, `[SVNR]`, `[ID_CARD]`, `[PASSPORT]`, `[IP]`, `[URL_CREDENTIALS]`,
  `[API_KEY]`. Gierige Treffer werden an Gruppengrenzen verkürzt
  („4111 1111 1111 1111 22“ → „[CARD] 22“). Der frühere `RegexMasker` (nur E-Mail, Karte,
  IBAN) ist entfallen; Falsch-Positive der breiteren Erkennung im Log sind akzeptiert
  (User-Entscheidung 2026-11-13).
- **Kürzen ohne Lecks:** Im Modus `masked` wird zuerst grob auf 4 × `payload_max_bytes` an einem
  harten Trenner (`"`, Zeilenumbruch, `,`, Klammern …) vorgekürzt – dort kann kein maskiertes
  Muster zerschnitten werden –, dann maskiert, dann auf die Zielgröße gekürzt.

## 6. Aufbewahrung

Eigener Task (erster Lauf beim Start, dann alle `sweep_interval_secs`, Standard 1 h):

| Daten | Frist (Standard) | Löschen |
|---|---|---|
| `request_payload` | `payload_days` (7), höchstens so lange wie Rohlogs | `at <` Frist, Häppchen à 10 000 |
| `request_log` | `request_log_days` (30) | `at <` Frist, Häppchen à 10 000 |
| `usage_rollup` | `rollup_days` (400) | `bucket_start <` Frist |

`0` = unbegrenzt. Häppchen: `DELETE … WHERE id IN (SELECT id … WHERE at < $1 LIMIT $2)` (beide
Datenbanken), damit Sperren kurz bleiben.

## 7. Export und Metriken

- `LogExporter` (dyn-kompatibel, `export(&[LogEntry]) -> ExportFuture`, `flush()`), Eintrag =
  `RequestLogRow` + optional `PayloadRow`. OTLP-Logs (§12.4) und künftig Webhook implementieren
  denselben Trait.
- `JsonLinesExporter`: eine JSON-Zeile je Anfrage (Felder wie `request_log` in camelCase,
  optional `payload`), angehängt an `observe.jsonl.path`, Flush je Batch, Schreiben per
  `spawn_blocking`.
- Zähler (`ObserveMetrics::render_prometheus`, `GET /metrics` der Steuerebene): `aigw_log_dropped_total`,
  `aigw_log_written_total`, `aigw_log_write_failed_total`, `aigw_log_batches_total`,
  `aigw_log_export_errors_total`, `aigw_log_purged_total`. Gateway-Metriken: §11.

## 8. Konfiguration

```yaml
observe:
  request_log: true          # Rohlog in DB (Rollups immer)
  queue_capacity: 16384
  batch_size: 1000
  flush_interval_ms: 500
  shutdown_timeout_ms: 5000
  payload: "off"             # off | metadata | masked | full  (YAML: "off" quoten)
  payload_max_bytes: 65536
  retention: {request_log_days: 30, payload_days: 7, rollup_days: 400, sweep_interval_secs: 3600}
  jsonl: {enabled: false, path: aigw-requests.jsonl, include_payload: false}
  metrics: {enabled: true, max_model_groups: 200, max_deployments: 500}      # §11
  otlp:                                                                       # §12
    endpoint: ""               # z. B. http://otel-collector:4318 – leer = kein OTLP-Export
    headers: {}                # z. B. {authorization: "Bearer …"} (Werte nicht in Debug-Ausgaben)
    timeout_ms: 10000
    service_name: aigw
    traces:
      enabled: false           # Standard: aus
      sampling_ratio: 1.0      # 0.0–1.0, nach Trace-ID (wenn kein übernommenes Sampling-Flag)
      parent_based: true       # Sampling-Flag eines eingehenden traceparent übernehmen
      accept_traceparent: true # eingehenden W3C-traceparent übernehmen
      propagate: true          # traceparent an den Upstream weitergeben
      queue_capacity: 8192
      batch_size: 512
      flush_interval_ms: 1000
    logs: {enabled: false, include_payload: false}
    retry: {max_attempts: 4, initial_backoff_ms: 500, max_backoff_ms: 5000}   # F102
```

ENV wie üblich, z. B. `AIGW_OBSERVE__PAYLOAD=masked`, `AIGW_OBSERVE__RETENTION__PAYLOAD_DAYS=3`,
`AIGW_OBSERVE__OTLP__ENDPOINT=http://collector:4318`, `AIGW_OBSERVE__OTLP__TRACES__ENABLED=true`,
`AIGW_OBSERVE__OTLP__HEADERS__AUTHORIZATION=…`.

Start im Binary (`crates/aigw/src/runtime.rs` → `observe::start`, Shutdown in
`Runtime::shutdown` nach dem Schließen der Listener; `/metrics` auf der Steuerebene):

```rust
let observe = ObserveBuilder::new(cfg.observe.clone()).store(store.clone()).start();
let observer: Arc<dyn RequestObserver> = observe.observer();   // → ProxyState (Metriken + Log)
// … beim Herunterfahren, nach dem Schließen der Listener:
observe.shutdown().await;
```

## 9. Messwerte

Buildbox (x86-64, 8 vCPU), Release-Profil, `cargo bench -p aigw-observe`.

`record()` im Hot Path (divan, Median / Mittel):

| Fall | Median | Mittel |
|---|---|---|
| Einreihen, Writer liest parallel | 342 ns | 300 ns |
| Kanal voll → verwerfen + zählen (inkl. Freigabe des Eintrags) | 95 ns | 142 ns |
| zum Vergleich: Eintrag erzeugen (außerhalb `record()`) | 880 ns | 1,0 µs |

Writer-Durchsatz (100 000 Einträge, 50 Schlüssel über 3 Minuten, Batch 1000, Zeit bis alles
geschrieben ist):

| Datenbank | Payload `off` | Payload `masked` (~1 KB Request) |
|---|---|---|
| SQLite (Datei) | ≈ 38 800 /s | ≈ 23 300 /s |
| Postgres 17 (Docker, lokal) | ≈ 27 800 /s | ≈ 14 900 /s |

Mehr als genug für das MVP-Ziel; der Writer arbeitet Batches nacheinander ab (Aufbereitung →
DB → Exporter), Pipelining wäre der nächste Schritt, falls nötig.

## 10. Tests

- Einheiten: Maskierung (E-Mail, Luhn, IBAN, Verkürzen, Header in allen Modi, Kürzen, Binär),
  Aggregation je Minute/Schlüssel, Histogramm-Buckets, Prometheus-Text, Konfig + ENV.
- `crates/aigw-observe/tests/observe.rs`: Schreiben bei Batchgröße und nach Intervall,
  Überlauf mit exaktem Drop-Zähler (mit und ohne laufenden Writer, nach Shutdown),
  Shutdown-Flush (1500 Einträge), JSON-Lines-Export ohne DB, Rollup-Szenario inkl. UPSERT über
  Batches, Payload-Ablage und Aufbewahrung gegen SQLite (Speicher, Datei) und Postgres
  (`AIGW_TEST_POSTGRES=docker`).

## 11. Prometheus-Metriken (AIGW-44)

`GET /metrics` der Steuerebene, Textformat 0.0.4 (`text/plain; version=0.0.4`). OpenMetrics
ist (noch) nicht umgesetzt.

### 11.1 Registry

Eigene, schlanke Umsetzung aus Atomics (`aigw_observe::prom::GatewayMetrics`) statt
`prometheus-client`: Die Metrikmenge ist fest, die Labels sind bekannt, und der Hot Path soll
ohne Allokation und ohne Lock auskommen – das lässt sich mit einer zugeschnittenen Struktur
einfacher garantieren als mit einer generischen Family-Map (die je Zugriff einen eigenen
Label-Schlüssel braucht und ein `RwLock` nimmt).

- **Befüllung nur über den `RequestObserver`-Pfad:** `MetricsObserver` sitzt vor dem
  Request-Log (`ObserveHandle::observer()` = Metriken → `RequestLogger`), zählt den
  `RequestRecord` per Referenz und reicht ihn weiter. Einziger neuer Aufruf im Proxy:
  `RequestObserver::request_started()` (Default leer) in `Recorder::new` für
  `aigw_inflight_requests`; zu jedem Aufruf folgt genau ein `record()` (auch bei Abbruch über
  `Drop`).
- **Hot Path:** Hash der Label-Werte (geliehen aus dem Datensatz) → `ArcSwap`-Map
  (`u64` → Einträge mit Vergleich der Rohwerte) → `Relaxed`-Inkremente. Nur die erste Anfrage
  einer neuen Kombination nimmt einen Mutex und veröffentlicht eine neue Map (Copy-on-Write).
- **Kardinalität:** Labels nur `model_group`, `deployment` (ID), `provider`, `ingress`,
  `status_class` (`1xx` … `5xx`) bzw. `type`/`class`/`result`. **Keine** Key-, Team-, Org- oder
  Nutzer-IDs. `model_group` und `deployment` sind auf `max_model_groups` (200) bzw.
  `max_deployments` (500) unterschiedliche Werte begrenzt; weitere Werte zählen unter `other`.
  Die Map-Einträge (inkl. Aliasse auf `other`) sind zusätzlich gedeckelt; darüber nimmt eine
  neue Kombination jedes Mal den langsamen Pfad (nur bei pathologischer Label-Vielfalt).

### 11.2 Metriken

| Metrik | Typ | Labels | Quelle |
|---|---|---|---|
| `aigw_requests_total` | counter | model_group, deployment, provider, ingress, status_class | `RequestRecord` |
| `aigw_request_duration_seconds` | histogram | wie oben | `duration`; Buckets 0,05 … 300 s |
| `aigw_ttft_seconds` | histogram | wie oben | `ttft` (nur Streams); Buckets 0,01 … 30 s |
| `aigw_tokens_total` | counter | model_group, deployment, provider, ingress, type (`input`, `output`, `cache_read`, `cache_write`, `reasoning`) | `usage` (gemeldet oder geschätzt) |
| `aigw_cost_total` | counter | model_group, deployment, provider, ingress | `cost` – Platzhalter bis AIGW-31 |
| `aigw_stream_client_aborts_total` | counter | model_group, deployment, provider, ingress | `stream && client_aborted` |
| `aigw_upstream_errors_total` | counter | class (`ErrorClass`: `rate_limit`, `timeout`, `connect`, `server_error` …) | jeder fehlgeschlagene Upstream-Versuch aus `RequestRecord::attempts` (Retries, Fallbacks) sowie ein Fehler im Stream nach erfolgreichem Versuch |
| `aigw_upstream_attempts_total` | counter | – | alle Upstream-Versuche (F86) |
| `aigw_upstream_retries_total` | counter | – | erneute Versuche in derselben Modellgruppe (aus `attempts`) |
| `aigw_fallbacks_total` | counter | – | Wechsel auf eine Fallback-Modellgruppe |
| `aigw_deployment_breaker_state` | gauge | model_group, deployment (Name) | Circuit Breaker: 0 geschlossen, 1 offen, 2 halboffen (beim Abruf aus dem Routing, höchstens `max_deployments` Serien) |
| `aigw_deployment_inflight_requests` | gauge | model_group, deployment | offene Upstream-Anfragen dieser Instanz |
| `aigw_inflight_requests` | gauge | – | `request_started` − `record` |
| `aigw_snapshot_version` | gauge | – | aktive `config_version` |
| `aigw_snapshot_reload_total` | counter | result (`ok`, `failed`, `check_failed`) | `ReloadMetrics` |
| `aigw_snapshot_stale_seconds` | gauge | – | Zeit seit dem ersten fehlgeschlagenen Abgleich (0 = aktuell) |
| `aigw_snapshot_build_seconds` | gauge | – | Dauer des letzten Neubaus |
| `aigw_counter_backend_degraded` | gauge | – | `CounterBackend::health()` über `GatewayMetrics::set_counter_health`; erscheint erst, wenn das Zähler-Backend im Binary verdrahtet ist (Limits) |
| `aigw_log_*` | counter | – | Request-Log (§7) |
| `aigw_otlp_spans_exported_total`, `aigw_otlp_spans_dropped_total`, `aigw_otlp_logs_exported_total`, `aigw_otlp_export_errors_total{signal}`, `aigw_otlp_export_retries_total{signal}` | counter | – | OTLP-Export (§12) |

Tokens, Kosten und Abbrüche werden beim Ausgeben über die Statusklassen summiert.

### 11.3 Kosten im Hot Path

Buildbox (x86-64, 8 vCPU), Release, `cargo bench -p aigw-observe --bench record` (divan):

| Fall | Median | Mittel |
|---|---|---|
| nur Metriken (`GatewayMetrics::observe`, Serie existiert) | 234 ns | 251 ns |
| nur Einreihen ins Request-Log | 212 ns | 280 ns |
| **gesamt** (`request_started` + Metriken + Einreihen) | **316 ns** | 370 ns |

Ziel < 1 µs gesamt eingehalten.

## 12. OpenTelemetry: Traces und Logs (AIGW-45)

### 12.1 Umsetzung und Wire-Format

- **OTLP/HTTP mit Protobuf** (`POST {endpoint}/v1/traces` bzw. `/v1/logs`,
  `content-type: application/x-protobuf`) statt gRPC: wird von allen Collectoren angenommen
  (Port 4318), braucht kein `tonic`/gRPC-Framing und nutzt denselben hyper-util-Client mit
  rustls/aws-lc-rs wie die Upstreams. Klartext-HTTP ist für einen Collector im selben Netz
  bzw. als Sidecar erlaubt.
- **Eigene Umsetzung ohne OpenTelemetry-SDK:** ein kleiner Protobuf-Encoder für genau die
  gesendeten Nachrichten (`otel/proto.rs`, Feldnummern nach `opentelemetry-proto` v1) und ein
  `tracing`-Layer (`otel/trace.rs`). Keine neuen Crates im Abhängigkeitsbaum (hyper,
  hyper-util, hyper-rustls, rustls, tracing-subscriber, arc-swap sind bereits
  Workspace-Abhängigkeiten). Mit `opentelemetry` + `opentelemetry_sdk` + `opentelemetry-otlp`
  + `tracing-opentelemetry` kämen u. a. `opentelemetry-proto`, `prost` und `reqwest` bzw.
  `tonic` hinzu.
- **Kein Export ohne Konfiguration:** `observe.otlp.endpoint` leer oder `traces.enabled: false`
  → der Layer ist eingehängt, aber inaktiv; die Trace-Callsites haben dann das Interesse
  `never` (zusammen mit dem Log-Filter `aigw::otel=off`), der Proxy erhält `Span::none()`.

### 12.2 Spans

Instrumentiert wird nur über `tracing` mit `target: "aigw::otel"`
(`aigw_core::observe::TRACE_TARGET`); Proxy und Adapter kennen kein OpenTelemetry. Besondere
Felder: `otel.name`, `otel.kind`, `otel.status_code`, `otel.status_message`, am Wurzel-Span
`otel.traceparent` (eingehender Header, wird nicht als Attribut exportiert).

```
chat {model}               server   je Anfrage (aigw-proxy), bis zum letzten Byte bzw. Abbruch
├── auth                   internal Schlüsselprüfung (Fehler → Status ERROR, error.type)
├── route                  internal Modell → Route nachschlagen (aigw.model_group); die
│                                   Deployment-Wahl je Versuch steckt im Routing (AIGW-24 ff.)
├── plugins                internal Pre-Plugins (aigw.plugins.phase = request, aigw.plugins.count;
│                                   Reject → Status ERROR), siehe 12-plugins.md
├── chat {provider_model}  client   je Upstream-Versuch inkl. Retries/Fallbacks
│                                   (aigw-providers; bei Streams bis zu
│                                   den Antwort-Headern): server.address/port,
│                                   http.response.status_code, error.type (ErrorClass)
├── plugins                internal Post-Plugins (aigw.plugins.phase = response), nur wenn die
│                                   Kette Antwort-Haken hat; Stream-Haken ohne eigenen Span
└── Ereignis first_token            bei Streams, Attribut aigw.ttft_ms
```

Attribute des Wurzel-Spans (GenAI-Semantic-Conventions): `gen_ai.operation.name` (`chat`),
`gen_ai.system` (`openai`, `anthropic`, `az.ai.openai`, `aws.bedrock`, `gcp.vertex_ai`,
`mistral_ai`, sonst der Anbieter-Bezeichner) und zusätzlich der Nachfolger
`gen_ai.provider.name` (F101; gleiche Werte, Azure als `azure.ai.openai`), `gen_ai.request.model`,
`gen_ai.request.max_tokens`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`,
`gen_ai.usage.output_tokens`, `http.response.status_code`, `error.type`, dazu
`aigw.request_id`, `aigw.ingress`, `aigw.model_group`, `aigw.deployment` (zuletzt
verwendetes), `aigw.attempts`, `aigw.fallback`, `aigw.stream`, `aigw.ttft_ms`,
`aigw.usage_estimated`, `aigw.client_aborted`. Keine Prompt- oder
Antwortinhalte in Spans.

### 12.3 Kontext und Sampling

- **Eingehend:** Ein gültiger W3C-`traceparent` (Version 00; `ff`, Null-IDs und Großbuchstaben
  werden verworfen) liefert Trace-ID und Eltern-Span (`accept_traceparent`).
- **Sampling** (Kopf-Sampling am Wurzel-Span, gilt für alle Kind-Spans): Mit übernommenem
  Kontext und `parent_based: true` entscheidet dessen Flag `sampled`; sonst
  `sampling_ratio`, deterministisch nach Trace-ID (untere 63 Bit, wie `TraceIdRatioBased`).
  Nicht gesampelte Spans tragen nur ihren Kontext und werden nicht exportiert.
- **Ausgehend** (`propagate: true`): Der Adapter setzt `traceparent` mit der Span-ID des
  Upstream-Versuchs und dem Sampling-Flag (`aigw_core::observe::current_traceparent`) – je
  Anbieterart (F98): standardmäßig nur an selbst betriebene Ziele (`vllm`, `ollama`,
  `openai-compatible`), nicht an externe Clouds (OpenAI, Azure, Anthropic, Bedrock, Vertex,
  Mistral). Überschreibbar je Deployment bzw. Modellgruppe mit
  `settings.propagateTraceContext: true|false`; `propagate: false` schaltet global ab.
- **Client-Request-ID** (F11): `x-client-request-id` (sonst `x-request-id`) des Clients steht als
  `aigw.client_request_id` am Wurzel-Span und in `request_log.client_request_id`.
- **Export:** fertige Spans per `try_send` in einen begrenzten Kanal (`queue_capacity`); voll →
  verwerfen + `aigw_otlp_spans_dropped_total`. Ein Task sendet Batches (`batch_size`,
  spätestens nach `flush_interval_ms`); Fehler werden gezählt und geloggt.
  Beim Herunterfahren wird der Puffer noch exportiert (höchstens 5 s).
- **Wiederholung (F102, Traces und Logs):** `429`, `502`, `503`, `504`, Verbindungsfehler und
  Zeitlimit werden mit exponentiellem Backoff (Jitter 50–100 %) wiederholt, höchstens
  `observe.otlp.retry.max_attempts` (Standard 4) Versuche, Wartezeit
  `initial_backoff_ms` (500) bis `max_backoff_ms` (5000). `Retry-After` (Sekunden) wird
  beachtet; verlangt der Collector mehr als `max_backoff_ms` (oder ein HTTP-Datum), wird der
  Batch verworfen statt den Export (bei Logs: den Request-Log-Writer) zu blockieren. Zähler
  `aigw_otlp_export_retries_total{signal}`. Andere 4xx/5xx werden nicht wiederholt.
- **HTTP:** gemeinsamer Outbound-Baustein (`aigw-outbound`, 06 §4.1) mit den Vertrauensankern
  aus `upstream.*`, ohne Adressfilter (Collector = Startkonfiguration), keine Weiterleitungen.

### 12.4 Logs

`OtlpLogExporter` (implementiert `LogExporter`, aktiv mit `observe.otlp.logs.enabled`): ein
`LogRecord` je Anfrage aus dem aufbereiteten Request-Log-Batch, `event_name = aigw.request`,
Schweregrad nach Status (`INFO` < 400, `WARN` 4xx, `ERROR` 5xx), Attribute wie §12.2 bzw.
`aigw.*` (Latenz, TTFT, Cache, Key-/Team-/Org-ID, `client.address`). Body = maskierter
Payload als JSON-Text nur mit `logs.include_payload: true`. Fehler zählen wie bei allen
Exportern in `aigw_log_export_errors_total` und zusätzlich in
`aigw_otlp_export_errors_total{signal="logs"}`.

### 12.5 Tests

- `crates/aigw-observe/tests/otlp.rs`: Fake-Collector (axum) mit unabhängigem
  Protobuf-Decoder; Traces durch den echten Proxy mit `Upstream` gegen einen Fake-OpenAI-Server
  (Wurzel- und Kind-Spans, GenAI-Attribute, TTFT-Ereignis, eingehender `traceparent` gesampelt
  und nicht gesampelt, Weitergabe an den Upstream, Fehlerstatus, abgeschaltete Übernahme und
  Weitergabe), Request-Log als OTLP-Logs, Fehlerzählung bei HTTP 500.
- `crates/aigw-observe/tests/otlp.rs` (F102): 503 mit `Retry-After` + 429 → Erfolg im dritten
  Versuch; `Retry-After` über `max_backoff_ms` → sofort verworfen; dauerhaft 503 → genau
  `max_attempts` Versuche; 400 → keine Wiederholung.
- `crates/aigw-observe/tests/otelcol.rs` (F103): **Gegentest gegen den offiziellen Collector**
  `otel/opentelemetry-collector-contrib:0.165.0` (`AIGW_TEST_OTELCOL=docker scripts/check.sh`;
  OTLP/HTTP-Empfänger, file-Exporter als OTLP-JSON). Geprüft wird, was der Collector dekodiert
  hat: Ressource (`service.name`, `telemetry.sdk.name`), Span-Baum (Trace-ID, Eltern-IDs inkl.
  übernommenem `traceparent`, Art SERVER/CLIENT, Kind-Spans auth/route/Upstream-Versuch),
  GenAI-Attribute (`gen_ai.system` + `gen_ai.provider.name`, Usage, Status), Log-Eintrag mit
  `eventName = aigw.request`, Schweregrad und Attributen. Ohne Schalter übersprungen.
- `crates/aigw-observe/tests/otel_callsite.rs`: Callsites abgeschaltet, solange inaktiv.
- `scripts/runtime-smoke.sh`: Binary mit lokalem Fake-Collector (python3) – Traces und Logs
  kommen als Protobuf an; `/metrics` mit Gateway-, Snapshot- und OTLP-Metriken.

## 13. Dashboards und Log-Explorer (AIGW-46)

Lese-API: [08-admin-api.md](08-admin-api.md) §5.11. Hier: Datenbasis, Aggregation, Messwerte.

### 13.1 Datenbasis (Migration `0005_analytics.sql`)

- **Stunden-Rollups** `usage_rollup_hour`: gleiche Spalten und gleicher Schlüssel wie
  `usage_rollup`, Bucket = Stundenbeginn (UTC). Der Writer schreibt im **selben Batch** (eine
  Transaktion) zusätzlich die je Stunde zusammengefassten Deltas als additiven UPSERT –
  `aigw_store`-intern, der Writer in `aigw-observe` bleibt unverändert. Bestehende
  Minuten-Rollups übernimmt die Migration. Aufbewahrung wie Minuten-Rollups (gelöscht wird
  eine Stunde erst, wenn sie ganz vor der Frist liegt).
- **TTFT-Histogramm** `ttft_le_25` … `ttft_inf` in beiden Rollup-Tabellen (Grenzen wie
  Latenz, nur Anfragen mit TTFT, d. h. Streams). Ältere Zeilen haben 0 → `ttft_p95 = null`.
- `request_log.attempts` (JSON-Array der Upstream-Versuche: Modellgruppe, Deployment,
  Fehlerklasse, Upstream-Status, Dauer) und `request_log.client_request_id`
  (`x-request-id`/`x-client-request-id` des Clients; nur sichtbares ASCII, höchstens 128
  Zeichen). Ältere Einträge: leer.
- `request_log.parent_request_id` (F191, AIGW-73: Spalte, Index und Store-Filter
  `LogQuery::parent_request_id`): auslösende Anfrage, z. B. das Embedding des semantischen
  Caches. Noch ungefüllt – der Writer setzt sie, sobald `RequestRecord` das Feld trägt.
- Indizes: `usage_rollup (api_key_id|organization_id, bucket_start)`, je Dimension auf
  `usage_rollup_hour`, `request_log (organization_id|model_group|deployment_id, at)` und
  `request_log (client_request_id)`.

### 13.2 Abfrage

`Store::query_rollups` zerlegt `[from, to)` in Abschnitte: ganze Stunden aus
`usage_rollup_hour`, angebrochene Stunden am Rand aus `usage_rollup`. Bei Schritten unter
einer Stunde (`1m`, `5m`) nur Minuten. Jeder Abschnitt ist **eine** `GROUP BY`-Abfrage
(`(bucket_start / step) * step`, Gruppenspalte, `SUM` je Zählspalte); die Teilergebnisse
addiert der Store. Ein Dashboard über 7 Tage liest damit ≈ 168 statt ≈ 10 000 Minuten je
Schlüssel.

- **Zeitraum:** Standard die letzten 24 h; höchstens 400 Tage. Ränder werden auf Minuten,
  bei Zeitreihen auf `step` ausgerichtet (an der Epoche, d. h. Tage in UTC).
- **Schrittwahl:** ohne `step` der kleinste aus `1m`, `5m`, `1h`, `1d` mit höchstens 300
  Punkten (1 h → `1m`, 24 h → `5m`, 7 Tage → `1h`, 30 Tage → `1d`); ein expliziter `step`
  darf höchstens 1500 Punkte ergeben (sonst `400`). Punkte sind lückenlos (Zähler 0,
  Quoten/Latenzen `null`).
- **Top-N + „other“:** erst Gesamtsummen je Gruppe im Zeitraum (Rang), dann die Zeitreihe mit
  `CASE WHEN spalte IN (…) THEN … ELSE '__other__' END` als Gruppe – die Datenbank liefert
  höchstens N + 1 Gruppen je Bucket, auch bei Tausenden Keys. Rang: summierbare Kennzahlen
  (Anfragen, Tokens, Kosten, Fehler) nach dem Wert, Quoten und Latenzen nach Anfragen.
- **Anbieter** stehen nicht in den Rollups: Gruppierung und Filter bilden Deployments über die
  **aktuelle** Konfiguration auf Anbieter ab (gelöschte Deployments → `unknown`). Im Rohlog
  steht der Anbieter direkt.
- **Kennzahlen:** `requests`, `tokens` (Eingabe + Ausgabe), `input_tokens`, `output_tokens`,
  `cost`, `errors`, `error_rate`, `cache_hit_rate` (Treffer im Antwort-Cache / Anfragen),
  `latency_avg`, `latency_p50|p95|p99`, `ttft_p95`.
- **KPIs** (`summary`): Zeitraum und gleich lange Vorperiode direkt davor; `deltaPercent` =
  (Wert − Vorwert) / Vorwert · 100, auch für Quoten (relativ, nicht Prozentpunkte); `null`
  ohne Vorwert.

### 13.3 Perzentile aus Histogrammen

Wie Prometheus `histogram_quantile` (`aigw_store::analytics::histogram_quantile`): Rang
`q · n`, erster Bucket mit kumulierter Anzahl ≥ Rang, darin **linear interpoliert** zwischen
Unter- und Obergrenze (Annahme: gleichverteilt; der erste Bucket beginnt bei 0 ms). Fällt der
Rang in den `+Inf`-Bucket, ist das Ergebnis 60 000 ms (Untergrenze). Die Genauigkeit ist durch
die Bucket-Grenzen beschränkt (z. B. p95 irgendwo in 250–500 ms → Fehler bis ±125 ms). Über
Gruppen und Zeit wird erst summiert, dann das Quantil gebildet (exakt bezüglich der Buckets,
kein Mitteln von Perzentilen).

### 13.4 Messwerte

Buildbox (x86-64, 8 vCPU), Release, `AIGW_ANALYTICS_PERF=1 cargo test --release -p aigw-admin
--test analytics_perf -- --nocapture --test-threads 1`: **1 Mio. Minuten-Rollups** über 7 Tage
(100 Keys, 12 Teams, 6 Modellgruppen, 12 Deployments; ≈ 100 Zeilen je Minute) und **500 000
Rohlogs**, Antwortzeit über HTTP inkl. Rechteprüfung und Namensauflösung, Median aus 5 Läufen.
Postgres 17 im Docker-Container auf derselben Box.

| Abfrage | SQLite (Datei) | Postgres 17 |
|---|---|---|
| `summary` 7 Tage (mit Vorperiode) | 42 ms | 31 ms |
| `timeseries` 7 Tage `requests` (auto → 1h) | 12 ms | 12 ms |
| `timeseries` 7 Tage `cost`, `groupBy=model_group` | 32 ms | 37 ms |
| `timeseries` 7 Tage `latency_p95`, `groupBy=key` (Top 5 von 100) | 63 ms | 48 ms |
| `timeseries` 7 Tage `ttft_p95`, `groupBy=provider` | 56 ms | 34 ms |
| `top` 7 Tage Keys nach Kosten | 20 ms | 19 ms |
| `summary` / `timeseries groupBy=team` 7 Tage, team-scoped viewer | 7 / 8 ms | 13 / 17 ms |
| `timeseries` 24 h `requests` (auto → 5m, nur Minuten-Rollups, ≈ 144 000 Zeilen) | **380 ms** | 79 ms |
| `request-logs` Seite 1 / Seite 2 (Cursor) | 1,8 / 1,6 ms | 7,0 / 3,6 ms |
| `request-logs?status=error` / `modelGroup` + 24 h / `clientRequestId` | 6,7 / 1,7 / 0,8 ms | 5,2 / 3,6 / 2,8 ms |

**Nachmessung AIGW-73** (getrennte Treiber, Postgres mit `UUID`/`TIMESTAMPTZ`, F7): gleiche
Messung, alter Stand (vor AIGW-73) und neuer Stand direkt nacheinander auf der geteilten,
ausgelasteten Buildbox (Absolutwerte daher nur untereinander vergleichbar), Median aus 5 Läufen:

| Abfrage | SQLite alt → neu | Postgres alt → neu |
|---|---|---|
| `summary` 7 Tage | 28,1 → 29,1 ms | 17,5 → 21,1 ms |
| `timeseries` 7 Tage `requests` (1h) | 7,5 → 6,8 ms | 5,1 → 7,6 ms |
| `timeseries` 7 Tage `cost`, `groupBy=model_group` | 30,5 → 27,8 ms | 26,8 → 24,0 ms |
| `timeseries` 7 Tage `latency_p95`, `groupBy=key` | 38,9 → 37,1 ms | 30,1 → 26,8 ms |
| `timeseries` 7 Tage `ttft_p95`, `groupBy=provider` | 27,9 → 27,9 ms | 25,3 → 22,0 ms |
| `top` 7 Tage Keys nach Kosten | 11,6 → 11,7 ms | 13,9 → 10,1 ms |
| `timeseries` 24 h `requests` (5m) | 317,9 → 309,1 ms | 52,8 → 51,8 ms |
| `summary` / `timeseries groupBy=team`, team-scoped | 7,7 / 9,1 → 8,0 / 9,3 ms | 7,5 / 6,6 → 6,1 / 6,6 ms |
| `request-logs` Seite 1 / Seite 2 | 1,5 / 1,7 → 2,0 / 2,3 ms | 2,5 / 2,7 → 2,3 / 2,4 ms |
| `request-logs?status=error` / `modelGroup` + 24 h / `clientRequestId` | 2,2 / 1,7 / 0,7 → 5,0 / 1,8 / 1,5 ms | 4,6 / 3,1 / 1,9 → 3,4 / 2,6 / 2,0 ms |

Ergebnis: im Rahmen der Messschwankung unverändert (Läufe derselben Version streuen auf der
geteilten Box um ±30 %). Postgres gruppiert Zeitreihen nach dem Roh-Bucket (`bucket_start`)
und fasst auf den Schritt im Speicher zusammen; ein erster Versuch mit `date_bin(…)` je Zeile
war bei 24 h/5 min deutlich langsamer (84 statt 56 ms).
| `request-logs` team-scoped | 1,4 ms | 3,5 ms |

Ziel < 200 ms für Dashboard-Abfragen über 7 Tage: eingehalten (Stunden-Rollups). Ohne
Stunden-Rollups lagen dieselben 7-Tage-Abfragen bei rund 0,5–1 s. Summiert werden nur die
Spalten, die die Kennzahl braucht (`RollupQuery::columns`; Zeitreihe 24 h vorher 650 ms
SQLite / 520 ms Postgres). Offen: Schritte unter einer Stunde über 24 h auf SQLite (380 ms) –
möglich wäre eine dritte Stufe mit 5-Minuten-Rollups. Schreiben (Seed): 1 Mio. Rollups in
16 s (SQLite) bzw. 43 s (Postgres) inkl. Stunden-Rollups.

### 13.5 Tests

- `crates/aigw-admin/tests/analytics_api.rs` (SQLite Speicher/Datei, Postgres): Zeitreihen über
  Stunden- und Minuten-Rollups inkl. angebrochener Stunden und doppelt geschriebener Minute,
  Schrittwahl, Kosten exakt, KPIs mit Vorperiode, Top-N + `__other__`, Perzentile, TTFT,
  Anbieter, Rangliste mit Anteil, Fehlerfälle; Geltungsbereiche (team-scoped viewer, org-admin,
  globale Rollen; `403` bei fremdem Team/Key); Log-Explorer mit Cursor (inkl. gleicher
  Zeitpunkte), allen Filtern, Namen, Versuchen; Payload nur `admin`/`auditor`
  (`payloadStatus`, `403`/`404` der Payload-Route); Aufbewahrung der Stunden-Rollups.
- Einheiten: Abschnittszerlegung, Interpolation, Schrittwahl, Status-/Cursor-Parser,
  `ReadPayload` in `rbac.rs`.
- Last: `crates/aigw-admin/tests/analytics_perf.rs` (nur mit `AIGW_ANALYTICS_PERF=1`).
