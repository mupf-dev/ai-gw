// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Konverter OpenAI Chat Completions ↔ IR.

use serde_json::{Map, Value, json};

use super::{
    ChatCompletion, ChatCompletionRequest, ChatMessage, CompletionChoice, ErrorBody, ErrorObject,
    FunctionCall, Model, ModelList, ResponseMessage, Stop, ToolCall, Usage as WireUsage,
};
use crate::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorKind, FunctionTool, MediaSource,
    Message, ModelInfo, Part, PartKind, Reasoning, RequestHints, ResponseFormat, Role, Sampling,
    StopReason, StreamOptions, Tool, ToolChoice, ToolKind, Usage,
};
use crate::wire::{Losses, TextOrParts, WireFormat, from_map, keep, split_data_url, type_of};

const FMT: WireFormat = WireFormat::OpenAiChat;

// ---------------------------------------------------------------------------------------------
// Anfrage: Wire → IR
// ---------------------------------------------------------------------------------------------

/// Parst einen Request-Body in die IR.
pub fn decode_request(body: &[u8]) -> Result<ChatRequest, ApiError> {
    let wire: ChatCompletionRequest = serde_json::from_slice(body)?;
    request_from_wire(wire)
}

/// Übersetzt eine geparste Anfrage in die IR.
pub fn request_from_wire(w: ChatCompletionRequest) -> Result<ChatRequest, ApiError> {
    let mut extra = w.extra;
    let messages = w
        .messages
        .into_iter()
        .enumerate()
        .map(|(i, m)| message_from_wire(m).map_err(|e| e.with_param(format!("messages[{i}]"))))
        .collect::<Result<Vec<_>, _>>()?;

    let mut tools = Vec::new();
    for t in w.tools.unwrap_or_default() {
        tools.push(tool_from_wire(t)?);
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

    let response_format = match w.response_format {
        None => None,
        Some(v) => match response_format_from_wire(v) {
            Ok(rf) => Some(rf),
            Err(v) => {
                extra.insert("response_format".into(), v);
                None
            }
        },
    };

    let mut hints = RequestHints::default();
    let max_tokens = match (w.max_completion_tokens, w.max_tokens) {
        (Some(m), legacy) => {
            if let Some(l) = legacy {
                extra.insert("max_tokens".into(), Value::from(l));
            }
            Some(m)
        }
        (None, Some(l)) => {
            hints.legacy_max_tokens = true;
            Some(l)
        }
        (None, None) => None,
    };
    let stop = match w.stop {
        None => Vec::new(),
        Some(Stop::One(s)) => {
            hints.stop_as_string = true;
            vec![s]
        }
        Some(Stop::Many(v)) => v,
    };

    let stream_options = w.stream_options.map(|mut m| StreamOptions {
        include_usage: m.remove("include_usage").and_then(|v| v.as_bool()),
        extra: m,
    });

    Ok(ChatRequest {
        model: w.model,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls: w.parallel_tool_calls,
        sampling: Sampling {
            temperature: w.temperature,
            top_p: w.top_p,
            top_k: None,
            frequency_penalty: w.frequency_penalty,
            presence_penalty: w.presence_penalty,
            seed: w.seed,
        },
        max_tokens,
        stop,
        response_format,
        reasoning: w.reasoning_effort.map(|e| Reasoning {
            effort: Some(e),
            thinking: None,
        }),
        user: w.user,
        metadata: w.metadata,
        n: w.n,
        stream: w.stream,
        stream_options,
        hints,
        extra,
        source: Some(FMT),
    })
}

fn role_from_wire(role: &str) -> Result<Role, ApiError> {
    Ok(match role {
        "system" => Role::System,
        "developer" => Role::Developer,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => {
            return Err(ApiError::invalid_request(format!(
                "unbekannte Rolle '{other}'"
            )));
        }
    })
}

fn message_from_wire(m: ChatMessage) -> Result<Message, ApiError> {
    let role = role_from_wire(&m.role)?;
    let content = match role {
        Role::Tool => {
            let id = m.tool_call_id.ok_or_else(|| {
                ApiError::invalid_request("Nachricht mit Rolle 'tool' ohne tool_call_id")
            })?;
            let inner = match m.content {
                Some(c) => content_from_wire(c)?,
                None => Content::Text(String::new()),
            };
            Content::Parts(vec![Part::new(PartKind::ToolResult {
                tool_call_id: id,
                content: inner,
                is_error: None,
            })])
        }
        Role::Assistant if m.tool_calls.is_some() || m.refusal.is_some() => {
            let mut parts = match m.content {
                Some(c) => content_from_wire(c)?.into_parts(),
                None => Vec::new(),
            };
            if let Some(r) = m.refusal {
                parts.push(Part::new(PartKind::Refusal { text: r }));
            }
            for tc in m.tool_calls.unwrap_or_default() {
                parts.push(tool_call_from_wire(tc));
            }
            Content::Parts(parts)
        }
        Role::Assistant => match m.content {
            Some(c) => content_from_wire(c)?,
            None => Content::Parts(Vec::new()),
        },
        _ => content_from_wire(
            m.content
                .ok_or_else(|| ApiError::invalid_request("Nachricht ohne content"))?,
        )?,
    };
    Ok(Message {
        role,
        content,
        name: m.name,
        extra: m.extra,
    })
}

fn tool_call_from_wire(tc: ToolCall) -> Part {
    Part {
        kind: PartKind::ToolCall {
            id: tc.id,
            name: tc.function.name,
            arguments: tc.function.arguments,
        },
        cache_control: None,
        extra: tc.extra,
    }
}

fn content_from_wire(c: TextOrParts<Map<String, Value>>) -> Result<Content, ApiError> {
    Ok(match c {
        TextOrParts::Text(t) => Content::Text(t),
        TextOrParts::Parts(parts) => Content::Parts(
            parts
                .into_iter()
                .map(part_from_wire)
                .collect::<Result<_, _>>()?,
        ),
    })
}

#[derive(serde::Deserialize)]
struct TextPart {
    text: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(serde::Deserialize)]
struct RefusalPart {
    refusal: String,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(serde::Deserialize)]
struct ImagePart {
    image_url: ImageUrl,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(serde::Deserialize)]
struct ImageUrl {
    url: String,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(serde::Deserialize)]
struct AudioPart {
    input_audio: InputAudio,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(serde::Deserialize)]
struct InputAudio {
    data: String,
    format: String,
}

#[derive(serde::Deserialize)]
struct FilePart {
    file: FileRef,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(serde::Deserialize)]
struct FileRef {
    #[serde(default)]
    file_data: Option<String>,
    #[serde(default)]
    file_id: Option<String>,
    #[serde(default)]
    filename: Option<String>,
}

fn part_from_wire(mut map: Map<String, Value>) -> Result<Part, ApiError> {
    let Some(ty) = type_of(&map).map(str::to_owned) else {
        return Err(ApiError::invalid_request("Inhaltsteil ohne 'type'"));
    };
    let bad = |e: serde_json::Error| ApiError::invalid_request(format!("Inhaltsteil '{ty}': {e}"));
    let part = match ty.as_str() {
        "text" => {
            map.remove("type");
            let p: TextPart = from_map(map).map_err(bad)?;
            Part {
                kind: PartKind::Text { text: p.text },
                cache_control: None,
                extra: p.extra,
            }
        }
        "refusal" => {
            map.remove("type");
            let p: RefusalPart = from_map(map).map_err(bad)?;
            Part {
                kind: PartKind::Refusal { text: p.refusal },
                cache_control: None,
                extra: p.extra,
            }
        }
        "image_url" => {
            map.remove("type");
            let p: ImagePart = from_map(map).map_err(bad)?;
            let source = match split_data_url(&p.image_url.url) {
                Some((media, data)) => MediaSource::Base64 {
                    media_type: (!media.is_empty()).then(|| media.to_owned()),
                    data: data.to_owned(),
                },
                None => MediaSource::Url {
                    url: p.image_url.url,
                },
            };
            Part {
                kind: PartKind::Image {
                    source,
                    detail: p.image_url.detail,
                },
                cache_control: None,
                extra: p.extra,
            }
        }
        "input_audio" => {
            map.remove("type");
            let p: AudioPart = from_map(map).map_err(bad)?;
            Part {
                kind: PartKind::Audio {
                    data: p.input_audio.data,
                    format: p.input_audio.format,
                },
                cache_control: None,
                extra: p.extra,
            }
        }
        "file" => {
            map.remove("type");
            let p: FilePart = from_map(map).map_err(bad)?;
            let source = match (p.file.file_id, p.file.file_data) {
                (Some(id), _) => MediaSource::FileId { file_id: id },
                (None, Some(data)) => match split_data_url(&data) {
                    Some((media, d)) => MediaSource::Base64 {
                        media_type: Some(media.to_owned()),
                        data: d.to_owned(),
                    },
                    None => MediaSource::Base64 {
                        media_type: None,
                        data,
                    },
                },
                (None, None) => {
                    return Err(ApiError::invalid_request(
                        "Inhaltsteil 'file' braucht file_id oder file_data",
                    ));
                }
            };
            Part {
                kind: PartKind::File {
                    source,
                    filename: p.file.filename,
                },
                cache_control: None,
                extra: p.extra,
            }
        }
        _ => Part::new(PartKind::Opaque {
            format: FMT,
            value: Value::Object(map),
        }),
    };
    Ok(part)
}

fn tool_from_wire(mut map: Map<String, Value>) -> Result<Tool, ApiError> {
    if type_of(&map) != Some("function") {
        return Ok(Tool {
            kind: ToolKind::Opaque {
                format: FMT,
                value: Value::Object(map),
            },
            cache_control: None,
            extra: Map::new(),
        });
    }
    map.remove("type");
    let Some(Value::Object(mut f)) = map.remove("function") else {
        return Err(ApiError::invalid_request("Tool ohne 'function'").with_param("tools"));
    };
    let name = match f.remove("name") {
        Some(Value::String(s)) => s,
        _ => return Err(ApiError::invalid_request("Tool ohne Namen").with_param("tools")),
    };
    let description = match f.remove("description") {
        Some(Value::String(s)) => Some(s),
        Some(Value::Null) | None => None,
        Some(v) => {
            f.insert("description".into(), v);
            None
        }
    };
    let parameters = f.remove("parameters").filter(|v| !v.is_null());
    let strict = match f.remove("strict") {
        Some(Value::Bool(b)) => Some(b),
        Some(Value::Null) | None => None,
        Some(v) => {
            f.insert("strict".into(), v);
            None
        }
    };
    Ok(Tool {
        kind: ToolKind::Function(FunctionTool {
            name,
            description,
            parameters,
            strict,
            extra: f,
        }),
        cache_control: None,
        extra: map,
    })
}

fn tool_choice_from_wire(v: &Value) -> Option<ToolChoice> {
    match v {
        Value::String(s) => match s.as_str() {
            "auto" => Some(ToolChoice::Auto),
            "none" => Some(ToolChoice::None),
            "required" => Some(ToolChoice::Required),
            _ => None,
        },
        Value::Object(o) if o.len() == 2 && o.get("type")?.as_str()? == "function" => {
            let f = o.get("function")?.as_object()?;
            if f.len() != 1 {
                return None;
            }
            Some(ToolChoice::Tool {
                name: f.get("name")?.as_str()?.to_owned(),
            })
        }
        _ => None,
    }
}

/// Liefert das Original zurück (`Err`), wenn es nicht verlustfrei abbildbar ist.
fn response_format_from_wire(v: Value) -> Result<ResponseFormat, Value> {
    let Value::Object(o) = &v else { return Err(v) };
    match (o.get("type").and_then(Value::as_str), o.len()) {
        (Some("text"), 1) => Ok(ResponseFormat::Text),
        (Some("json_object"), 1) => Ok(ResponseFormat::JsonObject),
        (Some("json_schema"), 2) => {
            let Some(Value::Object(js)) = o.get("json_schema") else {
                return Err(v);
            };
            let known = ["name", "description", "schema", "strict"];
            if js.keys().any(|k| !known.contains(&k.as_str())) {
                return Err(v);
            }
            let Some(schema) = js.get("schema").cloned() else {
                return Err(v);
            };
            Ok(ResponseFormat::JsonSchema {
                name: js.get("name").and_then(Value::as_str).map(str::to_owned),
                description: js
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                schema,
                strict: js.get("strict").and_then(Value::as_bool),
            })
        }
        _ => Err(v),
    }
}

// ---------------------------------------------------------------------------------------------
// Anfrage: IR → Wire
// ---------------------------------------------------------------------------------------------

/// Serialisiert eine IR-Anfrage als OpenAI-Request-Body.
pub fn encode_request(req: ChatRequest, losses: &mut Losses) -> Result<Vec<u8>, ApiError> {
    let wire = request_to_wire(req, losses)?;
    serde_json::to_vec(&wire).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// Übersetzt eine IR-Anfrage in den OpenAI-Wire-Typ.
pub fn request_to_wire(
    req: ChatRequest,
    losses: &mut Losses,
) -> Result<ChatCompletionRequest, ApiError> {
    let k = req.keeps_extra_for(FMT);
    let mut extra = keep(req.extra, k, losses, "Anfrage");

    let mut messages = Vec::with_capacity(req.messages.len());
    for m in req.messages {
        message_to_wire(m, k, losses, &mut messages);
    }

    let tools: Vec<Map<String, Value>> = req
        .tools
        .into_iter()
        .filter_map(|t| tool_to_wire(t, k, losses))
        .collect();

    let tool_choice = req.tool_choice.map(|tc| match tc {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({"type": "function", "function": {"name": name}}),
    });
    let tool_choice = tool_choice.or_else(|| extra.remove("tool_choice"));

    let response_format = req
        .response_format
        .map(|rf| match rf {
            ResponseFormat::Text => json!({"type": "text"}),
            ResponseFormat::JsonObject => json!({"type": "json_object"}),
            ResponseFormat::JsonSchema {
                name,
                description,
                schema,
                strict,
            } => {
                let mut js = Map::new();
                js.insert(
                    "name".into(),
                    Value::String(name.unwrap_or_else(|| "response".to_owned())),
                );
                if let Some(d) = description {
                    js.insert("description".into(), Value::String(d));
                }
                js.insert("schema".into(), schema);
                if let Some(s) = strict {
                    js.insert("strict".into(), Value::Bool(s));
                }
                json!({"type": "json_schema", "json_schema": js})
            }
        })
        .or_else(|| extra.remove("response_format"));

    let (max_tokens, max_completion_tokens) = if req.hints.legacy_max_tokens && k {
        (req.max_tokens, None)
    } else {
        (
            extra
                .remove("max_tokens")
                .and_then(|v| v.as_u64())
                .filter(|_| k),
            req.max_tokens,
        )
    };

    let stop = match req.stop.len() {
        0 => None,
        1 if req.hints.stop_as_string && k => req.stop.into_iter().next().map(Stop::One),
        _ => Some(Stop::Many(req.stop)),
    };

    let reasoning_effort = req.reasoning.and_then(|r| {
        if r.thinking.is_some() {
            losses.note("thinking-Konfiguration hat in OpenAI keine Entsprechung");
        }
        r.effort.map(|e| match e.as_str() {
            "max" => "high".to_owned(),
            _ => e,
        })
    });

    if req.sampling.top_k.is_some() {
        losses.note("top_k wird von OpenAI nicht unterstützt");
    }

    let stream_options = req.stream_options.map(|so| {
        let mut m = keep(so.extra, k, losses, "stream_options");
        if let Some(iu) = so.include_usage {
            m.insert("include_usage".into(), Value::Bool(iu));
        }
        m
    });

    Ok(ChatCompletionRequest {
        model: req.model,
        messages,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        parallel_tool_calls: req.parallel_tool_calls,
        temperature: req.sampling.temperature,
        top_p: req.sampling.top_p,
        frequency_penalty: req.sampling.frequency_penalty,
        presence_penalty: req.sampling.presence_penalty,
        seed: req.sampling.seed,
        max_tokens,
        max_completion_tokens,
        stop,
        response_format,
        reasoning_effort,
        n: req.n,
        user: req.user,
        metadata: req.metadata,
        stream: req.stream,
        stream_options,
        extra,
    })
}

fn role_to_wire(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn new_message(role: &str, content: Option<TextOrParts<Map<String, Value>>>) -> ChatMessage {
    ChatMessage {
        role: role.to_owned(),
        content,
        name: None,
        tool_calls: None,
        tool_call_id: None,
        refusal: None,
        extra: Map::new(),
    }
}

fn message_to_wire(m: Message, k: bool, losses: &mut Losses, out: &mut Vec<ChatMessage>) {
    let mut extra = keep(m.extra, k, losses, "Nachricht");
    let mut name = m.name;
    match (m.role, m.content) {
        (Role::Assistant, Content::Text(t)) => {
            let mut msg = new_message("assistant", Some(TextOrParts::Text(t)));
            msg.name = name;
            msg.extra = extra;
            out.push(msg);
        }
        (Role::Assistant, Content::Parts(parts)) => {
            let mut content = Vec::new();
            let mut refusal: Option<String> = None;
            let mut tool_calls = Vec::new();
            for p in parts {
                match p.kind {
                    PartKind::ToolCall {
                        id,
                        name,
                        arguments,
                    } => tool_calls.push(ToolCall {
                        id,
                        kind: "function".to_owned(),
                        function: FunctionCall { name, arguments },
                        extra: keep(p.extra, k, losses, "tool_calls[]"),
                    }),
                    PartKind::Refusal { text } => match &mut refusal {
                        Some(r) => r.push_str(&text),
                        None => refusal = Some(text),
                    },
                    PartKind::Reasoning { .. } | PartKind::RedactedReasoning { .. } => {
                        losses.note("Reasoning-Blöcke in Assistant-Nachrichten verworfen");
                    }
                    kind => {
                        if let Some(w) = part_to_wire(Part { kind, ..p }, k, losses) {
                            content.push(w);
                        }
                    }
                }
            }
            let content = if tool_calls.is_empty() && refusal.is_none() {
                if content.is_empty() {
                    None
                } else {
                    Some(TextOrParts::Parts(content))
                }
            } else {
                match content.len() {
                    0 => None,
                    1 if is_plain_text(&content[0]) => {
                        content.pop().and_then(|mut m| match m.remove("text") {
                            Some(Value::String(s)) => Some(TextOrParts::Text(s)),
                            _ => None,
                        })
                    }
                    _ => Some(TextOrParts::Parts(content)),
                }
            };
            let mut msg = new_message("assistant", content);
            msg.name = name;
            msg.refusal = refusal;
            msg.tool_calls = (!tool_calls.is_empty()).then_some(tool_calls);
            msg.extra = extra;
            out.push(msg);
        }
        (role, Content::Text(t)) => {
            // Role::Tool mit reinem Text kommt aus der IR nicht vor; als Nutzertext behandeln.
            let role = if role == Role::Tool {
                "user"
            } else {
                role_to_wire(role)
            };
            let mut msg = new_message(role, Some(TextOrParts::Text(t)));
            msg.name = name;
            msg.extra = extra;
            out.push(msg);
        }
        (role, Content::Parts(parts)) => {
            // Tool-Ergebnisse werden zu eigenen `tool`-Nachrichten (vor dem Rest).
            let mut rest = Vec::new();
            let mut had_results = false;
            for p in parts {
                match p.kind {
                    PartKind::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } => {
                        had_results = true;
                        if is_error == Some(true) {
                            losses.note("is_error eines Tool-Ergebnisses verworfen");
                        }
                        let content = tool_result_content_to_wire(content, k, losses);
                        let mut msg = new_message("tool", Some(content));
                        msg.tool_call_id = Some(tool_call_id);
                        msg.name = name.take();
                        msg.extra = std::mem::take(&mut extra);
                        let pe = keep(p.extra, k, losses, "Tool-Ergebnis");
                        msg.extra.extend(pe);
                        out.push(msg);
                    }
                    kind => {
                        if let Some(w) = part_to_wire(Part { kind, ..p }, k, losses) {
                            rest.push(w);
                        }
                    }
                }
            }
            if !rest.is_empty() || !had_results {
                let role = if role == Role::Tool {
                    "user"
                } else {
                    role_to_wire(role)
                };
                let mut msg = new_message(role, Some(TextOrParts::Parts(rest)));
                msg.name = name;
                msg.extra = extra;
                out.push(msg);
            }
        }
    }
}

fn is_plain_text(m: &Map<String, Value>) -> bool {
    m.len() == 2 && m.get("type").and_then(Value::as_str) == Some("text") && m.contains_key("text")
}

fn tool_result_content_to_wire(
    content: Content,
    k: bool,
    losses: &mut Losses,
) -> TextOrParts<Map<String, Value>> {
    match content {
        Content::Text(t) => TextOrParts::Text(t),
        Content::Parts(parts) => {
            let mut out = Vec::new();
            for p in parts {
                if matches!(p.kind, PartKind::Text { .. }) || k {
                    if let Some(w) = part_to_wire(p, k, losses) {
                        out.push(w);
                    }
                } else {
                    losses.note("Nicht-Text-Inhalt in Tool-Ergebnis verworfen");
                }
            }
            TextOrParts::Parts(out)
        }
    }
}

fn data_url(media_type: Option<&str>, data: &str) -> String {
    let media = media_type.unwrap_or("");
    let mut s = String::with_capacity(media.len() + data.len() + 13);
    s.push_str("data:");
    s.push_str(media);
    s.push_str(";base64,");
    s.push_str(data);
    s
}

fn part_to_wire(p: Part, k: bool, losses: &mut Losses) -> Option<Map<String, Value>> {
    if p.cache_control.is_some() {
        losses.note("cache_control wird von OpenAI nicht unterstützt");
    }
    let mut m = keep(p.extra, k, losses, "Inhaltsteil");
    match p.kind {
        PartKind::Text { text } => {
            m.insert("type".into(), json!("text"));
            m.insert("text".into(), Value::String(text));
        }
        PartKind::Refusal { text } => {
            m.insert("type".into(), json!("refusal"));
            m.insert("refusal".into(), Value::String(text));
        }
        PartKind::Image { source, detail } => {
            let url = match source {
                MediaSource::Url { url } => url,
                MediaSource::Base64 { media_type, data } => data_url(media_type.as_deref(), &data),
                _ => {
                    losses.note("Bildquelle (file_id/Text) in OpenAI nicht abbildbar");
                    return None;
                }
            };
            let mut iu = Map::new();
            iu.insert("url".into(), Value::String(url));
            if let Some(d) = detail {
                iu.insert("detail".into(), Value::String(d));
            }
            m.insert("type".into(), json!("image_url"));
            m.insert("image_url".into(), Value::Object(iu));
        }
        PartKind::Audio { data, format } => {
            m.insert("type".into(), json!("input_audio"));
            m.insert(
                "input_audio".into(),
                json!({"data": data, "format": format}),
            );
        }
        PartKind::File { source, filename } => {
            let mut f = Map::new();
            match source {
                MediaSource::FileId { file_id } => {
                    f.insert("file_id".into(), Value::String(file_id));
                }
                MediaSource::Base64 {
                    media_type: Some(mt),
                    data,
                } => {
                    f.insert(
                        "file_data".into(),
                        Value::String(data_url(Some(&mt), &data)),
                    );
                }
                MediaSource::Base64 {
                    media_type: None,
                    data,
                } => {
                    f.insert("file_data".into(), Value::String(data));
                }
                MediaSource::Text { data, .. } => {
                    // Klartext-Dokument → Textteil.
                    m.insert("type".into(), json!("text"));
                    m.insert("text".into(), Value::String(data));
                    return Some(m);
                }
                MediaSource::Url { .. } | MediaSource::Other { .. } => {
                    losses.note("Dokumentquelle (URL/sonstige) in OpenAI nicht abbildbar");
                    return None;
                }
            }
            if let Some(n) = filename {
                f.insert("filename".into(), Value::String(n));
            }
            m.insert("type".into(), json!("file"));
            m.insert("file".into(), Value::Object(f));
        }
        PartKind::Opaque { format, value } if format == FMT && k => {
            return value.as_object().cloned();
        }
        PartKind::Opaque { .. } => {
            losses.note("unbekannter Inhaltsteil eines anderen Formats verworfen");
            return None;
        }
        PartKind::ToolCall { .. }
        | PartKind::ToolResult { .. }
        | PartKind::Reasoning { .. }
        | PartKind::RedactedReasoning { .. } => {
            losses.note("Tool-/Reasoning-Teil an unerwarteter Stelle verworfen");
            return None;
        }
    }
    Some(m)
}

fn tool_to_wire(t: Tool, k: bool, losses: &mut Losses) -> Option<Map<String, Value>> {
    if t.cache_control.is_some() {
        losses.note("cache_control an Tools wird von OpenAI nicht unterstützt");
    }
    match t.kind {
        ToolKind::Function(f) => {
            let mut func = keep(f.extra, k, losses, "Tool-Funktion");
            func.insert("name".into(), Value::String(f.name));
            if let Some(d) = f.description {
                func.insert("description".into(), Value::String(d));
            }
            if let Some(p) = f.parameters {
                func.insert("parameters".into(), p);
            }
            if let Some(s) = f.strict {
                func.insert("strict".into(), Value::Bool(s));
            }
            let mut m = keep(t.extra, k, losses, "Tool");
            m.insert("type".into(), json!("function"));
            m.insert("function".into(), Value::Object(func));
            Some(m)
        }
        ToolKind::Opaque { format, value } if format == FMT && k => value.as_object().cloned(),
        ToolKind::Opaque { .. } => {
            losses.note("anbieterspezifisches Tool eines anderen Formats verworfen");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Antwort
// ---------------------------------------------------------------------------------------------

/// Parst einen Response-Body in die IR.
pub fn decode_response(body: &[u8]) -> Result<ChatResponse, ApiError> {
    let wire: ChatCompletion = serde_json::from_slice(body)?;
    Ok(response_from_wire(wire))
}

/// Übersetzt eine OpenAI-Antwort in die IR.
pub fn response_from_wire(w: ChatCompletion) -> ChatResponse {
    let choices = w
        .choices
        .into_iter()
        .map(|c| {
            let m = c.message;
            let content = if m.tool_calls.is_none() && m.refusal.is_none() {
                match m.content {
                    Some(t) => Content::Text(t),
                    None => Content::Parts(Vec::new()),
                }
            } else {
                let mut parts = Vec::new();
                if let Some(t) = m.content {
                    parts.push(Part::text(t));
                }
                if let Some(r) = m.refusal {
                    parts.push(Part::new(PartKind::Refusal { text: r }));
                }
                for tc in m.tool_calls.unwrap_or_default() {
                    parts.push(tool_call_from_wire(tc));
                }
                Content::Parts(parts)
            };
            Choice {
                index: c.index,
                message: Message {
                    role: Role::Assistant,
                    content,
                    name: None,
                    extra: m.extra,
                },
                stop_reason: c.finish_reason.map(|r| stop_reason_from_wire(&r)),
                stop_sequence: None,
                extra: c.extra,
            }
        })
        .collect();
    ChatResponse {
        id: w.id,
        model: w.model,
        created: Some(w.created),
        choices,
        usage: w.usage.map(usage_from_wire),
        extra: w.extra,
        source: Some(FMT),
    }
}

/// OpenAI `finish_reason` → IR.
pub(crate) fn stop_reason_from_wire(r: &str) -> StopReason {
    match r {
        "stop" => StopReason::EndTurn,
        "length" => StopReason::MaxTokens,
        "tool_calls" => StopReason::ToolUse,
        "content_filter" => StopReason::ContentFilter,
        other => StopReason::Other(other.to_owned()),
    }
}

/// IR → OpenAI `finish_reason`.
pub(crate) fn stop_reason_to_wire(r: &StopReason) -> String {
    match r {
        StopReason::EndTurn | StopReason::StopSequence | StopReason::PauseTurn => "stop",
        StopReason::MaxTokens => "length",
        StopReason::ToolUse => "tool_calls",
        StopReason::ContentFilter | StopReason::Refusal => "content_filter",
        StopReason::Other(s) => s,
    }
    .to_owned()
}

/// OpenAI-Usage → IR.
pub(crate) fn usage_from_wire(u: WireUsage) -> Usage {
    let mut extra = u.extra;
    let mut cache_read = None;
    if let Some(mut d) = u.prompt_tokens_details {
        cache_read = d.remove("cached_tokens").and_then(|v| v.as_u64());
        if !d.is_empty() || cache_read.is_none() {
            extra.insert("prompt_tokens_details".into(), Value::Object(d));
        }
    }
    let mut reasoning = None;
    if let Some(mut d) = u.completion_tokens_details {
        reasoning = d.remove("reasoning_tokens").and_then(|v| v.as_u64());
        if !d.is_empty() || reasoning.is_none() {
            extra.insert("completion_tokens_details".into(), Value::Object(d));
        }
    }
    Usage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        cache_read_tokens: cache_read,
        cache_write_tokens: None,
        reasoning_tokens: reasoning,
        images: None,
        audio_ms: None,
        characters: None,
        extra,
    }
}

/// IR-Usage → OpenAI.
pub(crate) fn usage_to_wire(u: Usage, k: bool, losses: &mut Losses) -> WireUsage {
    let mut extra = keep(u.extra, k, losses, "usage");
    let mut ptd = match extra.remove("prompt_tokens_details") {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    if let Some(c) = u.cache_read_tokens {
        ptd.get_or_insert_with(Map::new)
            .insert("cached_tokens".into(), Value::from(c));
    }
    let mut ctd = match extra.remove("completion_tokens_details") {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    };
    if let Some(r) = u.reasoning_tokens {
        ctd.get_or_insert_with(Map::new)
            .insert("reasoning_tokens".into(), Value::from(r));
    }
    if u.cache_write_tokens.is_some_and(|c| c > 0) {
        losses.note("cache_write_tokens hat in OpenAI keine Entsprechung");
    }
    WireUsage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        total_tokens: u.input_tokens.saturating_add(u.output_tokens),
        prompt_tokens_details: ptd,
        completion_tokens_details: ctd,
        extra,
    }
}

/// Serialisiert eine IR-Antwort als OpenAI-Response-Body (Reasoning als
/// `reasoning_content`, F10).
pub fn encode_response(resp: ChatResponse, losses: &mut Losses) -> Result<Vec<u8>, ApiError> {
    encode_response_with(resp, true, losses)
}

/// Wie [`encode_response`]; `expose_reasoning = false` verwirft Reasoning-Text
/// (`settings.exposeReasoning`, F10).
pub fn encode_response_with(
    resp: ChatResponse,
    expose_reasoning: bool,
    losses: &mut Losses,
) -> Result<Vec<u8>, ApiError> {
    let wire = response_to_wire_with(resp, expose_reasoning, losses);
    serde_json::to_vec(&wire).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// Übersetzt eine IR-Antwort in den OpenAI-Wire-Typ (Reasoning als `reasoning_content`).
pub fn response_to_wire(resp: ChatResponse, losses: &mut Losses) -> ChatCompletion {
    response_to_wire_with(resp, true, losses)
}

/// Übersetzt eine IR-Antwort in den OpenAI-Wire-Typ.
///
/// Reasoning-Text (`PartKind::Reasoning`) geht – wie bei DeepSeek, vLLM und anderen
/// kompatiblen Servern – als `message.reasoning_content` an den Client, wenn
/// `expose_reasoning` gesetzt ist (Standard, F10). Signaturen und verschlüsseltes Reasoning
/// haben in Chat Completions keine Entsprechung und werden verworfen.
pub fn response_to_wire_with(
    resp: ChatResponse,
    expose_reasoning: bool,
    losses: &mut Losses,
) -> ChatCompletion {
    let k = resp.source == Some(FMT);
    let choices = resp
        .choices
        .into_iter()
        .map(|c| {
            let mut text: Option<String> = None;
            let mut refusal: Option<String> = None;
            let mut reasoning: Option<String> = None;
            let mut tool_calls = Vec::new();
            for p in c.message.content.into_parts() {
                match p.kind {
                    PartKind::Text { text: t } => match &mut text {
                        Some(s) => s.push_str(&t),
                        None => text = Some(t),
                    },
                    PartKind::Refusal { text: t } => match &mut refusal {
                        Some(s) => s.push_str(&t),
                        None => refusal = Some(t),
                    },
                    PartKind::ToolCall {
                        id,
                        name,
                        arguments,
                    } => tool_calls.push(ToolCall {
                        id,
                        kind: "function".to_owned(),
                        function: FunctionCall { name, arguments },
                        extra: keep(p.extra, k, losses, "tool_calls[]"),
                    }),
                    PartKind::Reasoning { text: t, .. } if expose_reasoning => match &mut reasoning
                    {
                        Some(s) => s.push_str(&t),
                        None => reasoning = Some(t),
                    },
                    PartKind::Reasoning { .. } | PartKind::RedactedReasoning { .. } => {}
                    _ => losses.note("Antwortteil (Medien) in OpenAI nicht abbildbar"),
                }
            }
            let mut extra = keep(c.message.extra, k, losses, "message");
            // Vom Anbieter durchgereichte Felder nicht doppelt bzw. trotz Abschaltung ausgeben.
            extra.remove("reasoning_content");
            if matches!(extra.get("reasoning"), Some(Value::String(_))) {
                extra.remove("reasoning");
            }
            if let Some(r) = reasoning {
                extra.insert("reasoning_content".into(), Value::String(r));
            }
            // Wie OpenAI selbst (z. B. Reasoning-Modelle bei `finish_reason: length`, wenn das
            // Thinking alle Tokens verbraucht hat): ohne Text, Ablehnung und Tool-Aufrufe ist
            // `content` ein leerer String, nicht `null` (AIGW-86).
            if text.is_none() && refusal.is_none() && tool_calls.is_empty() {
                text = Some(String::new());
            }
            CompletionChoice {
                index: c.index,
                message: ResponseMessage {
                    role: "assistant".to_owned(),
                    content: text,
                    refusal,
                    tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                    extra,
                },
                finish_reason: c.stop_reason.as_ref().map(stop_reason_to_wire),
                extra: keep(c.extra, k, losses, "choices[]"),
            }
        })
        .collect();
    ChatCompletion {
        id: resp.id,
        object: "chat.completion".to_owned(),
        created: resp.created.unwrap_or_else(crate::wire::unix_now),
        model: resp.model,
        choices,
        usage: resp.usage.map(|u| usage_to_wire(u, k, losses)),
        extra: keep(resp.extra, k, losses, "Antwort"),
    }
}

// ---------------------------------------------------------------------------------------------
// Modelle und Fehler
// ---------------------------------------------------------------------------------------------

/// `GET /v1/models` im OpenAI-Format.
pub fn encode_models(models: &[ModelInfo]) -> ModelList {
    ModelList {
        object: "list".to_owned(),
        data: models
            .iter()
            .map(|m| Model {
                id: m.id.clone(),
                object: "model".to_owned(),
                created: m.created,
                owned_by: m.owned_by.clone(),
            })
            .collect(),
    }
}

/// OpenAI-`type` zu einer Fehlerklasse.
pub fn error_type(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest
        | ErrorKind::NotFound
        | ErrorKind::MethodNotAllowed
        | ErrorKind::RequestTooLarge => "invalid_request_error",
        ErrorKind::Authentication => "authentication_error",
        ErrorKind::PermissionDenied => "permission_error",
        ErrorKind::RateLimit => "rate_limit_error",
        ErrorKind::Timeout => "timeout_error",
        ErrorKind::NotImplemented => "not_implemented",
        ErrorKind::Internal | ErrorKind::BadGateway | ErrorKind::Overloaded => "server_error",
    }
}

fn default_code(kind: ErrorKind) -> Option<&'static str> {
    Some(match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::MethodNotAllowed => "method_not_allowed",
        ErrorKind::RequestTooLarge => "request_too_large",
        ErrorKind::Authentication => "invalid_api_key",
        ErrorKind::RateLimit => "rate_limit_exceeded",
        ErrorKind::Overloaded => "overloaded",
        ErrorKind::NotImplemented => "not_implemented",
        _ => return None,
    })
}

/// Fehler als OpenAI-Fehlerobjekt.
pub fn encode_error(err: &ApiError) -> ErrorBody {
    ErrorBody {
        error: ErrorObject {
            message: err.message.clone(),
            kind: error_type(err.kind).to_owned(),
            param: err.param.clone(),
            code: err
                .code
                .clone()
                .or_else(|| default_code(err.kind).map(str::to_owned)),
        },
    }
}

/// Parst einen OpenAI-Fehler-Body (Upstream); die Klasse folgt dem HTTP-Status.
pub fn decode_error(status: u16, body: &[u8]) -> ApiError {
    let kind = ErrorKind::from_status(status);
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(mut o)) => match o.remove("error") {
            Some(Value::Object(e)) => ApiError {
                kind,
                message: e
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                param: e.get("param").and_then(Value::as_str).map(str::to_owned),
                code: e.get("code").and_then(|c| match c {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                }),
                exact_status: None,
            },
            _ => ApiError::new(kind, String::from_utf8_lossy(body)),
        },
        _ => ApiError::new(kind, String::from_utf8_lossy(body)),
    }
}
