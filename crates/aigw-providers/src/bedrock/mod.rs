// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Adapter für AWS Bedrock (Converse-API, AIGW-18) – ohne AWS-SDK.
//!
//! - **Endpunkte:** `POST {endpoint}/model/{modelId}/converse` bzw. `…/converse-stream`;
//!   Standard-Endpunkt `https://bedrock-runtime.{region}.amazonaws.com`. Die Modell-ID
//!   (auch Inference-Profile-ID oder ARN) wird als ein Pfadsegment kodiert.
//! - **Auth:** SigV4 ([`sigv4`], Dienst `bedrock`) mit Access Key/Secret/Session-Token aus der
//!   Credential-Ressource (`accessKeyId`, `secretAccessKey`, `sessionToken`) über den
//!   Erweiterungspunkt [`AwsCredentialsProvider`]; alternativ Bedrock-API-Key (`apiKey`,
//!   `Authorization: Bearer`).
//! - **Stream:** `application/vnd.amazon.eventstream` ([`eventstream`]) → IR-Ereignisse.
//!
//! Einstellungen (`spec.settings`): `region` (Pflicht), `additionalModelRequestFields`
//! (Objekt, wird übernommen), `guardrailConfig` (Objekt, wird übernommen), `timeouts`.

pub mod eventstream;
pub mod sigv4;
mod stream;

use std::sync::Arc;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorClass, MediaSource, Message, Part,
    PartKind, Role, StopReason, ToolChoice, ToolKind, Usage,
};
use aigw_core::resource::SecretString;
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::{Losses, WireFormat};
use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use serde_json::{Map, Value, json};

use crate::codec::{base64, random_id, unix_now, uri_encode};
use crate::errmap::{
    class_from_status, internal, looks_like_context_length, protocol_error, retry_after_header,
    upstream_error,
};
use crate::settings::Settings;
use crate::{
    Capabilities, Capability, ConfigError, MaxTokensField, Provider, StreamDecoder, UpstreamRequest,
};
use aigw_core::ir::UpstreamError;
use aigw_core::ir::embeddings::{Embedding, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse};

pub use sigv4::{AwsCredentials, AwsCredentialsProvider, StaticCredentials};
pub use stream::BedrockStreamDecoder;

/// SigV4-Dienstname von Bedrock Runtime.
pub const SIGNING_SERVICE: &str = "bedrock";

const USER_AGENT_VALUE: &str = concat!("ai-gw/", env!("CARGO_PKG_VERSION"));

/// Authentifizierung gegenüber Bedrock.
#[derive(Debug, Clone)]
enum Auth {
    /// SigV4 mit (ggf. temporären) AWS-Zugangsdaten.
    SigV4(Arc<dyn AwsCredentialsProvider>),
    /// Bedrock-API-Key (`Authorization: Bearer`).
    Bearer(HeaderValue),
}

/// Adapter für AWS Bedrock (Converse/ConverseStream).
#[derive(Debug, Clone)]
pub struct BedrockAdapter {
    provider_model: String,
    region: String,
    uri: Uri,
    stream_uri: Uri,
    auth: Auth,
    additional_fields: Option<Map<String, Value>>,
    guardrail: Option<Map<String, Value>>,
    caps: Capabilities,
    /// Embedding-Modell (AIGW-21): Familie, `InvokeModel`-URL und `input_type` (Cohere).
    embedding: Option<(EmbeddingFamily, Uri)>,
    input_type: Option<String>,
}

/// Familie eines Embedding-Modells auf Bedrock (bestimmt den `InvokeModel`-Body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingFamily {
    /// Amazon Titan Text Embeddings (eine Eingabe je Aufruf).
    Titan,
    /// Cohere Embed (bis zu 96 Eingaben je Aufruf).
    Cohere,
}

/// Höchstens so viele Texte je Cohere-Aufruf.
const COHERE_BATCH: usize = 96;
/// Höchstens so viele Eingaben je Anfrage an Titan (je Eingabe ein Aufruf).
const TITAN_MAX_INPUTS: usize = 256;

/// Embedding-Familie aus `settings.embeddingFormat` bzw. der Modell-ID.
fn embedding_family(model: &str, s: &Settings<'_>) -> Result<Option<EmbeddingFamily>, ConfigError> {
    match s.str("embeddingFormat")? {
        Some("titan") => return Ok(Some(EmbeddingFamily::Titan)),
        Some("cohere") => return Ok(Some(EmbeddingFamily::Cohere)),
        Some(other) => {
            return Err(s.invalid(format!(
                "settings.embeddingFormat = {other:?} unbekannt (titan, cohere)"
            )));
        }
        None => {}
    }
    Ok(
        if model.contains("titan-embed-text") || model.contains("titan-embed-g1-text") {
            Some(EmbeddingFamily::Titan)
        } else if model.contains("cohere.embed") {
            Some(EmbeddingFamily::Cohere)
        } else {
            None
        },
    )
}

fn parse_uri(s: &Settings<'_>, url: &str) -> Result<Uri, ConfigError> {
    let uri: Uri = url
        .parse()
        .map_err(|e| s.invalid(format!("ungültige URL {url:?}: {e}")))?;
    match (uri.scheme_str(), uri.authority()) {
        (Some("http" | "https"), Some(_)) => Ok(uri),
        _ => Err(s.invalid(format!("URL {url:?} braucht http(s)://host"))),
    }
}

fn valid_region(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 32
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn cred<'a>(dep: &'a DeploymentCtx, names: &[&str]) -> Option<&'a SecretString> {
    let c = dep.credential.as_ref()?;
    names.iter().find_map(|n| c.get(n))
}

impl BedrockAdapter {
    /// Fabrik für die [`Registry`](crate::Registry).
    pub fn factory(dep: &DeploymentCtx) -> Result<Arc<dyn Provider>, ConfigError> {
        Ok(Arc::new(Self::from_deployment(dep)?))
    }

    /// Baut den Adapter; Zugangsdaten aus der Credential-Ressource.
    pub fn from_deployment(dep: &DeploymentCtx) -> Result<Self, ConfigError> {
        let s = Settings::of(dep);
        let auth = if let Some(id) = cred(dep, &["accessKeyId", "access_key_id"]) {
            let secret = cred(dep, &["secretAccessKey", "secret_access_key"])
                .ok_or_else(|| s.missing("Zugangsdaten (secretAccessKey)"))?;
            let creds = AwsCredentials {
                access_key_id: id.expose().to_owned(),
                secret_access_key: secret.clone(),
                session_token: cred(dep, &["sessionToken", "session_token"]).cloned(),
            };
            Auth::SigV4(Arc::new(StaticCredentials(Arc::new(creds))))
        } else if let Some(key) = cred(dep, &["apiKey", "api_key"]) {
            let mut v = HeaderValue::from_str(&format!("Bearer {}", key.expose()))
                .map_err(|_| s.invalid("Zugangsdaten enthalten unzulässige Zeichen"))?;
            v.set_sensitive(true);
            Auth::Bearer(v)
        } else {
            return Err(s.missing("Zugangsdaten (accessKeyId/secretAccessKey oder apiKey)"));
        };
        Self::with_auth(dep, auth)
    }

    /// Baut den Adapter mit einer eigenen Quelle für AWS-Zugangsdaten (z. B. IMDS,
    /// Web-Identity – Erweiterungspunkt).
    pub fn with_credentials_provider(
        dep: &DeploymentCtx,
        provider: Arc<dyn AwsCredentialsProvider>,
    ) -> Result<Self, ConfigError> {
        Self::with_auth(dep, Auth::SigV4(provider))
    }

    fn with_auth(dep: &DeploymentCtx, auth: Auth) -> Result<Self, ConfigError> {
        let s = Settings::of(dep);
        let spec = &dep.deployment.spec;
        let region = s
            .str("region")?
            .ok_or_else(|| s.missing("settings.region"))?;
        if !valid_region(region) {
            return Err(s.invalid(format!("settings.region {region:?} ungültig")));
        }
        if spec.provider_model.is_empty() {
            return Err(s.missing("providerModel (Bedrock-Modell-ID)"));
        }
        let base = spec
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://bedrock-runtime.{region}.amazonaws.com"));
        let base = base.trim_end_matches('/');
        let model = uri_encode(&spec.provider_model, false);
        let uri = parse_uri(&s, &format!("{base}/model/{model}/converse"))?;
        let stream_uri = parse_uri(&s, &format!("{base}/model/{model}/converse-stream"))?;
        let embedding_family = embedding_family(&spec.provider_model, &s)?;
        let embedding = match embedding_family {
            Some(f) => Some((f, parse_uri(&s, &format!("{base}/model/{model}/invoke"))?)),
            None => None,
        };
        let input_type = s.str("inputType")?.map(str::to_owned);
        Ok(Self {
            embedding,
            input_type,
            provider_model: spec.provider_model.clone(),
            region: region.to_owned(),
            uri,
            stream_uri,
            auth,
            additional_fields: s.object("additionalModelRequestFields")?.cloned(),
            guardrail: s.object("guardrailConfig")?.cloned(),
            caps: Capabilities {
                chat: true,
                streaming: true,
                tools: true,
                vision: true,
                json_schema: false,
                reasoning: true,
                embeddings: embedding_family.is_some(),
                images: false,
                audio_transcription: false,
                audio_speech: false,
                stream_usage: true,
                max_tokens_field: MaxTokensField::MaxTokens,
                n_choices: false,
            },
        })
    }

    /// Ziel-URL (nicht gestreamt).
    pub fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Signierte (bzw. per API-Key authentifizierte) POST-Anfrage.
    fn signed(
        &self,
        uri: &Uri,
        accept: &'static str,
        body: Vec<u8>,
    ) -> Result<UpstreamRequest, ApiError> {
        let mut http_req = http::Request::builder()
            .method(Method::POST)
            .uri(uri.clone())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(ACCEPT, HeaderValue::from_static(accept))
            .header(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE))
            .body(Bytes::from(body))
            .map_err(internal)?;
        match &self.auth {
            Auth::Bearer(v) => {
                http_req.headers_mut().insert(AUTHORIZATION, v.clone());
            }
            Auth::SigV4(p) => {
                let creds = p.credentials()?;
                sigv4::sign_request(
                    &mut http_req,
                    &creds,
                    &self.region,
                    SIGNING_SERVICE,
                    unix_now(),
                )
                .map_err(internal)?;
            }
        }
        Ok(http_req)
    }

    /// Ziel-URL (gestreamt).
    pub fn stream_uri(&self) -> &Uri {
        &self.stream_uri
    }

    fn is_anthropic(&self) -> bool {
        self.provider_model.contains("anthropic.")
    }

    /// IR → Converse-Body.
    pub fn converse_body(&self, req: ChatRequest, losses: &mut Losses) -> Result<Value, ApiError> {
        to_converse(req, self.is_anthropic(), losses).map(|mut body| {
            if let Some(extra) = &self.additional_fields {
                let target = body
                    .as_object_mut()
                    .map(|o| {
                        o.entry("additionalModelRequestFields")
                            .or_insert_with(|| Value::Object(Map::new()))
                    })
                    .and_then(Value::as_object_mut);
                if let Some(t) = target {
                    for (k, v) in extra {
                        t.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
            }
            if let (Some(g), Some(o)) = (&self.guardrail, body.as_object_mut()) {
                o.insert("guardrailConfig".into(), Value::Object(g.clone()));
            }
            body
        })
    }
}

impl Provider for BedrockAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Bedrock
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    fn native_format(&self) -> WireFormat {
        WireFormat::BedrockConverse
    }

    fn provider_model(&self) -> &str {
        &self.provider_model
    }

    fn build_request(
        &self,
        req: ChatRequest,
        losses: &mut Losses,
    ) -> Result<UpstreamRequest, ApiError> {
        let stream = req.is_stream();
        let body = self.converse_body(req, losses)?;
        let body = serde_json::to_vec(&body).map_err(internal)?;
        let uri = if stream { &self.stream_uri } else { &self.uri };
        self.signed(
            uri,
            if stream {
                "application/vnd.amazon.eventstream"
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
        let Some((family, uri)) = &self.embedding else {
            return Err(crate::unsupported(
                ProviderKind::Bedrock,
                Capability::Embeddings,
            ));
        };
        let EmbeddingInput::Text(texts) = req.input else {
            return Err(ApiError::invalid_request(
                "Bedrock-Embeddings brauchen Text-Eingaben (keine Token-IDs)",
            )
            .with_param("input"));
        };
        if req.user.is_some() {
            losses.note("bedrock embeddings: user verworfen");
        }
        let mut extra = req.extra;
        // `input_type` (Cohere) wird auch aus Anfragen anderer Formate übernommen.
        let input_type = match extra.remove("input_type") {
            Some(Value::String(t)) => Some(t),
            _ => self.input_type.clone(),
        };
        losses.extra("bedrock embeddings", &extra);
        let mut out = Vec::new();
        match family {
            EmbeddingFamily::Titan => {
                if texts.len() > TITAN_MAX_INPUTS {
                    return Err(ApiError::invalid_request(format!(
                        "Titan-Embeddings: höchstens {TITAN_MAX_INPUTS} Eingaben je Anfrage"
                    ))
                    .with_param("input"));
                }
                for t in texts {
                    let mut body = json!({ "inputText": t });
                    if let (Some(d), Some(o)) = (req.dimensions, body.as_object_mut()) {
                        o.insert("dimensions".into(), Value::from(d));
                    }
                    let body = serde_json::to_vec(&body).map_err(internal)?;
                    out.push(self.signed(uri, "application/json", body)?);
                }
            }
            EmbeddingFamily::Cohere => {
                for chunk in texts.chunks(COHERE_BATCH) {
                    let mut body = json!({
                        "texts": chunk,
                        "input_type": input_type.as_deref().unwrap_or("search_document"),
                        "embedding_types": ["float"],
                    });
                    if let (Some(d), Some(o)) = (req.dimensions, body.as_object_mut()) {
                        o.insert("output_dimension".into(), Value::from(d));
                    }
                    let body = serde_json::to_vec(&body).map_err(internal)?;
                    out.push(self.signed(uri, "application/json", body)?);
                }
            }
        }
        Ok(out)
    }

    fn parse_embeddings(
        &self,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<EmbeddingsResponse, ApiError> {
        let v: Value = serde_json::from_slice(body).map_err(protocol_error)?;
        let floats = |v: &Value| -> Option<Vec<f32>> {
            v.as_array()?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect()
        };
        let header_tokens = headers
            .get("x-amzn-bedrock-input-token-count")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.trim().parse::<u64>().ok());
        let (vectors, tokens) = match self.embedding.as_ref().map(|(f, _)| *f) {
            Some(EmbeddingFamily::Titan) => {
                let e = v
                    .get("embedding")
                    .or_else(|| v.pointer("/embeddingsByType/float"))
                    .and_then(floats)
                    .ok_or_else(|| protocol_error("Titan-Antwort ohne embedding"))?;
                let tokens = v
                    .get("inputTextTokenCount")
                    .and_then(Value::as_u64)
                    .or(header_tokens);
                (vec![e], tokens)
            }
            Some(EmbeddingFamily::Cohere) => {
                let list = match v.get("embeddings") {
                    Some(Value::Object(o)) => o.get("float"),
                    other => other,
                }
                .and_then(Value::as_array)
                .ok_or_else(|| protocol_error("Cohere-Antwort ohne embeddings"))?;
                let vectors = list
                    .iter()
                    .map(floats)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| protocol_error("Cohere-Vektor ungültig"))?;
                (vectors, header_tokens)
            }
            None => {
                return Err(crate::unsupported(
                    ProviderKind::Bedrock,
                    Capability::Embeddings,
                ));
            }
        };
        let data = vectors
            .into_iter()
            .zip(0u32..)
            .map(|(vector, index)| Embedding { index, vector })
            .collect();
        Ok(EmbeddingsResponse {
            model: self.provider_model.clone(),
            data,
            usage_estimated: tokens.is_none(),
            usage: Usage {
                input_tokens: tokens.unwrap_or(0),
                ..Usage::default()
            },
            extra: Map::new(),
        })
    }

    fn parse_response(&self, body: &[u8]) -> Result<ChatResponse, ApiError> {
        from_converse(body, &self.provider_model)
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(BedrockStreamDecoder::new(self.provider_model.clone()))
    }

    fn classify_error(
        &self,
        status: StatusCode,
        headers: &HeaderMap,
        body: &[u8],
    ) -> UpstreamError {
        classify_bedrock_error(status, headers, body)
    }
}

// ---------------------------------------------------------------------------------------------
// IR → Converse
// ---------------------------------------------------------------------------------------------

/// MIME-Typ → Converse-Bildformat.
fn image_format(media_type: Option<&str>) -> Option<&'static str> {
    match media_type? {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpeg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// MIME-Typ → Converse-Dokumentformat.
fn document_format(media_type: &str) -> Option<&'static str> {
    Some(match media_type {
        "application/pdf" => "pdf",
        "text/csv" => "csv",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "text/html" => "html",
        "text/plain" => "txt",
        "text/markdown" => "md",
        _ => return None,
    })
}

/// Dokumentnamen erlaubt Bedrock nur aus Buchstaben, Ziffern, Leerzeichen, `-()[]`.
fn document_name(name: Option<&str>, n: usize) -> String {
    let cleaned: String = name
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -()[]".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        format!("document {n}")
    } else {
        cleaned
    }
}

fn media_block(source: MediaSource, what: &str) -> Result<(Option<String>, Value), ApiError> {
    match source {
        MediaSource::Base64 { media_type, data } => Ok((media_type, json!({ "bytes": data }))),
        MediaSource::Text { media_type, data } => Ok((
            Some(media_type),
            json!({ "bytes": base64(data.as_bytes()) }),
        )),
        MediaSource::Url { url } if url.starts_with("s3://") => {
            Ok((None, json!({ "s3Location": { "uri": url } })))
        }
        MediaSource::Url { .. } => Err(ApiError::invalid_request(format!(
            "Bedrock unterstützt {what} nur als Base64-Daten oder s3://-URI, nicht als URL"
        ))),
        MediaSource::FileId { .. } | MediaSource::Other { .. } => Err(ApiError::invalid_request(
            format!("Bedrock unterstützt diese {what}-Quelle nicht"),
        )),
    }
}

/// Tool-Argumente (JSON-Text) → Objekt.
fn tool_input(arguments: &str) -> Result<Value, ApiError> {
    if arguments.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(arguments).map_err(|e| {
        ApiError::invalid_request(format!("Tool-Argumente sind kein gültiges JSON: {e}"))
            .with_param("messages")
    })
}

struct Blocks<'a> {
    out: Vec<Value>,
    losses: &'a mut Losses,
    documents: &'a mut usize,
}

impl Blocks<'_> {
    fn push(&mut self, part: Part) -> Result<(), ApiError> {
        let cache = part.cache_control.is_some();
        if !part.extra.is_empty() {
            self.losses.extra("Bedrock: Inhaltsteil", &part.extra);
        }
        match part.kind {
            PartKind::Text { text } | PartKind::Refusal { text } => {
                if !text.is_empty() {
                    self.out.push(json!({ "text": text }));
                }
            }
            PartKind::Image { source, detail } => {
                if detail.is_some() {
                    self.losses.note("Bedrock: Bild-detail");
                }
                let (media, src) = media_block(source, "Bilder")?;
                let format = image_format(media.as_deref()).or_else(|| {
                    // s3-Location ohne Typ: Format aus der Endung.
                    let uri = src.pointer("/s3Location/uri")?.as_str()?;
                    let ext = uri.rsplit('.').next()?.to_ascii_lowercase();
                    image_format(Some(&format!("image/{ext}")))
                });
                let format = format.ok_or_else(|| {
                    ApiError::invalid_request(
                        "Bedrock: Bildformat nicht unterstützt (png, jpeg, gif, webp)",
                    )
                })?;
                self.out
                    .push(json!({ "image": { "format": format, "source": src } }));
            }
            PartKind::File { source, filename } => {
                let (media, src) = media_block(source, "Dokumente")?;
                let format = media
                    .as_deref()
                    .and_then(document_format)
                    .or_else(|| {
                        let ext = filename.as_deref()?.rsplit('.').next()?;
                        [
                            "pdf", "csv", "doc", "docx", "xls", "xlsx", "html", "txt", "md",
                        ]
                        .into_iter()
                        .find(|f| f.eq_ignore_ascii_case(ext))
                    })
                    .ok_or_else(|| {
                        ApiError::invalid_request("Bedrock: Dokumentformat nicht unterstützt")
                    })?;
                *self.documents += 1;
                let name = document_name(filename.as_deref(), *self.documents);
                self.out.push(json!({
                    "document": { "format": format, "name": name, "source": src }
                }));
            }
            PartKind::Audio { .. } => self.losses.note("Bedrock: Audio-Teil verworfen"),
            PartKind::ToolCall {
                id,
                name,
                arguments,
            } => {
                self.out.push(json!({
                    "toolUse": { "toolUseId": id, "name": name, "input": tool_input(&arguments)? }
                }));
            }
            PartKind::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => {
                let mut inner = Blocks {
                    out: Vec::new(),
                    losses: &mut *self.losses,
                    documents: &mut *self.documents,
                };
                for p in content.into_parts() {
                    match p.kind {
                        PartKind::Text { .. } | PartKind::Image { .. } | PartKind::File { .. } => {
                            inner.push(p)?
                        }
                        _ => inner
                            .losses
                            .note("Bedrock: Nicht-Text-Inhalt im Tool-Ergebnis verworfen"),
                    }
                }
                let mut content = inner.out;
                if content.is_empty() {
                    content.push(json!({ "text": "" }));
                }
                let mut tr = json!({ "toolUseId": tool_call_id, "content": content });
                if is_error == Some(true) {
                    tr["status"] = json!("error");
                }
                self.out.push(json!({ "toolResult": tr }));
            }
            PartKind::Reasoning { text, signature } => {
                let mut rt = json!({ "text": text });
                if let Some(sig) = signature {
                    rt["signature"] = json!(sig);
                }
                self.out
                    .push(json!({ "reasoningContent": { "reasoningText": rt } }));
            }
            PartKind::RedactedReasoning { data } => {
                self.out
                    .push(json!({ "reasoningContent": { "redactedContent": data } }));
            }
            PartKind::Opaque { format, value } => {
                if format == WireFormat::BedrockConverse {
                    self.out.push(value);
                } else {
                    self.losses
                        .note("Bedrock: unbekannter Inhaltsteil verworfen");
                }
            }
        }
        if cache {
            // Anthropic `cache_control` → Converse-Cache-Punkt nach dem Block.
            self.out
                .push(json!({ "cachePoint": { "type": "default" } }));
        }
        Ok(())
    }
}

/// Übersetzt eine IR-Anfrage in den Converse-Body (ohne Modell-ID, die steht im Pfad).
fn to_converse(
    req: ChatRequest,
    anthropic_model: bool,
    losses: &mut Losses,
) -> Result<Value, ApiError> {
    let mut system: Vec<Value> = Vec::new();
    let mut messages: Vec<(Role, Vec<Value>)> = Vec::new();
    let mut documents = 0usize;
    let mut seen_conversation = false;
    let mut uses_tools = false;

    if !req.extra.is_empty() {
        losses.extra("Bedrock: Anfrage", &req.extra);
    }
    for msg in req.messages {
        if !msg.extra.is_empty() {
            losses.extra("Bedrock: Nachricht", &msg.extra);
        }
        if msg.name.is_some() {
            losses.note("Bedrock: name von Nachrichten");
        }
        let role = match msg.role {
            Role::System | Role::Developer => {
                if seen_conversation {
                    losses.note("Bedrock: spätere System-Nachricht in system verschoben");
                }
                let mut b = Blocks {
                    out: Vec::new(),
                    losses: &mut *losses,
                    documents: &mut documents,
                };
                for p in msg.content.into_parts() {
                    match p.kind {
                        PartKind::Text { .. } => b.push(p)?,
                        _ => b.losses.note("Bedrock: Nicht-Text-Inhalt im System-Prompt"),
                    }
                }
                system.extend(b.out);
                continue;
            }
            Role::User | Role::Tool => Role::User,
            Role::Assistant => Role::Assistant,
        };
        seen_conversation = true;
        let mut b = Blocks {
            out: Vec::new(),
            losses: &mut *losses,
            documents: &mut documents,
        };
        for p in msg.content.into_parts() {
            uses_tools |= matches!(
                p.kind,
                PartKind::ToolCall { .. } | PartKind::ToolResult { .. }
            );
            b.push(p)?;
        }
        let blocks = b.out;
        if blocks.is_empty() {
            continue;
        }
        // Converse verlangt abwechselnde Rollen: gleiche Rollen zusammenfassen (z. B. mehrere
        // OpenAI-`tool`-Nachrichten + Nutzertext).
        match messages.last_mut() {
            Some((r, existing)) if *r == role => existing.extend(blocks),
            _ => messages.push((role, blocks)),
        }
    }

    let mut body = Map::new();
    body.insert(
        "messages".into(),
        Value::Array(
            messages
                .into_iter()
                .map(|(r, content)| {
                    json!({
                        "role": if r == Role::Assistant { "assistant" } else { "user" },
                        "content": content
                    })
                })
                .collect(),
        ),
    );
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }

    let mut inference = Map::new();
    if let Some(m) = req.max_tokens {
        inference.insert("maxTokens".into(), json!(m));
    }
    if let Some(t) = req.sampling.temperature {
        inference.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.sampling.top_p {
        inference.insert("topP".into(), json!(p));
    }
    if !req.stop.is_empty() {
        inference.insert("stopSequences".into(), json!(req.stop));
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Value::Object(inference));
    }

    let mut additional = Map::new();
    if let Some(k) = req.sampling.top_k {
        if anthropic_model {
            additional.insert("top_k".into(), json!(k));
        } else {
            losses.note("Bedrock: top_k (nur für Anthropic-Modelle abgebildet)");
        }
    }
    for (name, set) in [
        (
            "frequency_penalty",
            req.sampling.frequency_penalty.is_some(),
        ),
        ("presence_penalty", req.sampling.presence_penalty.is_some()),
        ("seed", req.sampling.seed.is_some()),
    ] {
        if set {
            losses.note(format!("Bedrock: {name}"));
        }
    }
    if let Some(r) = req.reasoning {
        let had_effort = r.effort.is_some();
        if let Some(t) = r.thinking.as_ref().filter(|_| anthropic_model) {
            let mut th = Map::new();
            th.insert("type".into(), json!(t.mode));
            if let Some(b) = t.budget_tokens {
                th.insert("budget_tokens".into(), json!(b));
            }
            if let Some(d) = &t.display {
                th.insert("display".into(), json!(d));
            }
            additional.insert("thinking".into(), Value::Object(th));
            if had_effort {
                losses.note("Bedrock: reasoning effort (nur Anthropic-thinking abgebildet)");
            }
        } else if r.thinking.is_some() || had_effort {
            losses.note("Bedrock: Reasoning-Einstellungen (nur Anthropic-thinking abgebildet)");
        }
    }
    if !additional.is_empty() {
        body.insert(
            "additionalModelRequestFields".into(),
            Value::Object(additional),
        );
    }

    if req.response_format.is_some() {
        losses.note("Bedrock: response_format (Converse kennt keinen JSON-Modus)");
    }
    if req.parallel_tool_calls == Some(false) {
        losses.note("Bedrock: parallel_tool_calls");
    }
    if req.metadata.is_some() || req.user.is_some() {
        losses.note("Bedrock: metadata/user");
    }

    // Tools
    let mut tools: Vec<Value> = Vec::new();
    for tool in req.tools {
        let cache = tool.cache_control.is_some();
        match tool.kind {
            ToolKind::Function(f) => {
                if f.strict.is_some() {
                    losses.note("Bedrock: strict von Tools");
                }
                let mut spec = Map::new();
                spec.insert("name".into(), json!(f.name));
                if let Some(d) = f.description {
                    spec.insert("description".into(), json!(d));
                }
                let schema = f
                    .parameters
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                spec.insert("inputSchema".into(), json!({ "json": schema }));
                tools.push(json!({ "toolSpec": spec }));
            }
            ToolKind::Opaque { .. } => losses.note("Bedrock: anbieterspezifisches Tool verworfen"),
        }
        if cache {
            tools.push(json!({ "cachePoint": { "type": "default" } }));
        }
    }
    let choice = match req.tool_choice {
        None | Some(ToolChoice::Auto) => None,
        Some(ToolChoice::Required) => Some(json!({ "any": {} })),
        Some(ToolChoice::Tool { name }) => Some(json!({ "tool": { "name": name } })),
        Some(ToolChoice::None) => {
            if uses_tools {
                // Converse kennt kein „none“, verlangt aber toolConfig, sobald der Verlauf
                // Tool-Blöcke enthält.
                losses.note("Bedrock: tool_choice none");
            } else {
                tools.clear();
            }
            None
        }
    };
    if !tools.is_empty() {
        let mut tc = Map::new();
        tc.insert("tools".into(), Value::Array(tools));
        if let Some(c) = choice {
            tc.insert("toolChoice".into(), c);
        }
        body.insert("toolConfig".into(), Value::Object(tc));
    }
    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------------------------
// Converse → IR
// ---------------------------------------------------------------------------------------------

pub(crate) fn stop_reason(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "guardrail_intervened" | "content_filtered" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    }
}

/// Converse-`usage` → IR (Eingabe inkl. Cache-Lesen/-Schreiben).
pub(crate) fn usage(u: &Value) -> Usage {
    let n = |k: &str| u.get(k).and_then(Value::as_u64);
    let cache_read = n("cacheReadInputTokens");
    let cache_write = n("cacheWriteInputTokens");
    Usage {
        input_tokens: n("inputTokens")
            .unwrap_or(0)
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0)),
        output_tokens: n("outputTokens").unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        ..Usage::default()
    }
}

fn block_to_part(b: Value) -> Part {
    let Value::Object(mut o) = b else {
        return Part::new(PartKind::Opaque {
            format: WireFormat::BedrockConverse,
            value: b,
        });
    };
    if let Some(Value::String(t)) = o.remove("text") {
        return Part::text(t);
    }
    if let Some(tu) = o.get("toolUse") {
        let id = tu.get("toolUseId").and_then(Value::as_str);
        let name = tu.get("name").and_then(Value::as_str);
        if let (Some(id), Some(name)) = (id, name) {
            let args = tu
                .get("input")
                .map_or_else(|| "{}".to_owned(), Value::to_string);
            return Part::new(PartKind::ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: args,
            });
        }
    }
    if let Some(rc) = o.get("reasoningContent") {
        if let Some(rt) = rc.get("reasoningText") {
            return Part::new(PartKind::Reasoning {
                text: rt
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                signature: rt
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
        }
        if let Some(Value::String(d)) = rc.get("redactedContent") {
            return Part::new(PartKind::RedactedReasoning { data: d.clone() });
        }
    }
    Part::new(PartKind::Opaque {
        format: WireFormat::BedrockConverse,
        value: Value::Object(o),
    })
}

/// Parst eine Converse-Antwort.
fn from_converse(body: &[u8], model: &str) -> Result<ChatResponse, ApiError> {
    let v: Value = serde_json::from_slice(body)?;
    let content = v
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::invalid_request("Converse-Antwort ohne output.message"))?;
    let mut parts: Vec<Part> = content.iter().cloned().map(block_to_part).collect();
    let single_text = parts.len() == 1 && matches!(parts[0].kind, PartKind::Text { .. });
    let content = match parts.pop() {
        Some(Part {
            kind: PartKind::Text { text },
            ..
        }) if single_text => Content::Text(text),
        Some(p) => {
            parts.push(p);
            Content::Parts(parts)
        }
        None => Content::Parts(parts),
    };
    let mut extra = Map::new();
    for key in [
        "metrics",
        "additionalModelResponseFields",
        "trace",
        "performanceConfig",
    ] {
        if let Some(x) = v.get(key) {
            extra.insert(key.to_owned(), x.clone());
        }
    }
    Ok(ChatResponse {
        id: random_id("msg_bdrk_"),
        model: model.to_owned(),
        created: i64::try_from(unix_now()).ok(),
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content,
                name: None,
                extra: Map::new(),
            },
            stop_reason: v.get("stopReason").and_then(Value::as_str).map(stop_reason),
            stop_sequence: None,
            extra: Map::new(),
        }],
        usage: v.get("usage").map(usage),
        extra,
        source: Some(WireFormat::BedrockConverse),
    })
}

// ---------------------------------------------------------------------------------------------
// Fehlerklassifikation
// ---------------------------------------------------------------------------------------------

/// Kurzname des AWS-Fehlertyps (`x-amzn-ErrorType` bzw. `__type`, ohne Namensraum/URL).
fn error_type(headers: &HeaderMap, v: Option<&Value>) -> Option<String> {
    let raw = headers
        .get("x-amzn-errortype")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            v.and_then(|v| v.get("__type").or_else(|| v.get("code")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })?;
    let s = raw.split(':').next().unwrap_or(&raw);
    let s = s.rsplit('#').next().unwrap_or(s);
    Some(s.trim().to_owned())
}

/// Klasse eines Bedrock-Fehlertyps (HTTP und Stream-Ausnahmen; Groß-/Kleinschreibung egal).
pub(crate) fn class_of(error_type: &str, message: &str, status: u16) -> ErrorClass {
    let t = error_type.to_ascii_lowercase();
    let m = message.to_ascii_lowercase();
    match t.as_str() {
        "throttlingexception" if m.contains("per day") || m.contains("daily") => ErrorClass::Quota,
        "throttlingexception" => ErrorClass::RateLimit,
        "servicequotaexceededexception" => ErrorClass::Quota,
        "modelnotreadyexception" | "serviceunavailableexception" => ErrorClass::Overloaded,
        "internalserverexception" | "modelerrorexception" | "modelstreamerrorexception" => {
            ErrorClass::ServerError
        }
        "modeltimeoutexception" => ErrorClass::Timeout,
        "validationexception" if looks_like_context_length(message) => ErrorClass::ContextLength,
        "validationexception" => ErrorClass::InvalidRequest,
        "accessdeniedexception"
        | "unrecognizedclientexception"
        | "invalidsignatureexception"
        | "expiredtokenexception"
        | "incompletesignatureexception"
        | "missingauthenticationtokenexception" => ErrorClass::Auth,
        "resourcenotfoundexception" => ErrorClass::NotFound,
        _ if (400..500).contains(&status) && looks_like_context_length(message) => {
            ErrorClass::ContextLength
        }
        _ => class_from_status(status),
    }
}

/// Klassifiziert eine Bedrock-Fehlerantwort.
pub fn classify_bedrock_error(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> UpstreamError {
    let s = status.as_u16();
    let v: Option<Value> = serde_json::from_slice(body).ok();
    let message = v
        .as_ref()
        .and_then(|v| v.get("message").or_else(|| v.get("Message")))
        .and_then(Value::as_str)
        .map_or_else(
            || String::from_utf8_lossy(body).trim().to_owned(),
            str::to_owned,
        );
    let ty = error_type(headers, v.as_ref());
    let class = class_of(ty.as_deref().unwrap_or(""), &message, s);
    upstream_error(class, s, message, ty).with_retry_after(retry_after_header(headers))
}

#[cfg(test)]
mod tests;
