// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Adapter für Google Gemini (AIGW-19) – Gemini API und Vertex AI, ohne Google-SDK.
//!
//! | | Gemini API (`api: gemini`) | Vertex AI (`api: vertex`) |
//! |---|---|---|
//! | Basis-URL | `https://generativelanguage.googleapis.com/v1beta` | `https://{location}-aiplatform.googleapis.com/v1` (`global`: `https://aiplatform.googleapis.com/v1`) |
//! | Pfad | `/models/{model}:generateContent` | `/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent` |
//! | Auth | `x-goog-api-key` (`apiKey`) | OAuth-Bearer aus Service-Account ([`oauth`]) oder festes `accessToken` |
//!
//! Gestreamt wird `:streamGenerateContent?alt=sse` (SSE, je Ereignis eine vollständige
//! `GenerateContentResponse` mit den neuen Teilen).
//!
//! Einstellungen (`spec.settings`): `api` (`gemini`/`vertex`; Standard nach Zugangsdaten),
//! `project` (Vertex; Standard `project_id` der Schlüsseldatei), `location` (Vertex; Standard
//! `global`), `tokenUrl` (Vertex; Standard `token_uri` der Schlüsseldatei bzw.
//! `https://oauth2.googleapis.com/token`), `safetySettings` (Liste, wird übernommen),
//! `timeouts`. Zugangsdaten: `apiKey` bzw. `serviceAccountJson` (Inhalt der Schlüsseldatei)
//! oder `clientEmail` + `privateKey` (+ `privateKeyId`), alternativ `accessToken`.

pub mod oauth;
mod stream;

use std::collections::HashMap;
use std::sync::Arc;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorClass, ErrorKind, MediaSource,
    Message, Part, PartKind, ResponseFormat, Role, StopReason, ToolChoice, ToolKind, UpstreamError,
    Usage,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::{Losses, WireFormat};
use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use serde_json::{Map, Value, json};

use crate::auth::{StaticToken, TokenSource};
use crate::codec::{base64, base64_decode, base64url, random_id, unix_now};
use crate::errmap::{
    class_from_status, internal, looks_like_context_length, protocol_error, retry_after_header,
    upstream_error,
};
use crate::settings::Settings;
use crate::{
    Capabilities, ConfigError, MaxTokensField, PrepareFuture, Provider, StreamDecoder,
    UpstreamRequest,
};
use aigw_core::ir::embeddings::{Embedding, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse};
use aigw_core::ir::images::{GeneratedImage, ImageRequest, ImageResponse, ImageResponseFormat};

pub use oauth::{ServiceAccountKey, ServiceAccountTokenSource};
pub use stream::GeminiStreamDecoder;

/// Standard-Basis-URL der Gemini API.
pub const GEMINI_DEFAULT_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Standard-Region für Vertex AI.
pub const VERTEX_DEFAULT_LOCATION: &str = "global";

const USER_AGENT_VALUE: &str = concat!("ai-gw/", env!("CARGO_PKG_VERSION"));

/// Trenner zwischen Aufruf-ID und eingebetteter `thoughtSignature` in Tool-Call-IDs.
const SIGNATURE_SEPARATOR: &str = "__ts_";

/// Welche Google-API angesprochen wird.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiApi {
    /// Gemini API (generativelanguage.googleapis.com, API-Key).
    Gemini,
    /// Vertex AI (aiplatform.googleapis.com, OAuth).
    Vertex,
}

#[derive(Debug, Clone)]
enum Auth {
    ApiKey(HeaderValue),
    ServiceAccount(Arc<ServiceAccountTokenSource>),
    Token(Arc<dyn TokenSource>),
}

/// Adapter für Gemini (Gemini API und Vertex AI).
#[derive(Debug, Clone)]
pub struct VertexAdapter {
    api: GeminiApi,
    provider_model: String,
    uri: Uri,
    stream_uri: Uri,
    auth: Auth,
    safety_settings: Option<Value>,
    caps: Capabilities,
    /// Embeddings (`:batchEmbedContents` bzw. Vertex `:predict`) und Imagen (`:predict`),
    /// AIGW-21.
    embed_uri: Uri,
    predict_uri: Uri,
    embed_batch: usize,
    task_type: Option<String>,
}

/// Höchstens so viele Eingaben je `batchEmbedContents` (Gemini API).
const GEMINI_EMBED_BATCH: usize = 100;
/// Höchstens so viele Instanzen je `:predict` (Vertex, `text-embedding-*`).
const VERTEX_EMBED_BATCH: usize = 250;
/// Höchstens so viele Teilanfragen je Embedding-Anfrage.
const MAX_EMBED_CALLS: usize = 256;

fn parse_uri(s: &Settings<'_>, url: &str) -> Result<Uri, ConfigError> {
    let uri: Uri = url
        .parse()
        .map_err(|e| s.invalid(format!("ungültige URL {url:?}: {e}")))?;
    match (uri.scheme_str(), uri.authority()) {
        (Some("http" | "https"), Some(_)) => Ok(uri),
        _ => Err(s.invalid(format!("URL {url:?} braucht http(s)://host"))),
    }
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
}

fn valid_model_path(s: &str) -> bool {
    !s.is_empty()
        && s.split('/')
            .all(|seg| valid_segment(seg) && seg != ".." && seg != ".")
}

impl VertexAdapter {
    /// Fabrik für die [`Registry`](crate::Registry).
    pub fn factory(dep: &DeploymentCtx) -> Result<Arc<dyn Provider>, ConfigError> {
        Ok(Arc::new(Self::from_deployment(dep)?))
    }

    /// Baut den Adapter aus einem Deployment.
    pub fn from_deployment(dep: &DeploymentCtx) -> Result<Self, ConfigError> {
        Self::build(dep, None)
    }

    /// Vertex AI mit eigener [`TokenSource`] (z. B. Workload Identity, Metadaten-Server –
    /// Erweiterungspunkt); Zugangsdaten im Deployment sind dann nicht nötig.
    pub fn with_token_source(
        dep: &DeploymentCtx,
        tokens: Arc<dyn TokenSource>,
    ) -> Result<Self, ConfigError> {
        Self::build(dep, Some(tokens))
    }

    fn build(
        dep: &DeploymentCtx,
        tokens: Option<Arc<dyn TokenSource>>,
    ) -> Result<Self, ConfigError> {
        let s = Settings::of(dep);
        let spec = &dep.deployment.spec;
        let cred = |names: &[&str]| {
            dep.credential
                .as_ref()
                .and_then(|c| names.iter().find_map(|n| c.get(n)))
        };
        let api_key = cred(&["apiKey", "api_key"]);
        let sa_json = cred(&["serviceAccountJson", "service_account_json"]);
        let client_email = cred(&["clientEmail", "client_email"]);
        let access_token = cred(&["accessToken", "access_token"]);

        let api = match s.str("api")? {
            Some("gemini") => GeminiApi::Gemini,
            Some("vertex") => GeminiApi::Vertex,
            Some(other) => {
                return Err(s.invalid(format!(
                    "settings.api = {other:?} unbekannt (erlaubt: gemini, vertex)"
                )));
            }
            None if tokens.is_none()
                && api_key.is_some()
                && sa_json.is_none()
                && client_email.is_none() =>
            {
                GeminiApi::Gemini
            }
            None => GeminiApi::Vertex,
        };
        if !valid_model_path(&spec.provider_model) {
            return Err(s.invalid(format!("providerModel {:?} ungültig", spec.provider_model)));
        }

        let (base, auth) = match api {
            GeminiApi::Gemini => {
                let key = api_key.ok_or_else(|| s.missing("Zugangsdaten (apiKey)"))?;
                let mut v = HeaderValue::from_str(key.expose())
                    .map_err(|_| s.invalid("Zugangsdaten enthalten unzulässige Zeichen"))?;
                v.set_sensitive(true);
                let base = spec
                    .endpoint
                    .clone()
                    .unwrap_or_else(|| GEMINI_DEFAULT_ENDPOINT.to_owned());
                let model = spec
                    .provider_model
                    .strip_prefix("models/")
                    .unwrap_or(&spec.provider_model);
                (
                    format!("{}/models/{model}", base.trim_end_matches('/')),
                    Auth::ApiKey(v),
                )
            }
            GeminiApi::Vertex => {
                let key = if let Some(j) = sa_json {
                    Some(
                        ServiceAccountKey::from_json(j.expose())
                            .map_err(|e| s.invalid(e.to_string()))?,
                    )
                } else if let Some(email) = client_email {
                    let pem = cred(&["privateKey", "private_key"])
                        .ok_or_else(|| s.missing("Zugangsdaten (privateKey)"))?;
                    Some(
                        ServiceAccountKey::from_pem(email.expose(), pem.expose())
                            .map_err(|e| s.invalid(e.to_string()))?,
                    )
                } else {
                    None
                };
                let project = match s.str("project")? {
                    Some(p) => p.to_owned(),
                    None => key
                        .as_ref()
                        .and_then(|k| k.project_id().map(str::to_owned))
                        .ok_or_else(|| s.missing("settings.project"))?,
                };
                let location = s.str("location")?.unwrap_or(VERTEX_DEFAULT_LOCATION);
                if !valid_segment(&project) || !valid_segment(location) {
                    return Err(s.invalid("settings.project/location ungültig"));
                }
                let base = spec.endpoint.clone().unwrap_or_else(|| {
                    if location == "global" {
                        "https://aiplatform.googleapis.com/v1".to_owned()
                    } else {
                        format!("https://{location}-aiplatform.googleapis.com/v1")
                    }
                });
                let base = base.trim_end_matches('/');
                let m = &spec.provider_model;
                let url = if m.starts_with("projects/") {
                    format!("{base}/{m}")
                } else if m.starts_with("publishers/") {
                    format!("{base}/projects/{project}/locations/{location}/{m}")
                } else {
                    format!(
                        "{base}/projects/{project}/locations/{location}/publishers/google/models/{m}"
                    )
                };
                let auth = if let Some(t) = tokens {
                    Auth::Token(t)
                } else if let Some(key) = key {
                    let token_url = s
                        .str("tokenUrl")?
                        .map(str::to_owned)
                        .or_else(|| key.token_uri().map(str::to_owned))
                        .unwrap_or_else(|| oauth::DEFAULT_TOKEN_URL.to_owned());
                    let token_uri = parse_uri(&s, &token_url)?;
                    Auth::ServiceAccount(Arc::new(ServiceAccountTokenSource::new(
                        key,
                        token_uri,
                        oauth::CLOUD_PLATFORM_SCOPE,
                    )))
                } else if let Some(t) = access_token {
                    Auth::Token(Arc::new(StaticToken(t.clone())))
                } else {
                    return Err(s.missing(
                        "Zugangsdaten (serviceAccountJson, clientEmail+privateKey oder accessToken)",
                    ));
                };
                (url, auth)
            }
        };
        let safety_settings = match s.value("safetySettings") {
            None | Some(Value::Null) => None,
            Some(v @ Value::Array(_)) => Some(v.clone()),
            Some(_) => return Err(s.invalid("settings.safetySettings muss eine Liste sein")),
        };
        let default_batch = match api {
            GeminiApi::Gemini => GEMINI_EMBED_BATCH,
            // `gemini-embedding-*` nimmt auf Vertex nur eine Instanz je Aufruf an.
            GeminiApi::Vertex if spec.provider_model.contains("gemini-embedding") => 1,
            GeminiApi::Vertex => VERTEX_EMBED_BATCH,
        };
        let embed_batch = s
            .positive_u64("embeddingBatchSize")?
            .map_or(default_batch, |n| {
                usize::try_from(n).unwrap_or(default_batch)
            });
        Ok(Self {
            embed_uri: parse_uri(
                &s,
                &format!(
                    "{base}:{}",
                    match api {
                        GeminiApi::Gemini => "batchEmbedContents",
                        GeminiApi::Vertex => "predict",
                    }
                ),
            )?,
            predict_uri: parse_uri(&s, &format!("{base}:predict"))?,
            embed_batch,
            task_type: s.str("taskType")?.map(str::to_owned),
            api,
            provider_model: spec.provider_model.clone(),
            uri: parse_uri(&s, &format!("{base}:generateContent"))?,
            stream_uri: parse_uri(&s, &format!("{base}:streamGenerateContent?alt=sse"))?,
            auth,
            safety_settings,
            caps: Capabilities {
                chat: true,
                streaming: true,
                tools: true,
                vision: true,
                json_schema: true,
                reasoning: true,
                embeddings: true,
                images: true,
                audio_transcription: false,
                audio_speech: false,
                stream_usage: true,
                max_tokens_field: MaxTokensField::MaxTokens,
            },
        })
    }

    /// Angesprochene API.
    pub fn api(&self) -> GeminiApi {
        self.api
    }

    /// POST mit Auth-Headern (API-Key bzw. OAuth-Bearer).
    fn post(
        &self,
        uri: &Uri,
        accept: &'static str,
        body: Vec<u8>,
    ) -> Result<UpstreamRequest, ApiError> {
        let mut builder = http::Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static(accept))
            .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
        let token = match &self.auth {
            Auth::ApiKey(v) => {
                builder = builder.header(HeaderName::from_static("x-goog-api-key"), v.clone());
                None
            }
            Auth::ServiceAccount(src) => Some(src.bearer_token()?),
            Auth::Token(src) => Some(src.bearer_token()?),
        };
        if let Some(t) = token {
            let mut v = HeaderValue::from_str(&format!("Bearer {}", t.expose()))
                .map_err(|_| internal("Token enthält unzulässige Zeichen"))?;
            v.set_sensitive(true);
            builder = builder.header(AUTHORIZATION, v);
        }
        builder.body(Bytes::from(body)).map_err(internal)
    }

    /// Ziel-URL (nicht gestreamt).
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Ziel-URL (gestreamt).
    pub fn stream_uri(&self) -> &Uri {
        &self.stream_uri
    }

    /// Service-Account-Token-Quelle (falls so konfiguriert).
    pub fn service_account(&self) -> Option<&Arc<ServiceAccountTokenSource>> {
        match &self.auth {
            Auth::ServiceAccount(t) => Some(t),
            _ => None,
        }
    }

    /// IR → `generateContent`-Body.
    pub fn gemini_body(&self, req: ChatRequest, losses: &mut Losses) -> Result<Value, ApiError> {
        let mut body = to_gemini(req, &self.provider_model, losses)?;
        if let (Some(s), Some(o)) = (&self.safety_settings, body.as_object_mut()) {
            o.insert("safetySettings".into(), s.clone());
        }
        Ok(body)
    }
}

impl Provider for VertexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Vertex
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::Gemini
    }

    fn provider_model(&self) -> &str {
        &self.provider_model
    }

    fn prepare(&self) -> Option<PrepareFuture<'_>> {
        match &self.auth {
            Auth::ServiceAccount(src) => Some(Box::pin(src.ensure())),
            _ => None,
        }
    }

    fn build_request(
        &self,
        req: ChatRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let stream = req.is_stream();
        let body = self.gemini_body(req, losses)?;
        let body = serde_json::to_vec(&body).map_err(internal)?;
        self.post(
            if stream { &self.stream_uri } else { &self.uri },
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
            body,
        )
    }

    fn build_embeddings(
        &self,
        req: EmbeddingsRequest,
        losses: &mut Losses,
    ) -> Result<Vec<UpstreamRequest>, ApiError> {
        let EmbeddingInput::Text(texts) = req.input else {
            return Err(ApiError::invalid_request(
                "Gemini-Embeddings brauchen Text-Eingaben (keine Token-IDs)",
            )
            .with_param("input"));
        };
        if req.user.is_some() {
            losses.note("gemini embeddings: user verworfen");
        }
        let mut extra = req.extra;
        let task_type = match extra
            .remove("task_type")
            .or_else(|| extra.remove("taskType"))
        {
            Some(Value::String(t)) => Some(t),
            _ => self.task_type.clone(),
        };
        losses.extra("gemini embeddings", &extra);
        let batch = self.embed_batch.max(1);
        if texts.len().div_ceil(batch) > MAX_EMBED_CALLS {
            return Err(ApiError::invalid_request(format!(
                "zu viele Eingaben (höchstens {} je Anfrage)",
                MAX_EMBED_CALLS * batch
            ))
            .with_param("input"));
        }
        let model = format!(
            "models/{}",
            self.provider_model
                .strip_prefix("models/")
                .unwrap_or(&self.provider_model)
        );
        let mut out = Vec::new();
        for chunk in texts.chunks(batch) {
            let body = match self.api {
                GeminiApi::Gemini => {
                    let requests: Vec<Value> = chunk
                        .iter()
                        .map(|t| {
                            let mut r =
                                json!({"model": model, "content": {"parts": [{"text": t}]}});
                            if let Some(o) = r.as_object_mut() {
                                if let Some(d) = req.dimensions {
                                    o.insert("outputDimensionality".into(), Value::from(d));
                                }
                                if let Some(tt) = &task_type {
                                    o.insert("taskType".into(), Value::from(tt.as_str()));
                                }
                            }
                            r
                        })
                        .collect();
                    json!({ "requests": requests })
                }
                GeminiApi::Vertex => {
                    let instances: Vec<Value> = chunk
                        .iter()
                        .map(|t| {
                            let mut i = json!({ "content": t });
                            if let (Some(tt), Some(o)) = (&task_type, i.as_object_mut()) {
                                o.insert("task_type".into(), Value::from(tt.as_str()));
                            }
                            i
                        })
                        .collect();
                    let mut body = json!({ "instances": instances });
                    if let (Some(d), Some(o)) = (req.dimensions, body.as_object_mut()) {
                        o.insert("parameters".into(), json!({ "outputDimensionality": d }));
                    }
                    body
                }
            };
            let body = serde_json::to_vec(&body).map_err(internal)?;
            out.push(self.post(&self.embed_uri, "application/json", body)?);
        }
        Ok(out)
    }

    fn parse_embeddings(
        &self,
        _headers: &HeaderMap,
        body: &[u8],
    ) -> Result<EmbeddingsResponse, ApiError> {
        let v: Value = serde_json::from_slice(body).map_err(protocol_error)?;
        let floats = |v: &Value| -> Option<Vec<f32>> {
            v.as_array()?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect()
        };
        let mut tokens: Option<u64> = None;
        let vectors: Vec<Vec<f32>> = match self.api {
            GeminiApi::Gemini => v
                .get("embeddings")
                .and_then(Value::as_array)
                .ok_or_else(|| protocol_error("Antwort ohne embeddings"))?
                .iter()
                .map(|e| e.get("values").and_then(floats))
                .collect::<Option<_>>()
                .ok_or_else(|| protocol_error("Vektor ungültig"))?,
            GeminiApi::Vertex => {
                let preds = v
                    .get("predictions")
                    .and_then(Value::as_array)
                    .ok_or_else(|| protocol_error("Antwort ohne predictions"))?;
                let mut out = Vec::with_capacity(preds.len());
                for p in preds {
                    let e = p
                        .get("embeddings")
                        .ok_or_else(|| protocol_error("Vorhersage ohne embeddings"))?;
                    out.push(
                        e.get("values")
                            .and_then(floats)
                            .ok_or_else(|| protocol_error("Vektor ungültig"))?,
                    );
                    if let Some(n) = e.pointer("/statistics/token_count").and_then(Value::as_f64) {
                        tokens = Some(tokens.unwrap_or(0) + n.max(0.0) as u64);
                    }
                }
                out
            }
        };
        // Die Gemini API meldet keine Tokens: grob schätzen (≈ 4 Bytes je Token) – das
        // Request-Log kennzeichnet die Usage dann als geschätzt.
        Ok(EmbeddingsResponse {
            model: self.provider_model.clone(),
            data: vectors
                .into_iter()
                .zip(0u32..)
                .map(|(vector, index)| Embedding { index, vector })
                .collect(),
            usage_estimated: tokens.is_none(),
            usage: Usage {
                input_tokens: tokens.unwrap_or(0),
                ..Usage::default()
            },
            extra: Map::new(),
        })
    }

    fn build_images(
        &self,
        req: ImageRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let mut parameters = Map::new();
        parameters.insert("sampleCount".into(), Value::from(req.count()));
        if let Some(size) = &req.size {
            match imagen_aspect_ratio(size) {
                Some(r) => {
                    parameters.insert("aspectRatio".into(), Value::from(r));
                }
                None if size == "auto" => {}
                None => losses.note(format!("imagen: Größe {size} ohne Entsprechung")),
            }
        }
        if req.response_format == Some(ImageResponseFormat::Url) {
            losses.note("imagen: liefert nur Base64 (url nicht möglich)");
        }
        for (what, present) in [
            ("quality", req.quality.is_some()),
            ("style", req.style.is_some()),
            ("user", req.user.is_some()),
        ] {
            if present {
                losses.note(format!("imagen: {what} verworfen"));
            }
        }
        losses.extra("imagen", &req.extra);
        let body = json!({
            "instances": [{ "prompt": req.prompt }],
            "parameters": parameters,
        });
        let body = serde_json::to_vec(&body).map_err(internal)?;
        self.post(&self.predict_uri, "application/json", body)
    }

    fn parse_images(&self, body: &[u8]) -> Result<ImageResponse, ApiError> {
        let v: Value = serde_json::from_slice(body).map_err(protocol_error)?;
        let preds = v
            .get("predictions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut data = Vec::new();
        let mut filtered = None;
        for p in preds {
            match p.get("bytesBase64Encoded").and_then(Value::as_str) {
                Some(b64) => data.push(GeneratedImage {
                    b64_json: Some(b64.to_owned()),
                    url: None,
                    revised_prompt: p.get("prompt").and_then(Value::as_str).map(str::to_owned),
                    media_type: p.get("mimeType").and_then(Value::as_str).map(str::to_owned),
                }),
                None => {
                    filtered = p
                        .get("raiFilteredReason")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
        }
        if data.is_empty() {
            return Err(ApiError::invalid_request(format!(
                "Imagen hat kein Bild erzeugt{}",
                filtered.map(|r| format!(" ({r})")).unwrap_or_default()
            ))
            .with_code("content_filter"));
        }
        Ok(ImageResponse {
            created: i64::try_from(unix_now()).unwrap_or(0),
            usage: Usage {
                images: Some(u64::try_from(data.len()).unwrap_or(u64::MAX)),
                ..Usage::default()
            },
            data,
            extra: Map::new(),
            source: Some(WireFormat::Gemini),
        })
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, ApiError> {
        from_gemini(body, &self.provider_model)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GeminiStreamDecoder::new(self.provider_model.clone()))
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> UpstreamError {
        classify_gemini_error(status, headers, body)
    }
}

// ---------------------------------------------------------------------------------------------
// Tool-Call-IDs mit eingebetteter thoughtSignature
// ---------------------------------------------------------------------------------------------

/// Gemini (ab 2.5/3) verlangt, dass die `thoughtSignature` eines `functionCall` im nächsten
/// Zug unverändert zurückkommt. Clients (OpenAI-/Anthropic-Format) kennen das Feld nicht,
/// geben aber die Tool-Call-ID zurück – daher wird die Signatur (Base64url) an die ID gehängt.
pub(crate) fn tool_call_id(base: Option<&str>, signature: Option<&str>) -> String {
    let mut id = base
        .filter(|b| !b.is_empty())
        .map_or_else(|| random_id("call_"), str::to_owned);
    if let Some(sig) = signature.and_then(base64_decode) {
        id.push_str(SIGNATURE_SEPARATOR);
        id.push_str(&base64url(&sig));
    }
    id
}

/// OpenAI-Bildgröße → Imagen-Seitenverhältnis.
fn imagen_aspect_ratio(size: &str) -> Option<&'static str> {
    let (w, h) = size.split_once('x')?;
    let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    if w == 0 || h == 0 {
        return None;
    }
    // Nächstes unterstütztes Verhältnis (1:1, 3:4, 4:3, 9:16, 16:9).
    let r = f64::from(w) / f64::from(h);
    [
        ("1:1", 1.0),
        ("3:4", 0.75),
        ("4:3", 4.0 / 3.0),
        ("9:16", 9.0 / 16.0),
        ("16:9", 16.0 / 9.0),
    ]
    .into_iter()
    .min_by(|a, b| (a.1 - r).abs().total_cmp(&(b.1 - r).abs()))
    .map(|(n, _)| n)
}

/// Trennt eine Tool-Call-ID in (ID, thoughtSignature als Standard-Base64).
fn split_tool_call_id(id: &str) -> (&str, Option<String>) {
    match id.split_once(SIGNATURE_SEPARATOR) {
        Some((base, sig)) => (base, base64_decode(sig).map(|b| base64(&b))),
        None => (id, None),
    }
}

// ---------------------------------------------------------------------------------------------
// IR → Gemini
// ---------------------------------------------------------------------------------------------

fn guess_mime(url: &str, default: &str) -> String {
    let ext = url
        .rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("mp3") => "audio/mp3",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        _ => default,
    }
    .to_owned()
}

fn media_part(source: MediaSource, default_mime: &str) -> Result<Value, ApiError> {
    match source {
        MediaSource::Base64 { media_type, data } => Ok(json!({
            "inlineData": { "mimeType": media_type.as_deref().unwrap_or(default_mime), "data": data }
        })),
        MediaSource::Text { media_type, data } => Ok(json!({
            "inlineData": { "mimeType": media_type, "data": base64(data.as_bytes()) }
        })),
        MediaSource::Url { url } => Ok(json!({
            "fileData": { "mimeType": guess_mime(&url, default_mime), "fileUri": url }
        })),
        MediaSource::FileId { file_id } => Ok(json!({
            "fileData": { "mimeType": default_mime, "fileUri": file_id }
        })),
        MediaSource::Other { .. } => Err(ApiError::invalid_request(
            "Gemini unterstützt diese Medienquelle nicht",
        )),
    }
}

/// `functionResponse.response` muss ein Objekt sein.
fn function_response(content: Content, is_error: Option<bool>, losses: &mut Losses) -> Value {
    let mut text = String::new();
    for p in content.into_parts() {
        match p.kind {
            PartKind::Text { text: t } => text.push_str(&t),
            _ => losses.note("Gemini: Nicht-Text-Inhalt im Tool-Ergebnis verworfen"),
        }
    }
    let value = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(o)) if is_error != Some(true) => return Value::Object(o),
        Ok(v) => v,
        Err(_) => Value::String(text),
    };
    if is_error == Some(true) {
        json!({ "error": value })
    } else {
        json!({ "result": value })
    }
}

struct PartsOut<'a> {
    out: Vec<Value>,
    losses: &'a mut Losses,
    names: &'a HashMap<String, String>,
    /// Signatur eines leeren Reasoning-Teils, wird an den nächsten Teil gehängt.
    pending_signature: Option<String>,
}

impl PartsOut<'_> {
    fn emit(&mut self, mut v: Value) {
        if let (Some(sig), Some(o)) = (self.pending_signature.take(), v.as_object_mut()) {
            o.entry("thoughtSignature").or_insert(Value::String(sig));
        }
        self.out.push(v);
    }

    fn push(&mut self, part: Part) -> Result<(), ApiError> {
        if part.cache_control.is_some() {
            self.losses.note("Gemini: cache_control");
        }
        if !part.extra.is_empty() {
            self.losses.extra("Gemini: Inhaltsteil", &part.extra);
        }
        match part.kind {
            PartKind::Text { text } | PartKind::Refusal { text } => {
                if !text.is_empty() {
                    self.emit(json!({ "text": text }));
                }
            }
            PartKind::Image { source, detail } => {
                if detail.is_some() {
                    self.losses.note("Gemini: Bild-detail");
                }
                let p = media_part(source, "image/jpeg")?;
                self.emit(p);
            }
            PartKind::File { source, filename } => {
                if filename.is_some() {
                    self.losses.note("Gemini: Dateiname");
                }
                let p = media_part(source, "application/pdf")?;
                self.emit(p);
            }
            PartKind::Audio { data, format } => {
                self.emit(json!({
                    "inlineData": { "mimeType": format!("audio/{format}"), "data": data }
                }));
            }
            PartKind::ToolCall {
                id,
                name,
                arguments,
            } => {
                let args: Value = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&arguments).map_err(|e| {
                        ApiError::invalid_request(format!(
                            "Tool-Argumente sind kein gültiges JSON: {e}"
                        ))
                        .with_param("messages")
                    })?
                };
                let (_, sig) = split_tool_call_id(&id);
                let mut p = json!({ "functionCall": { "name": name, "args": args } });
                if let Some(sig) = sig {
                    p["thoughtSignature"] = json!(sig);
                }
                self.emit(p);
            }
            PartKind::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let name = self.names.get(&tool_call_id).cloned().ok_or_else(|| {
                    ApiError::invalid_request(format!(
                        "Tool-Ergebnis {tool_call_id:?} ohne vorherigen Tool-Aufruf (Gemini braucht den Funktionsnamen)"
                    ))
                    .with_param("messages")
                })?;
                let response = function_response(content, is_error, self.losses);
                self.emit(json!({ "functionResponse": { "name": name, "response": response } }));
            }
            PartKind::Reasoning { text, signature } => {
                if text.is_empty() {
                    self.pending_signature = signature;
                } else {
                    let mut p = json!({ "text": text, "thought": true });
                    if let Some(sig) = signature {
                        p["thoughtSignature"] = json!(sig);
                    }
                    self.out.push(p);
                }
            }
            PartKind::RedactedReasoning { .. } => {
                self.losses
                    .note("Gemini: verschlüsseltes Reasoning verworfen");
            }
            PartKind::Opaque { format, value } => {
                if format == WireFormat::Gemini {
                    self.emit(value);
                } else {
                    self.losses
                        .note("Gemini: unbekannter Inhaltsteil verworfen");
                }
            }
        }
        Ok(())
    }
}

/// Reasoning-Aufwand → `thinkingConfig` (Gemini 2.x: Budget, ab Gemini 3: `thinkingLevel`).
fn thinking_config(
    reasoning: aigw_core::ir::Reasoning,
    model: &str,
    losses: &mut Losses,
) -> Option<Value> {
    let legacy = model.contains("gemini-2");
    let mut cfg = Map::new();
    if let Some(t) = reasoning.thinking {
        match t.mode.as_str() {
            "disabled" => {
                cfg.insert("thinkingBudget".into(), json!(0));
            }
            _ => {
                if let Some(b) = t.budget_tokens {
                    cfg.insert("thinkingBudget".into(), json!(b));
                }
                if t.display.as_deref() != Some("omitted") {
                    cfg.insert("includeThoughts".into(), json!(true));
                }
            }
        }
    }
    if let Some(effort) = reasoning.effort
        && !cfg.contains_key("thinkingBudget")
    {
        if legacy {
            let budget = match effort.as_str() {
                "none" | "minimal" => 0,
                "low" => 1024,
                "medium" => 8192,
                _ => 24_576,
            };
            cfg.insert("thinkingBudget".into(), json!(budget));
        } else {
            let level = match effort.as_str() {
                "none" | "minimal" => "minimal",
                "low" => "low",
                "medium" => "medium",
                "high" => "high",
                other => {
                    losses.note(format!("Gemini: reasoning effort {other} → high"));
                    "high"
                }
            };
            cfg.insert("thinkingLevel".into(), json!(level));
        }
    }
    (!cfg.is_empty()).then_some(Value::Object(cfg))
}

fn to_gemini(req: ChatRequest, model: &str, losses: &mut Losses) -> Result<Value, ApiError> {
    if !req.extra.is_empty() {
        losses.extra("Gemini: Anfrage", &req.extra);
    }
    // Tool-Call-ID → Funktionsname (functionResponse verlangt den Namen).
    let mut names = HashMap::new();
    for m in &req.messages {
        if let Content::Parts(parts) = &m.content {
            for p in parts {
                if let PartKind::ToolCall { id, name, .. } = &p.kind {
                    names.insert(id.clone(), name.clone());
                }
            }
        }
    }

    let mut system: Vec<Value> = Vec::new();
    let mut contents: Vec<(Role, Vec<Value>)> = Vec::new();
    let mut seen_conversation = false;
    for msg in req.messages {
        if !msg.extra.is_empty() {
            losses.extra("Gemini: Nachricht", &msg.extra);
        }
        if msg.name.is_some() {
            losses.note("Gemini: name von Nachrichten");
        }
        let role = match msg.role {
            Role::System | Role::Developer => {
                if seen_conversation {
                    losses.note("Gemini: spätere System-Nachricht in systemInstruction verschoben");
                }
                for p in msg.content.into_parts() {
                    match p.kind {
                        PartKind::Text { text } if !text.is_empty() => {
                            system.push(json!({ "text": text }));
                        }
                        PartKind::Text { .. } => {}
                        _ => losses.note("Gemini: Nicht-Text-Inhalt im System-Prompt"),
                    }
                }
                continue;
            }
            Role::User | Role::Tool => Role::User,
            Role::Assistant => Role::Assistant,
        };
        seen_conversation = true;
        let mut out = PartsOut {
            out: Vec::new(),
            losses: &mut *losses,
            names: &names,
            pending_signature: None,
        };
        for p in msg.content.into_parts() {
            out.push(p)?;
        }
        let parts = out.out;
        if parts.is_empty() {
            continue;
        }
        match contents.last_mut() {
            Some((r, existing)) if *r == role => existing.extend(parts),
            _ => contents.push((role, parts)),
        }
    }

    let mut body = Map::new();
    body.insert(
        "contents".into(),
        Value::Array(
            contents
                .into_iter()
                .map(|(r, parts)| {
                    json!({ "role": if r == Role::Assistant { "model" } else { "user" }, "parts": parts })
                })
                .collect(),
        ),
    );
    if !system.is_empty() {
        body.insert("systemInstruction".into(), json!({ "parts": system }));
    }

    let mut config = Map::new();
    let s = &req.sampling;
    if let Some(v) = s.temperature {
        config.insert("temperature".into(), json!(v));
    }
    if let Some(v) = s.top_p {
        config.insert("topP".into(), json!(v));
    }
    if let Some(v) = s.top_k {
        config.insert("topK".into(), json!(v));
    }
    if let Some(v) = s.presence_penalty {
        config.insert("presencePenalty".into(), json!(v));
    }
    if let Some(v) = s.frequency_penalty {
        config.insert("frequencyPenalty".into(), json!(v));
    }
    if let Some(v) = s.seed {
        config.insert("seed".into(), json!(v));
    }
    if let Some(v) = req.max_tokens {
        config.insert("maxOutputTokens".into(), json!(v));
    }
    if !req.stop.is_empty() {
        config.insert("stopSequences".into(), json!(req.stop));
    }
    match req.response_format {
        None | Some(ResponseFormat::Text) => {}
        Some(ResponseFormat::JsonObject) => {
            config.insert("responseMimeType".into(), json!("application/json"));
        }
        Some(ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        }) => {
            if name.is_some() || description.is_some() || strict.is_some() {
                losses.note("Gemini: Name/Beschreibung/strict des JSON-Schemas");
            }
            config.insert("responseMimeType".into(), json!("application/json"));
            config.insert("responseJsonSchema".into(), schema);
        }
    }
    if let Some(r) = req.reasoning
        && let Some(tc) = thinking_config(r, model, losses)
    {
        config.insert("thinkingConfig".into(), tc);
    }
    if !config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(config));
    }

    if req.user.is_some() || req.metadata.is_some() {
        losses.note("Gemini: metadata/user");
    }
    if req.parallel_tool_calls.is_some() {
        losses.note("Gemini: parallel_tool_calls");
    }

    let mut decls = Vec::new();
    let mut other_tools = Vec::new();
    for tool in req.tools {
        if tool.cache_control.is_some() {
            losses.note("Gemini: cache_control an Tools");
        }
        match tool.kind {
            ToolKind::Function(f) => {
                if f.strict.is_some() {
                    losses.note("Gemini: strict von Tools");
                }
                let mut d = Map::new();
                d.insert("name".into(), json!(f.name));
                if let Some(desc) = f.description {
                    d.insert("description".into(), json!(desc));
                }
                if let Some(p) = f.parameters {
                    d.insert("parametersJsonSchema".into(), p);
                }
                decls.push(Value::Object(d));
            }
            ToolKind::Opaque {
                format: WireFormat::Gemini,
                value,
            } => {
                other_tools.push(value);
            }
            ToolKind::Opaque { .. } => losses.note("Gemini: anbieterspezifisches Tool verworfen"),
        }
    }
    if !decls.is_empty() || !other_tools.is_empty() {
        let mut tools = Vec::new();
        if !decls.is_empty() {
            tools.push(json!({ "functionDeclarations": decls }));
        }
        tools.extend(other_tools);
        body.insert("tools".into(), Value::Array(tools));
        let fcc = match req.tool_choice {
            None => None,
            Some(ToolChoice::Auto) => Some(json!({ "mode": "AUTO" })),
            Some(ToolChoice::None) => Some(json!({ "mode": "NONE" })),
            Some(ToolChoice::Required) => Some(json!({ "mode": "ANY" })),
            Some(ToolChoice::Tool { name }) => {
                Some(json!({ "mode": "ANY", "allowedFunctionNames": [name] }))
            }
        };
        if let Some(f) = fcc {
            body.insert("toolConfig".into(), json!({ "functionCallingConfig": f }));
        }
    }
    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------------------------
// Gemini → IR
// ---------------------------------------------------------------------------------------------

/// Endgründe, die eine Inhaltsfilterung bedeuten.
pub(crate) fn is_blocked(reason: &str) -> bool {
    matches!(
        reason,
        "SAFETY"
            | "RECITATION"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "IMAGE_SAFETY"
            | "IMAGE_PROHIBITED_CONTENT"
            | "IMAGE_RECITATION"
            | "LANGUAGE"
    )
}

pub(crate) fn finish_reason(reason: &str, has_tool_calls: bool) -> StopReason {
    match reason {
        "STOP" if has_tool_calls => StopReason::ToolUse,
        "STOP" => StopReason::EndTurn,
        "MAX_TOKENS" => StopReason::MaxTokens,
        r if is_blocked(r) => StopReason::ContentFilter,
        other => StopReason::Other(other.to_ascii_lowercase()),
    }
}

/// Fehler „vom Inhaltsfilter blockiert“ (Klasse `content_filter`).
pub(crate) fn content_filter_error(reason: &str) -> ApiError {
    ApiError::invalid_request(format!(
        "Vom Inhaltsfilter des Anbieters blockiert ({reason})"
    ))
    .with_code("content_filter")
}

/// `promptFeedback.blockReason`, falls die Anfrage blockiert wurde.
pub(crate) fn prompt_block(v: &Value) -> Option<String> {
    v.pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
        .filter(|r| *r != "BLOCKED_REASON_UNSPECIFIED")
        .map(str::to_owned)
}

pub(crate) fn usage(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    let thoughts = n("thoughtsTokenCount");
    Usage {
        input_tokens: n("promptTokenCount")
            .unwrap_or(0)
            .saturating_add(n("toolUsePromptTokenCount").unwrap_or(0)),
        output_tokens: n("candidatesTokenCount")
            .unwrap_or(0)
            .saturating_add(thoughts.unwrap_or(0)),
        cache_read_tokens: n("cachedContentTokenCount"),
        cache_write_tokens: None,
        reasoning_tokens: thoughts,
        ..Usage::default()
    }
}

/// Ein Gemini-Teil → IR-Teil (`None` = ohne Inhalt).
fn part_to_ir(p: &Value) -> Option<Part> {
    let o = p.as_object()?;
    let sig = o.get("thoughtSignature").and_then(Value::as_str);
    if let Some(fc) = o.get("functionCall") {
        return Some(Part::new(PartKind::ToolCall {
            id: tool_call_id(fc.get("id").and_then(Value::as_str), sig),
            name: fc
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: fc
                .get("args")
                .map_or_else(|| "{}".to_owned(), Value::to_string),
        }));
    }
    if let Some(t) = o.get("text").and_then(Value::as_str) {
        if o.get("thought").and_then(Value::as_bool) == Some(true) {
            return Some(Part::new(PartKind::Reasoning {
                text: t.to_owned(),
                signature: sig.map(str::to_owned),
            }));
        }
        return Some(Part::text(t));
    }
    Some(Part::new(PartKind::Opaque {
        format: WireFormat::Gemini,
        value: p.clone(),
    }))
}

fn from_gemini(body: &[u8], model: &str) -> Result<ChatResponse, ApiError> {
    let v: Value = serde_json::from_slice(body)?;
    let candidates = v
        .get("candidates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if candidates.is_empty() {
        if let Some(r) = prompt_block(&v) {
            return Err(content_filter_error(&r));
        }
        return Err(ApiError::invalid_request("Gemini-Antwort ohne candidates"));
    }
    let mut choices = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        let parts: Vec<Part> = c
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .map(|ps| ps.iter().filter_map(part_to_ir).collect())
            .unwrap_or_default();
        let reason = c.get("finishReason").and_then(Value::as_str);
        if parts.is_empty()
            && let Some(r) = reason.filter(|r| is_blocked(r))
        {
            return Err(content_filter_error(r));
        }
        let has_tools = parts
            .iter()
            .any(|p| matches!(p.kind, PartKind::ToolCall { .. }));
        let only_text = parts.len() == 1 && matches!(parts[0].kind, PartKind::Text { .. });
        let content = if only_text {
            Content::Text(Content::Parts(parts).text())
        } else {
            Content::Parts(parts)
        };
        let mut extra = Map::new();
        for key in [
            "safetyRatings",
            "citationMetadata",
            "groundingMetadata",
            "avgLogprobs",
        ] {
            if let Some(x) = c.get(key) {
                extra.insert(key.to_owned(), x.clone());
            }
        }
        choices.push(Choice {
            index: c
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(u32::try_from(i).unwrap_or(0)),
            message: Message {
                role: Role::Assistant,
                content,
                name: None,
                extra: Map::new(),
            },
            stop_reason: reason.map(|r| finish_reason(r, has_tools)),
            stop_sequence: None,
            extra,
        });
    }
    Ok(ChatResponse {
        id: v
            .get("responseId")
            .and_then(Value::as_str)
            .map_or_else(|| random_id("gemini_"), str::to_owned),
        model: v
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or(model)
            .to_owned(),
        created: i64::try_from(unix_now()).ok(),
        choices,
        usage: v.get("usageMetadata").map(usage),
        extra: Map::new(),
        source: Some(WireFormat::Gemini),
    })
}

// ---------------------------------------------------------------------------------------------
// Fehlerklassifikation
// ---------------------------------------------------------------------------------------------

/// `google.rpc.RetryInfo.retryDelay` (`"37s"`, `"1.5s"`).
fn retry_delay(details: &[Value]) -> Option<std::time::Duration> {
    details.iter().find_map(|d| {
        let t = d.get("@type")?.as_str()?;
        if !t.ends_with("google.rpc.RetryInfo") {
            return None;
        }
        let s = d.get("retryDelay")?.as_str()?.strip_suffix('s')?;
        s.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(std::time::Duration::from_secs_f64)
    })
}

/// Tageskontingent erschöpft (`QuotaFailure` mit „PerDay“), nicht nur Rate-Limit.
fn daily_quota(details: &[Value]) -> bool {
    details.iter().any(|d| {
        d.get("@type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.ends_with("google.rpc.QuotaFailure"))
            && d.get("violations")
                .and_then(Value::as_array)
                .is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.get("quotaId")
                            .and_then(Value::as_str)
                            .is_some_and(|q| q.contains("PerDay"))
                    })
                })
    })
}

fn has_reason(details: &[Value], reason: &str) -> bool {
    details
        .iter()
        .any(|d| d.get("reason").and_then(Value::as_str) == Some(reason))
}

/// Klassifiziert eine Fehlerantwort der Gemini API bzw. von Vertex AI
/// (`{"error":{"code","message","status","details"}}`).
pub fn classify_gemini_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    let s = status.as_u16();
    let v: Option<Value> = serde_json::from_slice::<Value>(body)
        .ok()
        // Vertex liefert bei Streams teils eine Liste mit einem Fehlerobjekt.
        .map(|v| match v {
            Value::Array(mut a) if !a.is_empty() => a.swap_remove(0),
            other => other,
        });
    let err = v.as_ref().and_then(|v| v.get("error"));
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map_or_else(
            || String::from_utf8_lossy(body).trim().to_owned(),
            str::to_owned,
        );
    let rpc_status = err
        .and_then(|e| e.get("status"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let details: Vec<Value> = err
        .and_then(|e| e.get("details"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let class = match rpc_status.as_deref() {
        _ if has_reason(&details, "API_KEY_INVALID") => ErrorClass::Auth,
        Some("RESOURCE_EXHAUSTED") | None if s == 429 => {
            if daily_quota(&details) {
                ErrorClass::Quota
            } else {
                ErrorClass::RateLimit
            }
        }
        Some("UNAUTHENTICATED" | "PERMISSION_DENIED") => ErrorClass::Auth,
        Some("NOT_FOUND") => ErrorClass::NotFound,
        Some("UNAVAILABLE") => ErrorClass::Overloaded,
        Some("DEADLINE_EXCEEDED") => ErrorClass::Timeout,
        Some("INTERNAL") => ErrorClass::ServerError,
        _ if (400..500).contains(&s) && looks_like_context_length(&message) => {
            ErrorClass::ContextLength
        }
        _ if (400..500).contains(&s)
            && message.to_ascii_lowercase().contains("safety")
            && message.to_ascii_lowercase().contains("block") =>
        {
            ErrorClass::ContentFilter
        }
        _ => class_from_status(s),
    };
    let retry = retry_delay(&details).or_else(|| retry_after_header(headers));
    let mut e = upstream_error(class, s, message, rpc_status);
    if class == ErrorClass::RateLimit || class == ErrorClass::Quota {
        e.error.kind = ErrorKind::RateLimit;
    }
    e.with_retry_after(retry)
}

#[cfg(test)]
mod tests;
