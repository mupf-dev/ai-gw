# ai-gw – Plugin-Kette (AIGW-36)

Stand: 2026-11-13 · Status: umgesetzt (Kette, Vererbung, `header`, `request-transform`,
`webhook` – AIGW-61; Zwiebelprinzip, Fehlermodus je Plugin-Art, Ausgabe-Ablehnung mit
`content_filter`, Prüfung beim Schreiben, Admin-API – AIGW-69),
Grundlage: [01-architektur.md](01-architektur.md) §3, §7, §9 · Code: `crates/aigw-plugins`,
Einhängepunkte `crates/aigw-proxy/src/plugins.rs`

## 1. Einordnung im Hot Path

```
Auth ─► Modellfreigabe ─► [Limits/Budgets reservieren, AIGW-30/31] ─► Pre-Plugins ─► [Cache, AIGW-39] ─► Routing/Upstream
                                                                          │ Respond / Reject
                                                                          ▼
Upstream-Antwort ─► nicht gestreamt/gepuffert: Post-Plugins ─► Encoder ─► Client
                 └► gestreamt: Decoder ─► IR-Ereignis ─► Stream-Haken ─► Encoder ─► Client
```

Die Stellen sind in `aigw-proxy` als benannte Schritte markiert (`handlers::chat_inner`):

| Schritt | Funktion | Bemerkung |
|---|---|---|
| 4 | `Limiter::admit` (AIGW-30/31) | **vor** den Pre-Plugins, damit abgelehnte Anfragen keine Plugin-Kosten (Webhooks) verursachen |
| 5 | `PluginRun::run_pre_plugins` | nach Auth/Modellfreigabe/Admission, vor Cache und Routing |
| 10 | `PluginRun::run_post_plugins` | ganze IR-Antwort vor dem Encoder (JSON und gepufferter Modus) |
| 10 | `PluginRun::wrap_stream` | Stream-Haken je IR-Ereignis vor dem Encoder (vor dem Request-Log, d. h. geloggt wird, was der Client sieht) |

Ohne Bindungen kostet die Kette einen Index-Lookup (§7); Kontext, Meta und Stream-Hülle werden
dann nicht angelegt.

### Zwiebelprinzip (F167b, Sicherheit)

Anfrage-Haken laufen in Kettenreihenfolge (`order` aufsteigend, §4), **Antwort- und
Stream-Haken in umgekehrter Reihenfolge** – wie Middleware-Schichten:

```
Anfrage:  pii (10) ─► webhook (20) ─► content-safety (30) ─► Upstream
Antwort:  pii (10) ◄─ webhook (20) ◄─ content-safety (30) ◄─ Upstream
```

Ein Plugin, das die Anfrage früh verändert, sieht die Antwort als letztes. Wichtigster Fall:
`pii` mit `pseudonymize` ersetzt E-Mail-Adressen in der Anfrage durch `[EMAIL_1]` und übersetzt
sie in der Antwort zurück – alle Plugins mit höherer `order` (Webhooks, externe Prüfdienste)
sehen in **beiden** Richtungen nur die Platzhalter (Test
`webhook::response_webhook_sees_only_pseudonyms`). Vor AIGW-69 liefen die Antwort-Haken in
derselben Reihenfolge wie die Anfrage-Haken; ein Antwort-Webhook hinter `pii` sah dadurch die
zurückübersetzten Originalwerte.

Faustregel für `order`: Plugins, die Daten **schützen** (Pseudonymisierung, Maskierung),
bekommen eine **niedrige** `order` (außen); Plugins, die die rohe Modellausgabe zuerst prüfen
sollen (z. B. `content-safety` auf der Ausgabe), eine **hohe** (innen). Im Stream gibt
`ctx.emit` Ereignisse an die nachfolgenden, also **äußeren** Plugins weiter.

Grenze: Im gepufferten Modus (§2) laufen reine Stream-Plugins (ohne Antwort-Phase) erst auf
den Ereignissen der fertig verarbeiteten Antwort, also nach allen Antwort-Haken – unabhängig von
ihrer `order`. Eingebaute Plugins haben immer beide Ausgabehaken, betroffen wären nur eigene
reine Stream-Plugins.

**Abrechnung bei Plugin-Ergebnissen:** Die Admission (Reservierung) wird immer im Recorder
abgeschlossen. `Reject` (auch Post-Plugins und `failMode: closed`) gleicht die
Token-Reservierung mit Verbrauch 0 ab; `Respond` mit der Usage, die das Plugin in seiner Antwort
angibt (ohne Angabe 0), ohne Kosten (kein Anbieter). Die Anfrage selbst zählt in Request-Limits
mit. Es bleibt kein reserviertes Kontingent stehen (Test
`limits::plugin_reject_and_respond_release_the_token_reservation`).

## 2. Plugin-Schnittstelle

```rust
pub trait Plugin: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn phases(&self) -> Phases;                                   // request / response / chunk
    fn needs_buffered_output(&self) -> bool { false }             // ganze Antwort nötig?
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow>;   // Continue | Respond | Reject | Block
    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow>; // Continue | Respond (ersetzen) | Reject | Block
    fn on_chunk(&self, ctx: &mut ChunkCtx<'_>, ev: &mut StreamEvent) -> Result<ChunkFlow, PluginError>; // synchron
    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError>;              // Ausleeren
}
enum Hook<'a, T> { Ready(Result<T, PluginError>), Pending(BoxFuture<'a, Result<T, PluginError>>) }
enum ChunkFlow { Continue, Drop, Reject(ApiError), Block(ApiError) }
```

`Reject` ist eine Ablehnung aus Richtlinie oder Protokoll (z. B. Webhook `reject`,
`request-transform` über dem Deckel); `Block` ist eine **Guardrail-Ablehnung des Inhalts**
(`pii` mit `reject`, `prompt-filter`, `content-safety`), die die Kette nach F160 behandelt
(§3a).

- **Asynchron ohne Zwangs-Allokation:** Synchrone Plugins geben `Hook::Ready` zurück (keine
  Box, kein `async_trait`); asynchrone (Webhook, AIGW-61; externe Guardrails, AIGW-37) geben
  `Hook::Pending` und laufen unter dem Zeitlimit ihrer Bindung.
- **Kontexte:** `CallInfo` (Request-ID, Eingangsformat, `KeyCtx` mit Team/Org, kanonische
  Modellgruppe, Stream-Flag) in allen Phasen; `ReqCtx` mit der IR-Anfrage (besitzend),
  Client-Headern (nur lesen), zusätzlichen Upstream-Headern, Antwort-Header-Änderungen und
  `Meta`; `RespCtx` mit der IR-Antwort; `ChunkCtx` mit Zustand je Plugin und Stream.
- **`Meta`** (`serde_json::Map`) wandert von der Anfrage- in die Antwort-/Stream-Phase (z. B.
  Zuordnungstabelle einer Pseudonymisierung, AIGW-37). JSON, damit Webhook- und spätere
  WASM-Plugins sie ohne Umbau lesen und schreiben können.
- **Phasen werden vorab aufgelöst:** Die Kette hält je Phase eine Indexliste; ein Plugin ohne
  Stream-Phase kostet im Stream nichts.
- **Modell-Umleitung gesperrt:** `model` und `stream` der Anfrage darf kein Plugin ändern
  (Modellfreigabe und Routing sind entschieden). Die Kette setzt beide zurück und behandelt die
  Änderung als Plugin-Fehler (Fehlermodus der Bindung).
- **Ausgabe-Plugins** (Prüfen/Verändern von Inhalten) implementieren **beide** Ausgabehaken:
  `on_response` für JSON und gepufferten Modus, `on_chunk` für Live-Streams. Je Antwort wird
  genau einer davon aufgerufen.

### Stream-Haken und Gleitfenster

`on_chunk` läuft synchron auf jedem IR-Ereignis (kein `await` im Chunk-Pfad, Q4). Werkzeuge:

- `ctx.state::<T>()` – Zustand je Plugin und Stream (z. B. `TextWindow`, ein Gleitfenster über
  die letzten *n* Bytes Text, an Zeichengrenzen geschnitten).
- `ChunkFlow::Drop` + `ctx.emit(ev)` – zurückhalten und später (ggf. verändert) ausgeben; die
  ausgegebenen Ereignisse durchlaufen die **nachfolgenden** Plugins.
- `on_stream_end` – Zurückgehaltenes am Stream-Ende ausgeben.
- `ChunkFlow::Reject(err)` – beendet den Stream mit `StreamEvent::Error` (Client bekommt das
  Fehlerereignis seines Formats), der Upstream wird abgebrochen; nachfolgende Plugins sehen
  noch die bis dahin ausgegebenen Ereignisse (nichts Zurückgehaltenes rutscht ungefiltert
  durch).

Backpressure bleibt erhalten: `PluginStream` liest den Upstream nur, wenn der Encoder das
nächste Ereignis anfordert.

### Gepufferter Modus

Hat eine Kette ein Plugin mit `needs_buffered_output() == true`, werden gestreamte Anfragen
gepuffert: Upstream nicht gestreamt, `on_response` auf der ganzen Antwort, danach als Stream im
Eingangsformat an den Client (`response_events`). Plugins **ohne** Antwort-Phase sehen die
erzeugten Ereignisse in `on_chunk` (jede Ausgabe genau einmal). Ein `Reject` auf der ganzen
Antwort kommt so noch als normale Fehlerantwort (vor dem ersten Byte).

Kosten: TTFT ≈ Gesamtdauer der Antwort. Der Client sieht `x-aigw-buffered: plugins`; das UI
soll die Route entsprechend kennzeichnen (Latenzkosten).

## 3. Fehler und Zeitlimits

| Fall | `failMode: open` | `failMode: closed` |
|---|---|---|
| Haken liefert `Err(PluginError)` | Warnung im Log, Plugin übersprungen | Anfrage `500 plugin_error` bzw. Stream-Fehlerereignis |
| asynchroner Haken überschreitet `timeoutMs` | wie oben | wie oben |
| `model`/`stream` verändert | zurückgesetzt, weiter | zurückgesetzt, `500 plugin_error` |
| Bindung ungültig (Snapshot-Aufbau) | geloggt, Bindung übersprungen | betroffene Ketten antworten `500 plugin_config_invalid` |

**Standard je Plugin-Art (F148)**, je Bindung über `failMode` überschreibbar:

| Plugin | `failMode` | `timeoutMs` | Begründung |
|---|---|---|---|
| `content-safety` | `closed` | 3000 | Sicherheitsprüfung: ohne Ergebnis keine Freigabe |
| `webhook` | `open` | 200 | meist Zusatzlogik/Audit; ein ausgefallener Dienst soll den Verkehr nicht stoppen. Policy-Webhooks setzen `failMode: closed` |
| alle anderen (`pii`, `prompt-filter`, `header`, `request-transform`, `cache`) | `closed` | 1000 | lokal, können zur Laufzeit praktisch nicht fehlschlagen |

Für **ungültige** Bindungen ohne eigenes `failMode` gilt `IndexOptions::invalid_binding`
(Standard `open` = überspringen); seit AIGW-69 werden Bindungen aber schon beim Schreiben
geprüft (§4). Die Fehlermeldung an den Client nennt nur den Bindungsnamen; Details stehen im
Log (mit Request-ID).

**Sichtbarkeit:**

- Metrik `aigw_plugin_failmode_triggered_total{plugin,mode}` (`/metrics` der Steuerebene):
  zählt jeden Plugin-Fehler, bei dem der Fehlermodus gegriffen hat – `mode="open"` =
  übersprungen (die Anfrage lief **ohne** das Plugin weiter), `mode="closed"` = abgelehnt.
  `plugin` ist der Plugin-Name (begrenzt); ungültige Bindungen zählen hier nicht (Log beim
  Snapshot-Aufbau, seit AIGW-69 ohnehin beim Schreiben abgewiesen). Alarmvorschlag:

  ```yaml
  - alert: AigwPluginFailOpen
    expr: sum by (plugin) (increase(aigw_plugin_failmode_triggered_total{mode="open"}[5m])) > 0
    annotations: {summary: "Plugin {{ $labels.plugin }} übersprungen (failMode open)"}
  ```
- Admin-API: `GET /admin/api/v1/plugin-bindings[/{name}]` liefert je Bindung `effective`
  mit `failMode`, `failModeSource` (`binding` | `plugin-default`), `timeoutMs` und
  `outputRejectMode` – die UI zeigt den wirksamen Fehlermodus an.

Nicht gezählt wird `content-safety` mit `sentenceOnError` im Satzmodus (eigene Regel, siehe
[13-guardrails.md](13-guardrails.md) §5).

### 3a. Guardrail-Ablehnung der Ausgabe (F160)

Blockiert ein Guardrail (`Flow::Block` / `ChunkFlow::Block`) **die Ausgabe**, entscheidet
`outputRejectMode` der Bindung:

| | `finish` (Standard, wie Azure OpenAI) | `error` |
|---|---|---|
| JSON | `200`, Inhalt aller Varianten entfernt, OpenAI `finish_reason: "content_filter"`, Anthropic `stop_reason: "refusal"`; Usage, ID und Modell bleiben (Upstream-Verbrauch wird abgerechnet) | `400` mit dem Fehler des Plugins (z. B. `pii_detected`, `content_filtered`) |
| gepufferter Stream | `200`, SSE mit leerem Inhalt und `content_filter` / `refusal`, sauberes Ende (`[DONE]` bzw. `message_stop`) | `400` vor dem ersten Byte |
| Live-Stream | bereits Gesendetes bleibt, dann Stop-Ereignis mit `content_filter` / `refusal` und sauberes Ende; der Upstream wird abgebrochen | Fehlerereignis im Format des Eingangs |

- Header `x-aigw-guardrail: <plugin>` (z. B. `pii`, `content-safety`) bei JSON und gepufferten
  Antworten, bei beiden Modi und auch bei **Eingabe**-Ablehnungen. Im Live-Stream sind die
  Header schon gesendet – dort zeigt nur `finish_reason`/`stop_reason` die Ablehnung an.
- **Eingabe-Ablehnungen bleiben `400`** (der Client muss seine Anfrage ändern).
- Nach einem `Block` laufen keine weiteren (äußeren) Antwort-Haken mehr – ein Plugin kann eine
  Ablehnung nicht rückgängig machen. Im Stream durchläuft das Endereignis noch die äußeren
  Plugins (sie geben Zurückgehaltenes aus); hält eines es zurück, gibt die Kette es trotzdem
  aus.
- Im Request-Record: Status `200`, Guardrail-Zähler mit Aktion `reject`. Fehlt im Live-Stream
  nach dem Abbruch die Usage des Upstreams, gilt dasselbe wie bei Client-Abbrüchen.

## 4. Bindungen und Vererbung

`PluginBinding` (Ressource, siehe [02-datenmodell.md](02-datenmodell.md)) bindet ein Plugin an
eine Ebene. `config` enthält die Plugin-Konfiguration und fünf **reservierte** Schlüssel der
Kettensteuerung (sie gehen nicht an das Plugin; als eigene Spec-Felder: F117, Folgeticket):

| Schlüssel | Werte | Standard | Bedeutung |
|---|---|---|---|
| `inherit` | `append` / `override` / `disable` | `append` | Verhältnis zu Bindungen **desselben Plugins** auf äußeren Ebenen |
| `locked` | bool | `false` | innere Ebenen können die Bindung weder ersetzen noch abschalten |
| `failMode` | `open` / `closed` | je Plugin-Art | siehe §3 |
| `timeoutMs` | 1–60000 | je Plugin-Art | Zeitlimit asynchroner Haken |
| `outputRejectMode` | `finish` / `error` | `finish` | Ausgabe-Ablehnung eines Guardrails, siehe §3a |

**Prüfung beim Schreiben (F118/F165):** Admin-API (`/admin/api/v1/plugin-bindings`, Anlegen
und Ersetzen) und GitOps-Plan (`aigw config validate|diff|apply`, `config:apply`) prüfen jede
Bindung mit der Registry der Plugin-Kette (`Registry::for_validation`): Kettensteuerung,
Plugin-Konfiguration (`deny_unknown_fields`, Wertebereiche) und die SSRF-Regeln von
`webhook`/`content-safety` mit `admin.allow_insecure_upstreams` / `allow_private_upstreams`.
Fehler → `422` mit der Meldung des Plugins, z. B. ``Plugin-Bindung (pii): unknown field
`acton`, expected one of …`` bzw. `Plugin-Bindung (webhook): url: …`. Credential-Referenzen
(`webhook.secret.credential`, `content-safety.credential`) werden erst zur Laufzeit aufgelöst;
`secret.env`/`secret.file` liest schon die Prüfung. Schreiben dürfen nur globale `admin`s
(Plugin-Bindungen sind sicherheitsrelevant, RBAC-Ziel wie der Modellkatalog); lesen jede
wirksame Rolle.

Zusammensetzung je (Org, Team, Key, Modellgruppe):

1. Ebenen in der Folge **global → Organisation → Team → API-Key → Modellgruppe**, innerhalb der
   Ebene nach `order`, dann Name.
2. `append` hängt an; `override` entfernt die geerbten Bindungen desselben Plugins und hängt sich
   an; `disable` entfernt sie nur. Ist eine geerbte Bindung `locked`, bleibt sie und `override`
   bzw. `disable` ist wirkungslos (Debug-Log).
3. Ausführungsreihenfolge der fertigen Kette: `order` aufsteigend **über alle Ebenen**, bei
   Gleichstand äußere Ebene zuerst, dann Name. `order` ist damit die Priorität.

```yaml
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: org-headers}
spec:
  scope: {level: organization, target: acme}
  plugin: header
  order: 10
  config:
    locked: true                  # Teams dürfen das nicht abschalten
    request: {set: {x-tenant: acme}}
---
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: research-cap}
spec:
  scope: {level: team, target: research}
  plugin: request-transform
  order: 20
  config:
    inherit: override             # ersetzt eine globale request-transform-Bindung
    caps: {maxTokens: 2048}
```

`enabled: false` nimmt eine Bindung ganz heraus (sie wirkt auch nicht als `disable`).

## 5. Eingebaute Plugins

### `header`

| Feld | Bedeutung |
|---|---|
| `request.forward: [name]` | Allowlist: diese Client-Header gehen an den Upstream (sonst keine) |
| `request.set: {name: wert}` | Header an den Upstream setzen |
| `request.remove: [name]` | von früheren Plugins gesetzte/durchgereichte Upstream-Header entfernen |
| `response.set: {name: wert}` | Antwort-Header an den Client setzen (auch bei Fehlern und Streams) |
| `response.remove: [name]` | Antwort-Header entfernen (z. B. `x-aigw-deployment`) |

Gesperrt für den Upstream (schon beim Snapshot-Aufbau abgewiesen): `authorization`,
`proxy-authorization`, `api-key`, `x-api-key`, `x-goog-api-key`, `cookie`, `host`,
Rahmen-/Hop-by-Hop-Header, `anthropic-version`, `traceparent`/`tracestate` (setzt der Upstream-Client, AIGW-45), `x-amz-*`, `proxy-*`, `x-aigw-*`. Der
Gateway-Key des Clients kann so nie zum Anbieter gelangen. Header des Adapters (Auth, Signatur)
haben am Upstream Vorrang. Geschützte Antwort-Header: `content-type`, `content-length`,
`content-encoding`, `transfer-encoding`, `connection`, `x-aigw-request-id`.

Transport: `ReqCtx.upstream_headers` → `ChatRequest.hints.upstream_headers` →
`aigw_providers::Upstream` setzt sie nach dem Bau der Anbieter-Anfrage.

### `request-transform`

Reihenfolge: `remove` → `defaults` (nur wenn der Client nichts angibt) → `overrides` (immer) →
`caps` (deckeln). Felder: `maxTokens`, `temperature`, `topP`, `topK`, `frequencyPenalty`,
`presencePenalty`, `seed`, `parallelToolCalls`, `user`; Deckel für `maxTokens`, `temperature`,
`topP`. Fehlt `max_tokens`, setzt der Deckel ihn (sonst gälte das Anbieter-Maximum).
`rejectOverCap: true` lehnt Überschreitungen mit `400 parameter_over_limit` ab statt zu deckeln.
Das Modell lässt sich nicht setzen (unbekanntes Feld → Konfigurationsfehler); Umleitungen sind
generell gesperrt (§2).

### `webhook` (AIGW-61)

Ruft einen externen HTTP-Dienst in der Anfrage- und/oder Antwort-Phase auf (eigene Plugins
ohne Gateway-Code, Architektur §9 / §17 Nr. 3). Code: `aigw-plugins/src/builtin/webhook.rs`,
HTTP/Signatur/SSRF in der Crate `aigw-webhook` (gemeinsam mit den Ereignis-Webhooks,
[11-limits-und-budgets.md](11-limits-und-budgets.md) §6.5).

```yaml
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: acme-policy}
spec:
  scope: {level: organization, target: acme}
  plugin: webhook
  order: 30
  config:
    url: https://policy.acme.example/aigw
    phases: [request, response]          # Standard [request]
    secret: {credential: acme-hooks}     # Feld webhookSecret; alternativ {credential, field} | {env} | {file}
    includeHeaders: [x-client-app]       # Client-Header im Payload (Allowlist)
    maxPayloadBytes: 1048576             # Standard 1 MiB, höchstens 16 MiB
    maxResponseBytes: 1048576
    timeoutMs: 200                       # Kettensteuerung, Standard 200 ms
    failMode: closed                     # Kettensteuerung, Standard open (F148)
```

| Feld | Bedeutung |
|---|---|
| `url` | Ziel; **SSRF-Regeln wie Deployments** (`https`, keine privaten/lokalen Ziele, keine Zugangsdaten in der URL; Ausnahmen über `admin.allow_insecure_upstreams` / `allow_private_upstreams`). Zusätzlich prüft der Client **jede aufgelöste IP-Adresse** beim Verbindungsaufbau (Schutz vor DNS-Rebinding und Namen, die auf private Adressen zeigen); Weiterleitungen werden nicht verfolgt. |
| `phases` | `request` (vor Cache/Routing) und/oder `response` (ganze Antwort). Mit `response` werden gestreamte Anfragen der Route **gepuffert** (`x-aigw-buffered: plugins`, TTFT = Gesamtdauer). Nie im Chunk-Pfad. |
| `secret` | HMAC-Schlüssel, **nie Klartext** in der Bindung: `credential` (Feld einer `ProviderCredential`, verschlüsselt in der DB, z. B. Anbieter `openai-compatible` mit Feld `webhookSecret`; bei jedem Aufruf aus dem aktuellen Snapshot gelesen → Rotation ohne Neustart), `env` bzw. `file` (beim Laden der Bindung gelesen). Ohne `secret` unsigniert. |
| `includeHeaders` | Client-Header, die in den Payload dürfen. `authorization`, `proxy-authorization`, `api-key`, `x-api-key`, `x-goog-api-key`, `cookie` sind gesperrt. |
| `maxPayloadBytes` / `maxResponseBytes` | Größenlimits (Überschreitung = Plugin-Fehler). |

**Aufruf** (`POST`, `content-type: application/json`, `user-agent: ai-gw/<version>`):

```json
{"version": 1, "phase": "request", "requestId": "…", "ingress": "open-ai-chat",
 "modelGroup": "gpt-4o", "stream": false,
 "key": {"id": "…", "name": "ci-key"}, "team": {"id": "…", "name": "research"},
 "organization": {"id": "…", "name": "acme"},
 "headers": {"x-client-app": "…"},
 "meta": {"…": "…"},
 "request": { …IR-Anfrage (03-ir-und-formate.md)… }}
```

`meta` (nur wenn nicht leer) ist die **externe Sicht** auf `Meta` früherer Plugins
(`MetaExt::external_view`): Schlüssel mit Präfix `aigw.` (interne Daten eingebauter Plugins,
z. B. `aigw.pii` mit der Pseudonym-Zuordnung samt Originalwerten, AIGW-37) werden zentral
entfernt und verlassen das Gateway nie. Dank Zwiebelprinzip (§1) sieht ein Webhook hinter
`pii` auch in der Antwort-Phase nur die Platzhalter.

In der Antwort-Phase steht statt `request` die IR-Antwort unter `response`. Nie enthalten:
Gateway-Keys und andere Zugangsdaten-Header, Upstream-Header früherer Plugins
(`request.hints.upstream_headers` wird entfernt), Anbieter-Zugangsdaten. `ingress` ist
`open-ai-chat` bzw. `anthropic-messages`; die IR ist formatneutral, ein Webhook bedient beide
Eingangsformate.

Header: `x-aigw-request-id`, `x-aigw-delivery` (`<request-id>:<phase>`), mit Geheimnis
`x-aigw-signature: t=<unix-sekunden>,v1=<hex>`.

**Signatur und Replay-Schutz:** `v1 = HMAC-SHA256(secret, "<t>." + roher Body)` (hex). Der
Empfänger prüft (Referenz: `aigw_webhook::sign::verify`):

1. `|jetzt − t| ≤ 300 s` (empfohlenes Zeitfenster; `sign::DEFAULT_TOLERANCE`),
2. die Signatur in konstanter Zeit (mehrere `v1=` erlaubt, für Schlüsselrotation beim
   Empfänger),
3. optional `x-aigw-delivery` als Nonce: innerhalb des Zeitfensters nur einmal annehmen. Da
   `t` signiert ist, genügt es, Nonces 300 s lang zu merken.

**Antwortprotokoll** (Status 2xx, JSON; `204` bzw. leerer Body = `continue`):

| `action` | Felder | Wirkung |
|---|---|---|
| `continue` | – | unverändert weiter |
| `modify` | `request` (Anfrage-Phase) bzw. `response` (Antwort-Phase), vollständige IR | ersetzt Anfrage/Antwort. Geprüft: gültige IR (serde), `messages` bzw. `choices` nicht leer, **`model` und `stream` unverändert** (sonst Plugin-Fehler). `hints`, `source`, `stream_options` übernimmt das Gateway aus dem Original – ein Webhook kann so keine Upstream-Header einschleusen. |
| `reject` | `status` (400–499, Standard 400), `message` (≤ 1024 Byte), `code` (Standard `webhook_rejected`) | Ablehnung im Format des Eingangs; der Status wird auf die Fehlerklasse abgebildet (401, 403, 404, 413, 429 bleiben, andere 4xx → 400). |
| `respond` | `response` (IR) | Anfrage-Phase: feste Antwort ohne Upstream (Usage wie bei anderen Plugins nur, wenn angegeben); fehlende `id`/`model` ergänzt das Gateway. Antwort-Phase: wie `modify`. |

Alles andere ist ein **Plugin-Fehler** und folgt `failMode` (§3, Standard für `webhook`:
`open`): Zeitüberschreitung (`timeoutMs`, Standard 200 ms – die Kette bricht den Aufruf ab),
Verbindungs-/SSRF-Fehler,
Status ≠ 2xx, ungültiges JSON bzw. unbekannte `action`, verbotene Änderung, Größenlimit, nicht
auflösbares Geheimnis. Fehlerdetails stehen nur im Log (mit Request-ID).

**Transport:** eigener Keep-Alive-Pool (hyper-util, rustls/aws-lc-rs, HTTP/2 und HTTP/1.1,
webpki-roots), getrennt von den Anbieter-Verbindungen und von allen Bindungen geteilt. Die
SSRF-Regeln kommen im Server aus `admin.*`; `Registry::with_builtin()` (Tests,
`Registry::validate`) nutzt strenge Regeln und kennt keine Credential-Quelle (Referenzen
scheitern dann erst beim Aufruf). **mTLS-Client-Zertifikat:** Erweiterungspunkt
`aigw_webhook::ClientOptions::tls` (beliebige rustls-Konfiguration, z. B.
`with_client_auth_cert`); eine Konfigurationsoberfläche je Bindung ist noch offen.

## 6. Vorberechnung und Hot Reload

`PluginIndex::build` läuft einmal je `config_version`:

1. Jede Bindung wird einmal validiert und instanziiert (`PluginFactory::build`, serde mit
   `deny_unknown_fields`). Unveränderte Bindungen (gleicher Name, Plugin und `config`) werden
   aus dem vorigen Index **wiederverwendet** (Regex, Verbindungspools bleiben).
2. **Präfixe:** Für jeden Key wird die Signatur „Org/Team/Key, soweit sie eigene Bindungen
   haben“ gebildet; Keys mit gleicher Signatur teilen sich die Kette (typisch: eine Kette je
   Team statt je Key). Keys ohne solche Bindungen bekommen Präfix 0 (nur global) und stehen
   nicht einmal in der Map.
3. Für jedes Präfix × (keine | jede Modellgruppe mit Bindungen) wird die Kette einmal
   zusammengesetzt (`Arc<Chain>`).

Lookup im Hot Path (`PluginChains::chain_for`): ArcSwap-Load, Versionsvergleich, Key-ID →
Präfix, Modellgruppe → Spalte, `Arc`-Klon. Ist der Snapshot der Anfrage (aus der
Schlüsselprüfung) neuer als der Index, wird der Index sofort aus ihm gebaut (Mutex, einmal je
Version) – Kette und Key/Team stammen so nie aus verschiedenen Ständen. Zusätzlich baut eine
Task (`PluginChains::spawn`) bei jeder neuen Version vorab neu.

Speicher: Präfixe × (1 + Modellgruppen mit Bindungen) `Arc`s; bei vielen Keys mit eigenen
Bindungen **und** vielen Modellgruppen mit Bindungen wächst das multiplikativ (siehe offene
Fragen).

## 7. Messwerte

`cargo bench -p aigw-plugins --bench chain` (divan, Release, Buildbox AMD EPYC Genoa, geteilt,
1 Thread, 2026-11-13, Median; Streuung durch parallele Last auf der Box):

| Benchmark | Median | Bemerkung |
|---|---|---|
| `hot_path_without_bindings` | 30–40 ns | was der Proxy ohne Bindungen tut (Lookup + `is_empty`) |
| `index_lookup_no_bindings` | 3 ns | reiner Index-Lookup ohne Bindungen |
| `index_lookup` | 31 ns | 10 000 Keys, 100 Teams + 20 Modellgruppen mit Bindungen |
| `request_baseline` | 25 ns | nur Anfrage bauen (Basis für die Zeilen darunter) |
| `empty_chain_request` | 100 ns | Kontext bauen + leere Kette laufen lassen (im Proxy übersprungen) |
| `request_transform_plugin_request` | 77 ns | Kontext + `request-transform` (≈ 50 ns netto) |
| `header_plugin_request` | 390–500 ns | Kontext + `header` (Forward, Set, Antwort-Header; HeaderMap-Allokationen) |
| `header_and_transform_request` | 370–515 ns | beide |
| `chunk_pass_through` | 45–75 ns | ein Stream-Ereignis durch ein durchreichendes Stream-Plugin |

Damit bleibt die leere Kette weit unter dem Budget Q1 (p50 < 200 µs) und die eingebauten
Plugins im Sub-µs-Bereich.

## 8. Andocken weiterer Plugins

- **Limits/Budgets (AIGW-30/31):** eigener Schritt 4 vor `run_pre_plugins` (kein Plugin).
- **PII/Guardrails (AIGW-37/38):** umgesetzt als `pii`, `prompt-filter`, `content-safety`,
  siehe [13-guardrails.md](13-guardrails.md); externe Dienste über `PluginHost`
  (`Registry::with_builtin_host`).
- **Cache (AIGW-39/40):** umgesetzt als eigener Pipeline-Schritt nach allen Pre-Plugins
  (nicht als `Respond`); das Plugin `cache` ohne Haken trägt nur die Richtlinie und nutzt die
  Vererbung der Bindungen. Begründung und Details: [14-cache.md](14-cache.md).
- **Webhook (AIGW-61):** umgesetzt (§5 `webhook`): `Hook::Pending` mit Zeitlimit/Fehlermodus
  der Bindung; nicht im Chunk-Pfad (keine `chunk`-Phase), für Ausgaben `needs_buffered_output`.
- **WASM (nach MVP):** Die Schnittstelle ist auf WIT abbildbar: `Meta` und IR sind JSON-fähig,
  `on_chunk` ist synchron, Zustand je Stream liegt beim Host (`ChunkCtx::state`).
- Neue Plugins registrieren eine `PluginFactory` in der `Registry`
  (`PluginChains::new(registry, opts)`) und legen mit `default_fail_mode`/`default_timeout`
  ihre Standardwerte fest (§3). `Registry::validate` prüft Bindungen ohne sie zu verwenden;
  Admin-API und GitOps nutzen dafür `Registry::for_validation(&policy)` (§4). Guardrails
  melden Inhaltsablehnungen als `Flow::Block`/`ChunkFlow::Block`, nicht als `Reject` (§3a).
