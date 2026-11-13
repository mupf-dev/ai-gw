# ai-gw – Beobachtung: Request-Log, Rollups, Payload-Logging (AIGW-43)

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
| Tabellen, Batch-Schreiben, Löschen, Lesen | `aigw_store::observe`, Migration `0002_observe.sql` |

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
- **Maskierung** über den Trait `PayloadMasker` (Schnittstelle für die PII-Engine AIGW-37,
  einsetzbar per `ObserveBuilder::masker`). Basis `RegexMasker`:
  - E-Mail → `[EMAIL]`,
  - Kreditkarte: 13–19 Ziffern (Leerzeichen/Bindestrich erlaubt), erste Ziffer 2–6, Luhn → `[CARD]`,
  - IBAN: Ländercode + Prüfziffern + 11–30 Zeichen, mod 97 → `[IBAN]`.
  Gierige Treffer werden von hinten verkürzt, bis die Prüfung passt (z. B.
  „4111 1111 1111 1111 22“ → „[CARD] 22“).
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
  `RequestLogRow` + optional `PayloadRow`. OTLP-Logs (AIGW-45) und Webhook implementieren denselben
  Trait.
- `JsonLinesExporter`: eine JSON-Zeile je Anfrage (Felder wie `request_log` in camelCase,
  optional `payload`), angehängt an `observe.jsonl.path`, Flush je Batch, Schreiben per
  `spawn_blocking`.
- Zähler (`ObserveMetrics::render_prometheus`, `GET /metrics` der Steuerebene): `aigw_log_dropped_total`,
  `aigw_log_written_total`, `aigw_log_write_failed_total`, `aigw_log_batches_total`,
  `aigw_log_export_errors_total`, `aigw_log_purged_total`.

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
```

ENV wie üblich, z. B. `AIGW_OBSERVE__PAYLOAD=masked`, `AIGW_OBSERVE__RETENTION__PAYLOAD_DAYS=3`.

Start im Binary (`crates/aigw/src/runtime.rs` → `observe::start`, Shutdown in
`Runtime::shutdown` nach dem Schließen der Listener; `/metrics` auf der Steuerebene):

```rust
let observe = ObserveBuilder::new(cfg.observe.clone()).store(store.clone()).start();
let observer: Arc<dyn RequestObserver> = observe.observer();   // → ProxyState
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
