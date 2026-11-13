# ai-gw – IR und Wire-Formate (AIGW-13)

Stand: 2026-11-13 · Status: umgesetzt (Chat), Grundlage: [01-architektur.md](01-architektur.md) §4, §7

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
├─ hints { legacy_max_tokens, stop_as_string }   ← nur für die verlustfreie Rückgabe
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
- **Erweiterbar:** Embeddings, Responses-API, Bilder und Audio bekommen eigene
  Anfrage-/Antworttypen neben `ChatRequest`; `PartKind`, `MediaSource`, `Usage`, `ApiError` und
  `WireFormat` (neue Varianten) werden wiederverwendet.

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
  `anthropic-version` gesetzt ist), `GET /healthz`. Unbekannte `/v1/*`-Pfade und falsche
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
