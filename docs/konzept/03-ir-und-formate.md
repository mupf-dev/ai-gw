# ai-gw – IR und Wire-Formate (AIGW-13, AIGW-21)

Stand: 2026-11-13 · Status: umgesetzt (Chat; Responses, Embeddings, Bilder, Audio seit AIGW-21), Grundlage: [01-architektur.md](01-architektur.md) §4, §7

Jede Anfrage der Datenebene wird vollständig in die kanonische Zwischenform (IR,
`aigw_core::ir`) geparst und für das Ziel neu serialisiert („immer IR“, §17.2). Die
Wire-Formate liegen in `aigw_core::wire::{openai, anthropic}`, SSE-Hilfen in `wire::sse`.

## 1. Struktur der IR

```
ChatRequest
├─ model, messages: Vec<Message>
│   Message { role: System|Developer|User|Assistant|Tool, content: Content, name, extra }
│   Content = Text(String) | Parts(Vec<Part>)
│   Part { kind: PartKind, cache_control, extra }
│     PartKind = Text | Image{source, detail} | Audio{data, format} | File{source, filename}
│              | ToolCall{id, name, arguments(JSON-Text)} | ToolResult{tool_call_id, content, is_error}
│              | Reasoning{text, signature} | RedactedReasoning{data} | Refusal{text}
│              | Opaque{format, value}                       ← unbekannter Teil, nur Ursprungsformat
│     MediaSource = Url | Base64{media_type, data} | Text{media_type, data} | FileId | Other(Rohwert)
├─ tools: Vec<Tool>   Tool { kind: Function{name, description, parameters, strict, extra} | Opaque, cache_control, extra }
├─ tool_choice: Auto|None|Required|Tool{name},  parallel_tool_calls
├─ sampling { temperature, top_p, top_k, frequency_penalty, presence_penalty, seed }
├─ max_tokens, stop: Vec<String>, response_format: Text|JsonObject|JsonSchema{name, description, schema, strict}
├─ reasoning { effort, thinking{mode, budget_tokens, display, extra} }
├─ user, metadata, stream, stream_options{include_usage, extra}
├─ hints { legacy_max_tokens, stop_as_string, anthropic_beta }   ← Rückgabe ins Ursprungsformat bzw. Client-Header
├─ extra                                         ← unbekannte Felder der obersten Ebene
└─ source: Option<WireFormat>                    ← steuert die Ausgabe aller `extra`

ChatResponse { id, model, created, choices: Vec<Choice{index, message, stop_reason, stop_sequence, extra}>, usage, extra, source }
Usage { input_tokens (inkl. Cache), output_tokens (inkl. Reasoning), cache_read_tokens, cache_write_tokens, reasoning_tokens, extra }
StreamEvent = Start{id, model, created} | TextDelta{index} | ReasoningDelta{index} | ReasoningSignature{index}
            | RedactedReasoning{index} | ToolCallStart{index, id, name} | ToolCallDelta{index, arguments}
            | Stop{reason, stop_sequence} | Usage(Usage) | Error(ApiError)
ApiError { kind: ErrorKind, message, param, code }
```

Grundsätze:

- **`extra` überall:** Request, Nachricht, Teil, Tool, Funktion, Usage, Antwort und Variante
  tragen ein `extra`-Objekt. Es wird nur ausgegeben, wenn das Zielformat gleich `source` ist;
  sonst verworfen und als Verlust gemeldet (`wire::Losses`, im Proxy als Log-Eintrag).
- **Unbekannte Strukturen bleiben roh:** unbekannte Inhaltsteile/-blöcke, Server-Tools,
  unbekannte `tool_choice`-/`response_format`-/`thinking`-Formen landen als `Opaque` bzw. im
  `extra` unter ihrem Originalschlüssel und kommen beim gleichen Format unverändert zurück.
- **Ein Ort für System-Anweisungen:** System/Developer sind Nachrichten. Das Anthropic-Feld
  `system` wird zur ersten Nachricht; beim Rückweg werden alle *führenden* System/Developer-
  Nachrichten wieder zu `system`, spätere bleiben `role: "system"` in `messages`.
- **Tool-Ergebnisse sind Teile:** OpenAI `role: tool` → Nachricht `Tool` mit einem
  `ToolResult`-Teil; Anthropic `tool_result`-Blöcke bleiben Teile der Nutzernachricht.
- **Usage-Semantik:** `input_tokens` enthält gecachte Tokens (OpenAI-Semantik). Anthropic:
  `input = input_tokens + cache_read + cache_creation` beim Lesen, Subtraktion beim Schreiben.
- **Erweiterbar:** Embeddings, Bilder und Audio haben eigene Anfrage-/Antworttypen neben
  `ChatRequest` (`ir::embeddings`, `ir::images`, `ir::audio`); die Responses API wird auf
  `ChatRequest`/`ChatResponse` abgebildet. `Usage`, `ApiError` und `WireFormat` (neue Varianten)
  werden wiederverwendet – siehe §6.

Effizienz: Geparst wird mit `serde_json::from_slice` direkt aus dem `Bytes`-Puffer in
Wire-Structs; Strings werden danach in die IR *verschoben*, nicht kopiert. Die IR besitzt ihre
Daten (`String`), keine Lebenszeit-Parameter (siehe offene Frage 1). Messwerte in §5.

## 2. Abbildung OpenAI Chat Completions ↔ Anthropic Messages

| Aspekt | OpenAI | IR | Anthropic |
|---|---|---|---|
| Systemprompt | `messages[].role = system` / `developer` | `Message{System/Developer}` | `system` (String/Blöcke); spätere → `role: system` |
| Nutzertext | `content: "…"` oder `[{type:text}]` | `Content::Text` / `Parts` | `content: "…"` oder `[{type:text}]` |
| Bild | `image_url.url` (http/`data:`-URL), `detail` | `Image{Url / Base64, detail}` | `image.source` `url` / `base64` / `file` |
| Datei | `file{file_data, file_id, filename}` | `File{source, filename}` | `document{source, title}` (pdf/text/url/file) |
| Audio | `input_audio{data, format}` | `Audio` | – (verworfen) |
| Tool-Definition | `{type:function, function:{name, description, parameters, strict}}` | `Tool{Function}` | `{name, description, input_schema, strict, cache_control}` |
| Server-/Spezial-Tools | `type ≠ function` | `Tool{Opaque}` | `type ≠ custom` (z. B. `web_search_*`) |
| Tool-Wahl | `auto` / `none` / `required` / `{function:{name}}` | `ToolChoice` | `auto` / `none` / `any` / `tool{name}` |
| Parallele Aufrufe | `parallel_tool_calls` | `parallel_tool_calls` | `tool_choice.disable_parallel_tool_use` (invertiert) |
| Tool-Aufruf | `assistant.tool_calls[].function.arguments` (String) | `ToolCall{arguments: JSON-Text}` | `tool_use.input` (Objekt) |
| Tool-Ergebnis | `role: tool, tool_call_id` | `ToolResult` | `user`-Block `tool_result{tool_use_id, is_error}` |
| Ablehnung | `assistant.refusal` | `Refusal` | Textblock |
| Reasoning (Inhalt) | – | `Reasoning{text, signature}` / `RedactedReasoning` | `thinking` / `redacted_thinking` |
| Reasoning (Steuerung) | `reasoning_effort` | `reasoning.effort` / `.thinking` | `output_config.effort` / `thinking{type, budget_tokens, display}` |
| Max. Tokens | `max_completion_tokens` (alt: `max_tokens`) | `max_tokens` | `max_tokens` (Pflicht) |
| Stopp | `stop` (String/Liste) | `stop` | `stop_sequences` |
| Sampling | `temperature`, `top_p`, `frequency_penalty`, `presence_penalty`, `seed` | `sampling` | `temperature`, `top_p`, `top_k` |
| JSON-Ausgabe | `response_format{json_object / json_schema}` | `ResponseFormat` | `output_config.format{type:json_schema, schema}` |
| Nutzerkennung | `user`, `metadata` | `user`, `metadata` | `metadata.user_id` |
| Prompt-Caching | – | `Part/Tool.cache_control`, oberste Ebene in `extra` | `cache_control` je Block/Tool/oben |
| Stream | `stream`, `stream_options.include_usage` | `stream`, `stream_options` | `stream` (Usage immer) |
| Endgrund | `stop` / `length` / `tool_calls` / `content_filter` | `StopReason` | `end_turn` / `max_tokens` / `stop_sequence` / `tool_use` / `refusal` / `pause_turn` |
| Usage | `prompt_tokens(+details.cached_tokens)`, `completion_tokens(+reasoning_tokens)` | `Usage` | `input_tokens`, `cache_read_input_tokens`, `cache_creation_input_tokens`, `output_tokens` |
| Fehler | `{"error":{message,type,param,code}}` | `ApiError` | `{"type":"error","error":{type,message},"request_id"}` |
| Streaming | `data: {chunk}` … `data: [DONE]` | `StreamEvent` | `message_start`, `content_block_*`, `message_delta`, `message_stop` |

## 3. Bekannte Verluste

Gleiches Format (OpenAI→IR→OpenAI, Anthropic→IR→Anthropic) ist für die Fixtures unter
`crates/aigw-core/tests/fixtures/` bitgenau auf JSON-Ebene. Bekannte Normalisierungen:

- `null`-Werte bekannter optionaler Felder (z. B. `"temperature": null`) entfallen; Feldreihenfolge
  und Zahlformat (`1` vs. `1.0` bei Fließkommafeldern) werden normalisiert.
- OpenAI: Assistant-Nachricht mit Tool-Aufrufen und `content` als Liste mit genau einem Textteil
  wird als String ausgegeben; fehlendes `content` wird `null`. `total_tokens` wird neu berechnet.
- Anthropic: leeres `metadata: {}` entfällt; `tool_result` mit `content: []` verliert das Feld.

Kreuzkonvertierung (wird als `Losses` gemeldet):

| Richtung | Verlust |
|---|---|
| OpenAI → Anthropic | `extra` (z. B. `n`, `logprobs`, `logit_bias`, `service_tier`, `store`), `frequency_penalty`, `presence_penalty`, `seed`, Audio-Teile, Bild-`detail`, `name` von Nachrichten, `metadata` (außer `user`), Name/Beschreibung/`strict` des JSON-Schemas, `json_object` wird zu Schema `{"type":"object"}`, `reasoning_effort` `minimal`/`none` → `low`, fehlendes `max_tokens` → 4096 (`DEFAULT_MAX_TOKENS`), Reasoning ohne Signatur (kompatible Anbieter) |
| Anthropic → OpenAI | `thinking`/`redacted_thinking`-Blöcke und `thinking`-Konfiguration, `cache_control` (überall), `top_k`, `is_error` von Tool-Ergebnissen, Nicht-Text-Inhalte in Tool-Ergebnissen, Server-Tools und unbekannte Blöcke (`search_result`, `server_tool_use` …), `citations`/`context` von Dokumenten, URL-Dokumente, `cache_write_tokens` in der Usage, `effort` `max` → `high` |
| Antwort Anthropic → OpenAI | Reasoning-Blöcke, `stop_details`, `stop_sequence`, Usage-Details (`cache_creation`, `service_tier`) |
| Antwort OpenAI → Anthropic | `n > 1` (nur erste Variante), `logprobs`, `system_fingerprint`, `annotations`, Details der Usage außer `cached_tokens` |
| Stream → OpenAI | Reasoning-Deltas (Chat Completions hat dafür kein Feld) |

## 4. Datenebene (aigw-proxy)

- `POST /v1/chat/completions`, `POST /v1/messages`, `GET /v1/models` (Anthropic-Format, wenn
  `anthropic-version` gesetzt ist), `GET /healthz`; seit AIGW-21 `POST /v1/responses`,
  `/v1/embeddings`, `/v1/images/generations`, `/v1/audio/transcriptions`,
  `/v1/audio/translations`, `/v1/audio/speech` (§6, Fehler im OpenAI-Format). Unbekannte `/v1/*`-Pfade und falsche
  Methoden antworten im Format des Pfads (`/v1/messages…` oder `anthropic-version` → Anthropic).
- Body-Grenze `server.data.max_body_bytes` (Standard 32 MiB), sonst `413` im Eingangsformat.
- Jede Antwort trägt `x-aigw-request-id` (UUIDv7); Anthropic-Fehler zusätzlich `request_id`.
- Auth-Header `Authorization: Bearer`, `api-key`, `x-api-key` werden ausgelesen und an den
  `Authenticator` übergeben (im Server `SnapshotAuthenticator`: Keys, Ablauf und
  Modellfreigaben aus dem Snapshot, siehe [08-admin-api.md](08-admin-api.md) §6; `AllowAll` nur
  für Tests).
- Anbieter: siehe [06-streaming-und-anbieter.md](06-streaming-und-anbieter.md); ohne Konfiguration beantwortet der Mock-Anbieter (`echo`) die Modelle `mock*`, auch gestreamt.
- Getrennte Listener: `server.data.listen` (Standard `127.0.0.1:4000`, `/v1/*` + `/healthz`) und
  `server.control.listen` (Standard `127.0.0.1:4001`, Admin-API/UI + `/healthz`); gleiche Adresse
  → ein Listener. Beide fahren bei SIGINT/SIGTERM gemeinsam geordnet herunter.

## 5. Messwerte (Mikro-Benchmark)

`cargo bench -p aigw-core --bench ir` (divan, Release-Profil, Buildbox cpx42, AMD EPYC Genoa,
1 Thread, 2026-11-13, Median):

| Benchmark | Eingabe | Median | Durchsatz |
|---|---|---|---|
| `openai_parse_to_ir_typical` – Bytes → IR | 2,5 KB (System, ~2 KB Text, 1 Tool) | 2,2 µs | 1,1 GB/s |
| `openai_roundtrip_typical` – Bytes → IR → OpenAI-JSON | 2,5 KB | 3,9 µs | 630 MB/s |
| `openai_to_anthropic_typical` – Bytes → IR → Anthropic-JSON | 2,5 KB | 3,8 µs | 650 MB/s |
| `openai_roundtrip_full_fixture` – alle Features | 2,4 KB (`request_full.json`) | 18,5 µs | 130 MB/s |
| `serde_value_baseline_typical` – nur JSON → `Value` → JSON | 2,5 KB | 3,5 µs | 710 MB/s |

Der vollständige Weg über die IR kostet bei einer typischen Anfrage etwa so viel wie ein reines
`serde_json::Value`-Roundtrip (+12 %) und liegt deutlich unter dem Budget Q1 (p50 < 200 µs
Gateway-Aufschlag). Die Fixture mit allen Sonderfällen ist teurer, weil Inhaltsteile, Tools und
`extra` über `serde_json::Map` laufen (`#[serde(flatten)]` puffert). Optimierungspotenzial (eigene
Deserializer ohne `flatten`, geliehene Strings) wird mit AIGW-57 gemessen.

## 6. Weitere Endpunkte (AIGW-21)

Alle Endpunkte laufen durch dieselbe Pipeline (`aigw-proxy/src/pipeline.rs`: Auth → Body →
Modellfreigabe → Route + Fähigkeit der Gruppe → Limits/Budgets (AIGW-30/31, eine Reservierung
je Anfrage) → Routing mit Retries/Fallbacks/Breaker/Kapazität → Antwort/Fehler im
OpenAI-Format inkl. Rate-Limit-Header, Request-Log, OpenTelemetry-Spans mit
`gen_ai.operation.name` `embeddings`/`image_generation`/`audio_transcription`/`audio_speech`/
`chat`) und dasselbe Routing wie Chat. Geschätzter Token-Bedarf für die Reservierung:
Embeddings Eingabetext / 4, Bilder Prompt / 4, Sprachausgabe Text / 4, Transkription 0
(Anfrage-Limits greifen), Responses wie Chat (Eingabe / 4 + `max_output_tokens`). Die
Plugin-Kette (AIGW-36) läuft für `/v1/responses` wie im Chat-Pfad (Pre-, Post-, Stream-Haken,
gepufferter Modus); Embeddings, Bilder und Audio haben noch keine Plugin-Haken. Die Modellgruppe
muss die Art der Anfrage in `settings.capabilities` freigeben (siehe
[10-routing.md](10-routing.md) §2a), sonst `400 unsupported_capability`.

| Endpunkt | IR | Wire (`aigw_core::wire::openai::…`) | `WireFormat` (Request-Log `endpoint`) |
|---|---|---|---|
| `POST /v1/responses` | `ChatRequest`/`ChatResponse`/`StreamEvent` | `responses` | `OpenAiResponses` (`openai-responses`) |
| `POST /v1/embeddings` | `EmbeddingsRequest`/`EmbeddingsResponse` | `embeddings` | `OpenAiEmbeddings` (`openai-embeddings`) |
| `POST /v1/images/generations` | `ImageRequest`/`ImageResponse` | `images` | `OpenAiImages` (`openai-images`) |
| `POST /v1/audio/transcriptions`, `/translations` | `TranscriptionRequest`/`TranscriptionResponse` | `audio` (multipart) | `OpenAiAudio` (`openai-audio`) |
| `POST /v1/audio/speech` | `SpeechRequest` → Byte-Strom | `audio` | `OpenAiAudio` |

**Usage (additiv):** `Usage` hat zusätzlich `images` (Anzahl erzeugter Bilder), `audio_ms`
(Dauer der Audio-Eingabe) und `characters` (Zeichen einer Sprachausgabe). Embedding-Tokens
stehen in `input_tokens` (`output_tokens = 0`). Meldet ein Anbieter keine Tokens (Gemini API
`batchEmbedContents`), wird aus dem Eingabetext geschätzt (≈ 4 Bytes/Token,
`usage_estimated`).

### 6.1 Responses API (zustandslos, über die Chat-IR)

Das Gateway speichert keine Antworten. `previous_response_id`, `conversation`,
`background: true`, `store: true` und `item_reference`-Items werden mit `400`
(`code: unsupported_parameter`, Meldung „… das Gateway speichert keine Antworten
(zustandslos) …“) abgelehnt; die Antwort trägt immer `store: false`. Fehlt `store`, gilt das
als `false` (OpenAI-Standard wäre `true`).

| Responses | IR |
|---|---|
| `instructions` (String) | erste Nachricht `System` |
| `input` (String) | eine Nutzernachricht |
| `input[]` `message` mit `input_text`, `input_image` (`image_url`/`file_id`, `detail`), `input_file` (`file_data`/`file_id`/`file_url`, `filename`), `input_audio`, `output_text`, `refusal` | Nachricht mit Teilen (`Text`, `Image`, `File`, `Audio`, `Refusal`; Unbekanntes → `Opaque`) |
| `input[]` `function_call` | `ToolCall` (an die vorige Assistant-Nachricht angehängt) |
| `input[]` `function_call_output` | Nachricht `Tool` mit `ToolResult` |
| `input[]` `reasoning` (`summary[].text`, `encrypted_content`) | `Reasoning{text, signature}` |
| `tools[]` `{type: function, name, parameters, strict}` | `Tool::Function`; andere Typen (`web_search` …) → `Opaque` |
| `tool_choice`, `parallel_tool_calls`, `temperature`, `top_p`, `max_output_tokens` | IR-Felder |
| `text.format` (`text`/`json_object`/`json_schema`) | `response_format` |
| `reasoning.effort` | `reasoning.effort` |
| `metadata`, `user` | `metadata`, `user` |
| sonst (`service_tier`, `prompt_cache_key`, `include`, `text.verbosity`, `reasoning.summary` …) | `extra` (Ursprungsformat `OpenAiResponses` → bei allen Zielen verworfen, als Verlust protokolliert) |

Antwort: Text und Ablehnung → `message`-Item (`output_text`/`refusal`), Tool-Aufrufe →
`function_call`-Items (`call_id` = IR-ID), Reasoning → `reasoning`-Item (`summary_text`;
die Signatur als `encrypted_content`, damit sie im nächsten Zug zurückkommt), `status`
`completed` bzw. `incomplete` (`max_output_tokens`, `content_filter`), Usage mit
`input_tokens_details.cached_tokens` und `output_tokens_details.reasoning_tokens`. Felder der
Anfrage (`instructions`, `tools`, `temperature` …) werden wie bei OpenAI wiederholt. Die
Antwort-ID ist neu (`resp_<uuidv7>`).

**Stream** (benannte SSE-Ereignisse, `sequence_number` fortlaufend): `response.created`,
`response.in_progress`, je Item `response.output_item.added` … `response.output_item.done`
mit `response.content_part.added/done` + `response.output_text.delta/done` (Text),
`response.reasoning_summary_part.added/done` + `response.reasoning_summary_text.delta/done`
(Reasoning), `response.function_call_arguments.delta/done` (Tools); Ende
`response.completed` bzw. `response.incomplete` mit dem vollständigen Objekt. Fehler mitten im
Stream → `error` + `response.failed`. Für Chat-Ziele wird `stream_options.include_usage`
erzwungen.

### 6.2 Embeddings

`input` als String, Liste von Strings, Token-Liste oder Liste von Token-Listen (Rückgabe in
derselben Form an OpenAI-Ziele). Vektoren liegen in der IR als `f32` vor;
`encoding_format: base64` (Little-Endian-`f32`) wird erst bei der Ausgabe an den Client
erzeugt. OpenAI/Azure werden immer mit `base64` angefragt (etwa halb so viele Bytes wie Zahlen, verlustfrei), andere
Ziele ohne `encoding_format` (Zahlen). `dimensions` und `user` gehen an OpenAI-Ziele;
Anbieter-Abbildung in [06](06-streaming-und-anbieter.md) §3f.

### 6.3 Bilder

`model`, `prompt`, `n` (1–10), `size`, `quality`, `style`, `response_format` (`url`/`b64_json`),
`user`; alles Weitere (`background`, `output_format`, `moderation` …) in `extra` (an
OpenAI/Azure unverändert). `stream: true` → `400`. **SSRF:** Das Gateway lädt nie eine
Bild-URL nach. Liefert der Anbieter nur URLs, obwohl `b64_json` gewünscht war, bleibt es bei der
URL (Verlust-Hinweis); Imagen liefert immer Base64.

### 6.4 Audio

Transkription/Übersetzung: `multipart/form-data` (eigener Parser/Builder
`aigw_core::wire::multipart`, Teile als `Bytes`-Ausschnitte ohne Kopie). Felder `file`
(Pflicht), `model` (Pflicht), `language`, `prompt`, `response_format`
(`json`/`text`/`srt`/`vtt`/`verbose_json`/andere), `temperature`,
`timestamp_granularities[]`; weitere Felder (`include[]`, `chunking_strategy` …) gehen an
OpenAI-Ziele mit. `stream: true` → `400` (noch nicht unterstützt). Größenlimit
`server.data.max_audio_bytes` (Standard 25 MiB, wirksam höchstens `max_body_bytes`) → `413`.
`text`/`srt`/`vtt` werden als Text (`text/plain`, `application/x-subrip`, `text/vtt`)
zurückgegeben, JSON-Formate mit `text`, `language`, `duration` und allen übrigen Feldern
(`segments`, `words`, `usage` …).

Sprachausgabe: JSON (`model`, `input`, `voice` String oder Objekt, `response_format`, `speed`,
`instructions`, `stream_format` `audio`/`sse`). Die Audio-Bytes werden mit Backpressure und den
Zeitlimits des Deployments ungepuffert weitergereicht (`Content-Type` des Anbieters). Bricht der
Anbieter mitten im Strom ab, wird die Verbindung zum Client abgebrochen (kein Fehlerformat
möglich); das Request-Log enthält die Fehlerklasse.
