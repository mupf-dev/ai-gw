# ai-gw – Streaming-Pipeline und Anbieter-Adapter (AIGW-14, AIGW-15, AIGW-16, AIGW-17, AIGW-18, AIGW-19, AIGW-20, AIGW-21)

Stand: 2026-11-13 · Status: umgesetzt (OpenAI-Familie, Anthropic, Ollama, vLLM, Mistral, Bedrock, Vertex/Gemini), Grundlage: [01-architektur.md](01-architektur.md) §5–§7,
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
| `aigw_providers` | `Provider`-Trait, `Capabilities`, `StreamDecoder`, `Registry` (Fabrik je `ProviderKind`), `UpstreamClient`, `Upstream` (Deployment = Adapter + Client + `Timeouts`), Adapter `openai::OpenAiAdapter` (mit Profilen `local` für Ollama/vLLM und `mistral`), `anthropic::AnthropicAdapter`, `bedrock::BedrockAdapter`, `vertex::VertexAdapter`, `with_group_defaults`, Erweiterungspunkte `auth::TokenSource`, `bedrock::AwsCredentialsProvider` |
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
    fn prepare(&self) -> Option<PrepareFuture<'_>> { None }      // async Vorbereitung (OAuth), AIGW-19
}
```

Abweichungen von der Skizze in §5, mit Begründung:

- **Eine Adapter-Instanz je Deployment** statt `build_request(&req, &Deployment)`: URL, Auth-Header
  (als *sensitive* markiert), `api-version` und Capabilities werden beim Aufbau aus dem Snapshot
  einmal berechnet, nicht pro Anfrage (Q1). Die `Registry` hält dafür Fabriken je `ProviderKind`.
- **`build_request` übernimmt die Anfrage** (keine Kopie großer Bodies mit Base64-Bildern);
  Retries (AIGW-25) klonen vorher bewusst.
- **`classify_error` bekommt die Header** (`Retry-After`, `retry-after-ms`).
- **`prepare`** (AIGW-19): optionale asynchrone Vorbereitung vor `build_request`, z. B. ein
  OAuth-Token holen, wenn keines im Cache liegt. Standard `None` – im Hot Path kostet das nur
  einen virtuellen Aufruf. `Upstream` wartet darauf innerhalb des Zeitlimits (erstes Byte bzw.
  Gesamtzeit). `build_request` bleibt synchron.
- Der Anthropic-Adapter (AIGW-16, §3a) passt ohne Änderung: `native_format = AnthropicMessages`,
  eigener `StreamDecoder` auf Basis von `wire::anthropic::StreamDecoder`, Header `x-api-key`.

`Capabilities`: `chat`, `streaming`, `tools`, `vision`, `json_schema`, `reasoning`, `embeddings`,
`images`, `audio_transcription`, `audio_speech`, `stream_usage` (unterstützt
`stream_options.include_usage`) und `max_tokens_field`; `Capabilities::supports(Capability)`
für die Endpunktarten (`Capability::{Chat, Embeddings, Images, AudioTranscription,
AudioSpeech}`).

**Weitere Endpunkte (AIGW-21)** als Trait-Methoden mit Standard „nicht unterstützt“
(`400 unsupported_capability`), damit bestehende Adapter unverändert bleiben:

```rust
fn build_embeddings(&self, req: EmbeddingsRequest, losses) -> Result<Vec<UpstreamRequest>, ApiError>;
fn parse_embeddings(&self, headers: &HeaderMap, body: &[u8]) -> Result<EmbeddingsResponse, ApiError>;
fn build_images(&self, req: ImageRequest, losses) -> Result<UpstreamRequest, ApiError>;
fn parse_images(&self, body: &[u8]) -> Result<ImageResponse, ApiError>;
fn build_transcription(&self, req: TranscriptionRequest, losses) -> Result<UpstreamRequest, ApiError>;
fn parse_transcription(&self, format: TranscriptFormat, body: &[u8]) -> Result<TranscriptionResponse, ApiError>;
fn build_speech(&self, req: SpeechRequest, losses) -> Result<UpstreamRequest, ApiError>;  // Antwort = Byte-Strom
```

`build_embeddings` darf mehrere Anfragen liefern (Anbieter mit einer bzw. wenigen Eingaben je
Aufruf); `Upstream::embeddings` führt sie mit höchstens 8 gleichzeitig aus und fügt die
Antworten in Reihenfolge zusammen (Indizes verschoben, Usage addiert). `Upstream::speech`
liefert `SpeechStream { content_type, body: ByteStream }` mit denselben Zeitlimits wie Streams
(erstes Byte, Pause, Gesamtzeit). Im Proxy entsprechen dem die `ChatProvider`-Methoden
`supports`, `embeddings`, `images`, `transcription`, `speech` (Standard: nur Chat).

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
`timeouts: {connectMs, firstByteMs, idleMs, totalMs}` (Standard 10/120/120/600 s; an der
Modellgruppe gesetzt gilt der Abschnitt für alle Deployments ohne eigenen, F44 – `connectMs`
wird geprüft; wirksam ist derzeit der Wert des gemeinsamen Outbound-Pools von 10 s, je Deployment
bräuchte es eine Pool-Variante wie bei `caFile` – Folgepunkt), `propagateTraceContext` (F98, siehe 07). `n > 1` schicken OpenAI, Azure,
vLLM und Mistral nativ; generisch kompatible Server und Ollama bekommen n parallele Anfragen
(F12, siehe 03). `stream_options` wird bei nicht gestreamten Anfragen
entfernt (OpenAI lehnt es sonst ab). Azure-Deployment-Namen werden auf `[A-Za-z0-9._-]` geprüft
(Pfadsegment).

**Entra ID (AIGW-82):** `settings.auth: entraId` schickt `Authorization: Bearer <token>` statt
`api-key`. Die Token-Quelle (`aigw_providers::entra`) unterstützt:

| `settings.entraCredential` | Ablauf | Herkunft der Angaben |
|---|---|---|
| `clientSecret` (Standard, wenn die Zugangsdaten `clientSecret` enthalten) | OAuth 2.0 Client Credentials | Zugangsdaten `tenantId`, `clientId`, `clientSecret` |
| `clientCertificate` (Standard bei `clientCertificate`) | Client Credentials mit JWT-Assertion (RS256, Header `x5t` = SHA-1-Fingerabdruck) | Zugangsdaten `tenantId`, `clientId`, `clientCertificate` (PEM: `CERTIFICATE` + `PRIVATE KEY`/`RSA PRIVATE KEY`, unverschlüsselt) |
| `workloadIdentity` | Client Credentials mit föderiertem Token als `client_assertion` (bei jedem Abruf neu gelesen) | `AZURE_FEDERATED_TOKEN_FILE`; Tenant/Client: Zugangsdaten → `settings.entraTenantId`/`entraClientId` → `AZURE_TENANT_ID`/`AZURE_CLIENT_ID` |
| `managedIdentity` | IMDS (`http://169.254.169.254/metadata/identity/oauth2/token`, `Metadata: true`, api-version 2018-02-01) bzw. App Service/Container Apps (`GET $IDENTITY_ENDPOINT?api-version=2019-08-01&resource=…&client_id=…` mit Header `X-IDENTITY-HEADER: $IDENTITY_HEADER`, Antwort mit `expires_on`; Container Apps: `http://localhost:<port>/msi/token`) | optional `clientId` (Zugangsdaten oder `settings.entraClientId`) für benutzerzugewiesene Identitäten |

- **Scope** `settings.entraScope`, Standard `https://cognitiveservices.azure.com/.default`
  (Managed Identity: als `resource` ohne `/.default`). Bei Workload/Managed Identity – der
  Identität des Gateways selbst – nur die Cognitive-Services-Scopes der Azure-Clouds
  (`.azure.com`, `.azure.us`, `.azure.cn`), damit sich über die Admin-API keine Tokens für
  andere Azure-Dienste (ARM, Key Vault) erzeugen lassen.
- **Authority** `settings.entraAuthority` (nur `https://host`, SSRF-Regeln wie Endpunkte),
  Standard `https://login.microsoftonline.com`; Sovereign Clouds z. B.
  `https://login.microsoftonline.us`, `https://login.chinacloudapi.cn`. Bei Workload Identity
  nur aus `AZURE_AUTHORITY_HOST` – das föderierte Token des Pods geht an keinen per Admin-API
  gewählten Host.
- **Cache je Credential:** Deployments mit gleichen Merkmalen (Art, Authority/Endpunkt,
  Tenant, Client, Scope, Geheimnis) teilen sich eine Token-Quelle (prozessweit, Schlüssel =
  SHA-256 der Merkmale; gilt über Konfigurationswechsel hinweg, solange ein Deployment sie
  nutzt). Ein rotiertes Secret ergibt eine neue Quelle.
- **Cache-Verhalten** (`auth::RefreshingToken`, gemeinsam mit Vertex): gültig bis Ablauf − 30 s
  (bei kurzen Laufzeiten höchstens die halbe Laufzeit), ab 5 min vor Ablauf Erneuerung im
  Hintergrund, die Anfrage nutzt das alte Token. Ohne gültiges Token wartet die Anfrage
  (`Provider::prepare`, begrenzt durch das Gesamt-Zeitlimit); alle gleichzeitig wartenden
  Anfragen teilen sich **einen** Abruf und dessen Ergebnis, auch einen Fehler (kein Thundering
  Herd). Nach einer gescheiterten Hintergrund-Erneuerung frühestens nach 5 s erneut.
- **Fehlerklassen:** Token-Endpunkt 4xx → `auth` (nicht retrybar, Konfiguration prüfen), 429 →
  `rate_limit`, 5xx sowie IMDS 404/410 → `server_error`, Verbindungsfehler → `connect`,
  Zeitlimit → `timeout`, unlesbare Antwort → `protocol` – jeweils als 502 mit Code
  `upstream_authentication_error`. Die Fehlerbeschreibung von Entra (`AADSTS…`) steht nur im
  Log (Warnung), nie in der Client-Antwort; Tokens und Geheimnisse erscheinen weder in Logs
  noch in `Debug`-Ausgaben.
- **HTTP:** alle Abrufe über aigw-outbound mit dem Client des Deployments (`upstream.roots`,
  SSRF-Regeln, keine Weiterleitungen, 30 s, 64 KiB). Der IMDS ist link-lokal und damit
  gesperrt; Managed Identity nutzt deshalb eine Client-Variante
  (`OutboundClient::with_exempt_host`), die **genau** den Host des Metadaten-Endpunkts (fest
  im Code bzw. `IDENTITY_ENDPOINT` aus der Prozessumgebung) freischaltet. Der Modell-Endpunkt
  und alle anderen Ziele unterliegen weiter den normalen Regeln.
- **Restrisiko:** Wer Deployments anlegen darf, kann ein Token der Gateway-Identität (Scope
  Cognitive Services) an einen beliebigen erlaubten Endpunkt schicken lassen – gleiche
  Vertrauensstufe wie das Hinterlegen von Zugangsdaten. Die Gateway-Identität sollte daher nur
  die Rolle „Cognitive Services OpenAI User“ auf den vorgesehenen Ressourcen haben.

`OpenAiAdapter::azure_with_token_source` bleibt für eigene `TokenSource`-Implementierungen.

**Mistral/Magistral (F61):** Reasoning-Modelle liefern `content` als Liste
(`{"type":"thinking","thinking":[{"type":"text",…}]}` + `{"type":"text",…}`); der Adapter fasst
sie – nicht gestreamt und je Stream-Chunk – zu Text + `reasoning_content` zusammen, daraus wird
IR-Reasoning. `reasoning_effort` bildet `settings.reasoningMode` ab: `prompt-mode` (Standard;
`prompt_mode: "reasoning"`, bei `none` ohne), `effort` (`reasoning_effort` unverändert, für
Modelle mit Effort-Steuerung) oder `off` (verwerfen).

## 3a. Anthropic (`anthropic`, AIGW-16)

`aigw_providers::anthropic::AnthropicAdapter`, Zielformat Anthropic Messages. Die Übersetzung
IR → Anthropic-Anfrage bzw. Antwort/SSE → IR stammt aus `aigw_core::wire::anthropic` (AIGW-13);
der Adapter ergänzt Pflichtfelder, Header und die Abbildung von OpenAI-Reasoning.

| | |
|---|---|
| URL | `{endpoint}/v1/messages`, Standard `https://api.anthropic.com`; endet `endpoint` auf `/v1`, wird nur `/messages` angehängt |
| Auth | `settings.auth: apiKey` (Standard): `x-api-key` aus Zugangsdaten `apiKey` (Pflicht, als *sensitive* markiert); `settings.auth: entraId`: `Authorization: Bearer <Entra-ID-Token>` (Claude in Azure AI Foundry, siehe unten) |
| `anthropic-version` | `settings.anthropicVersion`, Standard `2023-06-01` |
| `anthropic-beta` | `settings.anthropicBeta` (String mit Kommas oder Liste) + Betas aus dem `anthropic-beta`-Header von Anthropic-Clients (`settings.forwardClientBeta`, Standard `true`), ohne Duplikate; nur Zeichen `[A-Za-z0-9._-]`. Welche Client-Betas überhaupt ankommen, regelt die Modellgruppe: `settings.clientBetas` `allow-all` (Standard) \| `allowlist` (nur `settings.betaAllowlist`) \| `deny`; verworfene Kennungen nur im Debug-Log (F60) |
| `max_tokens` (Pflicht) | aus der Anfrage, sonst `settings.defaultMaxTokens` des Deployments, sonst der Modellgruppe (geerbt über `aigw_providers::with_group_defaults`), sonst 4096 |
| Tools | `tools` (`input_schema`, `strict`, `cache_control`), `tool_choice` `auto`/`none`/`any` (OpenAI `required`)/`tool`, `parallel_tool_calls: false` → `disable_parallel_tool_use` |
| Vision/Dokumente | Bilder Base64 (OpenAI Data-URL) und URL, Dokumente (`document`), `cache_control` je Block, Tool und (Anthropic-Clients) oben |
| Stream | alle Ereignisse inkl. `ping` (ignoriert) und `error` (→ Fehlerereignis, `api_error` → 502); Ende ohne `message_stop`/Endgrund → `upstream_stream_truncated` |
| Usage | `input_tokens` + `cache_creation_input_tokens` + `cache_read_input_tokens` = IR `input_tokens`; `cache_write_tokens`/`cache_read_tokens` getrennt; Usage aus `message_start` und kumulativ aus `message_delta` |

**Reasoning von OpenAI-Clients** (`reasoning_effort`; nur wenn die Anfrage nicht aus dem
Anthropic-Format stammt und keine eigene `thinking`-Konfiguration hat) nach
`settings.reasoningMode`:

| `reasoningMode` | Anfrage an Anthropic | geeignet für |
|---|---|---|
| `adaptive` (Standard) | `thinking: {type: adaptive}` + `output_config.effort` | Claude 4.6 und neuer |
| `effort` | nur `output_config.effort` | Modelle mit Thinking ohne Schalter (z. B. Opus 5.5) |
| `budget` | `thinking: {type: enabled, budget_tokens}` nach Tabelle, kein `effort`; ist `max_tokens` ≤ Budget, entscheidet `settings.thinkingBudgetPolicy` (F59): `raise-max-tokens` (Standard: `max_tokens` += Budget) oder `cap-budget` (Budget = `max_tokens`/2; unter 1 024 ohne Thinking) – je Deployment, von der Modellgruppe erbbar | ältere Modelle (Sonnet 4.5, Haiku 4.5 …) |
| `off` | Reasoning verworfen | Modelle ohne Thinking |

| `reasoning_effort` | `budget_tokens` (`budget`) | `effort` (`adaptive`/`effort`) |
|---|---|---|
| `none` | – (kein Thinking) | – |
| `minimal` | 1 024 | `low` |
| `low` | 2 048 | `low` |
| `medium` | 8 192 | `medium` |
| `high` | 16 384 | `high` |
| `xhigh` | 24 576 | `xhigh` |
| `max` | 32 768 | `max` |

Im Modus `budget` muss `budget_tokens < max_tokens` sein: Ist `max_tokens` ≤ Budget, wird
`max_tokens` um das Budget erhöht (die Antwort behält ihre Grenze). Schaltet der Adapter Thinking
ein, verwirft er `temperature`/`top_p`/`top_k` (die API akzeptiert mit Thinking nur
Standardwerte). Ab `anthropicVersion` ≥ `2026-10-21` werden diese drei Felder immer verworfen
(`settings.dropSampling` übersteuert). Alle Eingriffe landen in `Losses` (Debug-Log).

`RequestOptions::to_wire` ist öffentlich, damit Anthropic-Modelle auf Bedrock/Vertex dieselbe
Vorbereitung nutzen können.

### Claude in Azure AI Foundry

Foundry stellt Claude über einen Anthropic-kompatiblen Endpunkt bereit; der Anthropic-Adapter
spricht ihn ohne eigene Anbieterart an (`provider: anthropic`):

| | Foundry |
|---|---|
| `endpoint` | `https://<ressource>.services.ai.azure.com/anthropic` (mit oder ohne `/v1`) → `POST …/anthropic/v1/messages` – derselbe Pfad wie bei Anthropic, kein `api-version`-Parameter |
| `providerModel` | Name des Foundry-Deployments (z. B. `claude-sonnet-5-5`); er steht im Feld `model` |
| Header | `anthropic-version` (Pflicht, `2023-06-01`), `anthropic-beta` wie bei Anthropic |
| Auth | Ressource mit `local_auth_enabled=false` (`disableLocalAuth`): nur Entra ID → `settings.auth: entraId`, `Authorization: Bearer <token>`, Scope `https://cognitiveservices.azure.com/.default`. Mit Schlüssel ginge `x-api-key` (Zugangsdaten `apiKey`) |
| Rolle | Identität braucht auf der Foundry-Ressource „Cognitive Services User“ (bzw. „Azure AI User“) |
| Fast Mode | eigenes Foundry-Deployment `…-fast` (eigenes ai-gw-Deployment, kein `speed`-Feld) |

Die Entra-ID-Logik ist dieselbe wie bei Azure OpenAI (`aigw_providers::entra`, alle
`entraCredential`-Arten, Einstellungen `entraScope`, `entraAuthority`, `entraTenantId`,
`entraClientId`, gleiche Validierung). Zugangsdaten für `provider: anthropic` dürfen daher
auch `tenantId`+`clientId`+`clientSecret` bzw. `…+clientCertificate` enthalten. Ein
GPT-Deployment (`azure-openai`, `https://<ressource>.openai.azure.com`) und Claude-Deployments
derselben Identität teilen sich eine Token-Quelle (gleicher Scope). Bei `auth: entraId` ist
`endpoint` Pflicht – ein Entra-Token geht nie an den Standard-Endpunkt `api.anthropic.com`.
Fehler: Token-Abruf wie Azure OpenAI (502 `upstream_authentication_error`), fehlende Rolle
(401/403 von Foundry) → Klasse `auth`, Meldung verborgen. Beispiel:
[`config/examples/providers/resources-azure-foundry.yaml`](../../config/examples/providers/resources-azure-foundry.yaml).

## 3b. Ollama und vLLM (`ollama`, `vllm`, AIGW-17)

Profile des Adapters der OpenAI-Familie (gleicher Code, gleiches Wire-Format, gleicher
Stream-Decoder) über die OpenAI-kompatiblen Endpunkte.

| | Ollama | vLLM |
|---|---|---|
| Endpunkt | Standard `http://localhost:11434/v1` | Pflicht (z. B. `http://vllm:8000/v1`) |
| Auth | `Authorization: Bearer`, nur mit `apiKey` | ebenso (`--api-key`) |
| Ausgabegrenze | `max_tokens` | `max_tokens` |
| Usage im Stream | `stream_options.include_usage` wird gesendet; ältere Versionen ignorieren es und schicken die Usage im letzten Chunk mit Endgrund – der Decoder liest Usage aus **jedem** Chunk; fehlt sie ganz, wird geschätzt | `stream_options.include_usage` |
| Reasoning | `reasoning` (Delta/Nachricht) → IR-Reasoning | `reasoning_content` (neuer auch `reasoning`) → IR-Reasoning |
| `top_k` | verworfen | als Zusatzparameter `top_k` |
| Capabilities | `reasoning: true`, `stream_usage: true` | ebenso |

Reasoning-Text wird im Stream zu `ReasoningDelta` (Anthropic-Clients sehen einen
`thinking`-Block ohne Signatur), in nicht gestreamten Antworten zu einem `Reasoning`-Teil; für
OpenAI-Clients bleibt `reasoning_content` in nicht gestreamten Antworten unverändert erhalten.
`settings.streamUsage: false` und `settings.maxTokensField` gelten wie beim kompatiblen Adapter.

## 3c. Mistral (`mistral`, AIGW-20)

**Profil statt eigener Adapter:** Mistral spricht Chat Completions mit SSE und `[DONE]`;
Antwort-Parser, Stream-Decoder und Fehlerpfad sind identisch. Ein eigener Adapter würde diese
verdoppeln – das Profil (`aigw_providers::mistral`) behandelt nur die Abweichungen:

| | OpenAI | Mistral |
|---|---|---|
| Endpunkt/Auth | – | Standard `https://api.mistral.ai/v1`, `Authorization: Bearer` (Pflicht) |
| Seed | `seed` | `random_seed` |
| „mindestens ein Tool“ | `tool_choice: required` | `any` |
| Tool-Aufruf-IDs | beliebig | genau 9 Zeichen `[A-Za-z0-9]`; fremde IDs (z. B. `toolu_…` aus Anthropic-Verläufen) werden deterministisch (FNV-1a, Base62) umgeschrieben, Aufruf und Ergebnis passen weiter zusammen |
| Ausgabegrenze | `max_completion_tokens` | `max_tokens` |
| Usage im Stream | `stream_options` | kommt immer im letzten Chunk; `stream_options` wird nie gesendet (`stream_usage: false`) |
| nicht unterstützt | – | `user`, `metadata`, `reasoning_effort` werden verworfen (Mistral lehnt unbekannte Felder mit 422 ab) |
| `safe_prompt` | – | `settings.safePrompt`; OpenAI-Clients können es als unbekanntes Feld selbst senden |
| Endgrund | – | `model_length` → `max_tokens` |
| Fehler | `{"error":{…}}` | `{"object":"error","message":…}` bzw. `{"detail":[…]}` (422) werden normalisiert und wie OpenAI klassifiziert |
## 3d. AWS Bedrock (`bedrock`, AIGW-18)

Eigene Implementierung ohne AWS-SDK; Kryptografie (SHA-256, HMAC) über aws-lc-rs.

| | |
|---|---|
| URL | `POST {endpoint}/model/{modelId}/converse` bzw. `/converse-stream`; Standard-Endpunkt `https://bedrock-runtime.{region}.amazonaws.com`. Die Modell-ID (auch Inference-Profile-ID oder ARN) ist **ein** Pfadsegment (`:` → `%3A`, `/` → `%2F`). |
| Auth | **SigV4** (Dienst `bedrock`, Region aus `settings.region`): Zugangsdaten `accessKeyId`, `secretAccessKey`, optional `sessionToken` (→ `x-amz-security-token`). Alternativ Bedrock-API-Key `apiKey` (`Authorization: Bearer`). |
| Einstellungen | `region` (Pflicht), `additionalModelRequestFields` (Objekt, wird mit den abgeleiteten Feldern zusammengeführt), `guardrailConfig` (Objekt, unverändert), `timeouts` |
| Format | `WireFormat::BedrockConverse` (nur Zielformat; `extra` der Clients wird verworfen und als Verlust gemeldet) |

**SigV4** (`bedrock::sigv4`): kanonische Anfrage (Methode, Pfad – jedes Segment wie auf der
Leitung *nochmals* prozentkodiert, also `%3A` → `%253A`, wie für alle Dienste außer S3 –,
sortierte Query, Header klein/getrimmt/sortiert, signierte Header, SHA-256 des Bodys),
String to sign, Schlüsselableitung `AWS4+secret → Datum → Region → Dienst → aws4_request`,
`Authorization`-Header (als *sensitive* markiert). Signiert werden `host` und alle gesetzten
Header außer `user-agent`, `content-length` u. ä. (werden unterwegs verändert). Tests: acht
Fälle der offiziellen AWS-SigV4-Testsuite (`aws-c-auth/tests/aws-signing-test-suite/v4`, u. a.
`get-vanilla`, `post-x-www-form-urlencoded`, `get-utf8`, `post-sts-header-before`) und das
Ableitungsbeispiel der AWS-Doku; der Fake-Upstream prüft die Signatur serverseitig nach.

**Zugangsdaten** über den Erweiterungspunkt `AwsCredentialsProvider` (synchron wie
`TokenSource`; eingebaut `StaticCredentials`). IMDSv2, ECS-Container-Credentials und
`AssumeRoleWithWebIdentity` lassen sich als Implementierung mit Hintergrund-Erneuerung ergänzen
(`BedrockAdapter::with_credentials_provider`).

**Abbildung IR → Converse:**

| IR | Converse |
|---|---|
| System/Developer (alle) | `system: [{text}]` (spätere: Verlust-Hinweis „verschoben“) |
| User/Tool, Assistant | `messages[].role` `user`/`assistant`; aufeinanderfolgende gleiche Rollen werden zusammengefasst (OpenAI-`tool`-Nachrichten + Nutzertext) |
| Text, Bild (Base64, `s3://`), Datei | `{text}`, `{image:{format,source:{bytes}}}`, `{document:{format,name,source}}`; Bild-URLs (http) werden mit 400 abgelehnt |
| `ToolCall` / `ToolResult` | `toolUse{toolUseId,name,input}` / `toolResult{toolUseId,content,status:error}` |
| Reasoning / redacted | `reasoningContent.reasoningText{text,signature}` / `reasoningContent.redactedContent` |
| `cache_control` an Teil/Tool | folgender Block `{cachePoint:{type:default}}` |
| `max_tokens`, `temperature`, `top_p`, `stop` | `inferenceConfig.{maxTokens,temperature,topP,stopSequences}` |
| `top_k`, Anthropic-`thinking` | `additionalModelRequestFields.{top_k,thinking}` (nur `anthropic.*`-Modelle) |
| Tools, `tool_choice` | `toolConfig.tools[].toolSpec{name,description,inputSchema.json}`; `auto` → weglassen, `required` → `{any:{}}`, Tool → `{tool:{name}}`, `none` → Tools weglassen (mit Tool-Verlauf: Verlust-Hinweis) |
| Verluste | `frequency_penalty`, `presence_penalty`, `seed`, `response_format`, `metadata`/`user`, `parallel_tool_calls: false`, `reasoning_effort`, Audio, Bild-`detail` |

Antwort: `output.message.content` → Teile, `stopReason` (`guardrail_intervened`/
`content_filtered` → `content_filter`), Usage `input = inputTokens + cacheRead + cacheWrite`
(wie bei Anthropic). `metrics`/`trace`/`additionalModelResponseFields` landen in `extra`.
Bedrock liefert keine Antwort-ID im Body; erzeugt wird `msg_bdrk_<zufall>`.

**ConverseStream / Eventstream** (`bedrock::eventstream`): eigenes Binärformat
`application/vnd.amazon.eventstream` – Prelude (Gesamtlänge, Header-Länge, **CRC32** der
Prelude), Header (Name, Typ 0–9, Wert), Payload, **CRC32** der Nachricht; inkrementeller
Decoder (nur der Rest einer unvollständigen Nachricht wird gepuffert, Grenze 16 MiB). Falsche
Prüfsummen/Längen → `upstream_protocol_error`. Ereignisse: `messageStart` → `Start`,
`contentBlockStart.toolUse` → `ToolCallStart`, `contentBlockDelta` (`text`, `toolUse.input`,
`reasoningContent.{text,signature,redactedContent}`) → Deltas, `messageStop` → `Stop`,
`metadata` → `Usage`. Ausnahmen (`:message-type: exception`) → `StreamEvent::Error` mit dem
Ausnahmetyp als Code.

**Fehlerklassen:**

| Bedrock (`x-amzn-ErrorType` bzw. `__type`) | `ErrorClass` |
|---|---|
| `ThrottlingException` | `rate_limit` (retrybar); mit „per day“/„daily“ in der Meldung `quota` |
| `ServiceQuotaExceededException` | `quota` |
| `ValidationException` mit „Input is too long“, „too many input tokens“, „context window“ … | `context_length` |
| `ValidationException` sonst | `invalid_request` |
| `AccessDenied`, `UnrecognizedClient`, `InvalidSignature`, `ExpiredToken` | `auth` (502, ohne Anbietermeldung) |
| `ResourceNotFoundException` | `not_found` |
| `ModelTimeoutException` | `timeout` |
| `ModelNotReady`, `ServiceUnavailable` | `overloaded` |
| `InternalServer`, `ModelError`, `ModelStreamError` | `server_error` |

## 3e. Google Gemini / Vertex AI (`vertex`, AIGW-19)

Eine Anbieterart `vertex`, zwei APIs (gleiches Format `generateContent`), ohne Google-SDK:

| | Gemini API (`settings.api: gemini`) | Vertex AI (`settings.api: vertex`) |
|---|---|---|
| Basis-URL | `https://generativelanguage.googleapis.com/v1beta` | `https://{location}-aiplatform.googleapis.com/v1`, bei `location: global` `https://aiplatform.googleapis.com/v1` |
| Pfad | `/models/{model}:generateContent` | `/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent` (`providerModel` mit `publishers/…` oder `projects/…` wird übernommen) |
| Stream | `:streamGenerateContent?alt=sse` | dto. |
| Auth | `x-goog-api-key` (Zugangsdaten `apiKey`) | OAuth-Bearer aus Service-Account (`serviceAccountJson` = Schlüsseldatei, oder `clientEmail` + `privateKey` [+ `privateKeyId`]); alternativ festes `accessToken` |
| Einstellungen | – | `project` (Standard `project_id` der Schlüsseldatei), `location` (Standard `global`), `tokenUrl` (Standard `token_uri` der Schlüsseldatei bzw. `https://oauth2.googleapis.com/token`) |

`api` ist optional: nur `apiKey` → Gemini API, sonst Vertex. Gemeinsam: `safetySettings`
(Liste, unverändert übernommen), `timeouts`; `spec.endpoint` ersetzt die Basis-URL (Tests,
Proxys, Private Service Connect).

**Service-Account-Token** (`vertex::oauth`): JWT `{"alg":"RS256","typ":"JWT","kid"}` mit
`iss`=`client_email`, `scope`=`cloud-platform`, `aud`=Token-URL, `iat`, `exp`=+1 h, signiert mit
RSASSA-PKCS1-v1_5/SHA-256 (aws-lc-rs, PKCS#8 aus dem PEM); Austausch per
`grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer`. **Cache:** gültig bis
`expires_in` − 30 s (bei sehr kurzen Laufzeiten höchstens die Hälfte); ab 5 min vor Ablauf
erneuert ein Hintergrund-Task (die Anfrage nutzt das noch gültige Token), ohne gültiges Token
wartet die Anfrage über `Provider::prepare` – parallele Anfragen teilen sich einen Abruf.
Fehler des Token-Endpunkts: 4xx → `auth` (502 `upstream_authentication_error`, Grund nur im
Log), 5xx/429 → `server_error`, nicht erreichbar → `connect`. Der Token-Abruf nutzt einen
eigenen, lazily gebauten `UpstreamClient` (die Fabrik kennt den gemeinsamen Client nicht).

**Abbildung IR → Gemini:**

| IR | Gemini |
|---|---|
| System/Developer | `systemInstruction.parts[].text` |
| User/Tool, Assistant | `contents[].role` `user`/`model` (gleiche Rollen zusammengefasst) |
| Text, Bild/Datei/Audio Base64, URL | `{text}`, `{inlineData:{mimeType,data}}`, `{fileData:{mimeType,fileUri}}` (MIME aus der Endung) |
| `ToolCall` | `{functionCall:{name,args}}` (+ `thoughtSignature`, siehe unten) |
| `ToolResult` | `{functionResponse:{name,response}}` – Name aus dem zugehörigen Aufruf im Verlauf (fehlt er: 400); JSON-Objekt direkt, sonst `{"result": …}` bzw. `{"error": …}` |
| Reasoning (Text) | `{text, thought: true, thoughtSignature}` |
| Sampling, `max_tokens`, `stop`, `seed`, Penalties | `generationConfig.{temperature,topP,topK,maxOutputTokens,stopSequences,seed,presencePenalty,frequencyPenalty}` |
| `response_format` | `responseMimeType: application/json` (+ `responseJsonSchema`) |
| `reasoning_effort` | Gemini 2.x: `thinkingConfig.thinkingBudget` (minimal 0, low 1024, medium 8192, high 24576); sonst (Gemini 3): `thinkingLevel` |
| Anthropic-`thinking` | `thinkingBudget` (`disabled` → 0) und `includeThoughts: true` (außer `display: omitted`) |
| Tools, `tool_choice` | `tools[{functionDeclarations[{name,description,parametersJsonSchema}]}]`; `functionCallingConfig.mode` `AUTO`/`NONE`/`ANY` (+ `allowedFunctionNames`) |
| Verluste | `cache_control`, `metadata`/`user`, `parallel_tool_calls`, redacted Reasoning, Dateinamen, Schema-Name/`strict` |

**`thoughtSignature` bei Tool-Aufrufen:** Gemini 2.5/3 verlangt, dass die Signatur eines
`functionCall` im nächsten Zug zurückkommt; OpenAI-/Anthropic-Clients kennen das Feld nicht,
geben aber die Tool-Call-ID zurück. Der Adapter hängt die Signatur daher Base64url-kodiert an
die ID (`call_<zufall>__ts_<signatur>`, nur `[A-Za-z0-9_-]`) und stellt sie beim nächsten
Request wieder her. IDs werden nicht an Gemini gesendet (Zuordnung über Name und Reihenfolge).

Antwort: Kandidaten → `choices`, `finishReason` `STOP` → `end_turn` (bzw. `tool_use`, wenn
Funktionsaufrufe enthalten sind), `MAX_TOKENS`, Sicherheits-/Rezitations-Gründe →
`content_filter`. Usage: `input = promptTokenCount + toolUsePromptTokenCount`,
`output = candidatesTokenCount + thoughtsTokenCount`, `cache_read = cachedContentTokenCount`,
`reasoning = thoughtsTokenCount`. Stream: je SSE-Ereignis die neuen Teile; aufeinanderfolgende
Text- bzw. Gedanken-Teile bilden einen Block, `functionCall` kommt vollständig
(`ToolCallStart` + ein `ToolCallDelta`), `finishReason` → `Stop` + `Usage`.

**Safety-Blockaden:** `promptFeedback.blockReason` bzw. ein blockierender Endgrund ohne jeden
Inhalt → Fehler `content_filter` (Klasse `content_filter`, 400; auch bei HTTP 200 – `Upstream`
übernimmt dafür den Code `content_filter` aus `parse_response`); mitten im Stream nach Inhalt →
`Stop{content_filter}`.

**Fehlerklassen** (`{"error":{code,message,status,details}}`): `RESOURCE_EXHAUSTED`/429 →
`rate_limit` mit `retry_after` aus `google.rpc.RetryInfo.retryDelay`, bei `QuotaFailure` mit
`…PerDay…` → `quota`; `UNAUTHENTICATED`, `PERMISSION_DENIED`, `API_KEY_INVALID` → `auth`;
`NOT_FOUND`; `UNAVAILABLE` → `overloaded`; `DEADLINE_EXCEEDED` → `timeout`; `INTERNAL` →
`server_error`; 400 mit „input token count … exceeds the maximum number of tokens“ →
`context_length`.

**Tests:** `crates/aigw-providers/tests/bedrock_vertex.rs` gegen lokale Fakes – Bedrock
(SigV4 serverseitig geprüft, Eventstream in 7/11-Byte-Stücken, Throttling, Kontextlänge,
Ausnahme mitten im Stream, falsche Signatur → `auth`) und Google (Fake-Token-Endpunkt prüft
JWT-Signatur und Claims, Token-Cache, parallele Anfragen = ein Abruf, Erneuerung im Vorlauf,
abgelaufenes Token, Token-Ablehnung, Gemini-API-Key, SSE, Safety-Block, `RESOURCE_EXHAUSTED`
mit `retryDelay`). Der RSA-Schlüssel wird je Lauf erzeugt; keine echten Zugangsdaten.

## 3f. Embeddings, Bilder und Audio je Anbieter (AIGW-21)

| Anbieter | Embeddings | Bilder | Transkription/Übersetzung | Sprachausgabe |
|---|---|---|---|---|
| `openai` | `{endpoint}/embeddings`, `encoding_format: base64` | `/images/generations` | `/audio/transcriptions`, `/audio/translations` | `/audio/speech` |
| `azure-openai` | `{endpoint}/openai/deployments/{dep}/embeddings?api-version=…` (bzw. `/openai/v1/…` mit `apiStyle: v1`), `base64` | `…/images/generations` | `…/audio/transcriptions`, `…/audio/translations` | `…/audio/speech` |
| `openai-compatible` | `{endpoint}/embeddings` (Zahlen) | – | ja | ja |
| `vllm` | ja (Zahlen) | – | ja | – |
| `ollama` | `{endpoint}/embeddings` (Zahlen) | – | – | – |
| `mistral` | `{endpoint}/embeddings`; `dimensions` → `output_dimension`, `user` entfällt | – | – | – |
| `bedrock` | `InvokeModel` `{endpoint}/model/{id}/invoke` (SigV4/API-Key): **Titan** (`amazon.titan-embed-text-*`) `{inputText, dimensions}` – eine Anfrage je Eingabe (höchstens 256), Tokens aus `inputTextTokenCount`; **Cohere** (`cohere.embed-*`) `{texts, input_type, embedding_types: [float], output_dimension}` – Stapel zu 96, Tokens aus `x-amzn-bedrock-input-token-count`. Familie aus der Modell-ID oder `settings.embeddingFormat: titan\|cohere`; `input_type` aus der Anfrage (auch von OpenAI-Clients als Zusatzfeld), sonst `settings.inputType`, sonst `search_document` | – | – | – |
| `vertex` (Gemini API) | `{base}/models/{m}:batchEmbedContents` (`requests[].{model, content.parts[].text, outputDimensionality, taskType}`, Stapel zu 100); keine Token-Zahlen → geschätzt | Imagen `:predict` (siehe unten) | – | – |
| `vertex` (Vertex AI) | `…/publishers/google/models/{m}:predict` (`instances[].{content, task_type}`, `parameters.outputDimensionality`); Stapel zu 250, `gemini-embedding-*` einzeln; Tokens aus `statistics.token_count` | Imagen `:predict` `{instances: [{prompt}], parameters: {sampleCount, aspectRatio}}` → `predictions[].bytesBase64Encoded` (gefilterte Bilder fallen weg; keines übrig → `content_filter`) | – | – |
| `anthropic` | – | – | – | – |

`task_type`/`taskType` (Gemini) kommt aus der Anfrage oder `settings.taskType`;
`settings.embeddingBatchSize` übersteuert die Stapelgröße. Token-Eingaben (Listen von IDs)
lehnen Bedrock und Gemini mit `400` ab. Imagen: `size` wird auf das nächste
Seitenverhältnis (1:1, 3:4, 4:3, 9:16, 16:9) abgebildet; `quality`, `style`, `user` und
`url`-Rückgabe gehen verloren (Verlust-Hinweis). Fehler werden wie bei Chat klassifiziert
(gleiches `classify_error`), Retries/Fallbacks gelten ebenso.

**Mock (`provider: mock`):** unterstützt alle Endpunkte (deterministische normierte Vektoren,
1×1-PNG, `Mock-Transkript (<n> Bytes)`, WAV mit Stille) – für Smoke- und E2E-Tests.

## 4. Upstream-Client und Timeouts

- `UpstreamClient` ist eine Hülle um den gemeinsamen **Outbound-Baustein** (Crate
  `aigw-outbound`, §4.1): hyper-util-Client, rustls mit aws-lc-rs, ALPN `h2` + `http/1.1`,
  Klartext-HTTP nur für freigeschaltete lokale Server. Pool je Endpunkt (Schema+Host+Port), bis
  1024 freie Verbindungen, 90 s Leerlauf, TCP-Keep-Alive 60 s, `TCP_NODELAY`, HTTP/2-Pings alle
  30 s, adaptives HTTP/2-Fenster (`ClientConfig`).
- **SSRF nach der Namensauflösung:** Der Betrieb baut den Client mit den Regeln aus
  `admin.allow_*_upstreams` (`SnapshotRoutes::with_policy`); jede aufgelöste Adresse wird beim
  Verbindungsaufbau geprüft. Ein Deployment, dessen Name auf eine private Adresse zeigt (auch
  erst später, per DNS-Rebinding), scheitert mit `502 upstream_address_blocked`
  (`ErrorClass::Connect`, Fallback greift). `UpstreamClient::with_defaults()` hat strenge Regeln,
  `UpstreamClient::unguarded()` keine (nur Tests/Benchmarks).
- **Eigene CA je Deployment:** `settings.caFile` (absoluter Pfad, PEM) ergänzt die globalen
  Vertrauensanker; das Deployment bekommt einen eigenen Pool. Unlesbare Datei → Deployment wird
  beim Routenaufbau ausgelassen (Warnung). Die Vertex-Token-Abrufe nutzen den Client des
  Deployments (gleiche Regeln und CAs).
- Timeouts (`Timeouts`, je Deployment): Verbindungsaufbau 10 s (global im `ClientConfig`),
  **erstes Byte** = Antwort-Header 120 s (nur Streams; nicht gestreamte Antworten kommen erst
  nach der Generierung), **Pause zwischen Chunks** 120 s, **Gesamtzeit** 600 s.
- Fehlerantworten werden bis 64 KiB gelesen, nicht gestreamte Antworten bis 64 MiB.
- Der Datenebenen-Listener setzt `TCP_NODELAY` (sonst verzögert Nagle kleine SSE-Chunks).

### 4.1 Outbound-Baustein (`aigw-outbound`, AIGW-72)

Ein Baustein für **alle** ausgehenden HTTP-Verbindungen (F63). Eigenes Crate statt Modul in
`aigw-core`, damit `aigw-core` frei von hyper/rustls bleibt (es wird auch von reinen
Logik-Crates genutzt) und TLS-, Pool- und Resolver-Logik genau einmal existiert.

| Nutzer | Adressfilter (`AddressGuard`) | Limits |
|---|---|---|
| Anbieter-Adapter (`UpstreamClient`), Vertex-Token | `admin.allow_*_upstreams` | §4 (`Timeouts`), Fehler 64 KiB, Antwort 64 MiB, Token 64 KiB/30 s |
| Webhooks (Plugin + Ereignisse) | `admin.allow_*_upstreams` | je Aufruf (`timeoutMs`, `maxResponseBytes`) |
| MCP-Client | `admin.allow_*_upstreams` | `mcp.*` |
| Guardrail-Dienste (`content-safety`, `GatewayHost`) | `admin.allow_*_upstreams` | `timeoutMs`, 256 KiB |
| OIDC-IdP | keiner (Startkonfiguration des Betreibers) | `http_timeout_secs`, 1 MiB |
| OTLP-Collector | keiner (Startkonfiguration des Betreibers) | `timeout_ms`, 64 KiB, Retry (07 §12.3) |

- **Resolver mit SSRF-Filter:** hyper-util ruft ihn bei jedem neuen Verbindungsaufbau;
  übrig bleiben nur Adressen mit `UpstreamPolicy::resolved_ip_allowed(host, ip)` (privat nur,
  wenn Name oder Netz in `allow_private_upstreams` steht; unspezifizierte, Broadcast- und
  Multicast-Adressen nie). Bleibt keine übrig → `OutboundError::Blocked` (nicht wiederholbar).
  IP-Literale umgehen die Auflösung und werden vor dem Senden direkt geprüft. Der Resolver
  ist austauschbar (`Resolve`, Tests: `StaticResolver` simuliert Rebinding).
- **Keine Weiterleitungen:** 3xx wird nie verfolgt; `fetch` meldet `OutboundError::Redirect`
  (Webhooks: 3xx als Antwort, der Aufrufer wertet es als Fehlschlag).
- **Grenzen:** Verbindungsaufbau-Zeitlimit, `FetchLimits` (Gesamtzeit inkl. Body, Größe).
- **Vertrauensanker (F48):** `upstream.roots: webpki | system | both` (Standard `webpki`,
  Mozilla-Wurzeln im Binary, unabhängig vom Container-Image) und `upstream.ca_file`
  (PEM) global; je Deployment zusätzlich `settings.caFile`, für den IdP zusätzlich
  `admin.auth.oidc.ca_file`. Die globalen Werte setzt `aigw serve` beim Start
  (`install_default_tls`; unlesbare Datei oder leerer Speicher → Startabbruch). rustls-Konfigurationen
  werden je Kombination zwischengespeichert; geänderte CA-Dateien (Größe/Zeitstempel) werden beim
  nächsten Routenaufbau neu gelesen. mTLS zum Upstream: Erweiterungspunkt
  `OutboundBuilder::rustls_config` (Webhook `ClientOptions::tls`).

```yaml
upstream:
  roots: webpki          # webpki | system | both
  # ca_file: /etc/aigw/ca.pem
```

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

**Anthropic** (`classify_anthropic_error`, nach `error.type`, sonst Status):

| Upstream | `ErrorClass` | retrybar | an den Client |
|---|---|---|---|
| `overloaded_error` / 529, 503 | `overloaded` | ja | OpenAI 503, Anthropic 529 `overloaded_error` |
| `rate_limit_error` / 429 | `rate_limit` | ja, `retry-after` | 429 + `Retry-After` |
| `billing_error` / 402 | `quota` | nein (Fallback sinnvoll) | 429 `insufficient_quota` |
| `invalid_request_error` „prompt is too long“ / „context limit“ | `context_length` | nein | 400 `context_length_exceeded` |
| `authentication_error`, `permission_error` / 401, 403 | `auth` | nein | 502 `upstream_authentication_error` |
| `not_found_error` / 404 | `not_found` | nein | 502 `upstream_not_found` |
| `request_too_large` / 413 | `invalid_request` | nein | 413 |
| `api_error` / 500 | `server_error` | ja | 502 |
| `timeout_error` / 504 | `timeout` | ja | 504 |

Mistral-Fehler werden in OpenAI-Form gebracht und wie oben klassifiziert; Ollamas
`{"error":"…"}` (String) versteht der OpenAI-Pfad direkt.

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

## 8. Routing

Seit AIGW-24/25/26 liefert `ModelRoutes::route(alias)` eine `GroupRoute` (Deployments nach
Priorität, Strategie, Retries, Fallbacks, Circuit Breaker); das Binary baut sie aus dem
Snapshot und bei jeder neuen `config_version` neu. Details: [10-routing.md](10-routing.md).

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
HTTP plus Übersetzung).

**AIGW-57:** End-to-End-Messung mit dem echten `aigw`-Binary (Key-Prüfung, Limits/Budgets,
Request-Log), Profil, Optimierungen (u. a. mimalloc), Lasttests und Kaltstart:
[../betrieb/leistung.md](../betrieb/leistung.md).

## 10. Offene Punkte

- Die Fehlerklasse (`ErrorClass`) steht nicht im `RequestRecord` (dort `ErrorKind`).
- `settings.caFile` liest jede absolute Datei des Gateway-Hosts als PEM (Admin-Recht nötig,
  Inhalt wird nie ausgegeben); eine Beschränkung auf ein Verzeichnis (wie `gitops.secret_dir`)
  wäre möglich.
- `client_ip` füllt der Proxy noch nicht (braucht `ConnectInfo` im Listener).
- Entra ID: verschlüsselte PEM-Schlüssel, `x5c` (Zertifikatskette für Subject-Name/Issuer-
  Anmeldung), Azure-Arc-Managed-Identity, Weitergabe der Nutzeridentität (On-Behalf-Of).
- Reasoning in Assistant-Nachrichten des Verlaufs von OpenAI-Clients (`reasoning_content`)
  wird noch nicht zurück in die IR übernommen (Anthropic bräuchte ohnehin die Signatur).
- Mistral: Reasoning im Verlauf (`thinking`-Blöcke in Assistant-Nachrichten) wird nicht an
  Mistral zurückgegeben.
- Bedrock: IMDSv2/ECS/Web-Identity-`AwsCredentialsProvider`, strukturierte Ausgabe
  (`response_format`), Application-Inference-Profiles/Prompt-Router nur als Modell-ID getestet.
- Vertex: Workload Identity/Metadaten-Server als `TokenSource`, Vertex-Express-Modus (API-Key auf
  aiplatform), `cachedContents`, Bild-Ausgabe (`inlineData` in Antworten → `Opaque`).
- AIGW-21: Bild-Bearbeitung/-Varianten (`/v1/images/edits`, `/variations`), gestreamte
  Transkription (`stream: true`) und gestreamte Bilder, Bedrock-Bildmodelle (Titan
  Image/Stability), Google Speech-to-Text/Text-to-Speech, Responses API nativ an OpenAI/Azure
  durchreichen (mit `store`/`previous_response_id`), Kosten je Bild/Audio-Sekunde in der
  Abrechnung.
