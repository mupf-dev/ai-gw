# ai-gw – Zähler für Limits und Budgets (AIGW-29)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [00-entscheidungen.md](00-entscheidungen.md) (E4), [01-architektur.md](01-architektur.md) §3 Schritt 4 und 11, §8
Code: `crates/aigw-counters`

## 1. Überblick

Der Proxy prüft vor jeder Upstream-Anfrage alle zutreffenden Limits (Key, Team, Organisation,
Modellgruppe, ggf. Deployment) mit **einem** Aufruf `reserve`. Passt die Menge in jedes Limit,
wird sie überall gebucht, sonst nirgends (alle oder keiner) und der Proxy antwortet mit `429`,
`Retry-After` und `x-ratelimit-*`. Nach der Antwort gleicht `settle` die Reservierung mit der
tatsächlichen Usage ab (±).

```rust
#[async_trait]
pub trait CounterBackend: Send + Sync + Debug {
    async fn reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection>;
    async fn settle(&self, reservation: Reservation, actual: u64);
    async fn get(&self, key: &str, window: Window) -> u64;
    fn kind(&self) -> CounterBackendKind;
    fn health(&self) -> Health;               // Ok | Degraded(Grund)
}

pub struct LimitCheck<'a> { key: &'a str, window: Window, limit: u64, amount: u64, settle: SettleMode }
```

Verfeinerungen gegenüber der Skizze in §8:

| §8 | umgesetzt | Grund |
|---|---|---|
| `reserve(key, amount, limit, window)` | `reserve(&[LimitCheck])` | mehrere Ebenen atomar in einem Aufruf (bei Redis ein Roundtrip) |
| `settle(r, actual)` | zusätzlich `SettleMode::{Adjust, Keep}` je Limit | Token-Limits werden korrigiert, Anfragezähler (1 Anfrage) nicht – beides in einer Reservierung |
| `Reservation` | enthält `quotas()` (Limit, Rest, Reset) je Limit, `tightest()` | Werte für `x-ratelimit-limit/remaining/reset-*` ohne zweiten Zugriff |
| – | `Rejection { index, key, window, limit, used, retry_after }` | `Retry-After`; `retry_after = None`, wenn die Menge größer als das Limit ist (nie erfüllbar → Fehler statt Warten) |
| – | alle Methoden unfehlbar, `health()` | Ausfall externer Backends führt nie zu Fehlern im Hot Path |
| – | `Window::Days30`, `Window::Period { start_ms, end_ms }` | 30 Tage gleitend (`30d`) und feste Kalenderzeiträume (F25) |
| – | `CounterBackend::stats()` | Nachtragen nach Ausfällen sichtbar machen (F28, Metriken) |

Die gleitenden Fenster sind `Second`, `Minute`, `Hour`, `Day`, `Days30` (= 30 × 24 h,
Konfiguration `30d`; hieß bis AIGW-76 `Month`). Dazu kommt **`Period`**: ein fester Zeitraum
`[start, end)` ohne Gleiten (Kalendertag/-woche/-monat, F25) – ein einziger Bucket mit der
Nummer `start`, Schätzung = Zählerstand, `Retry-After` und Reset = Zeit bis `end`, danach
zählt er nicht mehr. Den Zeitraum (Zeitzone, Monatslänge, Sommerzeit) berechnet der Aufrufer
(`aigw-proxy`, `billing.timezone`); der Proxy schreibt Art und Beginn zusätzlich in den
Schlüssel. Ein Zähler ist durch (Schlüssel, Fenster) bestimmt; derselbe Schlüssel kann in
mehreren Fenstern gezählt werden.
Schlüsselschema (Vorschlag für den Proxy): `<ebene>:<id>:<metrik>`, z. B. `team:<uuid>:tokens`.

Auswahl über die Startkonfiguration (`counters.backend`, `aigw_counters::from_config`):

```yaml
counters:
  backend: local            # local | postgres-sync | redis
  # redis_url: "redis://redis:6379/0"   # rediss:// für TLS; nur Einzelinstanz/Sentinel-Primary
  sync_interval_ms: 1000    # postgres-sync: Abgleichintervall (F26)
  redis_pool_size: 4        # redis: multiplexte Verbindungen (F26)
  redis_timeout_ms: 250     # redis: Zeitlimit je Operation, danach lokaler Rückfall (F26)
```

ENV wie üblich, z. B. `AIGW_COUNTERS__SYNC_INTERVAL_MS=2000`, `AIGW_COUNTERS__REDIS_POOL_SIZE=8`.

`postgres-sync` verwendet die Datenbank aus `store.url` (muss Postgres sein, sonst Startfehler).

## 2. Algorithmus: Sliding Window mit zwei Buckets

Jedes Fenster der Länge `L` ist in Buckets an Epochengrenzen eingeteilt (`bucket = now_ms / L`).
Gezählt werden der laufende Bucket `cur` und der vorige `prev`; vom vorigen zählt der Anteil, der
noch ins gleitende Fenster fällt (`elapsed` = Zeit seit Beginn des laufenden Buckets):

```text
geschätzt = cur + ⌊prev · (L − elapsed) / L⌋
zulassen  ⇔ geschätzt + amount ≤ limit
```

Das ist das bei Cloudflare/NGINX übliche Verfahren: O(1) Speicher je Zähler (zwei Zahlen), keine
Zeitstempel-Listen. Die Schätzung nimmt Gleichverteilung im vorigen Bucket an; der Fehler ist
nach oben und unten durch die Last im vorigen Bucket begrenzt (gegenüber festen Fenstern
verhindert es die doppelte Last an der Fenstergrenze).

**Retry-After** (bei unverändertem Verbrauch, `need = limit − amount`):

- `cur ≤ need`: noch im laufenden Bucket, sobald der Anteil von `prev` auf `need − cur` sinkt:
  `t = L − (need − cur) · L / prev`, Wartezeit `t − elapsed`.
- `cur > need`: erst im nächsten Bucket, in dem `cur` zum gewichteten Vorgänger wird:
  Wartezeit `(L − elapsed) + L − need · L / cur`.
- `amount > limit`: nie (`retry_after = None`).

Mindestens 1 ms; bei mehreren überschrittenen Limits zählt die längste Wartezeit. Der
Proxy rundet für den Header auf ganze Sekunden auf.

**Restkontingent:** `remaining = limit − (geschätzt + amount)` nach der Buchung; **Reset:** Zeit,
bis die Schätzung ohne neue Buchungen 0 erreicht (`cur > 0`: `2L − elapsed`, sonst `L − elapsed`).

**settle:** `actual − amount` wird im **ursprünglichen** Bucket verrechnet (Reservierung merkt sich
die Bucket-Nummer). Liegt dieser Bucket nicht mehr im Fenster, entfällt die Korrektur. Negative
Korrekturen sättigen bei 0. Eine nie abgeglichene Reservierung bleibt voll gebucht (konservativ).

## 3. Backends

### 3.1 `local`

- Je Fenster eine geshardete `DashMap<Schlüssel, Arc<Mutex<Slot>>>`; ein Slot hält zwei Buckets.
  Der kritische Abschnitt besteht aus wenigen Additionen; Konkurrenz entsteht nur bei
  gleichem Schlüssel (z. B. dem Org-Zähler). Atomics allein reichen nicht, weil Bucket-Wechsel
  und mehrere Schlüssel gemeinsam geprüft werden müssen.
- **Mehrfachschlüssel atomar:** Slots werden in Adressreihenfolge gesperrt (deadlock-frei),
  geprüft, und nur wenn alle passen gebucht. Doppelte Schlüssel in einem Aufruf zählen kumulativ.
  Wer einen Slot hält, greift nicht auf die Maps zu (Sperrregel gegen Deadlocks mit Shards).
- **Speicherbereinigung:** ein Hintergrund-Task (Standard alle 30 s) entfernt Slots, deren beide
  Buckets abgelaufen sind; gerade benutzte Slots (weitere `Arc`-Referenz) bleiben.
- **Genauigkeit:** exakt je Instanz; mehrere Instanzen zählen getrennt (Limit gilt je Instanz).

### 3.2 `postgres-sync`

- Entscheidung immer lokal (gleiche Engine wie `local`). Je Bucket gibt es zusätzlich
  `remote` (Summe der anderen Instanzen laut letztem Abgleich) und `unsynced` (lokale, noch nicht
  geschriebene Änderung). `geschätzt` rechnet mit `local + remote`.
- **Abgleich** alle `interval` (Standard 1 s, `PgSyncOptions`) in **einer** Anweisung:
  1. Deltas aller Buckets per `INSERT … ON CONFLICT (counter_key, window_start, instance_id) DO
     UPDATE SET value = value + EXCLUDED.value … RETURNING` (Arrays über `UNNEST`, eine Zeile je
     Schlüssel, Bucket und Instanz – kein Hotspot auf einer Zeile);
  2. im selben Statement `SUM(value)` der **anderen** Instanzen für alle aktiven Buckets
     (laufend und vorig) zurücklesen und als `remote` setzen.
- Tabelle `counter_state` aus Migration 0001 (unverändert): `counter_key = "<Fensterkürzel>|<Schlüssel>"`
  (`s`, `m`, `h`, `d`, `30d`; feste Zeiträume `p<Ende-ms>`), `window_start = Bucket · L` in ms
  (feste Zeiträume: Beginn), `instance_id` = UUIDv7 je Prozessstart (konfigurierbar).
- Bereinigung: alle 60 s löscht jede Instanz Zeilen, deren Bucket älter als zwei Fensterlängen ist
  (feste Zeiträume: Beginn älter als 32 Tage).
- **Sekundenlimits** (F29): Weil jede Instanz fremden Verbrauch erst nach dem nächsten Abgleich
  sieht, warnt das Gateway beim Start, wenn mit `postgres-sync` Sekundenlimits konfiguriert
  sind; die Admin-API liefert dieselben Hinweise unter `GET /admin/api/v1/usage/limit-warnings`
  (08 §5.12) für einen UI-Hinweis.
- Zugriff direkt über einen eigenen `sqlx::PgPool` (2 Verbindungen) statt `aigw_store::Store`:
  Der `Any`-Treiber des Stores kennt keine Arrays, und der Abgleich soll nicht mit der Admin-API um
  Verbindungen konkurrieren. Das Schema bleibt in der Store-Migration.
- **Genauigkeit:** lokal exakt; im Cluster sieht jede Instanz fremden Verbrauch mit bis zu
  `interval` + Laufzeit Verzögerung. Im ungünstigsten Fall können n Instanzen innerhalb eines
  Intervalls gemeinsam bis zu n × (Restkontingent) buchen. Für `Second`-Fenster ist das Backend
  daher nur je Instanz sinnvoll; ab `Minute` ist die Abweichung klein.
- **Ausfall:** Schlägt ein Abgleich fehl (Netz, Zeitlimit 5 s, Tabelle fehlt …), wird lokal mit
  dem zuletzt bekannten `remote` weitergezählt, einmalig gewarnt (`warn`, danach `debug`) und
  `health()` meldet `Degraded`. Die Deltas wandern in einen Rückstau (je Bucket zusammengefasst,
  höchstens 100 000 Einträge; älteste zuerst verworfen) und werden beim nächsten erfolgreichen
  Abgleich nachgeholt (`info`). Deltas von Buckets, die nicht mehr zum Fenster zählen, werden
  verworfen, da sie keine Entscheidung mehr beeinflussen. Eine beim Start nicht erreichbare
  Datenbank verhindert den Start nicht (verzögerter Pool).

### 3.3 `redis`

- **Client: `redis` (redis-rs 1.8)** mit `tokio-comp`, `tokio-rustls-comp` (rustls über den
  Workspace-Provider aws-lc-rs, kein OpenSSL/ring), `connection-manager`, `script`.
  Begründung gegenüber `fred`: offizieller, sehr verbreiteter Client; `ConnectionManager` bringt
  Multiplexing (nebenläufige Befehle werden automatisch auf einer Verbindung gepipelint) und
  Wiederverbindung mit; `Script` erledigt `EVALSHA` mit automatischem Nachladen bei `NOSCRIPT`;
  deutlich kleinerer Abhängigkeitsbaum. Cluster/Sentinel bräuchten weitere Features (offen).
- **Pool:** `counters.redis_pool_size` (Standard 4) multiplexte Verbindungen im Round-Robin.
- **Nur Einzelinstanz bzw. Sentinel-Primary (F27):** Beim Verbinden fragt das Backend
  `INFO cluster`; meldet der Server `cluster_enabled:1`, bricht der Start mit einer klaren
  Fehlermeldung ab (`CounterError::RedisCluster`) – ein `reserve` über mehrere Schlüssel
  bräuchte im Cluster denselben Hash-Slot. Wird der Cluster-Modus erst bei einer späteren
  Wiederverbindung erkannt, bleibt der lokale Rückfall aktiv (Fehler im Log).
- **Datenlayout:** ein Hash je Zähler `aigw:c:<Fensterkürzel>:<Schlüssel>`, Feld = Bucket-Nummer,
  `PEXPIRE` = 2 L + 1 s; ältere Felder werden beim Buchen entfernt. Feste Zeiträume:
  `aigw:c:p:<Schlüssel>`, Feld = Beginn, Ablauf 60 s nach dem Ende.
- **Skripte** (`EVALSHA`, beim Verbinden vorgeladen):
  - `reserve`: liest für alle Schlüssel `cur`/`prev`, prüft alle, bucht nur wenn alle passen –
    atomar, da Redis Skripte am Stück ausführt. Liefert Rohwerte; Rest, Reset und Retry-After
    rechnet Rust mit denselben Funktionen wie `local`.
  - `settle`: verbucht ± im ursprünglichen Bucket (falls noch im Fenster), sättigt bei 0.
  - `get`: liest den Stand.
  - Zeitbasis ist `TIME` des Redis-Servers: keine Uhrenabweichung zwischen Gateway-Instanzen.
- **Genauigkeit:** exakt im Cluster (ein Roundtrip je `reserve`, unabhängig von der Zahl der
  Limits). Lua rechnet in Gleitkomma; erst ab ~2⁵³ entstehen Rundungsabweichungen.
- **Ausfall:** Fehler oder Zeitlimit (`counters.redis_timeout_ms`, Standard 250 ms) → `warn`,
  `health()` = `Degraded`, alle weiteren Aufrufe zählen sofort in einer lokalen Engine (wie
  `local`, exakt je Instanz), statt bei jeder Anfrage auf Redis zu warten. Ein Hintergrund-Task
  prüft im Sekundentakt (`PING` bzw. Neuaufbau) und schaltet zurück (`info`). Ein in die
  Zeitüberschreitung gelaufenes Skript kann später doch noch ausgeführt werden – dann ist die
  Menge doppelt gebucht (konservativ). Ist Redis beim Start nicht erreichbar, startet das
  Backend im Rückfall.
- **Nachtragen nach der Rückkehr (F28):** Die lokale Engine schreibt im Rückfall ihre
  Änderungen je Bucket mit (wie bei `postgres-sync`). Nach der Rückkehr überträgt der
  Hintergrund-Task sie mit dem `settle`-Skript nach Redis (bis 256 Buckets je Aufruf) –
  **nur Buckets, die noch zum Fenster zählen**; abgelaufene werden verworfen (lokal geprüft und
  im Skript mit der Serverzeit). Ebenso wandern Abgleiche (`settle`) von Redis-Reservierungen,
  die während des Ausfalls anfielen, in diesen Rückstau statt verloren zu gehen
  (`lost_settles()` zählt sie); lief der Abgleich selbst in die Zeitüberschreitung, wird nur
  eine positive Korrektur zurückgestellt (eine Freigabe könnte sonst doppelt abziehen). Lokal
  gebuchte Reservierungen, die erst nach der Rückkehr abgeglichen werden, trägt der Task im
  nächsten Takt nach. Der Rückstau ist auf `max_backlog` (100 000) Buckets begrenzt (älteste
  zuerst verworfen) und wird während des Ausfalls laufend um abgelaufene Buckets bereinigt.
  Metriken (`CounterBackend::stats()` → `/metrics`): `aigw_counter_replayed_total`,
  `aigw_counter_replay_dropped_total`, `aigw_counter_replay_pending`; dazu
  `aigw_counter_backend_degraded`.
- Voraussetzung: Redis ≥ 7 oder Valkey (Skripte mit `TIME` und Schreibzugriffen).

## 4. Genauigkeit und Ausfall im Überblick

| Backend | Entscheidung | Genauigkeit im Cluster | Ausfall |
|---|---|---|---|
| `local` | lokal | je Instanz exakt, Instanzen unabhängig | – |
| `postgres-sync` | lokal + fremder Stand | ±`interval` (Standard 1 s) | lokal weiter mit letztem Fremdstand, Deltas werden nachgeholt |
| `redis` | Redis (Lua, atomar) | exakt | lokaler Rückfall je Instanz, Wiederverbindung im Sekundentakt, Nachtragen laufender Fenster (F28) |

## 5. Tests und Messwerte

- Unit-Tests: Fensterformel, Retry-After (Rasterprüfung), Bucket-Wechsel, Atomarität über mehrere
  Schlüssel, Duplikate, settle ±, Bereinigung, Sync-Buchführung, Nebenläufigkeit (16 Threads bzw.
  64 Tasks: Limit nie überschritten und voll ausgeschöpft).
- `tests/postgres.rs` (mit `AIGW_TEST_POSTGRES_URL`, `scripts/check.sh` mit
  `AIGW_TEST_POSTGRES=docker`): zwei Instanzen, gemeinsames Limit, Sliding Window über Instanzen,
  settle, Ausfall (Tabelle umbenannt) mit Rückstau und Nachholen, Bereinigung.
- `tests/redis.rs` (mit `AIGW_TEST_REDIS_URL` bzw. `AIGW_TEST_REDIS=docker`): zwei Instanzen,
  Atomarität, 64 Tasks auf zwei Instanzen (Limit nie überschritten), Rückfall per `CLIENT PAUSE`
  und Wiederverbindung, Nachtragen lokaler Buchungen und zurückgestellter Abgleiche mit
  Verwerfen abgelaufener Buckets (F28), feste Zeiträume (F25). Ohne Server laufen immer: Rückfall
  und die Ablehnung eines Servers im Cluster-Modus (Fake-Server, F27).
- `tests/postgres.rs` zusätzlich: feste Zeiträume über zwei Instanzen, Zeilenschlüssel
  `p<Ende>|…`, Bereinigung nach 32 Tagen (F25).
- Benchmark: `cargo run --release -p aigw-counters --example bench` (mit denselben Variablen für
  Postgres/Redis). Messwerte siehe Abschnitt 6.

## 6. Messwerte

Buildbox (AMD EPYC Genoa, 8 vCPU), Release-Build, Aufruf über `dyn CounterBackend`, je
Anfrage `reserve` + `settle`; „5 Limits“ = Key, Team, Org (Tag), Modell (Tokens) + Anfragezähler
(Sekunde). Postgres 17 und Redis 7 als lokale Docker-Container (Port-Weiterleitung über
docker-proxy, daher höhere Roundtrip-Zeit als im Netz üblich).

| Backend | reserve+settle, 1 Limit | reserve+settle, 5 Limits | 64 Tasks, 5 Limits |
|---|---|---|---|
| `local` | 0,37 µs | 0,63 µs | ≈ 0,5 Mio. Anfragen/s |
| `postgres-sync` | 0,38 µs | 0,63 µs | ≈ 0,85 Mio. Anfragen/s |
| `redis` | 430 µs | 384 µs | ≈ 15 800 Anfragen/s |

- `local`/`postgres-sync`: Der Hot Path ist identisch (Postgres nur im Hintergrund). Der
  Unterschied im Parallel-Durchsatz ist Messstreuung; Engpass sind die gemeinsamen Org-/Modell-
  Zähler (alle Tasks sperren denselben Slot).
- `postgres-sync`-Abgleich: 3 018 Deltas + 4 036 Buckets in 52 ms, ohne Deltas 14,6 ms – bei
  1 s Intervall ein kleiner Anteil einer Verbindung.
- `redis`: zwei Roundtrips je Anfrage (`reserve`, `settle`), unabhängig von der Zahl der Limits;
  sequentiell dominiert die Latenz, parallel bündelt das Multiplexing die Befehle.

## 7. Offene Punkte

- Erledigt mit AIGW-76: Kalenderfenster und `30d` (F25), Einstellungen in der Startkonfiguration
  (F26), Erkennung des Cluster-Modus (F27), Nachtragen nach Ausfällen (F28), Hinweis zu
  Sekundenlimits mit `postgres-sync` (F29).
- Redis Cluster bleibt nicht unterstützt: Ein `reserve` über mehrere Schlüssel bräuchte
  denselben Hash-Slot (Hash-Tags, z. B. `{org}`) oder eine Aufteilung je Slot.
