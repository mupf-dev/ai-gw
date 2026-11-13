# Datenhaltung, mehrere Instanzen, Backup und Upgrade

## 1. SQLite oder Postgres

| | SQLite (Standard) | Postgres (ab 15, getestet mit 17) |
|---|---|---|
| `store.url` | `sqlite:///data/aigw.db?mode=rwc` | `postgres://aigw:<passwort>@host:5432/aigw` (Passwort per `AIGW_STORE__URL`) |
| Instanzen | **genau eine** | beliebig viele |
| Änderungen anderer Prozesse | Versionsabfrage alle `store.reload_poll_secs` (30 s) | sofort per `LISTEN/NOTIFY`, Abfrage als Absicherung |
| Last | bis mittlere Last | empfohlen für Lastbetrieb |
| Betrieb | eine Datei, kein Dienst | Backup/HA über die üblichen Postgres-Werkzeuge |

**Empfehlung:** SQLite für Test, Entwicklung und kleine Einzelinstallationen; **Postgres für
Produktion unter Last und für jede Installation mit mehr als einer Instanz.** Unter Dauerlast
teilen sich Request-Log-Writer und Budget-Abgleich die eine Schreibsperre von SQLite; bei
mehr als 1 000 gleichzeitigen Verbindungen wurden langsame Statements (`budget_usage` bis 6 s)
gemessen. Der Hot Path bleibt davon unberührt (Request-Log asynchron, Budgets im RAM), aber
Budgetstände werden verzögert geschrieben ([leistung.md](leistung.md) §4, §7).

Weitere Stellschrauben: `store.max_connections` (Standard 10; bei Postgres mit mehreren
Instanzen Summe < `max_connections` des Servers), `observe.request_log: false` (nur JSON-Lines/
OTLP, kein Rohlog in der DB), `observe.retention.*` (Aufbewahrung).

Kaltstart-Messung mit 10 000 Keys und 1 000 Modellgruppen: Postgres 152–183 ms, SQLite
136–147 ms bis `/readyz` ([leistung.md](leistung.md) §5).

## 2. Zähler-Backends und Redis

Limits (RPM/TPM/…) und die Deployment-Kapazität werden über `counters.backend` gezählt
([05-zaehler.md](../konzept/05-zaehler.md)):

| Backend | Genauigkeit im Cluster | Latenz | Ausfall |
|---|---|---|---|
| `local` (Standard) | je Instanz exakt, Instanzen unabhängig | ≈ 0,4 µs | – |
| `postgres-sync` | ± `counters.sync_interval_ms` (Standard 1 s) | ≈ 0,4 µs | lokal weiter, Deltas werden nachgeholt |
| `redis` | exakt (Lua, atomar) | Netz-Rundreisen im Hot Path (gemessen +250–450 µs p50) | lokaler Rückfall je Instanz, Nachtragen nach Wiederverbindung |

- Eine Instanz: `local`.
- Mehrere Instanzen, Limits dürfen kurz überschritten werden: `postgres-sync` (keine
  zusätzliche Infrastruktur, geringste Latenz).
- Mehrere Instanzen, Limits müssen exakt sein: `redis` mit `counters.redis_url`
  (Redis 7 oder Valkey; Passwort in der URL per `AIGW_COUNTERS__REDIS_URL`). Redis-Persistenz ist
  nicht nötig – nach einem Redis-Neustart beginnen die Fenster neu.
- Budgets (Kosten) liegen unabhängig davon in der Datenbank (Tabelle `budget_usage`, Abgleich
  alle `billing.budget_sync_ms`).

Redis wird außerdem optional als gemeinsame Ebene des Antwort-Caches genutzt
(`cache.redis_url`) und kann als Vektor-Backend des semantischen Caches dienen (Redis Stack/
Valkey Search). Zustand im Betrieb: `aigw_counter_backend_degraded` ([monitoring.md](monitoring.md)).

## 3. Mehrere Instanzen (HA)

Voraussetzungen: Postgres als `store.url`, alle Instanzen mit **demselben Master-Key** und
gleicher Startkonfiguration, Zähler `postgres-sync` oder `redis`, Load Balancer vor der
Datenebene. Beispiel: [config/examples/ha-postgres-redis/](../../config/examples/ha-postgres-redis/aigw.yaml),
Kubernetes-Grundgerüst mit 2 Replikaten: [deploy/kubernetes/](../../deploy/kubernetes/).

- **Zustandslos bis auf Caches:** Jede Instanz hält einen Konfigurations-Snapshot im RAM;
  Änderungen über Admin-API/GitOps einer Instanz erreichen alle anderen per `NOTIFY` in
  Millisekunden. Sticky Sessions sind für die Datenebene nicht nötig.
- **Admin-UI-Sessions** liegen in der Datenbank – jede Instanz bedient jede Session.
- **MCP-Sitzungen** (`/mcp`) sind instanzgebunden: Load Balancer mit Session-Affinität auf
  den Header `Mcp-Session-Id` bzw. Client-IP, sonst muss der Client die Sitzung neu aufbauen.
- **Ereignis-Webhooks** werden clusterweit nur einmal zugestellt (`events.dedup`).
- **Snapshot-Cache** (`snapshot.cache_file`, je Instanz, verschlüsselt): erlaubt den
  Kaltstart, wenn die Datenbank beim Start nicht erreichbar ist; `/readyz` meldet dann
  `degraded`. Höchstalter `snapshot.cache_max_age_secs` (Standard 24 h).
- **Datenbankausfall im Betrieb:** Der Proxy arbeitet mit dem letzten Snapshot weiter
  (Keys, Modelle, Limits); Admin-API-Schreibzugriffe, Request-Log und Budget-Abgleich
  scheitern bzw. puffern, bis die Datenbank zurück ist. Nach `snapshot.stale_after_secs`
  (300 s) meldet `/readyz` `degraded` (weiter HTTP 200, damit nicht alle Instanzen
  gleichzeitig aus dem Load Balancer fallen).
- Migrationen laufen beim Start jeder Instanz; Postgres serialisiert sie über eine
  Advisory-Sperre, gleichzeitiges Starten ist sicher.

## 4. Backup und Restore

Zu sichern sind **Datenbank** und **Master-Key** (getrennt aufbewahren) sowie die
Startkonfiguration. Caches (Snapshot-Cache, Redis, semantischer Index) sind rekonstruierbar.

### Postgres

```sh
pg_dump -Fc -h db -U aigw aigw > aigw-$(date +%F).dump           # konsistent, im Betrieb
pg_restore --clean --if-exists -h db -U aigw -d aigw aigw-2026-11-13.dump
```

Für kurze RPO: kontinuierliche WAL-Archivierung (z. B. pgBackRest, Barman, Managed-Backups
des Cloud-Anbieters).

### SQLite

SQLite läuft im WAL-Modus; eine einfache Dateikopie im Betrieb ist **nicht** konsistent.

```sh
# im Betrieb (sqlite3 auf dem Host, nicht im distroless-Image):
sqlite3 /var/lib/docker/volumes/aigw-data/_data/aigw.db ".backup '/backup/aigw-$(date +%F).db'"
# oder: Container kurz anhalten, dann aigw.db, aigw.db-wal, aigw.db-shm zusammen kopieren
```

Restore: Instanz stoppen, Datei(en) zurückkopieren (`-wal`/`-shm` der alten Datei löschen),
Besitzer 65532 setzen, starten.

### Logische Sicherung der Konfiguration

Zusätzlich (nicht als Ersatz) lässt sich der Ressourcenbestand versionieren:
`aigw config export -c aigw.yaml --api-keys include --out bestand.yaml`. Der Export enthält
**keine** Geheimnisse (nur Verweise/Platzhalter) und keine Laufzeitdaten (Request-Log,
Nutzung, Audit-Log, Key-Hashes) – er ersetzt kein Datenbank-Backup, eignet sich aber zum
Umzug zwischen SQLite und Postgres (exportieren, Ziel-DB per `apply` befüllen, Geheimnisse
über die `AIGW_SECRET_*`-Variablen nachliefern; API-Keys müssen neu ausgegeben werden).

### Audit-Log prüfen

Nach einem Restore die Hash-Kette prüfen: `aigw audit verify -c aigw.yaml` bzw.
`GET /admin/api/v1/audit-log/verify`.

## 5. Upgrade und Migrationen

- Schema-Migrationen sind im Binary eingebettet (`crates/aigw-store/migrations/{sqlite,postgres}/`)
  und laufen **automatisch beim Start** (`aigw serve`, ebenso `aigw config …` mit `--config`),
  idempotent und je Migration in einer Transaktion.
- **Vor jedem Upgrade ein Backup** (Datenbank). Eine Datenbank mit neuerem Schemastand
  lässt sich von einer älteren Version **nicht** öffnen (Start bricht mit
  „Migration … passt nicht zu diesem Stand“ ab); ein Rückschritt geht nur über das Backup.
- Ablauf bei mehreren Instanzen: Backup → eine Instanz mit neuer Version starten (migriert) →
  übrige Instanzen rollierend ersetzen. Bis zum Ende des Rollouts laufen alte und neue Version
  parallel gegen das neue Schema. Ziel ist, dass Migrationen innerhalb einer Minor-Version nur
  additiv sind; Ausnahmen (Rollout mit Unterbrechung) nennt der [CHANGELOG](../../CHANGELOG.md).
- Konfigurationsänderungen (neue/umbenannte Felder) stehen im CHANGELOG; vor dem Rollout
  `aigw config check` mit dem neuen Binary laufen lassen (unbekannte Felder sind ein Fehler).
- Datenbanken aus der Entwicklungszeit vor AIGW-73 (gemeinsamer Migrationssatz) werden nicht
  übernommen: neu anlegen und per `aigw config export`/`apply` übertragen.
