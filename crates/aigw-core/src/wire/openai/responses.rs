// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! OpenAI Responses API (`POST /v1/responses`, AIGW-21) – abgebildet auf die Chat-IR.
//!
//! Das Gateway ist **zustandslos**: Es speichert keine Antworten. `previous_response_id`,
//! `conversation`, `background: true` und `store: true` werden mit `400` und einer klaren
//! Meldung abgelehnt; die Antwort trägt immer `store: false`. Fehlt `store`, gilt das als
//! `false` (OpenAI: Standard `true`).
//!
//! | Responses | IR |
//! |---|---|
//! | `instructions` | erste Nachricht `System` |
//! | `input` (String) | eine Nutzernachricht |
//! | `input[]` `message` (`input_text`, `input_image`, `input_file`, `input_audio`, `output_text`, `refusal`) | Nachricht mit Teilen |
//! | `input[]` `function_call` | `ToolCall` (an die vorige Assistant-Nachricht angehängt) |
//! | `input[]` `function_call_output` | Nachricht `Tool` mit `ToolResult` |
//! | `input[]` `reasoning` (`summary`, `encrypted_content`) | `Reasoning{text, signature}` |
//! | `tools[]` `{type: function, name, …}` | `Tool::Function`; andere Typen `Opaque` |
//! | `tool_choice`, `parallel_tool_calls`, `temperature`, `top_p`, `max_output_tokens` | entsprechende IR-Felder |
//! | `text.format` | `response_format` |
//! | `reasoning.effort` | `reasoning.effort` |
//! | `metadata`, `user` | `metadata`, `user` |
//!
//! Antwort: Text/Ablehnung → `message`-Item (`output_text`/`refusal`), Tool-Aufrufe →
//! `function_call`-Items, Reasoning → `reasoning`-Item (`summary_text`; Signatur als
//! `encrypted_content`, damit sie beim nächsten Zug zurückkommt). Gestreamt werden die
//! benannten SSE-Ereignisse (`response.created`, `response.output_item.added`,
//! `response.output_text.delta`, …, `response.completed`).

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::ir::{
    ApiError, ChatRequest, ChatResponse, Content, ErrorKind, FunctionTool, MediaSource, Message,
    Part, PartKind, Reasoning, ResponseFormat, Role, StopReason, StreamEvent, StreamOptions, Tool,
    ToolChoice, ToolKind, Usage,
};
use crate::wire::{Losses, WireFormat, split_data_url, sse, type_of, unix_now};

const FMT: WireFormat = WireFormat::OpenAiResponses;

/// Anfrage `POST /v1/responses`.
#[derive(Debug, Clone, Deserialize)]
struct WireRequest {
    model: String,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    instructions: Option<Value>,
    #[serde(default)]
    tools: Option<Vec<Map<String, Value>>>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    text: Option<Map<String, Value>>,
    #[serde(default)]
    reasoning: Option<Map<String, Value>>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stream_options: Option<Map<String, Value>>,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    previous_response_id: Option<Value>,
    #[serde(default)]
    conversation: Option<Value>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    metadata: Option<Map<String, Value>>,
    #[serde(default)]
    user: Option<String>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Felder der Anfrage, die das Antwortobjekt wiederholt (`instructions`, `tools`,
/// `temperature` …).
#[derive(Debug, Clone, PartialEq)]
pub struct ResponsesEcho {
    /// Angefragtes Modell.
    pub model: String,
    instructions: Value,
    tools: Vec<Value>,
    tool_choice: Value,
    parallel_tool_calls: bool,
    temperature: Value,
    top_p: Value,
    max_output_tokens: Value,
    metadata: Value,
    text: Value,
    reasoning: Value,
    user: Value,
}

fn stateless(param: &str, what: &str) -> ApiError {
    ApiError::invalid_request(format!(
        "{what} wird nicht unterstützt: das Gateway speichert keine Antworten (zustandslos). \
         Den Verlauf bitte vollständig in `input` mitsenden."
    ))
    .with_param(param)
    .with_code("unsupported_parameter")
}

/// Parst einen Request-Body in die Chat-IR (+ Felder für das Antwortobjekt).
pub fn decode_responses_request(body: &[u8]) -> Result<(ChatRequest, ResponsesEcho), ApiError> {
    let w: WireRequest = serde_json::from_slice(body)?;
    if w.previous_response_id
        .as_ref()
        .is_some_and(|v| !v.is_null())
    {
        return Err(stateless("previous_response_id", "previous_response_id"));
    }
    if w.conversation.as_ref().is_some_and(|v| !v.is_null()) {
        return Err(stateless("conversation", "conversation"));
    }
    if w.store == Some(true) {
        return Err(stateless("store", "store: true"));
    }
    if w.background == Some(true) {
        return Err(stateless("background", "background: true"));
    }

    let echo = ResponsesEcho {
        model: w.model.clone(),
        instructions: w.instructions.clone().unwrap_or(Value::Null),
        tools: w
            .tools
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(Value::Object)
            .collect(),
        tool_choice: w.tool_choice.clone().unwrap_or_else(|| json!("auto")),
        parallel_tool_calls: w.parallel_tool_calls.unwrap_or(true),
        temperature: w.temperature.map_or(Value::Null, Value::from),
        top_p: w.top_p.map_or(Value::Null, Value::from),
        max_output_tokens: w.max_output_tokens.map_or(Value::Null, Value::from),
        metadata: Value::Object(w.metadata.clone().unwrap_or_default()),
        text: w
            .text
            .clone()
            .map_or_else(|| json!({"format": {"type": "text"}}), Value::Object),
        reasoning: w
            .reasoning
            .clone()
            .map_or_else(|| json!({"effort": null, "summary": null}), Value::Object),
        user: w.user.clone().map_or(Value::Null, Value::from),
    };

    let mut extra = w.extra;
    let mut messages = Vec::new();
    match w.instructions {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => messages.push(Message::text(Role::System, s)),
        Some(Value::Array(items)) => input_items(items, &mut messages)?,
        Some(_) => {
            return Err(
                ApiError::invalid_request("instructions muss ein String sein")
                    .with_param("instructions"),
            );
        }
    }
    match w.input {
        None | Some(Value::Null) => {
            return Err(ApiError::invalid_request("input fehlt").with_param("input"));
        }
        Some(Value::String(s)) => messages.push(Message::text(Role::User, s)),
        Some(Value::Array(items)) => input_items(items, &mut messages)?,
        Some(_) => {
            return Err(ApiError::invalid_request(
                "input muss ein String oder eine Liste von Items sein",
            )
            .with_param("input"));
        }
    }

    let mut tools = Vec::new();
    for t in w.tools.unwrap_or_default() {
        tools.push(tool_from_wire(t));
    }
    let tool_choice = match w.tool_choice {
        None => None,
        Some(v) => match tool_choice_from_wire(&v) {
            Some(tc) => Some(tc),
            None => {
                extra.insert("tool_choice".into(), v);
                None
            }
        },
    };
    let mut response_format = None;
    if let Some(mut text) = w.text {
        if let Some(Value::Object(f)) = text.remove("format") {
            response_format = Some(format_from_wire(f)?);
        }
        if !text.is_empty() {
            extra.insert("text".into(), Value::Object(text));
        }
    }
    let mut reasoning = None;
    if let Some(mut r) = w.reasoning {
        if let Some(Value::String(e)) = r.remove("effort") {
            reasoning = Some(Reasoning {
                effort: Some(e),
                thinking: None,
            });
        }
        r.retain(|_, v| !v.is_null());
        if !r.is_empty() {
            extra.insert("reasoning".into(), Value::Object(r));
        }
    }
    // `n` kennt die Responses API nicht; ai-gw nimmt es als Erweiterung an (F12): mehrere
    // Varianten erscheinen nacheinander als eigene Ausgabe-Items.
    let n = match extra.remove("n") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    ApiError::invalid_request("n muss eine positive Ganzzahl sein").with_param("n")
                })?,
        ),
    };
    let stream = w.stream;
    if let Some(so) = w.stream_options {
        extra.insert("stream_options".into(), Value::Object(so));
    }
    let req = ChatRequest {
        model: w.model,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls: w.parallel_tool_calls,
        sampling: crate::ir::Sampling {
            temperature: w.temperature,
            top_p: w.top_p,
            ..Default::default()
        },
        max_tokens: w.max_output_tokens,
        stop: Vec::new(),
        response_format,
        reasoning,
        user: w.user,
        metadata: w.metadata.filter(|m| !m.is_empty()),
        n,
        // Responses liefert die Usage immer (in `response.completed`).
        stream_options: stream.filter(|s| *s).map(|_| StreamOptions {
            include_usage: Some(true),
            extra: Map::new(),
        }),
        stream,
        hints: Default::default(),
        extra,
        source: Some(FMT),
    };
    Ok((req, echo))
}

fn bad_item(i: usize, what: impl std::fmt::Display) -> ApiError {
    ApiError::invalid_request(format!("input[{i}]: {what}")).with_param(format!("input[{i}]"))
}

fn str_field(m: &Map<String, Value>, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// Hängt Teile an die letzte Assistant-Nachricht an (bzw. legt eine neue an).
fn push_assistant(messages: &mut Vec<Message>, parts: Vec<Part>) {
    if let Some(last) = messages.last_mut()
        && last.role == Role::Assistant
    {
        let mut existing = std::mem::take(&mut last.content).into_parts();
        existing.extend(parts);
        last.content = Content::Parts(existing);
        return;
    }
    messages.push(Message {
        role: Role::Assistant,
        content: Content::Parts(parts),
        name: None,
        extra: Map::new(),
    });
}

fn input_items(items: Vec<Value>, messages: &mut Vec<Message>) -> Result<(), ApiError> {
    for (i, item) in items.into_iter().enumerate() {
        let Value::Object(mut m) = item else {
            return Err(bad_item(i, "Item muss ein Objekt sein"));
        };
        let ty = type_of(&m).unwrap_or("message").to_owned();
        match ty.as_str() {
            "message" => {
                let role = match m.get("role").and_then(Value::as_str) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    Some("system") => Role::System,
                    Some("developer") => Role::Developer,
                    other => {
                        return Err(bad_item(i, format!("unbekannte Rolle {other:?}")));
                    }
                };
                let content = match m.remove("content") {
                    Some(Value::String(s)) => Content::Text(s),
                    Some(Value::Array(parts)) => Content::Parts(
                        parts
                            .into_iter()
                            .map(content_part)
                            .collect::<Result<_, _>>()
                            .map_err(|e| bad_item(i, e.message))?,
                    ),
                    _ => return Err(bad_item(i, "content fehlt")),
                };
                if role == Role::Assistant {
                    push_assistant(messages, content.into_parts());
                } else {
                    messages.push(Message {
                        role,
                        content,
                        name: None,
                        extra: Map::new(),
                    });
                }
            }
            "function_call" => {
                let (Some(call_id), Some(name)) = (str_field(&m, "call_id"), str_field(&m, "name"))
                else {
                    return Err(bad_item(i, "function_call braucht call_id und name"));
                };
                let arguments = str_field(&m, "arguments").unwrap_or_else(|| "{}".to_owned());
                push_assistant(
                    messages,
                    vec![Part::new(PartKind::ToolCall {
                        id: call_id,
                        name,
                        arguments,
                    })],
                );
            }
            "function_call_output" => {
                let Some(call_id) = str_field(&m, "call_id") else {
                    return Err(bad_item(i, "function_call_output braucht call_id"));
                };
                let content = match m.remove("output") {
                    Some(Value::String(s)) => Content::Text(s),
                    Some(Value::Array(parts)) => Content::Parts(
                        parts
                            .into_iter()
                            .map(content_part)
                            .collect::<Result<_, _>>()
                            .map_err(|e| bad_item(i, e.message))?,
                    ),
                    Some(other) => Content::Text(other.to_string()),
                    None => Content::Text(String::new()),
                };
                messages.push(Message {
                    role: Role::Tool,
                    content: Content::Parts(vec![Part::new(PartKind::ToolResult {
                        tool_call_id: call_id,
                        content,
                        is_error: None,
                    })]),
                    name: None,
                    extra: Map::new(),
                });
            }
            "reasoning" => {
                let text = m
                    .get("summary")
                    .and_then(Value::as_array)
                    .map(|s| {
                        s.iter()
                            .filter_map(|p| p.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_default();
                let signature = str_field(&m, "encrypted_content");
                if text.is_empty() && signature.is_none() {
                    continue;
                }
                push_assistant(
                    messages,
                    vec![Part::new(PartKind::Reasoning { text, signature })],
                );
            }
            "item_reference" => {
                return Err(stateless(
                    &format!("input[{i}]"),
                    "item_reference (Verweis auf gespeicherte Items)",
                ));
            }
            other => {
                return Err(bad_item(
                    i,
                    format!("Item-Typ {other:?} wird nicht unterstützt"),
                ));
            }
        }
    }
    Ok(())
}

/// Inhaltsteil einer Nachricht (`input_text`, `input_image`, …).
fn content_part(v: Value) -> Result<Part, ApiError> {
    let Value::Object(mut m) = v else {
        return Err(ApiError::invalid_request(
            "Inhaltsteil muss ein Objekt sein",
        ));
    };
    let ty = type_of(&m).unwrap_or_default().to_owned();
    let kind = match ty.as_str() {
        "input_text" | "output_text" | "text" => PartKind::Text {
            text: str_field(&m, "text").unwrap_or_default(),
        },
        "refusal" => PartKind::Refusal {
            text: str_field(&m, "refusal").unwrap_or_default(),
        },
        "input_image" => {
            let source = if let Some(url) = str_field(&m, "image_url") {
                media_from_url(url)
            } else if let Some(id) = str_field(&m, "file_id") {
                MediaSource::FileId { file_id: id }
            } else {
                return Err(ApiError::invalid_request(
                    "input_image braucht image_url oder file_id",
                ));
            };
            PartKind::Image {
                source,
                detail: str_field(&m, "detail"),
            }
        }
        "input_file" => {
            let source = if let Some(data) = str_field(&m, "file_data") {
                media_from_url(data)
            } else if let Some(id) = str_field(&m, "file_id") {
                MediaSource::FileId { file_id: id }
            } else if let Some(url) = str_field(&m, "file_url") {
                MediaSource::Url { url }
            } else {
                return Err(ApiError::invalid_request(
                    "input_file braucht file_data, file_id oder file_url",
                ));
            };
            PartKind::File {
                source,
                filename: str_field(&m, "filename"),
            }
        }
        "input_audio" => {
            let a = m.remove("input_audio").unwrap_or(Value::Null);
            PartKind::Audio {
                data: a
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                format: a
                    .get("format")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }
        }
        _ => PartKind::Opaque {
            format: FMT,
            value: Value::Object(m),
        },
    };
    Ok(Part::new(kind))
}

fn media_from_url(url: String) -> MediaSource {
    match split_data_url(&url) {
        Some((media, data)) => MediaSource::Base64 {
            media_type: (!media.is_empty()).then(|| media.to_owned()),
            data: data.to_owned(),
        },
        None => MediaSource::Url { url },
    }
}

fn tool_from_wire(mut t: Map<String, Value>) -> Tool {
    if type_of(&t) == Some("function")
        && let Some(Value::String(name)) = t.remove("name")
    {
        t.remove("type");
        let description = match t.remove("description") {
            Some(Value::String(d)) => Some(d),
            _ => None,
        };
        let parameters = t.remove("parameters").filter(|p| !p.is_null());
        let strict = t.remove("strict").and_then(|s| s.as_bool());
        return Tool {
            kind: ToolKind::Function(FunctionTool {
                name,
                description,
                parameters,
                strict,
                extra: t,
            }),
            cache_control: None,
            extra: Map::new(),
        };
    }
    Tool {
        kind: ToolKind::Opaque {
            format: FMT,
            value: Value::Object(t),
        },
        cache_control: None,
        extra: Map::new(),
    }
}

fn tool_choice_from_wire(v: &Value) -> Option<ToolChoice> {
    match v {
        Value::String(s) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => o
            .get("name")
            .and_then(Value::as_str)
            .map(|n| ToolChoice::Tool { name: n.to_owned() }),
        _ => None,
    }
}

fn format_from_wire(mut f: Map<String, Value>) -> Result<ResponseFormat, ApiError> {
    match type_of(&f) {
        Some("text") => Ok(ResponseFormat::Text),
        Some("json_object") => Ok(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let schema = f.remove("schema").ok_or_else(|| {
                ApiError::invalid_request("text.format.schema fehlt").with_param("text.format")
            })?;
            Ok(ResponseFormat::JsonSchema {
                name: str_field(&f, "name"),
                description: str_field(&f, "description"),
                schema,
                strict: f.get("strict").and_then(Value::as_bool),
            })
        }
        other => Err(ApiError::invalid_request(format!(
            "text.format.type {other:?} unbekannt (text, json_object, json_schema)"
        ))
        .with_param("text.format")),
    }
}

// ---------------------------------------------------------------------------------------------
// Antwort
// ---------------------------------------------------------------------------------------------

/// Neue Antwort-ID (`resp_…`).
pub fn new_response_id() -> String {
    format!("resp_{}", uuid::Uuid::now_v7().simple())
}

fn item_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}

fn usage_to_wire(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "input_tokens_details": {"cached_tokens": u.cache_read_tokens.unwrap_or(0)},
        "output_tokens": u.output_tokens,
        "output_tokens_details": {"reasoning_tokens": u.reasoning_tokens.unwrap_or(0)},
        "total_tokens": u.total_tokens(),
    })
}

/// Status und `incomplete_details` zum Endgrund.
fn status_of(reason: Option<&StopReason>) -> (&'static str, Value) {
    match reason {
        Some(StopReason::MaxTokens) => ("incomplete", json!({"reason": "max_output_tokens"})),
        Some(StopReason::ContentFilter) => ("incomplete", json!({"reason": "content_filter"})),
        _ => ("completed", Value::Null),
    }
}

struct ResponseShell<'a> {
    id: &'a str,
    created_at: i64,
    model: &'a str,
    status: &'a str,
    incomplete: Value,
    error: Value,
    output: Vec<Value>,
    usage: Value,
}

fn response_object(echo: &ResponsesEcho, s: ResponseShell<'_>) -> Value {
    json!({
        "id": s.id,
        "object": "response",
        "created_at": s.created_at,
        "status": s.status,
        "background": false,
        "error": s.error,
        "incomplete_details": s.incomplete,
        "instructions": echo.instructions,
        "max_output_tokens": echo.max_output_tokens,
        "model": s.model,
        "output": s.output,
        "parallel_tool_calls": echo.parallel_tool_calls,
        "previous_response_id": null,
        "reasoning": echo.reasoning,
        "store": false,
        "temperature": echo.temperature,
        "text": echo.text,
        "tool_choice": echo.tool_choice,
        "tools": echo.tools,
        "top_p": echo.top_p,
        "truncation": "disabled",
        "usage": s.usage,
        "user": echo.user,
        "metadata": echo.metadata,
    })
}

fn message_item(id: &str, status: &str, content: Vec<Value>) -> Value {
    json!({"id": id, "type": "message", "status": status, "role": "assistant", "content": content})
}

fn output_text(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": [], "logprobs": []})
}

fn reasoning_item(id: &str, text: &str, signature: Option<&str>) -> Value {
    let summary = if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "summary_text", "text": text})]
    };
    let mut item = json!({"id": id, "type": "reasoning", "summary": summary});
    if let (Some(sig), Some(o)) = (signature, item.as_object_mut()) {
        o.insert("encrypted_content".into(), Value::from(sig));
    }
    item
}

fn function_call_item(id: &str, call_id: &str, name: &str, args: &str, status: &str) -> Value {
    json!({"id": id, "type": "function_call", "status": status, "call_id": call_id,
           "name": name, "arguments": args})
}

/// Übersetzt eine IR-Antwort in ein Responses-Objekt.
pub fn response_to_wire(resp: ChatResponse, echo: &ResponsesEcho, losses: &mut Losses) -> Value {
    losses.extra("responses response", &resp.extra);
    let mut output = Vec::new();
    let mut reason = None;
    // Mehrere Varianten (`n > 1`, F12) nacheinander; der Status folgt der ersten.
    for (i, choice) in resp.choices.into_iter().enumerate() {
        if i == 0 {
            reason = choice.stop_reason;
        }
        let mut content: Vec<Value> = Vec::new();
        let flush = |content: &mut Vec<Value>, output: &mut Vec<Value>| {
            if !content.is_empty() {
                output.push(message_item(
                    &item_id("msg"),
                    "completed",
                    std::mem::take(content),
                ));
            }
        };
        for p in choice.message.content.into_parts() {
            match p.kind {
                PartKind::Text { text } => content.push(output_text(&text)),
                PartKind::Refusal { text } => {
                    content.push(json!({"type": "refusal", "refusal": text}));
                }
                PartKind::Reasoning { text, signature } => {
                    flush(&mut content, &mut output);
                    output.push(reasoning_item(&item_id("rs"), &text, signature.as_deref()));
                }
                PartKind::RedactedReasoning { data } => {
                    flush(&mut content, &mut output);
                    output.push(reasoning_item(&item_id("rs"), "", Some(&data)));
                }
                PartKind::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    flush(&mut content, &mut output);
                    output.push(function_call_item(
                        &item_id("fc"),
                        &id,
                        &name,
                        &arguments,
                        "completed",
                    ));
                }
                _ => losses.note("responses: Antwortteil ohne Entsprechung verworfen"),
            }
        }
        flush(&mut content, &mut output);
    }
    let (status, incomplete) = status_of(reason.as_ref());
    let model = if resp.model.is_empty() {
        echo.model.clone()
    } else {
        resp.model
    };
    response_object(
        echo,
        ResponseShell {
            id: &new_response_id(),
            created_at: resp.created.unwrap_or_else(unix_now),
            model: &model,
            status,
            incomplete,
            error: Value::Null,
            output,
            usage: resp.usage.as_ref().map_or(Value::Null, usage_to_wire),
        },
    )
}

/// Serialisiert eine IR-Antwort als Responses-Objekt.
pub fn encode_responses_response(
    resp: ChatResponse,
    echo: &ResponsesEcho,
    losses: &mut Losses,
) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(&response_to_wire(resp, echo, losses))
        .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------------------------

/// Offenes Ausgabe-Item im Stream.
#[derive(Debug)]
enum Open {
    Message {
        index: u32,
        id: String,
        output_index: usize,
        text: String,
    },
    Reasoning {
        index: u32,
        id: String,
        output_index: usize,
        text: String,
        signature: Option<String>,
        part_open: bool,
    },
    Tool {
        index: u32,
        id: String,
        output_index: usize,
        call_id: String,
        name: String,
        args: String,
    },
}

impl Open {
    fn index(&self) -> u32 {
        match self {
            Self::Message { index, .. }
            | Self::Reasoning { index, .. }
            | Self::Tool { index, .. } => *index,
        }
    }
}

/// Zustandsbehafteter Encoder IR-Ereignisse → Responses-SSE (benannte Ereignisse).
#[derive(Debug)]
pub struct ResponsesStreamEncoder {
    echo: ResponsesEcho,
    id: String,
    model: String,
    created_at: i64,
    seq: u64,
    output: Vec<Value>,
    open: Option<Open>,
    usage: Option<Usage>,
    reason: Option<StopReason>,
    started: bool,
    done: bool,
}

impl ResponsesStreamEncoder {
    /// Neuer Encoder für eine Anfrage.
    pub fn new(echo: ResponsesEcho) -> Self {
        Self {
            model: echo.model.clone(),
            echo,
            id: new_response_id(),
            created_at: unix_now(),
            seq: 0,
            output: Vec::new(),
            open: None,
            usage: None,
            reason: None,
            started: false,
            done: false,
        }
    }

    fn emit(&mut self, out: &mut Vec<u8>, ty: &str, mut data: Value) {
        if let Some(o) = data.as_object_mut() {
            o.insert("type".into(), Value::from(ty));
            o.insert("sequence_number".into(), Value::from(self.seq));
        }
        self.seq += 1;
        if let Ok(json) = serde_json::to_vec(&data) {
            sse::write_event(out, ty, &json);
        }
    }

    fn shell(&self, status: &str, incomplete: Value, error: Value, usage: Value) -> Value {
        response_object(
            &self.echo,
            ResponseShell {
                id: &self.id,
                created_at: self.created_at,
                model: &self.model,
                status,
                incomplete,
                error,
                output: self.output.clone(),
                usage,
            },
        )
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        let r = self.shell("in_progress", Value::Null, Value::Null, Value::Null);
        self.emit(out, "response.created", json!({ "response": r }));
        self.emit(out, "response.in_progress", json!({ "response": r }));
    }

    /// Schließt das offene Item ab.
    fn close(&mut self, out: &mut Vec<u8>) {
        let Some(open) = self.open.take() else {
            return;
        };
        match open {
            Open::Message {
                id,
                output_index,
                text,
                ..
            } => {
                self.emit(
                    out,
                    "response.output_text.done",
                    json!({"item_id": id, "output_index": output_index, "content_index": 0,
                           "text": text, "logprobs": []}),
                );
                self.emit(
                    out,
                    "response.content_part.done",
                    json!({"item_id": id, "output_index": output_index, "content_index": 0,
                           "part": output_text(&text)}),
                );
                let item = message_item(&id, "completed", vec![output_text(&text)]);
                self.finish_item(out, output_index, item);
            }
            Open::Reasoning {
                id,
                output_index,
                text,
                signature,
                part_open,
                ..
            } => {
                if part_open {
                    self.emit(
                        out,
                        "response.reasoning_summary_text.done",
                        json!({"item_id": id, "output_index": output_index, "summary_index": 0,
                               "text": text}),
                    );
                    self.emit(
                        out,
                        "response.reasoning_summary_part.done",
                        json!({"item_id": id, "output_index": output_index, "summary_index": 0,
                               "part": {"type": "summary_text", "text": text}}),
                    );
                }
                let item = reasoning_item(&id, &text, signature.as_deref());
                self.finish_item(out, output_index, item);
            }
            Open::Tool {
                id,
                output_index,
                call_id,
                name,
                args,
                ..
            } => {
                self.emit(
                    out,
                    "response.function_call_arguments.done",
                    json!({"item_id": id, "output_index": output_index, "name": name,
                           "arguments": args}),
                );
                let item = function_call_item(&id, &call_id, &name, &args, "completed");
                self.finish_item(out, output_index, item);
            }
        }
    }

    fn finish_item(&mut self, out: &mut Vec<u8>, output_index: usize, item: Value) {
        self.emit(
            out,
            "response.output_item.done",
            json!({"output_index": output_index, "item": item.clone()}),
        );
        self.output.push(item);
    }

    fn add_item(&mut self, out: &mut Vec<u8>, item: Value) -> usize {
        let output_index = self.output.len();
        self.emit(
            out,
            "response.output_item.added",
            json!({"output_index": output_index, "item": item}),
        );
        output_index
    }

    fn is_open(&self, index: u32, kind: u8) -> bool {
        matches!(
            (&self.open, kind),
            (Some(Open::Message { index: i, .. }), 0)
                | (Some(Open::Reasoning { index: i, .. }), 1)
                | (Some(Open::Tool { index: i, .. }), 2) if *i == index
        )
    }

    fn open_message(&mut self, out: &mut Vec<u8>, index: u32) {
        self.close(out);
        let id = item_id("msg");
        let output_index = self.add_item(out, message_item(&id, "in_progress", Vec::new()));
        self.emit(
            out,
            "response.content_part.added",
            json!({"item_id": id, "output_index": output_index, "content_index": 0,
                   "part": output_text("")}),
        );
        self.open = Some(Open::Message {
            index,
            id,
            output_index,
            text: String::new(),
        });
    }

    fn open_reasoning(&mut self, out: &mut Vec<u8>, index: u32) {
        self.close(out);
        let id = item_id("rs");
        let output_index = self.add_item(out, reasoning_item(&id, "", None));
        self.open = Some(Open::Reasoning {
            index,
            id,
            output_index,
            text: String::new(),
            signature: None,
            part_open: false,
        });
    }

    /// Kodiert ein Ereignis.
    pub fn encode(&mut self, ev: StreamEvent, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        match ev {
            StreamEvent::Start { model, created, .. } => {
                if !model.is_empty() {
                    self.model = model;
                }
                if let Some(c) = created {
                    self.created_at = c;
                }
                self.ensure_started(out);
            }
            StreamEvent::TextDelta { index, text } => {
                self.ensure_started(out);
                if !self.is_open(index, 0) {
                    self.open_message(out, index);
                }
                let mut data = None;
                if let Some(Open::Message {
                    id,
                    output_index,
                    text: acc,
                    ..
                }) = &mut self.open
                {
                    acc.push_str(&text);
                    data = Some(json!({"item_id": id, "output_index": output_index,
                                       "content_index": 0, "delta": text, "logprobs": []}));
                }
                if let Some(d) = data {
                    self.emit(out, "response.output_text.delta", d);
                }
            }
            StreamEvent::ReasoningDelta { index, text } => {
                self.ensure_started(out);
                if !self.is_open(index, 1) {
                    self.open_reasoning(out, index);
                }
                let mut events = Vec::new();
                if let Some(Open::Reasoning {
                    id,
                    output_index,
                    text: acc,
                    part_open,
                    ..
                }) = &mut self.open
                {
                    if !*part_open {
                        *part_open = true;
                        events.push((
                            "response.reasoning_summary_part.added",
                            json!({"item_id": id, "output_index": output_index,
                                   "summary_index": 0,
                                   "part": {"type": "summary_text", "text": ""}}),
                        ));
                    }
                    acc.push_str(&text);
                    events.push((
                        "response.reasoning_summary_text.delta",
                        json!({"item_id": id, "output_index": output_index,
                               "summary_index": 0, "delta": text}),
                    ));
                }
                for (ty, data) in events {
                    self.emit(out, ty, data);
                }
            }
            StreamEvent::ReasoningSignature { index, signature } => {
                self.ensure_started(out);
                if !self.is_open(index, 1) {
                    self.open_reasoning(out, index);
                }
                if let Some(Open::Reasoning { signature: s, .. }) = &mut self.open {
                    *s = Some(signature);
                }
            }
            StreamEvent::RedactedReasoning { index, data } => {
                self.ensure_started(out);
                self.open_reasoning(out, index);
                if let Some(Open::Reasoning { signature: s, .. }) = &mut self.open {
                    *s = Some(data);
                }
                self.close(out);
            }
            StreamEvent::ToolCallStart(t) => {
                self.ensure_started(out);
                self.close(out);
                let id = item_id("fc");
                let output_index = self.add_item(
                    out,
                    function_call_item(&id, &t.id, &t.name, "", "in_progress"),
                );
                self.open = Some(Open::Tool {
                    index: t.index,
                    id,
                    output_index,
                    call_id: t.id,
                    name: t.name,
                    args: String::new(),
                });
            }
            StreamEvent::ToolCallDelta { index, arguments } => {
                if self.open.as_ref().map(Open::index) != Some(index) {
                    return;
                }
                let mut data = None;
                if let Some(Open::Tool {
                    id,
                    output_index,
                    args,
                    ..
                }) = &mut self.open
                {
                    args.push_str(&arguments);
                    data = Some(json!({"item_id": id, "output_index": output_index,
                                       "delta": arguments}));
                }
                if let Some(d) = data {
                    self.emit(out, "response.function_call_arguments.delta", d);
                }
            }
            StreamEvent::Choice { .. } => {
                // Varianten kommen nacheinander (der Proxy ordnet sie); jede beginnt neue Items.
                self.close(out);
            }
            StreamEvent::Stop { reason, .. } => {
                self.ensure_started(out);
                self.close(out);
                // Bei mehreren Varianten bestimmt die erste den Status.
                if self.reason.is_none() {
                    self.reason = Some(reason);
                }
            }
            StreamEvent::Usage(u) => self.usage = Some(u),
            StreamEvent::Error(e) => {
                self.ensure_started(out);
                self.close(out);
                let code = e
                    .code
                    .clone()
                    .unwrap_or_else(|| super::error_type(e.kind).to_owned());
                self.emit(
                    out,
                    "error",
                    json!({"code": code, "message": e.message, "param": e.param}),
                );
                let usage = self.usage.as_ref().map_or(Value::Null, usage_to_wire);
                let r = self.shell(
                    "failed",
                    Value::Null,
                    json!({"code": code, "message": e.message}),
                    usage,
                );
                self.emit(out, "response.failed", json!({ "response": r }));
                self.done = true;
            }
        }
    }

    /// Ende des Streams: schließt offene Items und sendet `response.completed` bzw.
    /// `response.incomplete`.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.done {
            return;
        }
        self.done = true;
        self.ensure_started(out);
        self.close(out);
        let (status, incomplete) = status_of(self.reason.as_ref());
        let usage = self.usage.as_ref().map_or(Value::Null, usage_to_wire);
        let r = self.shell(status, incomplete, Value::Null, usage);
        let ty = if status == "completed" {
            "response.completed"
        } else {
            "response.incomplete"
        };
        self.emit(out, ty, json!({ "response": r }));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::ir::{Choice, ToolCallStart};

    fn decode(v: Value) -> Result<(ChatRequest, ResponsesEcho), ApiError> {
        decode_responses_request(v.to_string().as_bytes())
    }

    #[test]
    fn request_mapping() {
        let (req, echo) = decode(json!({
            "model": "gpt-x",
            "instructions": "Sei kurz.",
            "input": [
                {"role": "user", "content": "Wetter?"},
                {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "denke"}],
                 "encrypted_content": "sig"},
                {"type": "function_call", "call_id": "call_1", "name": "wetter", "arguments": "{\"ort\":\"Bonn\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "sonnig"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "Und morgen?"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "low"}]}
            ],
            "tools": [{"type": "function", "name": "wetter", "parameters": {"type": "object"}},
                      {"type": "web_search"}],
            "tool_choice": {"type": "function", "name": "wetter"},
            "max_output_tokens": 50,
            "text": {"format": {"type": "json_schema", "name": "x", "schema": {"type": "object"}}},
            "reasoning": {"effort": "low", "summary": "auto"},
            "store": false,
            "stream": true,
            "service_tier": "flex"
        }))
        .unwrap();
        assert_eq!(req.messages.len(), 5);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[2].role, Role::Assistant);
        let parts = req.messages[2].content.clone().into_parts();
        assert!(
            matches!(parts[0].kind, PartKind::Reasoning { ref signature, .. } if signature.as_deref() == Some("sig"))
        );
        assert!(matches!(parts[1].kind, PartKind::ToolCall { ref id, .. } if id == "call_1"));
        assert_eq!(req.messages[3].role, Role::Tool);
        assert_eq!(req.max_tokens, Some(50));
        assert!(matches!(
            req.response_format,
            Some(ResponseFormat::JsonSchema { .. })
        ));
        assert_eq!(
            req.tool_choice,
            Some(ToolChoice::Tool {
                name: "wetter".into()
            })
        );
        assert!(matches!(req.tools[1].kind, ToolKind::Opaque { .. }));
        assert_eq!(req.reasoning.unwrap().effort.as_deref(), Some("low"));
        assert_eq!(req.stream_options.unwrap().include_usage, Some(true));
        assert_eq!(req.extra["service_tier"], "flex");
        assert_eq!(req.source, Some(WireFormat::OpenAiResponses));
        assert_eq!(echo.tools.len(), 2);
    }

    #[test]
    fn stateful_features_are_rejected() {
        for body in [
            json!({"model": "m", "input": "x", "previous_response_id": "resp_1"}),
            json!({"model": "m", "input": "x", "store": true}),
            json!({"model": "m", "input": "x", "conversation": "conv_1"}),
            json!({"model": "m", "input": "x", "background": true}),
            json!({"model": "m", "input": [{"type": "item_reference", "id": "msg_1"}]}),
            json!({"model": "m"}),
        ] {
            let e = decode(body.clone()).unwrap_err();
            assert_eq!(e.status(), 400, "{body}");
        }
        assert!(decode(json!({"model": "m", "input": "x", "previous_response_id": null})).is_ok());
    }

    fn sample_response(reason: StopReason) -> ChatResponse {
        ChatResponse {
            id: "chatcmpl-1".into(),
            model: "gpt-x".into(),
            created: Some(7),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Content::Parts(vec![
                        Part::new(PartKind::Reasoning {
                            text: "hm".into(),
                            signature: Some("s".into()),
                        }),
                        Part::text("Hallo"),
                        Part::new(PartKind::ToolCall {
                            id: "call_9".into(),
                            name: "f".into(),
                            arguments: "{}".into(),
                        }),
                    ]),
                    name: None,
                    extra: Map::new(),
                },
                stop_reason: Some(reason),
                stop_sequence: None,
                extra: Map::new(),
            }],
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: Some(2),
                ..Usage::default()
            }),
            extra: Map::new(),
            source: None,
        }
    }

    #[test]
    fn response_object_shape() {
        let (_, echo) =
            decode(json!({"model": "gpt-x", "input": "hi", "temperature": 0.5})).unwrap();
        let mut losses = Losses::new();
        let r = response_to_wire(sample_response(StopReason::ToolUse), &echo, &mut losses);
        assert_eq!(r["object"], "response");
        assert_eq!(r["status"], "completed");
        assert_eq!(r["store"], false);
        assert_eq!(r["temperature"], 0.5);
        let out = r["output"].as_array().unwrap();
        assert_eq!(out[0]["type"], "reasoning");
        assert_eq!(out[0]["encrypted_content"], "s");
        assert_eq!(out[1]["type"], "message");
        assert_eq!(out[1]["content"][0]["text"], "Hallo");
        assert_eq!(out[2]["type"], "function_call");
        assert_eq!(out[2]["call_id"], "call_9");
        assert_eq!(r["usage"]["total_tokens"], 15);
        assert_eq!(r["usage"]["output_tokens_details"]["reasoning_tokens"], 2);
        let r = response_to_wire(sample_response(StopReason::MaxTokens), &echo, &mut losses);
        assert_eq!(r["status"], "incomplete");
        assert_eq!(r["incomplete_details"]["reason"], "max_output_tokens");
    }

    fn events(out: &[u8]) -> Vec<(String, Value)> {
        let mut p = sse::SseParser::new();
        p.push(out)
            .into_iter()
            .map(|e| {
                (
                    e.event.unwrap_or_default(),
                    serde_json::from_str(&e.data).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn stream_events() {
        let (_, echo) = decode(json!({"model": "gpt-x", "input": "hi", "stream": true})).unwrap();
        let mut enc = ResponsesStreamEncoder::new(echo);
        let mut out = Vec::new();
        for ev in [
            StreamEvent::Start {
                id: "x".into(),
                model: "gpt-x-2".into(),
                created: Some(1),
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "hm".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "Hal".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "lo".into(),
            },
            StreamEvent::ToolCallStart(ToolCallStart {
                index: 2,
                id: "call_1".into(),
                name: "f".into(),
            }),
            StreamEvent::ToolCallDelta {
                index: 2,
                arguments: "{}".into(),
            },
            StreamEvent::Stop {
                reason: StopReason::ToolUse,
                stop_sequence: None,
            },
            StreamEvent::Usage(Usage {
                input_tokens: 3,
                output_tokens: 4,
                ..Usage::default()
            }),
        ] {
            enc.encode(ev, &mut out);
        }
        enc.finish(&mut out);
        let evs = events(&out);
        let names: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        for (i, (name, data)) in evs.iter().enumerate() {
            assert_eq!(data["type"], name.as_str());
            assert_eq!(data["sequence_number"], i);
        }
        let done = &evs.last().unwrap().1["response"];
        assert_eq!(done["model"], "gpt-x-2");
        assert_eq!(done["output"][1]["content"][0]["text"], "Hallo");
        assert_eq!(done["output"][2]["arguments"], "{}");
        assert_eq!(done["usage"]["total_tokens"], 7);
    }

    #[test]
    fn stream_error() {
        let (_, echo) = decode(json!({"model": "m", "input": "hi"})).unwrap();
        let mut enc = ResponsesStreamEncoder::new(echo);
        let mut out = Vec::new();
        enc.encode(
            StreamEvent::TextDelta {
                index: 0,
                text: "a".into(),
            },
            &mut out,
        );
        enc.encode(
            StreamEvent::Error(ApiError::new(ErrorKind::BadGateway, "kaputt")),
            &mut out,
        );
        enc.finish(&mut out);
        let evs = events(&out);
        let names: Vec<&str> = evs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names[names.len() - 2..], ["error", "response.failed"]);
        assert_eq!(evs.last().unwrap().1["response"]["status"], "failed");
    }
}
