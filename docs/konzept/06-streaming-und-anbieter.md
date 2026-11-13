# ai-gw – Streaming-Pipeline und Anbieter-Adapter (AIGW-14, AIGW-15)

Stand: 2026-11-13 · Status: umgesetzt (OpenAI-Familie), Grundlage: [01-architektur.md](01-architektur.md) §5–§7,
[03-ir-und-formate.md](03-ir-und-formate.md)

## 1. Überblick

```
Client ─► Handler (aigw-proxy) ─► ModelRoutes ─► ChatProvider ──────────────► Upstream
           │ IR-Anfrage            Alias→Deployment  (aigw_providers::Upstream)   │
           │                                         Provider::build_request ────┘ HTTP (hyper, rustls,
           │                                                                       Pool je Endpunkt)
           │◄─ Encoder (Eingangsformat) ◄─ ObservedStream ◄─ UpstreamEvents ◄─ StreamDecoder ◄─ Body-Frames
           └─► Recorder ─► RequestObserver (Request-Log AIGW-43, später Abrechnung)
```

| Crate / Modul | Inhalt |
|---|---|
| `aigw_providers` | `Provider`-Trait, `Capabilities`, `StreamDecoder`, `Registry` (Fabrik je `ProviderKind`), `UpstreamClient`, `Upstream` (Deployment = Adapter + Client + `Timeouts`), Adapter `openai::OpenAiAdapter`, Erweiterungspunkt `auth::TokenSource` |
| `aigw_core::ir` | `ErrorClass`, `UpstreamError` (Ergebnis von `classify_error`) |
| `aigw_core::observe` | `RequestRecord`, `RequestObserver` (aus AIGW-43; der Proxy füllt ihn) |
| `aigw_proxy` | `ChatProvider` (ausführbares Deployment), `ModelRoutes`/`StaticRoutes`, Handler, `Recorder`/`ObservedStream`, SSE-Antwort |

## 2. Provider-Trait (Architektur §5)

```rust
pub trait Provider: Send + Sync + Debug {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> &Capabilities;
    fn native_format(&self) -> WireFormat;
    fn provider_model(&self) -> &str;
    fn build_request(&self, req: ChatRequest, losses: &mut Losses) -> Result<http::Request<Bytes>, ApiError>;
    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, ApiError>;
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;          // Bytes → IR-Ereignisse
    fn classify_error(&self, status: StatusCode, headers: &HeaderMap, body: &[u8]) -> UpstreamError;
}
```

Abweichungen von der Skizze in §5, mit Begründung:

- **Eine Adapter-Instanz je Deployment** statt `build_request(&req, &Deployment)`: URL, Auth-Header
  (als *sensitive* markiert), `api-version` und Capabilities werden beim Aufbau aus dem Snapshot
  einmal berechnet, nicht pro Anfrage (Q1). Die `Registry` hält dafür Fabriken je `ProviderKind`.
- **`build_request` übernimmt die Anfrage** (keine Kopie großer Bodies mit Base64-Bildern);
  Retries (AIGW-25) klonen vorher bewusst.
- **`classify_error` bekommt die Header** (`Retry-After`, `retry-after-ms`).
- Der Anthropic-Adapter (AIGW-16) passt ohne Änderung: `native_format = AnthropicMessages`,
  eigener `StreamDecoder` auf Basis von `wire::anthropic::StreamDecoder`, Header `x-api-key`.

`Capabilities`: `chat`, `streaming`, `tools`, `vision`, `json_schema`, `reasoning`, `embeddings`,
`stream_usage` (unterstützt `stream_options.include_usage`) und `max_tokens_field`.

## 3. Adapter der OpenAI-Familie

| | OpenAI (`openai`) | Azure OpenAI (`azure-openai`) | kompatibel (`openai-compatible`) |
|---|---|---|---|
| URL | `{endpoint}/chat/completions`, Standard `https://api.openai.com/v1` | `{endpoint}/openai/deployments/{providerModel}/chat/completions?api-version=…` (Standard `2024-10-21`) oder `{endpoint}/openai/v1/chat/completions` mit `apiStyle: v1` | `{endpoint}/chat/completions` (Pflicht) |
| Auth | `Authorization: Bearer` aus Zugangsdaten `apiKey` (Pflicht), optional `OpenAI-Organization`/`OpenAI-Project` | `api-key` aus `apiKey`; `auth: entraId` → `TokenSource` (Bearer) | `Authorization: Bearer`, nur wenn `apiKey` vorhanden |
| Ausgabegrenze | `max_completion_tokens` | `api-version` ≥ 2024-09-01 bzw. `v1`: `max_completion_tokens`, sonst `max_tokens` | `max_tokens` |

**`max_tokens` vs. `max_completion_tokens`** (offene Frage aus AIGW-13): Der Adapter normalisiert –
egal, welches Feld der Client schickte – auf das Feld aus `Capabilities::max_tokens_field`. OpenAI
lehnt `max_tokens` bei Reasoning-Modellen ab, viele kompatible Server kennen nur `max_tokens`.
Übersteuerbar je Deployment mit `settings.maxTokensField`.

Weitere Einstellungen (`spec.settings`): `streamUsage: false` (Server ohne `stream_options`),
`timeouts: {firstByteMs, idleMs, totalMs}`. `stream_options` wird bei nicht gestreamten Anfragen
entfernt (OpenAI lehnt es sonst ab). Azure-Deployment-Namen werden auf `[A-Za-z0-9._-]` geprüft
(Pfadsegment).

**Entra ID:** `auth::TokenSource::bearer_token()` ist synchron – Implementierungen halten das
Token im RAM und erneuern es im Hintergrund (kein Netzwerkzugriff je Anfrage). Eingebaut ist nur
`StaticToken`; `settings.auth: entraId` wird bis zur Implementierung mit einer klaren
Fehlermeldung abgelehnt (`OpenAiAdapter::azure_with_token_source` für programmatische Nutzung).

## 4. Upstream-Client und Timeouts

- hyper-util-Client, rustls mit aws-lc-rs und Mozilla-Wurzeln (`webpki-roots`), ALPN `h2` +
  `http/1.1`, Klartext-HTTP für lokale Server. Pool je Endpunkt (Schema+Host+Port), bis 1024 freie
  Verbindungen, 90 s Leerlauf, TCP-Keep-Alive 60 s, `TCP_NODELAY`, HTTP/2-Pings alle 30 s,
  adaptives HTTP/2-Fenster (`ClientConfig`).
- Timeouts (`Timeouts`, je Deployment): Verbindungsaufbau 10 s (global im `ClientConfig`),
  **erstes Byte** = Antwort-Header 120 s (nur Streams; nicht gestreamte Antworten kommen erst
  nach der Generierung), **Pause zwischen Chunks** 120 s, **Gesamtzeit** 600 s.
- Fehlerantworten werden bis 64 KiB gelesen, nicht gestreamte Antworten bis 64 MiB.
- Der Datenebenen-Listener setzt `TCP_NODELAY` (sonst verzögert Nagle kleine SSE-Chunks).

## 5. Streaming

- **Inkrementell:** Upstream-Body-Frame → `SseParser::push_into` (nur der Rest einer
  unvollständigen Zeile wird gepuffert, vollständige Zeilen werden direkt aus dem Frame gelesen)
  → `openai::StreamDecoder` → IR-Ereignisse → Encoder des Eingangsformats. Liegen schon weitere
  Ereignisse bereit, landen sie im selben Body-Frame (bis 16 KiB); es wird nie gewartet.
- **Backpressure:** `UpstreamEvents` liest den nächsten Upstream-Frame erst, wenn der
  Antwort-Body das nächste Ereignis anfordert – und hyper fordert erst an, wenn der vorige Frame
  geschrieben ist. Test: Ein endloser Upstream steht still, sobald der Client nicht mehr liest
  (`backpressure_and_client_abort`).
- **Client-Abbruch:** hyper verwirft den Antwort-Body → `ObservedStream` → `UpstreamEvents` →
  `Incoming`; die Upstream-Verbindung wird sofort geschlossen (HTTP/1.1) bzw. der Stream
  zurückgesetzt (HTTP/2). Der `Recorder` meldet dann aus `Drop` `client_aborted = true`. Auch bei
  nicht gestreamten Anfragen (Handler-Future verworfen).
- **Usage:** Für OpenAI-Ziele wird `stream_options.include_usage = true` erzwungen (auch für
  Anthropic-Clients, deren `message_delta` die Usage braucht). Wollte ein OpenAI-Client keine
  Usage, entfernt der Encoder den Usage-Chunk.
- **Fehler mitten im Stream** (Upstream-`{"error":…}`, Chunk-Pause/Gesamtzeit überschritten,
  Verbindungsabbruch, nicht lesbare Daten, Ende ohne `finish_reason`/`[DONE]`) werden zu
  `StreamEvent::Error` und im Eingangsformat ausgegeben (OpenAI `data: {"error":…}`, Anthropic
  `event: error`); danach endet der Stream, der Upstream wird geschlossen. Codes:
  `upstream_timeout`, `upstream_stream_interrupted`, `upstream_stream_truncated`,
  `upstream_protocol_error`.
- Azure-Vorab-Chunks nur mit `prompt_filter_results` (leere ID) eröffnen den Stream nicht.

## 6. Fehlerklassifikation (`classify_error`)

| Upstream | `ErrorClass` | retrybar | an den Client |
|---|---|---|---|
| 429 `insufficient_quota` | `quota` | nein (Fallback sinnvoll) | 429, Code bleibt |
| 429 sonst | `rate_limit` | ja, `Retry-After`/`retry-after-ms` | 429 + `Retry-After` |
| 400/404/413/422 mit `context_length_exceeded` bzw. „maximum context length“ | `context_length` | nein | 400 `context_length_exceeded` |
| 4xx `content_filter`/`content_policy_violation`, Azure `ResponsibleAIPolicyViolation` | `content_filter` | nein | 400 `content_filter` |
| 401/403 | `auth` | nein | **502** `upstream_authentication_error`, ohne Meldung des Anbieters |
| 404 | `not_found` | nein | 502 `upstream_not_found` |
| 408, 504, Zeitlimits | `timeout` | ja | 504 `upstream_timeout` |
| Verbindungsfehler | `connect` | ja | 502 `upstream_connect_error` (Details nur im Log) |
| 409, 500, 502, sonstige 5xx | `server_error` | ja | Status des Anbieters |
| 503, 529 | `overloaded` | ja | 503 |
| sonstige 4xx | `invalid_request` | nein | Status und Meldung des Anbieters |
| Antwort nicht lesbar | `protocol` | nein | 502 `upstream_protocol_error` |

Meldungen des Anbieters werden auf 1000 Zeichen gekürzt. Retries/Fallbacks selbst kommen mit
AIGW-25; `UpstreamError` trägt dafür `class`, `retryable`, `retry_after` und `upstream_status`.

## 7. Kennzahlen je Anfrage (`RequestObserver`)

Am Ende jeder Anfrage – Erfolg, Fehler, Abbruch – übergibt der Proxy genau einen
`aigw_core::observe::RequestRecord` (AIGW-43) an `ProxyState::observer` (im Binary der
Request-Log-Hook aus der Runtime). Gefüllt werden: Request-ID, Annahmezeit, Eingangsformat,
Modell-Alias, Modellgruppe, Deployment-ID, Anbieter, Anbieter-Modell, Status, `error_kind`
(auch bei Fehlern mitten im Stream; der Status bleibt dann 200), Usage + `usage_estimated`,
TTFT (Zeit bis zum ersten Inhaltsereignis, nur Streams), Gesamtdauer, Stream, Client-Abbruch,
`api_key_id`, `team_id`, `organization_id` (aus der Schlüsselprüfung, AIGW-28) und – nur wenn `payload_mode()` es verlangt –
Request-Header/-Body (ohne Kopie), JSON-Antwort bzw. bei Streams der zusammengesetzte Text.

**Usage-Schätzung:** Fehlt die Usage oder ist sie unvollständig (kein Usage-Ereignis nach dem
Endgrund, z. B. bei Abbruch), wird geschätzt: 1 Token ≈ 4 Bytes Text (Eingabe: Nachrichten,
Tool-Aufrufe/-Ergebnisse, Reasoning; Ausgabe: gestreamte Text-/Reasoning-/Tool-Fragmente).
Bekannte Werte des Anbieters (z. B. Eingabe-Tokens aus Anthropic `message_start`) haben Vorrang;
`usage_estimated = true`. Fehler vor dem ersten Byte erzeugen keine Usage.

## 8. Routing bis AIGW-24

`ModelRoutes::resolve(alias) → Arc<dyn ChatProvider>`. `StaticRoutes` ist eine feste Tabelle;
`StaticRoutes::from_model_groups(snapshot.model_groups.values(), &registry, &client)` baut sie aus
dem Snapshot (je Gruppe Name + Aliasse → erstes aktives Deployment mit unterstütztem Anbieter).
Das Binary nutzt noch die Mock-Routen; die Verdrahtung mit dem Snapshot-Halter (Neuaufbau bei
neuer `config_version`) gehört zu AIGW-24.

## 9. Messwerte

Rohdaten: [messwerte/06-latenz-2026-11-13.txt](messwerte/06-latenz-2026-11-13.txt). `scripts/bench-latency.sh` (Beispiel `crates/aigw-proxy/examples/latency.rs`): Fake-Upstream
(Kerne 0–1), Gateway (Kerne 2–3, 2 Worker-Threads) und Lastgenerator (Kerne 4–7) als getrennte
Prozesse, alle mit `SCHED_FIFO` (`RT=1`), weil die Buildbox parallel von anderen Agenten genutzt
wurde. Gemessen: Zeit bis zum ersten Body-Frame (bei Streams = TTFT) und bis zum Ende, je
Anfrage, nach Aufwärmen. Release-Profil, Buildbox cpx42 (8 vCPU AMD EPYC Genoa, geteilt),
2026-11-13. Aufschlag = Gateway − direkt.

| Szenario | direkt p50 / p99 | Gateway p50 / p99 | **Aufschlag p50 / p99** |
|---|---|---|---|
| 1 Verbindung, JSON | 36 / 64 µs | 166 / 405 µs | **130 / 341 µs** |
| 1 Verbindung, Stream (TTFT) | 36 / 61 µs | 166 / 341 µs | **130 / 280 µs** |
| 200 Verbindungen, 2 000 req/s, JSON | 106 / 221 µs | 248 / 585 µs | **142 / 364 µs** |
| 200 Verbindungen, 2 000 req/s, Stream (TTFT) | 107 / 165 µs | 261 / 567 µs | **154 / 402 µs** |
| 200 Verbindungen, 10 000 req/s, JSON | 183 / 392 µs | 504 / 1 115 µs | **321 / 723 µs** |
| 200 Verbindungen, 10 000 req/s, Stream (TTFT) | 174 / 381 µs | 506 / 1 106 µs | **332 / 725 µs** |
| 200 Verbindungen, Sättigung, JSON | 157 000 req/s | 33 000 req/s | – |
| 200 Verbindungen, Sättigung, Stream | 157 000 req/s | 25 000 req/s | – |

Bewertung: **Q1 p99 < 1 ms ist in allen Szenarien unterhalb der Sättigung erfüllt**, Q4 (TTFT
< 1 ms) ebenso. **Q1 p50 < 200 µs** ist bei niedriger und mittlerer Last erfüllt (130–154 µs),
bei 10 000 req/s auf 2 Kernen (≈ 30 % Auslastung) nicht (≈ 320 µs). Ein großer Teil des
Aufschlags ist der zusätzliche Netzwerk-Hop (zweimal Aufwecken über Loopback in einer VM); der
reine CPU-Anteil liegt bei ≈ 60 µs je Anfrage (2 Kerne / 33 000 req/s, Server- *und* Client-Seite
HTTP plus Übersetzung). Optimierungen (Profiling, Allokationen im Encoder, `Bytes` statt
`String` im SSE-Parser) folgen mit AIGW-57.

## 10. Offene Punkte

- Die Fehlerklasse (`ErrorClass`) steht nicht im `RequestRecord` (dort `ErrorKind`).
- `client_ip` füllt der Proxy noch nicht (braucht `ConnectInfo` im Listener).
- Entra-ID-`TokenSource`, Anthropic-Adapter (AIGW-16), Retries/Fallbacks (AIGW-25), Routing
  über den Snapshot (AIGW-24).
