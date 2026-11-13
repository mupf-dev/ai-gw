# ai-gw – Limits, Budgets und Kostenabrechnung (AIGW-30, AIGW-31, AIGW-67)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §3 Schritt 4 und §8, [05-zaehler.md](05-zaehler.md),
[10-routing.md](10-routing.md)
Code: `crates/aigw-proxy/src/limits/`, `crates/aigw-billing`, `crates/aigw-store/src/budget.rs`,
`crates/aigw-admin/src/usage.rs`

## 1. Überblick

```
Handler ─► Auth ─► Modellfreigabe ─► Limiter::admit ─────────────► Routing (exec) ─► Upstream
                                     │ Budgets prüfen (harte Grenze)   │ je Versuch: Deployment-
                                     │ alle Limits: 1× reserve          │ Kapazität über counters
                                     └► Admission ──► Recorder ─────────┴──► finish (Ende/Abbruch)
                                                                         │ Kosten = Preis × Usage
                                                                         │ Budgets buchen
                                                                         └ Reservierungen settle
```

- **Eine Einhängestelle** im Handler (`handlers::chat_inner`): nach Schlüsselprüfung und
  Modellfreigabe, vor dem Routing, genau ein Aufruf `st.limits.admit(…)`. Die Plugin-Kette
  (AIGW-36) hängt sich unabhängig davor/danach ein.
- **Abschluss** über den bestehenden Recorder-Pfad: Die `Admission` liegt im `Recorder` und wird
  in `Recorder::finish` abgeschlossen – regulär nach dem letzten Byte oder aus `Drop` beim
  Client-Abbruch. Dort entstehen `RequestRecord.cost` (→ Request-Log und Minuten-Rollups, Spalte
  `cost`), die Budgetbuchung und der Abgleich der Reservierungen.
- **Eine Reservierung je Client-Anfrage:** Retries und Fallbacks (AIGW-25) laufen innerhalb
  dieser Reservierung. Nur die Deployment-Kapazität wird je Versuch gezählt (sie beschreibt die
  Last am Deployment).

| Modul | Inhalt |
|---|---|
| `aigw-proxy/src/limits/mod.rs` | `Limiter` (Zähler-Backend, Budgets, Einstellungen), `admit`, `Admission::complete`, Ablehnungen |
| `aigw-proxy/src/limits/plan.rs` | Limits aus dem Snapshot → `LimitCheck`s, Zählerschlüssel |
| `aigw-proxy/src/limits/headers.rs` | `x-ratelimit-*` bzw. `anthropic-ratelimit-*` |
| `aigw-proxy/src/routing/exec.rs` | Deployment-Kapazität je Versuch über `aigw-counters` |
| `aigw-billing` (neues Crate) | Kosten (`cost`), Kalender-Zeiträume (`calendar`), Budgetstände mit Abgleich (`budget`) |
| `aigw-store/src/budget.rs` | `BudgetLedger` für SQLite/Postgres (`budget_usage`) |
| `aigw-admin/src/usage.rs` | `GET /admin/api/v1/usage/budgets` |

**Warum ein eigenes Crate `aigw-billing`?** Die Budget- und Kostenlogik wird von drei Schichten
gebraucht: vom Proxy (prüfen, buchen), von `aigw-store` (implementiert den Ledger) und von der
Admin-API (Zeiträume für die Übersicht). Als Modul im Proxy müsste die Admin-API vom Proxy
abhängen, als Teil von `aigw-store` hinge der Hot Path an sqlx. `aigw-billing` hängt nur an
`aigw-core` (plus `jiff` für Zeitzonen) und hat weder Datenbank- noch HTTP-Code.

## 2. Konfiguration

Limits und Budgets stehen an den Ressourcen (Admin-API/GitOps, Felder aus AIGW-9):

```yaml
apiVersion: aigw/v1alpha1
kind: Team
metadata: {name: research}
spec:
  organization: acme
  limits:
    - {metric: requests, limit: 600, window: minute}          # RPM
    - {metric: tokens, limit: 2000000, window: day}            # TPD
    - {metric: tokens, limit: 200000, window: minute, modelGroup: gpt-4o}   # nur für gpt-4o
  budget:
    amount: "500"            # Abrechnungswährung, Festkomma (9 Nachkommastellen)
    period: month            # day | week | month | total
    warnAtPercent: [50, 80, 100]
    hardLimit: true          # false = nur warnen
---
kind: ModelGroup
metadata: {name: gpt-4o}
spec:
  limits: [{metric: tokens, limit: 1000000, window: minute}]      # global für die Gruppe
---
kind: Price
metadata: {name: gpt-4o-2026-10}
spec:
  provider: openai
  model: gpt-4o
  validFrom: "2026-10-01T00:00:00Z"
  perMillionTokens: {input: "2.50", cachedInput: "1.25", output: "10"}
```

Startkonfiguration:

```yaml
counters:
  backend: local              # local | postgres-sync | redis (05-zaehler.md)
limits:
  default_max_tokens: 4096    # Reservierung ohne max_tokens
  response_headers: true      # Rate-Limit-Header auch bei Erfolg (bei 429 immer)
billing:
  timezone: UTC               # IANA-Name, z. B. Europe/Berlin
  budget_sync_ms: 1000        # Abgleich mit budget_usage
```

## 3. Limits (AIGW-30)

### 3.1 Ebenen, Metriken, Schlüssel

| Ebene | Quelle | Zählerschlüssel |
|---|---|---|
| API-Key | `ApiKey.spec.limits` | `key:<id>:<metrik>[:<gruppe>]` |
| Team | `Team.spec.limits` | `team:<id>:<metrik>[:<gruppe>]` |
| Organisation | `Organization.spec.limits` | `org:<id>:<metrik>[:<gruppe>]` |
| Modellgruppe (global) | `ModelGroup.spec.limits` | `group:<id>:<metrik>` |
| Deployment (Kapazität, §5) | `Deployment.spec.capacity` | `dep:<id>:requests`, `dep:<id>:tokens` |

- Metriken: `requests` (RPM/RPD), `tokens` (TPM/TPD), `input-tokens`, `output-tokens`; Fenster
  `second`, `minute`, `hour`, `day` (gleitend, 05 §2). Dasselbe Limit in zwei Fenstern sind
  zwei Zähler.
- `modelGroup` an einem Key-/Team-/Org-Limit: Das Limit gilt nur für Anfragen an diese Gruppe
  (Name oder Alias); im Schlüssel steht der kanonische Name. Ohne Snapshot (`AllowAll`) entfallen
  solche Limits.
- Limits gelten für die **angefragte** Modellgruppe; ein Fallback auf eine andere Gruppe
  (AIGW-25) verbraucht kein zusätzliches Kontingent der Fallback-Gruppe.

### 3.2 Reservierung und Abgleich

- Reserviert wird mit **einem** `CounterBackend::reserve` über alle Ebenen: alle oder keines
  (bei Redis atomar in einem Lua-Skript, sonst unter Slot-Sperren, 05 §3).
- Menge je Metrik: `requests` = 1 (nie korrigiert), `tokens` = geschätzte Eingabe +
  `max_tokens`, `input-tokens` = geschätzte Eingabe, `output-tokens` = `max_tokens`.
  Ohne `max_tokens` gilt `limits.default_max_tokens` (Standard 4096). Die Eingabe wird wie
  im Recorder geschätzt (≈ 1 Token je 4 Bytes Text, Tool-Aufrufe und -Ergebnisse
  eingeschlossen).
- **Abgleich** (`settle`) mit der tatsächlichen Usage am Ende der Anfrage, je Metrik mit dem
  passenden Wert (Summe, Eingabe, Ausgabe). Dafür wird die eine Reservierung mit
  `Reservation::split_off` (neu in `aigw-counters`) nach Metrik geteilt. Der Abgleich läuft
  als Tokio-Task, damit `finish` (auch aus `Drop`) nie wartet.
- **Client-Abbruch im Stream:** Der Recorder schätzt die Usage aus dem bis dahin gestreamten
  Text (`usage_estimated = true`); damit wird abgeglichen. **Fehler vor dem Upstream** bzw.
  ohne Antwort: Tokens werden auf 0 korrigiert, die Anfrage zählt für `requests`.
- Ist die Menge größer als das Limit (z. B. `max_tokens` über dem TPM-Limit), wird ohne
  `Retry-After` abgelehnt (Hinweis „max_tokens verringern“) – Warten hilft nicht.

### 3.3 Ablehnung und Header

Status `429`, Fehler im Eingangsformat (`rate_limit_exceeded` bzw. Anthropic
`rate_limit_error`), `Retry-After` in ganzen Sekunden (aufgerundet).

| OpenAI (`/v1/chat/completions`) | Anthropic (`/v1/messages`) |
|---|---|
| `x-ratelimit-limit-requests` / `-tokens` | `anthropic-ratelimit-requests-limit`, `-tokens-limit`, `-input-tokens-limit`, `-output-tokens-limit` |
| `x-ratelimit-remaining-requests` / `-tokens` | `anthropic-ratelimit-…-remaining` |
| `x-ratelimit-reset-requests` / `-tokens` (Dauer, z. B. `6m0s`, `1.5s`, `250ms`) | `anthropic-ratelimit-…-reset` (RFC 3339) |

- Je Metrik meldet der Header das **knappste** Limit (kleinster Rest) über alle Ebenen; OpenAI
  kennt nur `tokens`, dort steht das knappste aus `tokens`/`input-tokens`/`output-tokens`.
- Bei Erfolg: Stand **nach** der Reservierung (abschaltbar mit `limits.response_headers`).
  Bei Ablehnung: nur die abgelehnte Metrik (die übrigen wurden nicht geprüft/gebucht); Reset =
  Wartezeit.
- Gibt es kein Limit, fehlen die Header.

## 4. Kosten (AIGW-31)

- Preis: `Snapshot::price_at(provider, providerModel, Anfragezeitpunkt)` – jüngster Preis mit
  `validFrom ≤ Start der Anfrage`, anbieterspezifisch vor generisch. Ein Preiswechsel wirkt
  damit ab seinem Zeitpunkt, laufende Anfragen behalten den alten Preis.
- Formel (IR-Usage zählt inklusive, 03 §2):

  | Token-Art | Menge | Preis (fehlt → Ersatz) |
  |---|---|---|
  | Eingabe | `input − cache_read − cache_write` | `input` (→ 0) |
  | Cache lesen | `cache_read` | `cachedInput` (→ `input`) |
  | Cache schreiben | `cache_write` | `cacheWrite` (→ `input`) |
  | Ausgabe | `output − reasoning` | `output` (→ 0) |
  | Reasoning | `reasoning` | `reasoning` (→ `output`) |

  plus `perRequest`. Exakt in `i128` gerechnet (Nano-Einheiten × Tokens), erst die Summe wird
  auf ganze Nano-Einheiten abgerundet. Audio-/Bildpreise werden noch nicht angewendet.
- Ergebnis in `RequestRecord.cost` → `request_log.cost`, `usage_rollup.cost` (bestehender Pfad
  AIGW-43). `None` (= 0 in den Rollups), wenn kein Preis hinterlegt ist oder die Anfrage ohne
  Verbrauch scheiterte.
- Gerechnet wird mit dem zuletzt verwendeten Deployment (nach Retries/Fallbacks das, dessen
  Antwort der Client bekam) und der gemeldeten bzw. geschätzten Usage.

## 5. Deployment-Kapazität für das Routing

`capacity.rpm`/`capacity.tpm` eines Deployments zählen jetzt über dasselbe `aigw-counters`-Backend
wie die Limits (bisher lokale Minutenfenster in `routing/state.rs`):

- Beim Start **jedes Versuchs** reserviert `exec` `dep:<id>:requests` (1) und
  `dep:<id>:tokens` (geschätzte Eingabe + `max_tokens`) im Minutenfenster. Am Ende des Versuchs
  wird die Token-Reservierung auf die gemeldete Usage korrigiert (Fehler/Abbruch ohne Usage: 0).
- Lehnt der Zähler ab, merkt sich `DeploymentState` die Wartezeit (`capacity_until_ms`, nur
  Atomics) – solange überspringt die Auswahl das Deployment ohne Zählerzugriff (nächste
  Prioritätsgruppe; alle gesperrt → `503 no_deployment_available` mit `Retry-After`). Passt nur
  diese eine Anfrage nicht mehr in die Tokens der Minute, wird das Deployment nur für sie
  ausgelassen.
- Mit `postgres-sync`/`redis` gilt die Kapazität damit für alle Instanzen gemeinsam (vorher je
  Instanz). Ohne konfigurierte Kapazität gibt es keinen Zählerzugriff.
- Änderung gegenüber 10 §3: TPM wird nun vorab mit der Schätzung reserviert statt erst am Ende
  gezählt; das schützt PTU-Deployments besser vor Überbuchung.

## 6. Budgets (AIGW-31)

### 6.1 Zeiträume

`day`, `week` (ab Montag), `month` als Kalenderzeiträume in `billing.timezone` (IANA, Standard
UTC; Sommerzeit über die eingebettete tzdb von `jiff`, ein Tag kann 23/25 h haben); `total` ohne
Ende. Der Beginn des Zeitraums (ms seit Epoche) ist Teil des Schlüssels in `budget_usage` –
neue Zeiträume beginnen damit automatisch bei 0. Gebucht wird in den Zeitraum des
Anfragestarts.

### 6.2 Prüfung, Buchung, Warnungen

- **Vor dem Upstream-Aufruf** prüft `admit` die Budgets von Key, Team und Organisation mit
  `hardLimit: true`: Ist der bekannte Verbrauch ≥ Betrag, folgt `429` mit Code
  `budget_exceeded` (Format des Eingangs) und `Retry-After` bis zum Beginn des nächsten
  Zeitraums (bei `total` ohne `Retry-After`).
- **Nach der Anfrage** bucht `Admission::complete` die Kosten auf alle Budgets des Keys
  (auch ohne harte Grenze).
- **Warnschwellen** (`warnAtPercent`) und das Erreichen des Betrags lösen je Zeitraum, Schwelle
  und Instanz einmal ein `BudgetEvent` aus. Empfänger ist ein `BudgetEventSink`; Standard ist
  das Log (`warn`, Felder level/target/period/percent/used/amount). Mit `events.webhooks`
  gehen die Ereignisse zusätzlich als Webhook hinaus (§6.5, AIGW-61).
- Es wird **keine** geschätzte Kostenreservierung vorgenommen: Gleichzeitig laufende Anfragen
  können ein Budget um ihre Kosten überschreiten (wie bei LiteLLM). Siehe offene Fragen.

### 6.3 Persistenz und Genauigkeit

- Stand im RAM je (Ebene, Ziel, Zeitraum, Beginn): `synced` (Summe aller Instanzen laut letztem
  Abgleich) + `in_flight` + `pending` (eigene, noch nicht geschriebene Deltas).
- **Abgleich** alle `billing.budget_sync_ms` (Standard 1 s) in einer Transaktion: Deltas
  additiv schreiben (`INSERT … ON CONFLICT DO UPDATE SET amount = budget_usage.amount +
  excluded.amount`, SQLite und Postgres, kein Read-Modify-Write zwischen Instanzen), danach die
  Summen aller geführten Budgets zurücklesen. Beim Herunterfahren wird ein letztes Mal
  geschrieben.
- **Erster Zugriff** auf ein Budget je Zeitraum und Instanz liest den Stand einmal aus der
  Datenbank (Zeitlimit 2 s), damit ein Neustart oder eine neue Instanz nicht bei 0 beginnt.
- **Genauigkeit** wie `postgres-sync` (05 §3.2): je Instanz exakt, im Cluster sieht jede
  Instanz fremden Verbrauch mit bis zu einem Abgleichintervall Verzögerung. Im ungünstigsten
  Fall überschreiten n Instanzen ein Budget innerhalb eines Intervalls um die Kosten der in
  dieser Zeit beendeten Anfragen. Für Budgets (Beträge je Tag/Monat) ist das unkritisch.
- **Ausfall der Datenbank:** lokal weiterzählen (`warn` einmal, danach `debug`), Deltas bleiben
  vorgemerkt und werden beim nächsten erfolgreichen Abgleich nachgeholt (`info`).
- `budget_usage` (Migration 0001) bleibt unverändert, **keine Migration 0004 nötig**.
  `Store::prune_budget_usage(before)` löscht alte Zeiträume (Aufruf durch die Aufbewahrung folgt).

### 6.5 Ereignis-Webhooks (AIGW-61)

Budget-Ereignisse gehen über `aigw_billing::WebhookEvents` (ein `BudgetEventSink`, der
zusätzlich loggt) an den gemeinsamen Ereignis-Zusteller `aigw_webhook::EventDispatcher`. Er ist
für weitere Ereignistypen vorbereitet (`Event {id, kind, data, dedup}`).

```yaml
events:
  queue_capacity: 1024      # Puffer; voll → Ereignis verworfen (Zähler dropped, warn)
  concurrency: 8            # gleichzeitige Zustellungen
  retry_initial_ms: 1000    # Backoff: verdoppelt, Zufallsanteil 50–100 %, Retry-After beachtet
  retry_max_ms: 60000
  dedup: true               # im Cluster nur einmal (Tabelle event_dedup)
  webhooks:
    - name: finance
      url: https://hooks.example.org/aigw      # SSRF-Regeln wie Deployments (admin.allow_*)
      events: [budget.warning, budget.exhausted] # auch budget.* oder *; leer = alle
      secret: {credential: hooks}                # oder {env: NAME} / {file: /pfad}
      timeout_ms: 5000
      max_attempts: 6
```

**Typen und Nutzdaten:** `budget.warning` (`data.percent`) und `budget.exhausted`. Body:

```json
{"id": "<uuidv7>", "type": "budget.warning", "occurredAt": "2026-11-13T10:00:00Z",
 "data": {"level": "team", "targetId": "<id>", "period": "month",
          "periodStartMs": 1793491200000, "periodEndMs": 1796083200000,
          "percent": 80, "amount": "100", "used": "80.5", "hardLimit": true}}
```

Header: `x-aigw-event-type`, `x-aigw-delivery` (= Ereignis-ID, gleich über Wiederholungen →
Idempotenz beim Empfänger), `x-aigw-attempt`, `x-aigw-signature` (HMAC-SHA256 wie beim Plugin,
siehe [12-plugins.md](12-plugins.md) §5 `webhook`).

**Zustellung:** asynchron (der Hot Path ruft nur `try_send`), 2xx = zugestellt.
Zeitüberschreitung, Verbindungsfehler, 408/425/429/5xx werden wiederholt; nach `max_attempts`
bzw. bei anderen 4xx zählt `aigw_event_webhook_dead_letter_total` (Log `error` mit
Ereignis-ID). Ereignisse werden **nicht** persistiert: Puffer und laufende Wiederholungen gehen
bei einem Absturz verloren; beim geordneten Herunterfahren wird bis zu 5 s weiter zugestellt.
Zähler unter `/metrics`: `aigw_event_webhook_{enqueued,delivered,retries,dead_letter,dropped,
deduplicated,dedup_errors}_total`.

**Deduplizierung im Cluster:** Jede Instanz meldet eine Schwelle je Zeitraum einmal (RAM), nach
einem Neustart oder auf mehreren Instanzen also mehrfach. Deshalb beansprucht die Instanz vor
dem Senden den Schlüssel `budget:<ebene>:<ziel-id>:<zeitraum>:<beginn-ms>:<warning-<p>|exhausted>`
in `event_dedup` (Migration 0006, `INSERT … ON CONFLICT DO NOTHING`, SQLite und Postgres). Nur
die Instanz, deren Zeile angelegt wurde, sendet; die anderen zählen `deduplicated`. Gültig bis
Ende des Zeitraums + 1 Tag (`total`: 100 Jahre); abgelaufene Zeilen löscht der nächste Anspruch.
Ist die Datenbank nicht erreichbar, wird trotzdem gesendet (lieber doppelt als gar nicht,
`dedup_errors`). Grenze: Stirbt die beanspruchende Instanz vor der Zustellung, fällt das
Ereignis aus (höchstens einmal statt mindestens einmal – siehe offene Fragen in AIGW-61).

### 6.4 Admin-Lesezugriff

`GET /admin/api/v1/usage/budgets[?level=organization|team|api-key]` (Berechtigung `Read`):
alle Budgets (Organisationen, Teams, aktive Keys) mit laufendem Zeitraum (RFC 3339), Betrag,
Verbrauch, Rest, Prozent, Schwellen, `hardLimit`, `exhausted` sowie die Zeitzone. Quelle ist
`budget_usage` (hinkt bis zu einem Abgleichintervall hinterher).

```json
{"timezone": "UTC", "at": "2026-11-13T10:00:00Z", "items": [
  {"level": "team", "targetId": "0193…", "targetName": "research", "period": "month",
   "periodStart": "2026-11-01T00:00:00Z", "periodEnd": "2026-12-01T00:00:00Z",
   "amount": "500", "used": "123.45", "remaining": "376.55", "percent": 24,
   "warnAtPercent": [50, 80, 100], "hardLimit": true, "exhausted": false}]}
```

## 6a. Grenzen Key ≤ Team ≤ Organisation (AIGW-67)

Code: `aigw_core::validate::bounds` (Regeln, gemeinsam für Admin-API und GitOps),
`crates/aigw-admin/src/bounds.rs` (Rollen, `force`, `cascade`), `aigw-store`
(`WriteExtras`: Kürzungen und Audit in derselben Transaktion), GitOps `plan.rs`
(`check_bounds` in `check_rules`).

### 6a.1 Regeln

| Was | Regel |
|---|---|
| Limits | Ein Limit von Key bzw. Team darf ein **vergleichbares** Limit von Team bzw. Organisation nicht überschreiten. Vergleichbar: gleiche Metrik, gleiches Fenster (`concurrency`: ohne Fenster) und das obere Limit gilt für die Modellgruppe des unteren (gleiche Gruppe – Aliasse werden aufgelöst – oder oben ohne Gruppe). Geprüft wird gegen **alle** Ebenen darüber (ein Key auch direkt gegen die Organisation), gemeldet das engste Limit. |
| Fenster | Verschiedene Fenster werden **nicht** umgerechnet (Key `tokens/hour` gegen Team `tokens/minute` ist kein Verstoß). |
| Gruppenlimits | Oben gruppenbezogen, unten ohne Gruppe → nicht vergleichbar (das untere gilt für mehr Anfragen). Oben ohne Gruppe, unten gruppenbezogen → vergleichbar. |
| Budgets | Nur bei **gleichem Zeitraum** (`day`, `week`, `month`, `total`): Betrag unten ≤ Betrag oben. Keine Umrechnung (Monat ≠ 4 × Woche). Es gibt eine Abrechnungswährung, Beträge sind direkt vergleichbar. `hardLimit`/Warnschwellen werden nicht verglichen. |
| Modelle | Schränkt das Team `allowedModels` ein, muss jede Freigabe des Keys darin liegen (Name oder Alias, kanonisch verglichen). Leere Liste am Key = „alles, was das Team erlaubt“ → immer zulässig. |
| Fehlt etwas | Ohne vergleichbares Limit/Budget oben gibt es keine Grenze; ohne Limit unten gilt zur Laufzeit ohnehin das obere. Keys ohne Team haben keine Grenzen. |

### 6a.2 Admin-API

| Fall | Ergebnis |
|---|---|
| Geschriebenes Objekt (Key, Team) verstößt selbst | `422 validation-failed`, `detail` mit allen Verstößen, `violations[]` (`level`, `name`, `field`, `message`) |
| Team/Organisation wird gesenkt, bestehende Keys/Teams lägen darüber | `409 conflict` mit `violations[]` je betroffenem Objekt |
| … mit `?cascade=clamp` | Betroffene Teams/Keys werden in **derselben Transaktion** auf die neue Grenze gesetzt (Limit = Grenze, Budget = Grenze, nicht erlaubte Modelle entfernt); Audit `clamp` je Objekt (`before`/`after`) |
| … Key verlöre alle Modellfreigaben | auch mit `clamp` `409` (eine leere Liste hieße „alle“ – das wäre eine Erweiterung) |
| `?force=true` | nur **globaler** `admin` (sonst `403`, auch für `admin` einer Organisation); lässt eigene Verstöße und betroffene Objekte zu; zusätzlicher Audit-Eintrag `force` am Objekt mit `{force, violations, affected}` |

- Gilt für `team-admin`, `org-admin`, eingeschränkte `admin` und den globalen `admin` gleich
  (hart); nur `force` unterscheidet.
- Parameter: `POST/PUT /teams…`, `/organizations…` mit `?force=` und `?cascade=reject|clamp`;
  `POST/PUT /api-keys…` mit `?force=` (unbekannte Parameter → `400`).
- **Nur neue Verstöße blockieren:** Betroffene Objekte werden nur gemeldet, wenn der Verstoß
  durch die Änderung entsteht (Prüfung zusätzlich gegen die bisherigen Grenzen). Ältere
  Verstöße (vor AIGW-67 entstanden oder per `force`) blockieren andere Änderungen nicht;
  `clamp` bereinigt sie mit. Das geschriebene Objekt selbst muss immer passen.
- Rotation übernimmt die Spezifikation unverändert (keine neue Prüfung); Sperren/Entsperren
  prüft nicht.

```json
PUT /admin/api/v1/teams/platform   → 409
{"type": "urn:aigw:problem:conflict", "code": "conflict", "status": 409,
 "detail": "1 bestehende(s) Objekt(e) lägen über den neuen Grenzen – anpassen oder ?cascade=clamp verwenden: Key \"ci\": limits[0]: tokens/minute = 5000 überschreitet das Limit von Team \"platform\" (4000)",
 "violations": [{"level": "api-key", "name": "ci", "field": "limits[0]",
                 "message": "tokens/minute = 5000 überschreitet das Limit von Team \"platform\" (4000)"}]}
```

### 6a.3 GitOps

Dieselben Regeln in `check_rules` (Plan, Trockenlauf und Anwenden); alle Verstöße gesammelt
als `Issue`s. Geprüft wird jedes Team/jeder Key, bei dem das Objekt selbst oder eine Ebene
darüber im Manifest steht:

- Objekt im Manifest: Verstoß an seinem Dokument (`limits[0]: …`).
- Bestehendes Objekt außerhalb des Manifests (z. B. Keys bei `--api-keys ignore` oder ohne
  `--prune`): Verstoß am Dokument der geänderten Ebene (`bestehend: Key "ci": …`), aber nur,
  wenn er durch das Manifest neu entsteht.
- Kein `force`, kein `cascade` – das Manifest muss stimmig sein (Keys im Manifest anpassen).

## 7. Messwerte

Release-Build auf der Buildbox (AMD EPYC, geteilt), `cargo test --release -p aigw-proxy --lib
limits::tests::overhead -- --ignored --nocapture`: `admit` + `complete` je Anfrage über `local`,
Current-Thread-Laufzeit, 200 000 Anfragen, ohne Budgets/Preis.

| Fall | je Anfrage |
|---|---|
| ohne Limits (Key ohne Limits) | 0,14 µs |
| 5 Limits auf 4 Ebenen (Key RPM + TPM, Team TPM, Org RPD, Gruppe TPM) | 2,6 µs |

Ziel < 5 µs eingehalten. Den Großteil machen die Zählerschlüssel (Formatierung je Anfrage), die
Slot-Sperren der Engine (05 §6: 0,63 µs) und der Abgleich-Task aus. Budgets kosten im Hot Path
zusätzlich je Budget einen Map-Zugriff und eine Mutex-Sperre (keine I/O).

## 8. Tests

- `aigw-billing`: Kosten (alle Token-Arten, Ersatzpreise, Rundung, Preiswechsel über
  `price_at`), Kalender (UTC, Woche ab Montag, Monatswechsel, Europe/Berlin mit Sommerzeit),
  Budgets (harte Grenze, Warnschwellen einmalig, zwei Instanzen mit gemeinsamem Ledger, neue
  Instanz liest Stand, Ledger-Ausfall mit Nachholen, Bereinigung abgelaufener Zeiträume).
- `aigw-store/tests/budget.rs`: Ledger gegen SQLite und Postgres (additiv, Rücklesen, > 100
  Schlüssel, zwei `Budgets`-Instanzen auf einer Datenbank, Bereinigung).
- `aigw-counters`: `Reservation::split_off`.
- `aigw-proxy` (Unit): Planung (Sortierung, Gruppenbezug über Alias, `concurrency` ignoriert),
  Atomarität über Ebenen, Abgleich je Metrik, zu große Anfrage ohne `Retry-After`, Header.
- `aigw-proxy/tests/limits.rs` (HTTP): `requests` auf jeder Ebene (Key, Team, Org, Gruppe) mit
  429, `Retry-After` und Headern; Atomarität (Team lehnt ab → Key unverändert); Token-Abgleich je
  Metrik; Anthropic-Header und -Fehlerformat; Stream-Abbruch → Abgleich mit geschätzter Usage;
  Retries mit einer Reservierung je Client-Anfrage und Kapazität je Versuch; Kapazität zwischen
  zwei Instanzen über ein Backend; Kosten mit altem/aktuellem/künftigem Preis, Budget mit
  Warnschwelle, Erschöpfung und `budget_exceeded`; Kosten bei Streams; zwei Instanzen mit
  gemeinsamem Limit über Redis (`AIGW_TEST_REDIS_URL`) und `postgres-sync`
  (`AIGW_TEST_POSTGRES_URL`).

## 9. Offene Punkte

- `concurrency`-Limits (gleichzeitig offene Anfragen) sind im Ressourcenmodell vorgesehen, aber
  nicht umgesetzt (brauchen einen eigenen Zählertyp ohne Fenster bzw. mit Lease-Ablauf).
- Budgets ohne Reservierung geschätzter Kosten (siehe Fragen).
- Budget-Ereignisse tragen nur die Ziel-ID (kein Name); Anreicherung aus dem Snapshot folgt bei Bedarf.
- Audio-/Bildpreise, sobald die IR-Usage diese Mengen führt.
- Budgets je Nutzer (`user`) und je Modellgruppe sind im Ressourcenmodell nicht vorgesehen.
- Rate-Limit-Header für Budgets (z. B. `x-aigw-budget-remaining`) – bisher nur im Fehlertext.
- AIGW-67: Budgets verschiedener Zeiträume und Limits verschiedener Fenster werden nicht
  verglichen; `force`/`cascade` in GitOps nicht vorgesehen (siehe Fragen im Bericht).
