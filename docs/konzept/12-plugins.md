# ai-gw – Plugin-Kette (AIGW-36)

Stand: 2026-11-13 · Status: umgesetzt (Kette, Vererbung, `header`, `request-transform`),
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
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow>;   // Continue | Respond | Reject
    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow>; // Continue | Respond (ersetzen) | Reject
    fn on_chunk(&self, ctx: &mut ChunkCtx<'_>, ev: &mut StreamEvent) -> Result<ChunkFlow, PluginError>; // synchron
    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError>;              // Ausleeren
}
enum Hook<'a, T> { Ready(Result<T, PluginError>), Pending(BoxFuture<'a, Result<T, PluginError>>) }
enum ChunkFlow { Continue, Drop, Reject(ApiError) }
```

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

Standard: `failMode` je Plugin-Fabrik (eingebaute: `closed`; sie können zur Laufzeit nicht
fehlschlagen), `timeoutMs` 1000 (Fabrik kann abweichen, Webhook: 200). Für **ungültige**
Bindungen ohne eigenes `failMode` gilt `IndexOptions::invalid_binding` (Standard `open` =
überspringen). Die Fehlermeldung an den Client nennt nur den Bindungsnamen; Details stehen im
Log (mit Request-ID).

## 4. Bindungen und Vererbung

`PluginBinding` (Ressource, siehe [02-datenmodell.md](02-datenmodell.md)) bindet ein Plugin an
eine Ebene. `config` enthält die Plugin-Konfiguration und vier **reservierte** Schlüssel der
Kettensteuerung (sie gehen nicht an das Plugin):

| Schlüssel | Werte | Standard | Bedeutung |
|---|---|---|---|
| `inherit` | `append` / `override` / `disable` | `append` | Verhältnis zu Bindungen **desselben Plugins** auf äußeren Ebenen |
| `locked` | bool | `false` | innere Ebenen können die Bindung weder ersetzen noch abschalten |
| `failMode` | `open` / `closed` | Plugin | siehe §3 |
| `timeoutMs` | 1–60000 | Plugin | Zeitlimit asynchroner Haken |

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
- **PII/Guardrails (AIGW-37):** `on_request` (maskieren/ablehnen), `on_response` + `on_chunk`
  mit `TextWindow`/Zurückhalten (Rückübersetzung über `Meta`), optional
  `needs_buffered_output`.
- **Cache (AIGW-39):** `on_request` → `Flow::Respond` bei Treffer; Speichern in `on_response`
  bzw. über `Meta` + `on_stream_end`.
- **Webhook (AIGW-61):** `Hook::Pending` mit eigenem Zeitlimit/Fehlermodus der Bindung; nicht im
  Chunk-Pfad (keine `chunk`-Phase), für Ausgaben `needs_buffered_output`.
- **WASM (nach MVP):** Die Schnittstelle ist auf WIT abbildbar: `Meta` und IR sind JSON-fähig,
  `on_chunk` ist synchron, Zustand je Stream liegt beim Host (`ChunkCtx::state`).
- Neue Plugins registrieren eine `PluginFactory` in der `Registry`
  (`PluginChains::new(registry, opts)`); `Registry::validate` prüft Bindungen ohne sie zu
  verwenden (für Admin-API/`aigw config validate`).
