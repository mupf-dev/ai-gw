# ai-gw – Antwort-Cache (AIGW-39, AIGW-40)

Stand: 2026-11-13 · Status: umgesetzt (exakt, semantisch mit eigenem HNSW, pgvector, Redis Stack),
Grundlage: [01-architektur.md](01-architektur.md) §3, §10 · Code: `crates/aigw-cache`,
`crates/aigw-proxy/src/cache.rs`, Plugin `cache` in `crates/aigw-plugins/src/builtin/cache.rs`

## 1. Einordnung: Pipeline-Schritt statt Plugin-`Respond`

```
Auth ─► Modellfreigabe ─► Limits ─► Pre-Plugins ─► [6 Cache] ─Treffer─┐
                                                       │ Miss          │
                                                       ▼               ▼
                                              Routing/Upstream ─► Post-/Stream-Plugins ─► Encoder
                                                       │ (rohe Antwort speichern)
```

Der Cache ist ein eigener Schritt in `aigw-proxy::handlers::chat_inner` (Schritt 6, nach allen
Pre-Plugins). Das Plugin `cache` hat **keine Haken**; es trägt nur die Richtlinie und nutzt die
Bindungs-Vererbung der Plugin-Kette (global → Org → Team → Key → Modellgruppe, `override`,
`locked`). Begründung gegenüber einem Plugin mit `Flow::Respond`:

| Punkt | Plugin-`Respond` | Pipeline-Schritt (gewählt) |
|---|---|---|
| Pre-Plugins (Guardrails, PII, `request-transform`) | Treffer überspringt alle Plugins mit höherer `order` – ein Prompt-Filter könnte umgangen werden | laufen immer vollständig |
| Schlüssel | hängt von der `order` ab | immer über die Anfrage, die an den Upstream ginge |
| Ausgabe-Plugins (Ausgabe-Guardrails, Rückübersetzung von Pseudonymen) | laufen bei `Respond` nicht | laufen auf Treffern wie auf Upstream-Antworten |
| Semantischer Cache | bräuchte Zugriff auf das Routing (Embedding-Modellgruppe) | Proxy stellt den `Embedder` bereit |
| Abrechnung/Log | nur über Usage im Plugin-Ergebnis | Recorder: Usage 0, `RequestRecord.cache`, Modellgruppe |

Gespeichert wird die **rohe Upstream-Antwort** (vor den Post-Plugins). Ohne Bindung kostet der
Schritt nur das Durchsuchen der ohnehin geladenen Kette.

## 2. Richtlinie (Plugin `cache`)

```yaml
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: team-cache}
spec:
  scope: {level: team, target: research}
  plugin: cache
  config:
    inherit: override              # Kettensteuerung (siehe 12-plugins.md)
    mode: semantic                 # exact (Standard) | semantic (erst exakt, dann semantisch)
    scope: team                    # key | team (Standard) | organization | global (Warnung im Log)
    ttlSecs: 3600
    clientControl: true            # Cache-Control / x-aigw-cache-ttl auswerten
    clientMaxTtlSecs: 86400
    cacheNonZeroTemperature: false # Standard: nur temperature == 0
    cacheToolCalls: false
    semantic:
      threshold: 0.95              # Kosinus-Ähnlichkeit
      thresholdByGroup: {gpt-4o: 0.97}
      embeddingModel: text-embedding-3-small   # Modellgruppe des Gateways
      maxChars: 8000
```

Mehrere `cache`-Bindungen in einer Kette (`inherit: append`): die **letzte** in
Ausführungsreihenfolge gilt. Die Schwelle je Modellgruppe geht über `thresholdByGroup` oder eine
Bindung auf Modellgruppen-Ebene.

**Scope** (nie mandantenübergreifend außer ausdrücklich `global`): `key` = Key-ID, `team` =
Team-ID (Key ohne Team → Key), `organization` = Org-ID (sonst Team, sonst Key). Fehlt eine
Ebene, gilt die nächst engere, nie eine weitere.

## 3. Exakter Cache (AIGW-39)

**Schlüssel** = BLAKE3(`"aigw-cache/exact/v1"` ‖ Scope ‖ Modellgruppe ‖ kanonisches JSON).
Normalisiert wird die IR-Anfrage nach allen Pre-Plugins: entfernt `model` (die kanonische
Gruppe steht im Schlüssel, Aliasse teilen Einträge), `user`, `metadata`, `stream`,
`stream_options`, `source` (OpenAI- und Anthropic-Eingang teilen Einträge), Format-Hinweise
außer `anthropic_beta`, `cache_control` an Teilen und Tools. Kanonisches JSON: Objektschlüssel
rekursiv sortiert, Zahlen in `serde_json`-Normalform – unabhängig von der Reihenfolge im
Client-Body.

**Speicher:** RAM-LRU mit Byte-Grenze (`cache.memory_max_bytes`, 16 Shards mit eigenem Mutex,
TTL je Eintrag), optional Redis/Valkey als gemeinsame zweite Ebene (`cache.redis_url`,
`SET … PX`). Ein Redis-Treffer wird für 60 s in den RAM übernommen.

**Was gecacht wird:**

| Regel | Standard | Steuerung |
|---|---|---|
| Temperatur | nur `temperature == 0` (fehlende Angabe = Anbieter-Standard > 0) | `cacheNonZeroTemperature` |
| Tool-Aufrufe in der Antwort | nicht speichern | `cacheToolCalls` |
| Fehler, Abbruch, Stream-Fehlerereignis, Endgrund `content_filter`/`refusal`/`pause_turn` | nicht speichern | – |
| Größe | ≤ `cache.max_entry_bytes` (1 MiB) | Konfiguration |

**Client-Steuerung** (`clientControl: true`): `Cache-Control: no-cache` = nicht nachschlagen,
frische Antwort speichern (`bypass`); `no-store` = nachschlagen, nicht speichern; beides =
umgehen; `x-aigw-cache-ttl: <s>` setzt die TTL (0 = nicht speichern, gedeckelt durch
`clientMaxTtlSecs` und `cache.max_ttl_secs`).

**Streams:** Gestreamte Antworten werden unterwegs zur IR-Antwort zusammengesetzt
(`StreamCollector`, vor den Stream-Plugins) und nur bei regulärem Ende gespeichert. Ein Treffer
wird im **Eingangsformat** wiedergegeben – gestreamt mit simuliertem Chunking
(`cache.replay_chunk_chars`, Standard 32 Zeichen, an Zeichengrenzen), nicht gestreamt als JSON.
Ein gestreamt gespeicherter Eintrag bedient auch nicht gestreamte Anfragen und umgekehrt.

**Header** `x-aigw-cache: hit-exact | hit-semantic | miss | bypass` (nur wenn eine Bindung
gilt; das Plugin `header` kann ihn entfernen).

**Abrechnung:** Ein Treffer ruft keinen Upstream auf: `RequestRecord.usage` = 0, keine Kosten,
Token-Reservierung der Limits wird vollständig freigegeben; die Anfrage zählt in Request-Limits,
Log (`request_log.cache`) und Rollup (`cache_hits`). Der Body enthält die **ursprüngliche**
Usage der gecachten Antwort (siehe offene Fragen).

## 4. Semantischer Cache (AIGW-40)

1. Nur bei `mode: semantic`, wenn ein Vektor-Backend und ein Embedder verfügbar sind und die
   **letzte** Nachricht eine reine Text-Nutzernachricht ist (≤ `maxChars`).
2. Erst exakt; bei Miss Embedding der letzten Nutzernachricht über die Embedding-Modellgruppe
   (`semantic.embeddingModel` bzw. `cache.semantic.embedding_model`) – internes Routing des
   Gateways, kein externer Direktaufruf; Zeitlimit `embed_timeout_ms`.
3. **Harter Filter (Partition)** = BLAKE3 über Scope, Modellgruppe und die normalisierte Anfrage
   **ohne** die letzte Nutzernachricht: Systemprompt, Tools, Tool-Wahl, Antwortformat,
   Sampling-Parameter und der bisherige Verlauf. Gesucht wird nur innerhalb der Partition.
4. Suche k = 1 mit Mindest-Kosinus-Ähnlichkeit; das Embedding wird beim Miss zum Speichern
   wiederverwendet (ID = exakter Schlüssel, erneutes Speichern ersetzt).
5. Ausfall von Embedding oder Vektor-Backend = Miss (Metrik, Debug-Log), nie ein Fehler.

### VectorIndex-Trait

```rust
#[async_trait]
pub trait VectorIndex: Send + Sync {
    fn name(&self) -> &'static str;
    async fn insert(&self, e: VectorEntry) -> Result<(), IndexError>;          // ersetzt gleiche ID
    async fn search(&self, q: VectorQuery<'_>) -> Result<Vec<VectorHit>, IndexError>; // k, threshold, Partition
    async fn evict(&self, what: Evict) -> Result<u64, IndexError>;             // Id | Partition | Expired
}
```

| Backend | Eigenschaften |
|---|---|
| `memory` (Standard) | eigener HNSW je Partition im RAM (Kosinus, `m`/`ef_construction`/`ef_search`); Ersetzen/Löschen über Grabsteine, Neuaufbau einer Partition, sobald Grabsteine > lebende Knoten (und > 64); TTL bei der Suche + Aufräum-Task (60 s); Grenzen `max_entries`/`max_bytes` (älteste zuerst verdrängt); Warmstart aus Tabelle `cache_vector` (Migration 0007, Vektor + Antwort AES-256-GCM-verschlüsselt mit dem Master-Key, AAD = ID), asynchron gebündelt geschrieben |
| `pgvector` | Postgres + Erweiterung `vector`; Tabelle `aigw_semantic_cache_<dim>` mit HNSW-Index (`vector_cosine_ops`), vom Backend selbst angelegt (`CREATE EXTENSION` braucht ggf. erhöhte Rechte); Suche mit `SET LOCAL hnsw.ef_search`, ab pgvector 0.8 `hnsw.iterative_scan = relaxed_order` gegen zu wenige Treffer nach dem Partitionsfilter; Verbindung verzögert, Zeitlimit `backend_timeout_ms` |
| `redis` | Redis Stack / Valkey Search: `FT.CREATE … VECTOR HNSW … DISTANCE_METRIC COSINE`, Partition als TAG, `FT.SEARCH … =>[KNN k @v $B EF_RUNTIME ef AS score]`; TTL nativ (`PEXPIREAT`) |

Für `pgvector`/`redis` ist `cache.semantic.dimensions` Pflicht (Spaltentyp bzw. Index).

### Eigener HNSW

`aigw_cache::hnsw`: Ebenen `⌊−ln U · 1/ln M⌋`, Ebene 0 mit `2M` Nachbarn, Nachbarwahl mit der
Heuristik aus dem Paper (wie hnswlib), Besuchslisten mit Generationszähler aus einem Pool,
Skalarprodukt mit acht Akkumulatoren (Autovektorisierung). Suchen parallel (`RwLock` je
Partition), Einfügen exklusiv je Partition.

Messung: `cargo run --release -p aigw-cache --example hnsw_recall` (Buildbox AMD EPYC Genoa,
8 vCPU geteilt, 1 Thread je Suche, 100 000 Vektoren × 768 Dim., 500 Anfragen, k = 10, M = 16,
efConstruction = 200; Rohwerte in
[messwerte/14-cache-hnsw-2026-11-13.txt](messwerte/14-cache-hnsw-2026-11-13.txt)):

| Daten | efSearch | Recall@10 | Recall@1 | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| geclustert (embedding-ähnlich) | 32 | 0,924 | 0,928 | 256 µs | 471 µs |
| geclustert | **64 (Standard)** | **0,987** | 0,988 | 359 µs | **651 µs** |
| geclustert | 128 | 1,000 | 1,000 | 495 µs | 721 µs |
| isotrop gleichverteilt (Worst Case) | 256 | 0,176 | 0,214 | 3,4 ms | 4,1 ms |

Abnahme (Recall ≥ 0,95 bei 100k × 768) ist für embedding-ähnliche Daten ab `ef_search` 64
erfüllt. Isotrope Zufallsvektoren in 768 Dimensionen haben praktisch keine „nächsten“
Nachbarn (alle Ähnlichkeiten ≈ 0) – dort scheitert jeder Graph-Index; für den Cache ist nur
der Treffer oberhalb der Schwelle (≥ 0,9) relevant, der in diesem Fall gar nicht existiert.
Aufbau: ≈ 0,95 ms je Vektor (single-threaded, efConstruction = 200); der Cache fügt einzeln im
Hintergrund ein.

## 5. Konfiguration (`cache`)

```yaml
cache:
  enabled: true
  memory_max_bytes: 268435456     # RAM-LRU exakt
  max_entry_bytes: 1048576
  max_ttl_secs: 604800
  redis_url: redis://redis:6379/0 # optional, exakte zweite Ebene
  redis_key_prefix: "aigw:cache:"
  backend_timeout_ms: 250         # Redis/pgvector; Überschreitung = Miss
  replay_chunk_chars: 32
  semantic:
    backend: memory               # memory | pgvector | redis
    embedding_model: text-embedding-3-small
    dimensions: 1536              # Pflicht für pgvector/redis
    embed_timeout_ms: 2000
    max_entries: 100000
    max_bytes: 536870912
    hnsw: {m: 16, ef_construction: 200, ef_search: 64}
    persist: true                 # memory: Warmstart aus der DB (verschlüsselt)
    pgvector_url: postgres://…    # Standard: store.url, falls Postgres
    redis_url: redis://…          # Standard: cache.redis_url
```

ENV-Overrides wie üblich, z. B. `AIGW_CACHE__SEMANTIC__HNSW__EF_SEARCH=128`.

## 6. Kennzahlen

`/metrics`: `aigw_cache_lookups_total{result}`, `aigw_cache_stores_total{tier}`,
`aigw_cache_store_skipped_total{reason}`, `aigw_cache_backend_errors_total{backend}`,
`aigw_cache_embeddings_total{result}`, `aigw_cache_embedding_seconds_{sum,count}`,
`aigw_cache_memory_{bytes,entries,evictions_total}`, `aigw_cache_vector_entries`. Je Anfrage:
`RequestRecord.cache` (`request_log.cache`, Rollup `cache_hits`), Span-Attribut `aigw.cache`.

## 7. Tests

- `aigw-cache`: Schlüssel/Normalisierung, LRU, HNSW (Recall gegen Brute-Force, 3 000 × 64),
  Speicher-Backend (Ersetzen, TTL, Grenzen, Neuaufbau), Verschlüsselung der Persistenz,
  gemeinsame Backend-Prüfung für `memory`, `pgvector` (`AIGW_TEST_PGVECTOR_URL`) und `redis`
  (`AIGW_TEST_REDIS_STACK_URL`), Ausfälle als Miss, Warmstart über SQLite.
- `aigw-proxy/tests/cache.rs`: Treffer ohne Upstream mit Usage 0, Formatwechsel
  OpenAI ↔ Anthropic, Stream-Wiedergabe mit Chunking, Client-Steuerung, Temperatur,
  Fehler/Tool-Aufrufe, semantischer Treffer über einen Test-Embedder.
- `scripts/check.sh`: `AIGW_TEST_POSTGRES=docker` startet `pgvector/pgvector:pg17`,
  `AIGW_TEST_REDIS=docker` startet `redis/redis-stack-server` (beide Obermengen der bisherigen
  Images) und setzen die Cache-Variablen mit.

## 8. Offene Punkte

- **Embedder:** `aigw_proxy::cache::RoutedEmbedder` routet die Embedding-Modellgruppe wie
  `POST /v1/embeddings` (AIGW-21: Auswahl, Retries, Fallbacks, Breaker, Kapazität), ohne
  Key-Prüfung; die Gruppe braucht die Fähigkeit `embeddings`.
- **Abrechnung (F169, AIGW-71):** Jeder Embedding-Aufruf aus einer Anfrage wird als eigener
  `RequestRecord` geloggt (Endpunkt `embeddings`, `cache = cache-embedding`, Key/Team/Org der
  auslösenden Anfrage, `client_request_id` = deren Request-ID, Kosten aus der Preistabelle) und
  auf die Budgets des Keys gebucht (ohne Reservierung, zählt nicht für Rate-Limits). Ein durch
  `embed_timeout_ms` abgebrochener Aufruf wird nicht erfasst. Offen: eigene Spalte
  `parent_request_id` statt `client_request_id`, Filter `cache=cache-embedding` im Log-Explorer.
