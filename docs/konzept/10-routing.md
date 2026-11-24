# ai-gw – Routing, Retries, Fallbacks und Circuit Breaker (AIGW-24, AIGW-25, AIGW-26)

Stand: 2026-11-13 · Status: umgesetzt, Grundlage: [01-architektur.md](01-architektur.md) §6,
[06-streaming-und-anbieter.md](06-streaming-und-anbieter.md)

## 1. Überblick

```
Handler ─► ModelRoutes::route(modell) ─► GroupRoute ─► exec::execute ──► ChatProvider (Upstream / Mock)
             (Name oder Alias)            Prioritätsgruppen   │ Auswahl (Strategie, Breaker, Kapazität)
                                          Strategie, Policy   │ Retries/Backoff/Retry-After/Budget
                                          Fallback-Kette      │ Fallback auf andere Modellgruppe
                                                              └► DeploymentState (EWMA, offene Anfragen,
                                                                 Breaker, RPM/TPM) je Deployment-ID
SnapshotHandle ─subscribe()─► SnapshotRoutes::rebuild ─► RouteTable (ArcSwap, lock-frei gelesen)
                             └ Takt 1 s ─► Health Checks
```

| Modul (`crates/aigw-proxy/src/routing/`) | Inhalt |
|---|---|
| `mod.rs` | `GroupRoute` (Deployments nach Priorität, Strategie, Policy, Fallbacks), `Member`, `MemberSpec`, Auswahl `pick` |
| `select.rs` | Strategien `weighted`, `least-latency`, `least-busy`, `cost`; Zufall (wyrand je Thread) |
| `exec.rs` | Ausführung: Retries, Backoff mit Jitter, `Retry-After`, Gesamtbudget, Fallback-Ketten, Erfassung der Versuche |
| `breaker.rs` | Circuit Breaker (gleitendes Fenster, offen, halboffen mit Probe) |
| `health.rs` | Optionale aktive Health Checks |
| `state.rs` | `DeploymentState`: offene Anfragen, EWMA TTFT/Latenz, Breaker, Sperre nach `Retry-After`, Sperre bei erschöpfter Kapazität |
| `snapshot.rs` | `SnapshotRoutes` (implementiert `ModelRoutes`), `RouteTable`, `RoutesTask` |
| `policy.rs` | `GroupPolicy` aus `ModelGroup.spec.settings` (`retry`, `circuitBreaker`, `healthCheck`) |

`ModelRoutes::route(name) → Option<Arc<GroupRoute>>` ersetzt das bisherige
`resolve(name) → ChatProvider`. `StaticRoutes` (Tests, Benchmarks, `ProxyState::mock()`) bildet
jeden Namen auf eine `GroupRoute` mit genau einem Deployment ohne Retries, Breaker und
`Retry-After`-Sperre ab – das Verhalten bisheriger Tests bleibt unverändert.

## 2. Konfiguration

Alles steht an den Ressourcen (Datenbank/GitOps), nicht in der Start-Config:

```yaml
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata: {name: gpt-4o}
spec:
  aliases: [gpt-4o-latest]
  routing: weighted              # weighted (Standard) | least-latency | least-busy | cost
  fallbacks:                     # Kette in Reihenfolge
    - {modelGroup: gpt-4o-128k, on: [context-length]}
    - {modelGroup: claude-sonnet}            # on leer = any
  settings:
    retry:
      maxAttempts: 3             # Versuche je Modellgruppe inkl. des ersten (1–20)
      initialBackoffMs: 100
      maxBackoffMs: 2000
      budgetMs: 30000            # Gesamtbudget für neue Versuche inkl. Fallbacks
      maxRetryAfterMs: 10000     # längeres Retry-After → nicht warten
    circuitBreaker:
      enabled: true
      failureRate: 0.5           # (0, 1]
      minRequests: 5
      windowSecs: 30             # ≤ 3600
      openSecs: 30
    healthCheck:                 # ohne Abschnitt: aus
      intervalSecs: 30
      timeoutMs: 5000
      unhealthyThreshold: 2
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata: {name: gpt-4o-ptu}
spec: {modelGroup: gpt-4o, provider: azure-openai, providerModel: gpt-4o, credential: az,
       priority: 0, weight: 1, capacity: {rpm: 600, tpm: 300000}}
```

Die gezeigten Werte sind die Standardwerte. Unbekannte Felder in `retry`, `circuitBreaker`,
`healthCheck` sind Fehler (Tippfehler); andere Schlüssel in `settings` werden ignoriert.
`GroupPolicy::from_settings` ist öffentlich, damit die Admin-API (AIGW-65) beim Schreiben prüfen
kann. Eine ungültige Policy im Snapshot führt zu Standardwerten und einer Fehlermeldung im Log
(die Gruppe bleibt erreichbar). Zeitlimits (Verbindung, erstes Byte, Chunk-Pause, Gesamtzeit)
stehen je Deployment in `settings.timeouts` (siehe 06 §4).

Start-Config: `server.data.routing_headers: true` (Standard) setzt die Antwort-Header aus §7.

**Anbieterart `mock`:** Deployments mit `provider: mock` nutzen den eingebauten Echo-Anbieter
(kein Netzwerk). `providerModel` steuert das Verhalten: `mock` (Echo), `mock-error`
(Überlastung, retrybar), `mock-stream-error` (Abbruch nach dem ersten Textstück). Damit laufen
Smoke-Tests und Demos ohne echte Anbieter; der Mock-Fallback für beliebige Modellnamen im
Binary entfällt.

## 2b. Weitere Einstellungen der Modellgruppe (AIGW-70)

Alle Felder prüft `GroupPolicy::from_settings` (damit Admin-API und GitOps beim Schreiben):

| Einstellung | Werte (Standard fett) | Wirkung |
|---|---|---|
| `fallbackIgnoresGrants` | **false** \| true | Fallbacks ohne Freigabeprüfung (§5, F80) |
| `responseModel` | **upstream** \| requested | `model` im Antwort-Body: wie vom Anbieter gemeldet oder der angefragte Name (Gruppe/Alias); Header `x-aigw-model` trägt immer das Anbieter-Modell (F49) |
| `clientBetas` / `betaAllowlist` | **allow-all** \| allowlist \| deny / Liste | `anthropic-beta` der Clients weitergeben, filtern oder verwerfen (F60; die Betas des Deployments bleiben) |
| `exposeReasoning` | **true** \| false | Reasoning als `reasoning_content` an OpenAI-Clients (F10); zusätzlich `exposeReasoning: false` an Organisation, Team oder Key (Vererbung, F185) |
| `timeouts` | `{connectMs, firstByteMs, idleMs, totalMs}` | gilt für alle Deployments ohne eigenen Abschnitt (F44, 06 §3) |
| `thinkingBudgetPolicy` | **raise-max-tokens** \| cap-budget | Erbwert für Anthropic-Deployments (F59, 06 §3a) |
| `propagateTraceContext` | true \| false (Standard je Anbieterart) | Erbwert für die Trace-Weitergabe (F98, 07 §12) |

`n > 1` (F12): Anbieter mit nativem `n` (OpenAI, Azure, vLLM, Mistral) bekommen eine Anfrage;
sonst schickt die Ausführung `n` parallele Anfragen an **dasselbe** Deployment (ein Versuch;
schlägt eine fehl, wird der Versuch wie üblich wiederholt bzw. fällt zurück). RPM zählt einen
Versuch, TPM die geschätzten Tokens × `n` bzw. die summierte Usage; Limits/Budgets reservieren die
Ausgabe × `n` und rechnen die summierte Usage ab. Antworten mit `n > 1` werden nicht gecacht.

## 2a. Fähigkeiten einer Modellgruppe (AIGW-21)

```yaml
kind: ModelGroup
metadata: {name: text-embedding}
spec:
  capabilities: [embeddings]   # chat, embeddings, images, audio-transcription, audio-speech
```

`capabilities` ist seit F123 ein typisiertes Feld der Modellgruppe (Admin-API, GitOps,
OpenAPI, UI-Formular). Ältere Gruppen mit `settings.capabilities` werden beim Lesen
übernommen (Eingabe mit beidem und verschiedenen Werten → Fehler).

Ohne Angabe nimmt eine Gruppe nur **Chat** an (Chat Completions, Messages, Responses). Eine
Anfrage anderer Art an die Gruppe → `400 unsupported_capability` im Eingangsformat (auch für
Chat an eine reine Embedding-Gruppe). Zusätzlich muss mindestens ein Deployment die Art
technisch können (Adapter-`Capabilities`, z. B. Anthropic keine Embeddings); Deployments ohne
die Fähigkeit werden nie gewählt, Fallback-Gruppen ohne sie übersprungen. Unbekannte Werte
oder eine leere Liste sind Fehler (Standardwerte + Log, wie bei den übrigen Einstellungen).
Health Checks (Chat-Ping) laufen nur für Gruppen mit `chat`.

Technisch läuft jede Art über dieselbe Ausführung (`exec::execute_op` mit einer `Operation`:
`ChatOp`, `EmbeddingsOp`, `ImagesOp`, `TranscriptionOp`, `SpeechOp`); `exec::execute` (Chat)
ist ein dünner Wrapper. Retries, Backoff, `Retry-After`, Breaker, Fallbacks und RPM/TPM gelten
für alle Arten gleich (TPM zählt Embedding-Tokens bzw. gemeldete Bild-/Audio-Tokens).

## 3. Auswahl (AIGW-24)

1. **Prioritätsgruppen:** Deployments mit `priority` 0 vor 1 vor 2 … (z. B. PTU vor PAYG).
   Eine Gruppe ist **gesperrt**, wenn jedes Deployment darin
   - einen offenen Circuit Breaker hat (bzw. halboffen mit laufender Probe),
   - nach `Retry-After` des Anbieters gesperrt ist, oder
   - an seiner Kapazitätsgrenze ist (`capacity.rpm`/`capacity.tpm`, gleitende Minute über
     `aigw-counters`, also mit `postgres-sync`/`redis` instanzübergreifend; beim Start jedes
     Versuchs werden 1 Anfrage und geschätzte Eingabe + `max_tokens` reserviert, die Tokens am
     Ende des Versuchs auf die Usage korrigiert – Details in
     [11-limits-und-budgets.md](11-limits-und-budgets.md) §5).
   - sein **Budget** bis `switchAtPercent` verbraucht hat (`Deployment.spec.budget`,
     AIGW-126; Verbrauch aus dem RAM der Budgetstände, abgeglichen mit `budget_usage`). Vor
     jedem Versuch reserviert `strict` die geschätzten Kosten; passen sie nicht mehr in den
     Rest, wird das Deployment für diese Anfrage übersprungen. Abgerechnet wird nach der
     Antwort mit den tatsächlichen Kosten auf das Deployment, das geantwortet hat.
   Dann wird die nächste Gruppe genommen. Sind alle gesperrt: **503**
   `no_deployment_available` mit `Retry-After` = kürzeste verbleibende Sperre – außer alle
   (nicht ausgeschlossenen) Deployments sind **nur** wegen `Retry-After` des Anbieters oder
   erschöpfter Kapazität gesperrt (kein offener Breaker): dann **429**
   `deployments_rate_limited` mit `Retry-After` (F83; Drosselung, kein Ausfall). Sind alle
   wegen ihres Budgets ausgeschlossen: **429** `deployment_budget_exhausted` (Klasse `quota`,
   Fallback-Auslöser `rate-limit`/`any`; `Retry-After` bis zum nächsten Zeitraum, bei `total`
   ohne).
2. **Strategie** innerhalb der Gruppe:
   - `weighted`: gewichtet zufällig nach `weight` (alle 0 → gleichverteilt).
   - `least-latency`: kleinste EWMA (α = 0,3) der TTFT (Streams) bzw. Antwortzeit (sonst; fehlt
     einer der Werte, gilt der andere). Ohne Messwert gilt 0 → neue Deployments werden zuerst
     probiert. Alles bis +10 % über dem Besten gilt als gleich gut und wird gewichtet zufällig
     verteilt.
   - `least-busy`: wenigste offene Anfragen dieser Instanz; Gleichstand gewichtet zufällig.
   - `cost`: kleinster Preis (Eingabe + Ausgabe je 1 Mio. Tokens aus der Preisliste, zum
     Zeitpunkt des Snapshot-Aufbaus); ohne Preis zuletzt; Gleichstand gewichtet zufällig.
   - `budget-balanced` (AIGW-126): Kosten gleichmäßig verteilen. „Power of two choices“: zwei
     Kandidaten gewichtet zufällig ziehen, der mit dem geringeren Budgetverbrauch in Prozent
     (inkl. laufender Reservierungen dieser Instanz) gewinnt; Gleichstand → höheres Gewicht,
     sonst Zufall. Haben nicht alle Kandidaten ein Budget, zählen die absoluten Kosten
     (ohne Budget: Kosten dieser Instanz seit Start). Kein striktes Minimum, damit unter Last
     nicht alle gleichzeitigen Anfragen auf dasselbe Deployment fallen (Herdenverhalten).
     Rezept: [../betrieb/rezept-accounts-rotieren.md](../betrieb/rezept-accounts-rotieren.md).
3. Halboffene Breaker lassen genau eine Probe durch (`compare_exchange`); verliert ein
   Kandidat das Rennen, wird ohne ihn neu gewählt.

Die Auswahl liest nur Atomics (kein Lock, keine Allokation außer einem kleinen Puffer je
Anfrage). Gemessen (Release, Buildbox cpx42 geteilt, 2026-11-13, je 200 000 Auswahlen):

| Strategie | Deployments | ns je Auswahl |
|---|---|---|
| weighted | 1 | 64–128 |
| weighted | 4 (2 Prioritätsgruppen) | 65–136 |
| least-latency | 4 | 84–102 |
| least-busy | 4 | 83–105 |
| cost | 4 | 74–97 |
| weighted | 16 | 64–82 |

(`cargo test --release -p aigw-proxy --test routing selection -- --nocapture`; Streuung durch
die parallel genutzte Box.) Gegenüber dem gemessenen Gateway-Aufschlag von ≈ 130 µs (06 §9)
ist das vernachlässigbar.

## 4. Retries (AIGW-25)

- **Nur vor dem ersten Byte an den Client.** Bei Streams wartet der Proxy das erste Ereignis
  des Anbieters ab, bevor Antwort-Header an den Client gehen. Ist es ein Fehler (z. B. Azure
  schickt `200` und sofort `{"error": …}`), gilt der Versuch als fehlgeschlagen. Ab dem ersten
  weitergereichten Ereignis gibt es keinen Retry mehr; spätere Fehler gehen als Fehlerereignis
  an den Client (06 §5) und zählen nur für den Breaker.
- **Was wird wiederholt** (nach `UpstreamError.retryable` bzw. `ErrorClass`):

  | Klasse | Verhalten |
  |---|---|
  | `rate_limit`, `timeout`, `connect`, `server_error`, `overloaded` | Retry; anderes Deployment bevorzugt, dasselbe nur, wenn kein anderes verfügbar ist |
  | `auth`, `not_found`, `quota` | Retry nur auf **anderen** Deployments (dieses ist für die Anfrage ausgeschlossen) |
  | `invalid_request`, `context_length`, `content_filter`, `protocol`, `internal` | kein Retry in der Gruppe; nur ein passender Fallback (§5) |

- **Backoff** nur, wenn dasselbe Deployment noch einmal versucht wird: exponentiell ab
  `initialBackoffMs`, gedeckelt bei `maxBackoffMs`, „equal jitter“ (Hälfte fest, Hälfte
  zufällig). Der Wechsel auf ein anderes Deployment erfolgt sofort.
- **`Retry-After`** (`retry-after`, `retry-after-ms`): das Deployment wird so lange gesperrt
  (höchstens 5 min; gilt auch für andere Anfragen). Ist danach kein anderes Deployment
  verfügbar, wartet der Proxy die Sperre ab – wenn sie ≤ `maxRetryAfterMs` ist und ins Budget
  passt –, sonst geht der Fehler (z. B. 429 mit `Retry-After`) an den Client.
- **Budget:** `maxAttempts` je Modellgruppe; `budgetMs` (aus der Ursprungsgruppe) gilt für die
  ganze Kette: danach startet kein neuer Versuch und kein Fallback. Ein laufender Versuch wird
  nicht abgebrochen (dafür gelten die Zeitlimits des Deployments).
- Die Anfrage wird je Versuch geklont, außer es ist ohnehin nur ein Versuch möglich
  (`maxAttempts: 1` ohne Fallbacks, `StaticRoutes`).

## 5. Fallback-Ketten (AIGW-25)

Ist eine Modellgruppe erschöpft (Versuche, Budget oder alle Deployments gesperrt), geht der
Proxy die `fallbacks` der **Ursprungsgruppe** in Reihenfolge durch und nimmt die erste, deren
Auslöser zur Klasse des letzten Fehlers passt und die noch nicht besucht wurde. Jede
Fallback-Gruppe nutzt ihre eigenen Deployments, ihre Strategie und ihr `maxAttempts`; die
`fallbacks` der Fallback-Gruppen werden nicht verfolgt. Höchstens 8 Gruppen je Anfrage
(Ursprung + 7); Admin-API und GitOps lehnen längere Ketten und Zyklen ab (F69,
`aigw_core::validate::MAX_FALLBACKS`).

**Freigaben (F80):** Eine Fallback-Gruppe wird nur genommen, wenn der API-Key (und sein Team)
sie nutzen darf (`allowedModels`, wie bei der direkten Anfrage). Nicht freigegebene Gruppen
werden übersprungen und im Header `x-aigw-fallback-skipped` (kommagetrennt) sowie am
Wurzel-Span (`aigw.fallback_skipped`) genannt. `settings.fallbackIgnoresGrants: true` an der
**Ursprungsgruppe** schaltet die Prüfung für ihre Kette ab (z. B. interne Ausweichmodelle, die
Keys nicht direkt ansprechen sollen).

| Auslöser (`on`) | greift bei |
|---|---|
| leer / `any` | allen retrybaren und deployment-bezogenen Klassen (`rate_limit`, `timeout`, `connect`, `server_error`, `overloaded`, `auth`, `not_found`, `quota`) und „kein Deployment verfügbar“ – **nicht** bei `context_length`, `content_filter`, `invalid_request` |
| `rate-limit` | `rate_limit`, `quota` |
| `context-length` | `context_length` (→ Modell mit größerem Kontext) |
| `content-filter` | `content_filter` (nur wenn ausdrücklich konfiguriert) |
| `unavailable` | `timeout`, `connect`, `server_error`, `overloaded`, `auth`, `not_found`, kein Deployment verfügbar |

## 6. Circuit Breaker und Health Checks (AIGW-26)

- Je Deployment ein Breaker mit gleitendem Fenster aus Sekunden-Buckets (`windowSecs`).
  **Fehler** (öffnen): `timeout`, `connect`, `server_error`, `overloaded`, `protocol`, `auth`,
  `not_found`, `quota` sowie Fehlerereignisse mitten im Stream. **Erfolg**: Antwort bzw.
  Stream-Ende ohne Fehler; auch Client-Fehler (`400`, Kontextlänge, Content-Filter) – der
  Anbieter hat ja geantwortet. **Nicht gewertet**: `rate_limit` (dafür die `Retry-After`-Sperre)
  und Abbrüche durch den Client.
- **`quota` öffnet sofort** (AIGW-125): Guthaben/Ausgabenlimit des Kontos erschöpft (Anthropic
  „credit balance is too low“, „reached your specified API usage limits“,
  `enforced_spend_limit_reached`; OpenAI/Azure `insufficient_quota`,
  `billing_hard_limit_reached`, `billing_not_active`) hält an, bis nachgeladen wird – der Breaker
  wartet nicht auf die Fehlerquote; nach `openSecs` prüft eine Probe.
- Öffnet bei `fehler ≥ failureRate × gesamt` und `gesamt ≥ minRequests`, bleibt `openSecs`
  offen, lässt dann genau eine Probe durch (halboffen): Erfolg schließt, Fehler öffnet erneut.
  Kommt die Probe nicht zurück (Client-Abbruch), darf nach weiteren `openSecs` eine neue laufen.
- **Health Checks** (optional, `settings.healthCheck`): je Deployment im Abstand
  `intervalSecs` ein leichter Chat-Request (`ping`, `max_tokens: 1`, nicht gestreamt) über
  denselben Adapter, mit `timeoutMs`. Nach `unhealthyThreshold` aufeinanderfolgenden
  Fehlschlägen wird der Breaker geöffnet; ein Erfolg schließt einen offenen/halboffenen
  Breaker. Health Checks zählen nicht als offene Anfrage und nicht für RPM/TPM.

## 7. Antwort-Header und Request-Log

| Header | Inhalt |
|---|---|
| `x-aigw-deployment` | Name des Deployments, das die Antwort lieferte (bzw. zuletzt versucht wurde) |
| `x-aigw-attempts` | Anzahl der Versuche (fehlt, wenn kein Deployment angefragt wurde) |
| `x-aigw-fallback` | Modellgruppe, falls die Antwort über eine Fallback-Kette kam |
| `x-aigw-fallback-skipped` | übersprungene, für den Key nicht freigegebene Fallback-Gruppen (F80) |
| `x-aigw-model` | Modell, das geantwortet hat – wie vom Anbieter gemeldet, sonst `providerModel` (F49; bei jeder erfolgreichen Antwort) |

Abschaltbar mit `server.data.routing_headers: false`. Im `RequestRecord` stehen zusätzlich
(additiv) alle Versuche: `attempts: Vec<Attempt>` mit Modellgruppe, Deployment (Name, ID),
Fehlerklasse, Upstream-Status und Dauer. `model_group`, `deployment_id`, `provider`,
`provider_model` beschreiben das zuletzt verwendete Deployment (nach Fallback die Zielgruppe).
Die Versuche werden noch nicht in `request_log` gespeichert (Spalte/Tabelle folgt mit einer
Migration in `aigw-observe`).

## 8. Snapshot-Wechsel (Hot Reload)

- Das Binary baut beim Start `SnapshotRoutes` (eingebaute Adapter + Standard-HTTP-Client) und
  startet `SnapshotRoutes::spawn(snapshot_handle)`: Neuaufbau bei jeder neuen `config_version`
  (`SnapshotHandle::subscribe()`), dazu der 1-s-Takt für Health Checks.
- Laufende Anfragen behalten ihre `GroupRoute` (Arc) bis zum Ende.
- `DeploymentState` hängt an der **Deployment-ID** und wird übernommen, solange Anbieter,
  Endpunkt, Anbieter-Modell, Name der Zugangsdaten und `settings` gleich bleiben (Gewicht,
  Priorität und Kapazität dürfen sich ändern). Sonst beginnt das Deployment mit frischem
  Zustand; Zustände entfernter Deployments werden verworfen.
- Deployments, deren Adapter sich nicht bauen lässt (Anbieter ohne Adapter, z. B. `anthropic`
  vor AIGW-16; fehlende Zugangsdaten), werden ausgelassen und gemeldet; der Rest der Gruppe
  bleibt erreichbar.

## 9. Tests

`crates/aigw-proxy/tests/routing.rs` gegen einen lokalen Fake-Upstream (OpenAI-kompatibel,
Verhalten je Deployment umschaltbar): Gewichtsverteilung (100 000 Auswahlen ± 1 %, 400
Anfragen ± 8 %), Prioritätsgruppen und Retry auf anderes Deployment, `least-latency`,
`least-busy`, `cost`, kein Retry nach dem ersten Byte (aber vor dem ersten Ereignis),
`Retry-After` und `maxRetryAfterMs`, Fallback-Kette nach Fehlerklasse (Kontextlänge → größeres
Modell, Content-Filter → kein Fallback, Kette über zwei Gruppen), Breaker öffnet/lehnt ab/
schließt nach Probe, Health Checks öffnen/schließen, Hot Reload (neue Routen, Zustand bleibt für
unveränderte Deployments), Mock-Deployments und nicht unterstützte Anbieter. Unit-Tests für
Breaker, EWMA, Minutenfenster, Backoff, Auslöser und Policy-Parser.
`scripts/runtime-smoke.sh` legt per GitOps-Apply (`POST /admin/api/v1/config:apply`, AIGW-33)
eine Modellgruppe mit zwei Mock-Deployments an (Retry auf das zweite, Header geprüft, auch als
Anthropic-Stream) und danach eine weitere (Hot Reload der Routen).

## 10. Offene Punkte

- Health Check per `/models` statt Chat-Request braucht eine Methode im `Provider`-Trait.
- Kennzahlen je Modellgruppe für Retries/Fallbacks (derzeit global, F86); Breaker-Zustand ist je
  Deployment in `/metrics`.
- Statusanzeige der Deployments (Breaker, EWMA, offene Anfragen) in Admin-API/UI.
