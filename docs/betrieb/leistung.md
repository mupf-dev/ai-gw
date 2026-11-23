# Leistung: Last- und Latenztests (AIGW-57)

Ziele aus [06-streaming-und-anbieter.md](../konzept/06-streaming-und-anbieter.md): **Q1** –
Gateway-Aufschlag p99 < 1 ms ohne Plugins (p50 < 200 µs), **Q4** – TTFT-Aufschlag bei Streams
< 1 ms. Dieses Dokument beschreibt den Messaufbau, die Zahlen (vorher/nachher), die
Optimierungen, Lasttests (Parallelität, Speicher, Zähler-Backends) und den Kaltstart mit
Postgres. Rohdaten: [messwerte/](messwerte/).

## 1. Messaufbau

**Hardware:** Hetzner-Buildbox cpx42 – 8 vCPU AMD EPYC Genoa (KVM), 16 GB RAM, Ubuntu,
Kernel 7.0. Die Box wurde während der Messungen **parallel von anderen Projekten genutzt**
(Lastdurchschnitt 2–18); alle Messprozesse liefen deshalb mit `SCHED_FIFO` (`RT=1`) auf
festen Kernen. Absolute Werte schwanken zwischen Läufen um ±20–30 %, Vergleiche sind nur
innerhalb eines Laufs (gleiche Datei) belastbar.

**Aufbau (`scripts/bench-e2e.sh`):** drei getrennte Prozesse auf getrennten Kernen

| Rolle | Prozess | Kerne |
|---|---|---|
| Fake-Upstream (OpenAI-kompatibel, JSON + SSE, antwortet sofort) | `latency upstream` (Beispiel in `crates/aigw-proxy/examples/latency.rs`) | 0–1 |
| Gateway | **echtes `aigw serve`** (Release-Profil) | 2–3 (Tokio: 2 Worker) |
| Lastgenerator (hyper, Keep-Alive) | `latency load` | 4–7 |

Realistische Kette **ohne Plugins**: SQLite-Store, API-Key (SHA-256-Prüfung gegen den
Snapshot), Organisation/Team/Key mit Limits (Anfragen/Minute, Tokens/Minute, Anfragen/Sekunde
je Modellgruppe, Parallelität) und Budgets (Org, Team, Key; Kosten über einen Preis), Zähler
`local`, Request-Log an (SQLite, Batch-Writer), Rate-Limit-Header an, Routing über die
Snapshot-Routen (Retries/Breaker aktiv), Antwort-Cache eingeschaltet (ohne Cache-Bindung),
`log.level: warn`. Eingänge OpenAI (`/v1/chat/completions`, `Authorization: Bearer`) und
Anthropic (`/v1/messages`, `x-api-key`) – beide übersetzen über die IR zum
OpenAI-kompatiblen Upstream.

**Gemessen** je Anfrage nach Aufwärmen: Zeit bis zum ersten Body-Frame (`ttfb`; bei Streams =
TTFT) und bis zum Ende. Szenarien: 1 Verbindung (seriell), 200 Verbindungen mit fester Rate
(2 000 bzw. 10 000 req/s, gleichmäßig versetzt) und Sättigung (so schnell wie möglich).
**Aufschlag = Gateway − direkt** (gleiches Szenario direkt gegen den Upstream; Perzentile
voneinander abgezogen, also eine Näherung).

Zum Vergleich läuft am Ende des Latenzteils das **Beispiel-Gateway** (`latency gateway`: nur
Proxy-Kern – Übersetzung, Routing, Upstream-Client; kein Key, keine Limits, kein Request-Log)
auf denselben Kernen.

## 2. Ergebnisse Latenz

### 2.1 Vorher/nachher (OpenAI-Eingang, `ttfb`, µs)

Vorher = `main` (f4de6bd, glibc-malloc), nachher = dieser Stand (mimalloc, Rate-Limit-Header;
die größere Accept-Warteschlange wirkt erst bei Verbindungsspitzen, §4). **Aufschlag** =
Gateway − direkt im selben Lauf. Die Verschlechterung bei „2 000 req/s, JSON“ liegt im
Rauschen der geteilten Box (siehe unten).
Rohdaten: [e2e-vorher](messwerte/57-e2e-vorher.txt), [e2e-nachher](messwerte/57-e2e-nachher.txt).

| Szenario (OpenAI-Eingang) | direkt p50 / p99 | Gateway vorher p50 / p99 | **Aufschlag vorher** p50 / p99 | Gateway nachher p50 / p99 | **Aufschlag nachher** p50 / p99 |
|---|---|---|---|---|---|
| 1 Verbindung, JSON | 42 / 86 → 40 / 53 | 173 / 246 | **131 / 160** | 132 / 252 | **92 / 199** |
| 1 Verbindung, Stream (TTFT) | 43 / 74 → 32 / 48 | 190 / 340 | **147 / 266** | 112 / 225 | **80 / 177** |
| 200 Verb., 2 000 req/s, JSON | 101 / 178 → 68 / 147 | 159 / 326 | **58 / 148** | 190 / 397 | **122 / 250** |
| 200 Verb., 2 000 req/s, Stream | 104 / 170 → 58 / 140 | 343 / 543 | **239 / 373** | 298 / 478 | **240 / 338** |
| 200 Verb., 10 000 req/s, JSON | 126 / 224 → 116 / 206 | 615 / 4 068 | **489 / 3 844** | 536 / 1 531 | **420 / 1 325** |
| 200 Verb., 10 000 req/s, Stream | 129 / 244 → 65 / 175 | 784 / 4 804 | **655 / 4 560** | 543 / 1 877 | **478 / 1 702** |
| 200 Verb., Sättigung, JSON | 154 000 req/s | 15 300 req/s | – | **27 600 req/s** | – |
| 200 Verb., Sättigung, Stream | 147 000 req/s | 12 500 req/s | – | **24 000 req/s** | – |

Anthropic-Eingang (nachher; Übersetzung Anthropic → IR → OpenAI und zurück):

| Szenario | Gateway p50 / p99 / p99.9 | Aufschlag p50 / p99 |
|---|---|---|
| 1 Verbindung, JSON | 100 / 192 / 354 | 60 / 139 |
| 1 Verbindung, Stream (TTFT) | 116 / 239 / 816 | 84 / 191 |
| 2 000 req/s, JSON | 188 / 448 / 1 090 | 120 / 301 |
| 2 000 req/s, Stream | 344 / 521 / 945 | 286 / 381 |
| 10 000 req/s, JSON | 495 / 1 490 / 1 899 | 379 / 1 284 |
| 10 000 req/s, Stream | 409 / 1 456 / 2 013 | 344 / 1 281 |
| Sättigung JSON / Stream | 20 800 / 23 600 req/s | – |

Beispiel-Gateway (nur Proxy-Kern) im selben Lauf: Sättigung 44 600 (JSON) / 37 500 (Stream)
req/s, 2 000 req/s JSON p50/p99 148/301 µs, 1 Verbindung JSON 85/154 µs. Die volle Kette
(Key, Limits, Budgets, Request-Log, Header) kostet damit bei niedriger Last ≈ 40–50 µs je
Anfrage und ≈ 40 % Durchsatz bei Sättigung (davon ≈ 13 Prozentpunkte der SQLite-Writer des
Request-Logs auf denselben Kernen).

Die Schwankungen zwischen Läufen sind groß (geteilte Box): dieselbe Messung „2 000 req/s,
JSON“ ergab in drei Läufen Gateway-p50 159, 190 und 281 µs. Belastbar sind vor allem die
Sättigungswerte und die Lastszenarien mit 10 000 req/s, bei denen die CPU-Kosten je Anfrage
die Warteschlangen bestimmen.

### 2.2 Bewertung Q1/Q4

- **Q1 p99 < 1 ms: erfüllt** für 1 Verbindung und 200 Verbindungen bei 2 000 req/s, beide
  Eingänge, JSON und Stream (Aufschlag p99 140–380 µs; auch das Gesamt-p99.9 des Gateways
  liegt um 1 ms). **Nicht erfüllt** bei 10 000 req/s auf **2 Kernen**: Aufschlag p99
  1,3–1,7 ms (vorher 3,8–4,6 ms). Dort liegt die Auslastung bei ≈ 40 % der Sättigung, und
  die Warteschlangen der zwei Worker bestimmen das p99.
- **Q4 (TTFT-Aufschlag < 1 ms):** wie Q1 – erfüllt bis 2 000 req/s, bei 10 000 req/s auf
  2 Kernen p99 1,3–1,7 ms.
- **Q1 p50 < 200 µs:** erfüllt für 1 Verbindung (60–92 µs) und JSON bei 2 000 req/s
  (≈ 120 µs); Streams bei 2 000 req/s 240–290 µs, 10 000 req/s 340–480 µs – nicht erfüllt.
- Ursachen siehe §3 (Kernel/Loopback ≈ 37 %, Request-Log-Writer ≈ 13 % auf denselben Kernen,
  geteilte Box) und Vorschläge in §7. Mit mehr Kernen sinkt die Auslastung je Kern
  proportional (Tokio-Worker skalieren; im Profil keine prozessweite Sperre im Hot Path, nur
  kurze Mutexe je Budgetstand und je Deployment-Breaker, `pthread_mutex_lock` ≈ 1 % – und
  davon der Großteil die Arena-Sperre von glibc-malloc).

## 3. Profil und Optimierungen

Profil mit `perf` (`PROFILE=1 PARTS=profile scripts/bench-e2e.sh`: eigenes Zielverzeichnis
`target/prof` mit Frame-Pointern und Zeilentabellen, 10 000 req/s, 8 s, Gateway-Prozess).
Verteilung der Gateway-CPU vor den Optimierungen (JSON; Stream ähnlich):

| Anteil | Bereich |
|---|---|
| ≈ 37 % | Kernel: Syscalls, TCP über Loopback (zwei Verbindungen je Anfrage: Client und Upstream), conntrack/nftables der Box |
| ≈ 17 % | **glibc-malloc** (`_int_malloc`/`_int_free`/`malloc_consolidate`, Arena-Mutex, `realloc`) |
| ≈ 25 % | hyper (Server und Client, HTTP/1-Parser und -Encoder), Tokio |
| ≈ 13 % | Request-Log-Writer (SQLite: B-Baum-Inserts, Bindings, Rollups – eigener Thread, gleiche Kerne) |
| ≈ 3 % | Limits/Budgets (`Limiter::admit`, `Budgets::settle`) |
| ≈ 2 % | Rate-Limit-Header |
| Rest | Übersetzung (serde_json), Routing, Recorder, Snapshot |

Umgesetzt:

1. **mimalloc als globaler Allokator** (`crates/aigw`, Feature `mimalloc`, Standard an;
   `--no-default-features` = System-Allokator). Größter Einzelposten: der Gateway erzeugt je
   Anfrage viele kurzlebige Allokationen auf zwei Worker-Threads; glibc-malloc verliert dabei
   Zeit in Arena-Sperren und Konsolidierung. Ergebnis siehe A/B-Vergleich unten.
2. **Rate-Limit-Header ohne Zwischen-Strings**: Header-Namen als statische Konstanten statt
   `format!` + `HeaderName::from_bytes`, Zahlen direkt als `HeaderValue::from(u64)`
   (`aigw-proxy/src/limits/headers.rs`); spart je Antwort 6–12 Allokationen.
3. **Accept-Warteschlange der Datenebene 4096** statt Tokios fester 1024
   (`aigw/src/server.rs`, vom Kernel auf `net.core.somaxconn` begrenzt): verhindert
   SYN-Überläufe bei Verbindungsspitzen (§4).
4. **Messbarkeit:** `load_ms` im Start-Log „Konfigurations-Snapshot geladen“ (Dauer
   Verbindung + Migration + Snapshot), p99.9 und Fehlerzählung im Lastgenerator,
   Fake-Upstream mit langen Streams (`CHUNKS`, `CHUNK_DELAY_MS`).

### 3.1 A/B Allokator (zwei Läufe unmittelbar nacheinander)

Unmittelbar nacheinander, gleicher Code, nur `ALLOC=system` vs. Standard
([system](messwerte/57-e2e-system.txt), [mimalloc](messwerte/57-e2e-nachher.txt)):

| Szenario (OpenAI) | glibc-malloc | mimalloc |
|---|---|---|
| Sättigung JSON | 14 700 req/s | **27 600 req/s** (+88 %) |
| Sättigung Stream | 12 200 req/s | **24 000 req/s** (+96 %) |
| 10 000 req/s JSON p50 / p99 / p99.9 | 626 / 3 397 / 4 688 µs | **536 / 1 531 / 1 940 µs** |
| 10 000 req/s Stream p50 / p99 / p99.9 | 924 / 7 682 / 11 500 µs | **543 / 1 877 / 2 569 µs** |
| 2 000 req/s JSON p50 / p99 | 230 / 417 µs | 190 / 397 µs |
| 1 Verbindung JSON p50 / p99 | 153 / 301 µs | 132 / 252 µs |
| RSS nach dem Latenzteil | ≈ 82 MiB | ≈ 98 MiB (Höchststand 123 MiB) |

Der Durchsatz je Kern verdoppelt sich fast; der Preis sind ≈ 15–20 MiB mehr Grundspeicher
(mimalloc hält freigegebene Seiten länger).

## 4. Lasttests

Rohdaten: [57-last.txt](messwerte/57-last.txt). Gleicher Aufbau (Gateway auf 2 Kernen),
Fake-Upstream mit **langen Streams**: 200 Chunks im Abstand von 5 ms (nominal ≈ 1 s je
Antwort). Jede Runde ist ein neuer Lastgenerator-Prozess, öffnet also alle Verbindungen neu.

| Szenario | Durchsatz | TTFT p50 / p99 | Dauer p50 / p99 | Gateway-CPU | RSS danach |
|---|---|---|---|---|---|
| direkt gegen den Fake-Upstream, 1 000 Verbindungen | 722 Streams/s | 0,8 ms / 37 ms | 1,37 s / 1,43 s | – | – |
| Start (Leerlauf) | – | – | – | – | 54 MiB |
| Gateway, 1 000 Verbindungen, 10 000 Streams, Runde 1 | 479 Streams/s | 23 ms / 0,7 s¹ | 1,9 s / 2,7 s | 39,9 s | 187 MiB |
| Runde 2 | 431 Streams/s | 21 ms / 5,4 s¹ | 1,8 s / 6,8 s | 38,8 s | 178 MiB |
| Runde 3 | 444 Streams/s | 23 ms / 3,5 s¹ | 1,9 s / 5,3 s | 39,7 s | 182 MiB |
| 4 000 Verbindungen, 8 000 Streams | 455 Streams/s | 295 ms / 1,0 s | 8,2 s / 12,8 s | 35,2 s | 532 MiB |
| 1 000 Verbindungen, JSON-Sättigung, 200 000 Anfragen | 22 500 req/s | 15 ms / 166 ms | – | – | 198 MiB |
| 2 s Leerlauf danach | – | – | – | – | **177 MiB** |

¹ Jede Runde öffnet 1 000 Verbindungen neu, während das Gateway voll ausgelastet ist;
einzelne Verbindungen warten dabei auf SYN-Wiederholungen (Ausreißer im p99, keine Fehler).

Bewertung:

- **Keine Fehler**, keine abgebrochenen Streams, auch nicht mit 4 000 gleichzeitigen
  Streams auf 2 Kernen. **Vor** der Änderung der Accept-Warteschlange
  (`aigw/src/server.rs`: 4096 statt Tokios fester 1024) endete derselbe Test mit 613
  Verbindungsfehlern und Wartezeiten bis 109 s (SYN-Überlauf, Wiederholungen nach 1/3/7/15/
  31/63 s) – [57-last-vorher.txt](messwerte/57-last-vorher.txt).
- **Speicher stabil, kein Leck:** über drei gleiche Runden 178–187 MiB; 4 000 parallele
  Streams belegen ≈ 85 KiB je Stream (Client- und Upstream-Verbindung, Puffer), nach dem
  Ende fällt RSS wieder auf ≈ 180 MiB (mimalloc gibt Seiten verzögert zurück).
- **Backpressure:** Der Encoder liest das nächste Upstream-Ereignis erst, wenn hyper den
  vorigen Frame in den Socket geschrieben hat (`aigw-proxy/src/stream.rs`); ein langsamer
  Client bremst also den Upstream, statt Speicher zu füllen. Abgesichert durch den Test
  `backpressure_and_client_abort` (`crates/aigw-proxy/tests/upstream.rs`). Bei
  Überlast sieht man Backpressure hier als längere Stream-Dauer (4 000 Streams: p50 8,3 s
  statt 1 s), nicht als Speicherwachstum oder Fehler.
- Durchsatz-Grenze bei langen Streams: Das Gateway ist mit ≈ 450 Streams/s × 206 Ereignissen
  ≈ 90 000 Chunks/s auf 2 Kernen ausgelastet (39–40 s CPU je Runde mit 2,06 Mio. Chunks
  ≈ **19 µs CPU je Chunk**, überwiegend Syscalls: jeder Chunk kommt 5 ms nach dem vorigen,
  also je Chunk ein Lesen vom Upstream und ein Schreiben zum Client). Der Fake-Upstream allein
  schafft 722 Streams/s.
- **SQLite unter Dauerlast:** Request-Log-Writer und Budget-Abgleich teilen sich die eine
  Schreibsperre von SQLite; bei 1 000+ Verbindungen meldet sqlx langsame Statements
  (`budget_usage` bis 6 s, Pool-Wartezeit bis 7 s). Der Hot Path ist davon nicht betroffen
  (Request-Log per `try_send`, Budgets im RAM), aber Budgetstände werden verzögert
  geschrieben. Für Lastbetrieb: Postgres.

### 4.1 Zähler-Backend Redis

Redis 7 im Container (Host-Netz, eigener Kern 5, ohne Persistenz), sonst gleicher Aufbau
([57-last.txt](messwerte/57-last.txt), Abschnitt „redis“):

| Szenario | local (§2.1, nachher) | redis |
|---|---|---|
| 1 Verbindung, JSON p50 / p99 | 132 / 252 µs | 400 / 4 056 µs |
| 2 000 req/s, JSON p50 / p99 | 190 / 397 µs | 631 / 2 338 µs |
| 2 000 req/s, Stream p50 / p99 | 298 / 478 µs | 706 / 2 440 µs |
| Sättigung JSON / Stream | 27 600 / 24 000 req/s | 13 300 / 13 200 req/s |

Jede Anfrage braucht mit `redis` mehrere Redis-Rundreisen im Hot Path (Reservierung aller
Limits, Abrechnung am Ende); auf derselben Maschine kosten sie ≈ 250–450 µs p50 und machen das
p99 empfindlich für Planungsverzögerungen des Redis-Prozesses. **Q1 ist mit `redis` nicht
erreichbar**, solange Limits synchron in Redis geprüft werden – dafür ist `postgres-sync`
(lokal zählen, periodisch abgleichen) bzw. `local` gedacht (05-zaehler.md).

### 4.2 Eigene Accept-Schleife mit Verbindungsschutz (AIGW-80)

`axum::serve` wurde durch eine eigene Schleife mit Verbindungsgrenze und Lese-Timeouts
ersetzt (S-32, `aigw/src/serve.rs`): je Verbindung eine Wache (Timer im Abstand ≥ 100 ms,
höchstens 5 s), je Anfrage ein Zähler im Antwortkörper, beim Lesen eine Atomics-Abfrage.
A/B auf derselben Box, abwechselnd Basis (main 4041b0e) / AIGW-80, je zweimal
`RT=1 PARTS=latency` – Gateway-`ttfb` p50 / p99 in µs, OpenAI-Eingang. Rohdaten:
[80-ab-base-1](messwerte/80-ab-base-1.txt), [80-ab-new-1](messwerte/80-ab-new-1.txt),
[80-ab-base-2](messwerte/80-ab-base-2.txt), [80-ab-new-2](messwerte/80-ab-new-2.txt).

| Szenario | Basis 1 | AIGW-80 1 | Basis 2 | AIGW-80 2 |
|---|---|---|---|---|
| 1 Verbindung, JSON | 97 / 185 | 162 / 283 | 95 / 179 | 93 / 163 |
| 1 Verbindung, Stream | 111 / 272 | 135 / 258 | 99 / 175 | 105 / 200 |
| 200 Verb., 2 000 req/s, JSON | 272 / 434 | 193 / 387 | 171 / 378 | 183 / 353 |
| 200 Verb., 2 000 req/s, Stream | 297 / 528 | 193 / 441 | 194 / 394 | 200 / 411 |
| 200 Verb., 10 000 req/s, JSON | 292 / 928 | 255 / 771 | 293 / 1 040 | 324 / 1 063 |
| 200 Verb., 10 000 req/s, Stream | 516 / 1 869 | 525 / 1 728 | 371 / 1 351 | 508 / 1 684 |

Bewertung: **kein messbarer Unterschied** – die Abweichungen zwischen zwei Läufen derselben
Variante (geteilte Box, Last 5–7) sind größer als zwischen den Varianten; in keinem Lauf
Fehler. Der Ausreißer „1 Verbindung“ in AIGW-80-Lauf 1 wiederholte sich in Lauf 2 nicht.

## 5. Kaltstart mit Postgres

`scripts/bench-coldstart.sh`: Postgres 17 (Container, Host-Port), 10 000 API-Keys (mit
Limits und Modellfreigaben), 1 000 Modellgruppen mit je einem Alias und 2 Deployments, 10
Teams mit Limits/Budgets, eine Organisation. Angewendet per `aigw config apply` (GitOps,
14 Teildateien, siehe unten), danach dreimal `aigw serve` gestartet bis `/readyz` = 200.
Rohdaten: [57-kaltstart.txt](messwerte/57-kaltstart.txt).

| Datenbank | Apply (23 011 Ressourcen) | Start bis `/readyz` | davon Snapshot (`load_ms`) | RSS nach Start |
|---|---|---|---|---|
| Postgres 17 | 16,2 s | **152–183 ms** | 135–153 ms | 65–76 MiB |
| SQLite | 4,0 s | **136–147 ms** | 121–124 ms | 91–92 MiB |

`load_ms` umfasst Verbindungsaufbau, Migrationsprüfung und das Laden des Snapshots (neu im
Start-Log „Konfigurations-Snapshot geladen“, Feld `load_ms`). Der Kaltstart ist damit auch
mit 10 000 Keys deutlich unter einer Sekunde; der Rest bis `/readyz` sind Listener, Routing-
und Plugin-Tabellen und der erste Poll.

Befund: Ein GitOps-Manifest darf **höchstens 1 024 YAML-Dokumente** enthalten (Grenze des
YAML-Parsers `serde-saphyr`, „budget breached: Documents“). Große Bestände müssen auf mehrere
Dateien verteilt werden; das Skript schreibt Teildateien zu je 1 000 Dokumenten.

## 6. Reproduktion

```sh
# auf einer Maschine mit ≥ 8 vCPU (Buildbox), Release-Profil; Docker für Redis/Postgres
RT=1 scripts/bench-e2e.sh                        # Latenz + Parallelität + Redis (≈ 15 Min.)
RT=1 PARTS=latency scripts/bench-e2e.sh          # nur Latenz (≈ 5 Min.)
ALLOC=system RT=1 PARTS=latency scripts/bench-e2e.sh   # ohne mimalloc (A/B)
PROFILE=1 PARTS=profile scripts/bench-e2e.sh     # perf-Profil → target/bench/perf-*.txt
scripts/bench-coldstart.sh                       # Kaltstart Postgres + SQLite (≈ 5 Min.)
KEYS=100000 MODEL_GROUPS=5000 DBS=postgres scripts/bench-coldstart.sh
scripts/bench-latency.sh                         # nur Proxy-Kern (Beispiel-Gateway, AIGW-14/15)
RT=1 scripts/bench-litellm.sh                    # Vergleich mit LiteLLM, Abschnitt 8 (≈ 35 Min.)
```

Ergebnisse landen in `target/bench/`. Die Skripte sind bewusst **nicht** Teil von
`scripts/check.sh` (Laufzeit, Bedarf an freien Kernen); die Korrektheit von Backpressure,
Client-Abbruch und Streaming prüfen die Tests in `crates/aigw-proxy/tests/upstream.rs`
(`backpressure_and_client_abort` u. a.).

## 7. Offene Punkte und Vorschläge

Q1 (p99 < 1 ms) ist bis mittlere Last erfüllt, bei 10 000 req/s auf nur 2 Kernen nicht
(p99-Aufschlag 1,3–1,7 ms). Ursachen und Vorschläge, nach erwartetem Nutzen:

1. **Request-Log nicht auf denselben Kernen in SQLite schreiben.** Der SQLite-Writer kostete
   ≈ 13 % der Gateway-CPU und konkurriert mit dem Budget-Abgleich um die Schreibsperre. Für
   Lastbetrieb Postgres verwenden (Writer ist dann überwiegend I/O-Warten); optional den
   Writer auf einen eigenen Thread/Kern legen oder `observe.request_log: false` und nur
   JSON-Lines/OTLP.
2. **Mehr Kerne:** Die Kosten je Anfrage liegen nach den Optimierungen bei ≈ 70 µs
   CPU (2 Kerne / 27 600 req/s), davon ≈ 37 % Kernel/Loopback. 10 000 req/s mit p99 < 1 ms
   brauchen nach diesen Zahlen ≈ 4 Kerne (Auslastung < 20 %).
3. **Netzwerkpfad:** conntrack/nftables (Docker auf der Box) kosten messbar Zeit auf
   Loopback; HTTP/2 zum Upstream (weniger Verbindungen, weniger Syscalls) bei Anbietern, die
   es können (der Client spricht H2 per ALPN, der Fake-Upstream nur H1).
4. **Zähler `redis`:** Reservierung und Abrechnung je Anfrage in einem Lua-Aufruf bzw. per
   Pipeline bündeln; bis dahin `postgres-sync`/`local` für latenzkritische Installationen.
5. **Stream-p50 > 200 µs:** Je Chunk entstehen mehrere kleine Allokationen (SSE-Zeile als
   `String`, IR-Ereignis, JSON-Kodierung); ein Bytes-basierter SSE-Parser und
   wiederverwendete Kodierpuffer sind die nächsten Kandidaten (Profil: Decoder + Encoder
   ≈ 7 % der CPU bei Streams).
6. **GitOps-Manifeste > 1 024 Dokumente:** Grenze dokumentieren oder das Dokument-Budget
   des YAML-Parsers (`serde-saphyr`-Optionen) für GitOps anheben.
7. **Messumgebung:** Für belastbare absolute Zahlen eine exklusive Maschine ohne fremde Last
   (die Buildbox war mit Lastdurchschnitt bis 18 geteilt).

## 8. Vergleich mit LiteLLM (AIGW-102)

Lastvergleich mit dem Vorbild LiteLLM-Proxy unter gleichen Bedingungen:
`scripts/bench-litellm.sh`, Lauf vom 2026-11-15 auf der Buildbox (cpx42, 8 vCPU EPYC Genoa,
geteilt; Lastdurchschnitt 2–22 während des Laufs). Rohdaten:
[102-litellm.txt](messwerte/102-litellm.txt), Kurzfassung
[102-litellm-zusammenfassung.txt](messwerte/102-litellm-zusammenfassung.txt).

### 8.1 Methode und Konfiguration

| Rolle | Prozess | Kerne |
|---|---|---|
| Fake-Upstream (OpenAI-kompatibel, antwortet sofort; Stream = 6 Chunks + Usage) | `latency upstream` | 0–1 |
| Gateway (Container, `--network host`, `--cpuset-cpus 2,3`) | ai-gw bzw. LiteLLM | 2–3 |
| Lastgenerator (hyper, Keep-Alive) | `latency load` | 4–6 |
| Postgres 17 (nur `*-pg`) | `postgres:17-alpine` | 7 |

Upstream, Last und alle Prozesse der Gateway-Container laufen mit `SCHED_FIFO` (`RT=1`).

| Variante | Image | Store / Key-Prüfung |
|---|---|---|
| `aigw-sqlite` | ai-gw, lokal aus dem Dockerfile gebaut (Basis main 928d45c) | SQLite; API-Key mit Org/Team/Key-Limits, Budgets, Preis, Request-Log an (wie `bench-e2e.sh`) |
| `aigw-pg` | dito | Postgres 17, sonst wie oben |
| `litellm` | `ghcr.io/berriai/litellm:v1.108.8` (LTS, Digest im Rohprotokoll) | ohne DB; Prüfung gegen den Master-Key (ohne DB die einzige Key-Art) |
| `litellm-pg` | `ghcr.io/berriai/litellm-database:v1.108.8` | Postgres 17; virtueller Key (`/key/generate`) mit `models`, `max_budget`, `rpm_limit`, `tpm_limit`; Spend-Logs an |

LiteLLM nach „Best Practices for Production“ (docs.litellm.ai/docs/proxy/prod): ein Container
mit `--num_workers 2` (= vCPU des Containers), `LITELLM_MODE=PRODUCTION`,
`LITELLM_LOG=ERROR`, `set_verbose: false`, `json_logs: true`, `request_timeout: 600`,
`proxy_batch_write_at: 60`, `database_connection_pool_limit: 10`, `disable_error_logs: true`;
zusätzlich `LITELLM_LOCAL_MODEL_COST_MAP=True` (kein Nachladen der Preisliste beim Start).
Redis (empfohlen ab mehreren Instanzen) entfällt bei einer Instanz; ai-gw nutzt ebenfalls
Zähler `local`. Tokio nutzt im Container 2 Worker (cpuset).

Gemessen: **Aufschlag** = Gateway − direkt (Perzentile voneinander abgezogen) bei 1
Verbindung seriell (2 000 Anfragen) und bei 10 Verbindungen mit 200 req/s; **Durchsatz** bei
Sättigung mit 10/100/500 Verbindungen (ai-gw 40 000, LiteLLM 10 000 Anfragen je Lauf,
5 Aufwärm-Anfragen je Verbindung); **Fehler** (HTTP-Status ≠ 2xx bzw. Verbindungsfehler);
**CPU je Anfrage** (cgroup `cpu.stat` des Containers / erfolgreiche Anfragen);
**Speicher** wie `docker stats` (cgroup `memory.current` ohne `inactive_file`; Höchststand
alle 0,1 s abgetastet); **Kaltstart** = `docker run` bis Readiness (ai-gw `/readyz`,
LiteLLM `/health/readiness`), erster Start mit leerer DB und Neustart mit vorhandener DB.

### 8.2 Ergebnisse

**Aufschlag gegenüber direktem Upstream** (OpenAI-Eingang; json = Gesamtzeit, stream = TTFT;
p50 / p90 / p99):

| Szenario | direkt | aigw-sqlite | aigw-pg | litellm | litellm-pg |
|---|---|---|---|---|---|
| json, 1 Verb. seriell | 51 / 78 / 122 µs | +0,32 / +0,41 / +0,46 ms | +0,30 / +0,42 / +0,50 ms | +8,2 / +12,3 / +17,2 ms | +10,7 / +15,3 / +32,6 ms |
| stream, 1 Verb. seriell | 85 / 127 / 248 µs | +0,33 / +0,41 / +0,45 ms | +0,32 / +0,41 / +0,43 ms | +10,4 / +17,7 / +22,2 ms | +11,1 / +18,4 / +26,9 ms |
| json, 10 Verb., 200 req/s | 211 / 265 / 352 µs | +0,40 / +0,47 / +0,59 ms | +0,45 / +0,51 / +0,82 ms | +11,1 / +28,9 / +44,0 ms | +25,0 / +55,8 / +101 ms ¹ |
| stream, 10 Verb., 200 req/s | 204 / 268 / 368 µs | +0,44 / +0,52 / +0,67 ms | +0,41 / +0,53 / +0,60 ms | +24,8 / +59,6 / +99,3 ms ¹ | +36,5 / +82,2 / +133 ms ¹ |

¹ Zielrate nicht erreicht (litellm stream 124 req/s, litellm-pg json 188 bzw. stream
90 req/s) – LiteLLM ist hier bereits gesättigt, die Werte enthalten Wartezeit.

**Durchsatz bei Sättigung** (req/s; Fehler in Klammern, sonst 0):

| Modus, Verbindungen | aigw-sqlite | aigw-pg | litellm | litellm-pg |
|---|---|---|---|---|
| json, 10 | 16 413 | 18 076 | 269 | 206 |
| json, 100 | 26 313 | 20 210 | 151 (0,5 %) | 157 |
| json, 500 | 17 734 | 24 023 | 144 (3,5 %) | 103 (4,6 %) |
| stream, 10 | 14 087 | 15 501 | 122 | 120 |
| stream, 100 | 15 264 | 16 364 | 155 | 95 |
| stream, 500 | 9 562 | 9 824 | 136 | 109 |

p99 der Gesamtzeit bei 500 Verbindungen: ai-gw 65–75 ms, LiteLLM 5,7–8,4 s.

**Ressourcen:**

| | aigw-sqlite | aigw-pg | litellm | litellm-pg |
|---|---|---|---|---|
| CPU je Anfrage (json, 100 Verb.) | 79 µs | 101 µs | 8 810 µs | 10 598 µs |
| CPU je Anfrage (stream, 100 Verb.) | 134 µs | 124 µs | 13 420 µs | 18 561 µs |
| Speicher Leerlauf (nach Start + 1 Anfrage) | 35 MiB | 29 MiB | 1 626 MiB | 1 700 MiB |
| Speicher Höchststand unter Last | 317 MiB | 180 MiB | 1 959 MiB | 2 323 MiB |
| Speicher nach der Last | 285 MiB | 162 MiB | 1 955 MiB | 2 318 MiB |
| Kaltstart erster Start (leere DB) | 0,60 s | 0,36 s | 25,6 s | 28,3 s |
| Kaltstart Neustart | 0,27 s | 0,35 s | 22,6 s | 24,2 s |
| Image entpackt / komprimiert | 111 / 30 MiB | 111 / 30 MiB | 1 730 / 398 MiB | 1 648 / 383 MiB |

### 8.3 Bewertung und Auffälligkeiten

- **Größenordnungen:** Auf denselben zwei Kernen hat ai-gw einen um Faktor ≈ 25–35 kleineren
  Aufschlag (seriell), ≈ 60–230-fachen Durchsatz und ≈ 100–150-fach weniger CPU je Anfrage;
  Leerlaufspeicher ≈ 50-fach kleiner, Kaltstart ≈ 40–80-fach schneller, Image ≈ 13-fach
  kleiner (komprimiert). Der Abstand ist so groß, dass Schwankungen der geteilten Box (±30 %) die
  Aussage nicht ändern.
- **LiteLLM unter Überlast:** ab 100 Verbindungen Verbindungsabbrüche (Status 0 im
  Lastgenerator), mit Postgres bei 500 Verbindungen zusätzlich 282 × `503`; Latenzen im
  Sekundenbereich. ai-gw blieb in allen Läufen fehlerfrei.
- **LiteLLM-Streams:** TTFT steigt bei Last weniger stark als die Gesamtzeit (z. B. 500
  Verbindungen: TTFT p50 79 ms, Gesamt p50 3,1 s) – die Worker verteilen die Chunks vieler
  Streams nacheinander.
- **ai-gw-Speicher unter Last:** SQLite-Variante bis 317 MiB (Request-Log-Puffer und
  mimalloc-Seiten bleiben nach der Last reserviert); Postgres-Variante 180 MiB.
- **ai-gw-Aufschlag höher als in §2** (seriell ≈ 0,3 ms statt ≈ 0,09 ms mit dem nativen
  Binary): Container-cgroup, die stark geteilte Box (Lastdurchschnitt bis 22) und Aufwachkosten
  bei geringer Rate (CPU je Anfrage seriell ≈ 300 µs gegenüber 80–130 µs unter Last). Für
  Q1/Q4 gelten weiter die Messungen aus §2.

### 8.4 Grenzen der Aussage

- Fake-Upstream antwortet sofort; bei echten Anbietern (Hunderte ms bis Sekunden) ist der
  Aufschlag beider Gateways relativ klein – relevant sind dann vor allem CPU je Anfrage,
  Speicher, Durchsatz je Kern und das Verhalten bei vielen gleichzeitigen Streams.
- Nur 2 Kerne je Gateway und eine Instanz. LiteLLM skaliert horizontal über Worker/Pods
  (laut Doku 1 vCPU und ≥ 4 GiB je Worker); mehr Worker, Redis, andere ASGI-Server
  (Gunicorn/Granian aus „Server Tuning“) und `litellm` ohne Spend-Tracking wurden nicht
  gemessen. Mehr Worker und Redis: Nachmessung in §8.5 (AIGW-107).
- `litellm` (ohne DB) prüft nur den Master-Key und ist damit **leichter** als die
  ai-gw-Kette (Limits, Budgets, Preis, Request-Log); `litellm-pg` ist die funktional
  vergleichbare Variante.
- Nur OpenAI-Eingang; kein Anthropic-Eingang, keine Plugins/Guardrails, kein Caching.
- Geteilte Buildbox (parallel Builds anderer Projekte); absolute Zahlen schwanken zwischen
  Läufen, die Verhältnisse sind belastbar.
- Der ai-gw-Container nutzt ein Image vom Arbeitsstand (Dockerfile, Release-Profil); die
  LiteLLM-Version ist auf v1.108.8 (LTS, 2026-11-13) gepinnt (`LITELLM_TAG`).

### 8.5 Ergänzung: mehrere Worker und Redis (AIGW-107)

Offene Punkte aus §8.4 nachgemessen: LiteLLM mit **4 Workern auf 4 Kernen** und mit **Redis**
(Mehrinstanz-Aufbau nach „Best Practices for Production“), ai-gw mit Redis-Zählern.
`VARIANTS="aigw-pg aigw-pg-redis litellm-pg litellm-pg-w4 litellm-pg-redis-w4"
RT=1 BUILD_IMAGE=0 OUT=target/bench/107-litellm.txt scripts/bench-litellm.sh`, Lauf vom
2026-11-23 auf der Buildbox (cpx42, 8 vCPU EPYC Genoa; Lastdurchschnitt vorher 8, danach 14 –
geteilt mit anderen Projekten). Rohprotokoll [107-litellm.txt](messwerte/107-litellm.txt),
Kurzfassung [107-litellm-zusammenfassung.txt](messwerte/107-litellm-zusammenfassung.txt).
Alles lokal per Docker gegen den Fake-Upstream, keine externen APIs, keine Keys.

| Variante | Aufbau | Kerne Gateway / Last |
|---|---|---|
| `aigw-pg` | wie §8 (Postgres 17, Zähler `local`) | 2–3 / 4–6 |
| `aigw-pg-redis` | wie `aigw-pg`, `counters.backend: redis` (Redis 7.4, Kern 7 mit Postgres) | 2–3 / 4–6 |
| `litellm-pg` | wie §8 (2 Worker, virtueller Key mit Budget/RPM/TPM, Spend-Logs) | 2–3 / 4–6 |
| `litellm-pg-w4` | 4 Worker | **2–5** / 6 |
| `litellm-pg-redis-w4` | 4 Worker + Redis: `router_settings.redis_host/redis_port` (RPM/TPM über Instanzen), `general_settings.use_redis_transaction_buffer` mit `REDIS_HOST/REDIS_PORT` (Spend-Updates gepuffert) | **2–5** / 6 |

LiteLLM bekommt in den `*-w4`-Varianten doppelt so viele Kerne wie ai-gw.

| Kennzahl | aigw-pg | aigw-pg-redis | litellm-pg | litellm-pg-w4 | litellm-pg-redis-w4 |
|---|---|---|---|---|---|
| Aufschlag json seriell, p50 (direkt 50 µs) | +0,09 ms | +0,37 ms | +15,9 ms | +16,5 ms | +20,0 ms |
| erreichte Rate bei Ziel 200 req/s (json / stream) | 200 / 200 | 200 / 200 | 86 / 80 | 116 / 142 | 137 / 74 |
| Sättigung json, 10 / 100 / 500 Verb. (req/s) | 26 771 / 29 653 / 18 176 | 4 249 / 5 886 / 7 250 | 139 / 141 / 129 ¹ | 114 / 208 ¹ / 107 ² | 127 / 176 / 168 ¹ |
| Sättigung stream, 10 / 100 / 500 Verb. (req/s) | 28 170 / 27 773 / 4 628 ³ | 4 159 / 6 323 / 7 701 | 67 / 37 ¹ / 37 ¹ | 106 / 71 / 112 | 102 / 64 / 73 |
| p99 gesamt json, 500 Verb. | 68 ms | 124 ms | 7,5 s | 4,9 s | 8,0 s |
| CPU je Anfrage (json, 100 Verb.) | 69 µs | 211 µs | 13 115 µs | 15 113 µs | 16 493 µs |
| Speicher Leerlauf / Höchststand | 19 / 163 MiB | 19 / 154 MiB | 1 818 / 2 159 MiB | 2 789 / 3 787 MiB | 2 786 / 3 721 MiB |
| Kaltstart erster Start / Neustart | 0,32 / 0,29 s | 0,24 / 0,25 s | 35,2 / 34,3 s | 36,2 / 30,0 s | 34,2 / 30,8 s |
| Redis-Befehle je Anfrage (ca.) | – | ≈ 44 | – | – | ≈ 17 |

¹ Verbindungsabbrüche (Status 0 im Lastgenerator): litellm-pg json 500: 450, stream 100/500:
256/261; litellm-pg-w4 json 100: 32; litellm-pg-redis-w4 json 500: 114.
² 263 × `503`. ³ Ausreißer (fremde Last auf der Box während dieses Laufs; die anderen
ai-gw-Läufe und §8 liegen bei 10–16 k req/s).

**Bewertung:**

- **Mehr Worker helfen LiteLLM kaum:** Doppelte Kerne und Worker heben den Sättigungsdurchsatz
  nur punktuell (json 100 Verb. 141 → 208 req/s), stream bleibt bei ≈ 70–110 req/s; der Aufschlag
  je Anfrage (≈ 16 ms seriell, ≈ 13–17 ms CPU) ist unabhängig von der Worker-Zahl. Speicher
  wächst mit den Workern (≈ 0,5 GiB je zusätzlichem Worker; 4 Worker ≈ 2,8 GiB im Leerlauf).
- **Redis kostet LiteLLM wenig** (≈ +4 ms seriell, Durchsatz im Rauschen) – er ist dort
  ohnehin durch die Python-Verarbeitung je Anfrage begrenzt.
- **Redis kostet ai-gw deutlich:** Mit `counters.backend: redis` sinkt der Durchsatz auf
  ≈ 4–8 k req/s (statt 18–30 k) und der serielle Aufschlag steigt auf ≈ 0,37 ms – ≈ 44
  Redis-Befehle je Anfrage (Limits Org/Team/Key, Kapazität, Concurrency, Reservierung und
  Abrechnung, einzeln). Das bestätigt Punkt 4 in §7 (Reservierung/Abrechnung je Anfrage in
  einem Lua-Aufruf bzw. Pipeline bündeln). Auch so liegt ai-gw mit Redis noch ≈ 30–100-fach
  über LiteLLM mit Redis und doppelt so vielen Kernen.
- **Unter Überlast** bricht LiteLLM in allen Varianten Verbindungen ab bzw. antwortet `503`;
  ai-gw blieb in allen Läufen fehlerfrei.
- Ein erster Lauf am 2026-11-22 bei Lastdurchschnitt 45–65 auf der Box (fremde Builds) ergab
  dieselben Verhältnisse bei ≈ 40–50 % niedrigeren Absolutwerten; er wurde verworfen.

