// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Konverter Anthropic Messages ↔ IR.

use serde_json::{Map, Value, json};

use super::{
    AnthropicMessage, ErrorBody, ErrorObject, MessagesRequest, MessagesResponse, Model, ModelList,
    Usage as WireUsage,
};
use crate::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorKind, FunctionTool, MediaSource,
    Message, ModelInfo, Part, PartKind, Reasoning, RequestHints, ResponseFormat, Role, Sampling,
    StopReason, Thinking, Tool, ToolChoice, ToolKind, Usage,
};
use crate::wire::{Losses, TextOrParts, WireFormat, keep, type_of};

const FMT: WireFormat = WireFormat::AnthropicMessages;

/// `max_tokens`, wenn die Anfrage aus einem Format ohne Pflichtfeld kommt (OpenAI).
pub const DEFAULT_MAX_TOKENS: u64 = 4096;

fn take_string(m: &mut Map<String, Value>, key: &str) -> Option<String> {
    match m.remove(key) {
        Some(Value::String(s)) => Some(s),
        Some(Value::Null) | None => None,
        Some(other) => {
            m.insert(key.to_owned(), other);
            None
        }
    }
}

fn require_string(m: &mut Map<String, Value>, key: &str, ctx: &str) -> Result<String, ApiError> {
    take_string(m, key)
        .ok_or_else(|| ApiError::invalid_request(format!("{ctx}: Feld '{key}' fehlt")))
}

// ---------------------------------------------------------------------------------------------
// Anfrage: Wire → IR
// ---------------------------------------------------------------------------------------------

/// Parst einen Request-Body in die IR.
pub fn decode_request(body: &[u8]) -> Result<ChatRequest, ApiError> {
    let wire: MessagesRequest = serde_json::from_slice(body)?;
    request_from_wire(wire)
}

/// Übersetzt eine geparste Anfrage in die IR.
pub fn request_from_wire(w: MessagesRequest) -> Result<ChatRequest, ApiError> {
    let mut extra = w.extra;
    let max_tokens = w.max_tokens.ok_or_else(|| {
        ApiError::invalid_request("max_tokens: Feld fehlt").with_param("max_tokens")
    })?;

    let mut messages = Vec::with_capacity(w.messages.len() + 1);
    if let Some(system) = w.system {
        messages.push(Message {
            role: Role::System,
            content: content_from_wire(system).map_err(|e| e.with_param("system"))?,
            name: None,
            extra: Map::new(),
        });
    }
    for (i, m) in w.messages.into_iter().enumerate() {
        messages.push(message_from_wire(m).map_err(|e| e.with_param(format!("messages[{i}]")))?);
    }

    let mut tools = Vec::new();
    for t in w.tools.unwrap_or_default() {
        tools.push(tool_from_wire(t).map_err(|e| e.with_param("tools"))?);
    }

    let (tool_choice, parallel_tool_calls) = match w.tool_choice {
        None => (None, None),
        Some(tc) => match tool_choice_from_wire(&tc) {
            Some(x) => x,
            None => {
                extra.insert("tool_choice".into(), Value::Object(tc));
                (None, None)
            }
        },
    };

    let mut reasoning = Reasoning::default();
    if let Some(mut t) = w.thinking {
        match take_string(&mut t, "type") {
            Some(mode) => {
                let budget_tokens = match t.remove("budget_tokens") {
                    Some(Value::Number(n)) if n.as_u64().is_some() => n.as_u64(),
                    Some(other) => {
                        t.insert("budget_tokens".into(), other);
                        None
                    }
                    None => None,
                };
                let display = take_string(&mut t, "display");
                reasoning.thinking = Some(Thinking {
                    mode,
                    budget_tokens,
                    display,
                    extra: t,
                });
            }
            None => {
                extra.insert("thinking".into(), Value::Object(t));
            }
        }
    }

    let mut response_format = None;
    if let Some(mut oc) = w.output_config {
        reasoning.effort = take_string(&mut oc, "effort");
        if let Some(f) = oc.remove("format") {
            match response_format_from_wire(f) {
                Ok(rf) => response_format = Some(rf),
                Err(f) => {
                    oc.insert("format".into(), f);
                }
            }
        }
        if !oc.is_empty() {
            extra.insert("output_config".into(), Value::Object(oc));
        }
    }

    let (user, metadata) = match w.metadata {
        None => (None, None),
        Some(mut m) => {
            let user = take_string(&mut m, "user_id");
            (user, (!m.is_empty()).then_some(m))
        }
    };

    Ok(ChatRequest {
        model: w.model,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls,
        sampling: Sampling {
            temperature: w.temperature,
            top_p: w.top_p,
            top_k: w.top_k,
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
        },
        max_tokens: Some(max_tokens),
        stop: w.stop_sequences.unwrap_or_default(),
        response_format,
        reasoning: (reasoning != Reasoning::default()).then_some(reasoning),
        user,
        metadata,
        n: None,
        stream: w.stream,
        stream_options: None,
        hints: RequestHints::default(),
        extra,
        source: Some(FMT),
    })
}

fn message_from_wire(m: AnthropicMessage) -> Result<Message, ApiError> {
    let role = match m.role.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        other => {
            return Err(ApiError::invalid_request(format!(
                "unbekannte Rolle '{other}'"
            )));
        }
    };
    Ok(Message {
        role,
        content: content_from_wire(m.content)?,
        name: None,
        extra: m.extra,
    })
}

fn content_from_wire(c: TextOrParts<Map<String, Value>>) -> Result<Content, ApiError> {
    Ok(match c {
        TextOrParts::Text(t) => Content::Text(t),
        TextOrParts::Parts(blocks) => Content::Parts(
            blocks
                .into_iter()
                .map(part_from_wire)
                .collect::<Result<_, _>>()?,
        ),
    })
}

fn only_keys(m: &Map<String, Value>, keys: &[&str]) -> bool {
    m.keys().all(|k| keys.contains(&k.as_str()))
}

fn source_from_wire(v: Value) -> MediaSource {
    let Value::Object(mut m) = v else {
        return MediaSource::Other { value: v };
    };
    let src = match type_of(&m) {
        Some("base64") if only_keys(&m, &["type", "media_type", "data"]) => {
            match (
                take_string(&mut m, "media_type"),
                take_string(&mut m, "data"),
            ) {
                (mt, Some(data)) => Some(MediaSource::Base64 {
                    media_type: mt,
                    data,
                }),
                _ => None,
            }
        }
        Some("url") if only_keys(&m, &["type", "url"]) => {
            take_string(&mut m, "url").map(|url| MediaSource::Url { url })
        }
        Some("file") if only_keys(&m, &["type", "file_id"]) => {
            take_string(&mut m, "file_id").map(|file_id| MediaSource::FileId { file_id })
        }
        Some("text") if only_keys(&m, &["type", "media_type", "data"]) => {
            match (
                take_string(&mut m, "media_type"),
                take_string(&mut m, "data"),
            ) {
                (Some(media_type), Some(data)) => Some(MediaSource::Text { media_type, data }),
                _ => None,
            }
        }
        _ => None,
    };
    src.unwrap_or(MediaSource::Other {
        value: Value::Object(m),
    })
}

fn part_from_wire(mut m: Map<String, Value>) -> Result<Part, ApiError> {
    let Some(ty) = type_of(&m).map(str::to_owned) else {
        return Err(ApiError::invalid_request("Inhaltsblock ohne 'type'"));
    };
    let known = [
        "text",
        "image",
        "document",
        "tool_use",
        "tool_result",
        "thinking",
        "redacted_thinking",
    ];
    if !known.contains(&ty.as_str()) {
        return Ok(Part::new(PartKind::Opaque {
            format: FMT,
            value: Value::Object(m),
        }));
    }
    m.remove("type");
    let cache_control = m.remove("cache_control").filter(|v| !v.is_null());
    let ctx = format!("Block '{ty}'");
    let kind = match ty.as_str() {
        "text" => PartKind::Text {
            text: require_string(&mut m, "text", &ctx)?,
        },
        "image" => PartKind::Image {
            source: source_from_wire(
                m.remove("source")
                    .ok_or_else(|| ApiError::invalid_request(format!("{ctx}: 'source' fehlt")))?,
            ),
            detail: None,
        },
        "document" => PartKind::File {
            source: source_from_wire(
                m.remove("source")
                    .ok_or_else(|| ApiError::invalid_request(format!("{ctx}: 'source' fehlt")))?,
            ),
            filename: take_string(&mut m, "title"),
        },
        "tool_use" => {
            let id = require_string(&mut m, "id", &ctx)?;
            let name = require_string(&mut m, "name", &ctx)?;
            let input = m.remove("input").unwrap_or_else(|| json!({}));
            PartKind::ToolCall {
                id,
                name,
                arguments: input.to_string(),
            }
        }
        "tool_result" => {
            let tool_call_id = require_string(&mut m, "tool_use_id", &ctx)?;
            let content = match m.remove("content") {
                None | Some(Value::Null) => Content::Parts(Vec::new()),
                Some(Value::String(s)) => Content::Text(s),
                Some(Value::Array(items)) => {
                    let mut parts = Vec::with_capacity(items.len());
                    for it in items {
                        match it {
                            Value::Object(o) => parts.push(part_from_wire(o)?),
                            _ => {
                                return Err(ApiError::invalid_request(format!(
                                    "{ctx}: Inhalt muss aus Blöcken bestehen"
                                )));
                            }
                        }
                    }
                    Content::Parts(parts)
                }
                Some(_) => {
                    return Err(ApiError::invalid_request(format!(
                        "{ctx}: ungültiger Inhalt"
                    )));
                }
            };
            let is_error = match m.remove("is_error") {
                Some(Value::Bool(b)) => Some(b),
                Some(Value::Null) | None => None,
                Some(other) => {
                    m.insert("is_error".into(), other);
                    None
                }
            };
            PartKind::ToolResult {
                tool_call_id,
                content,
                is_error,
            }
        }
        "thinking" => PartKind::Reasoning {
            text: require_string(&mut m, "thinking", &ctx)?,
            signature: take_string(&mut m, "signature"),
        },
        _ => PartKind::RedactedReasoning {
            data: require_string(&mut m, "data", &ctx)?,
        },
    };
    Ok(Part {
        kind,
        cache_control,
        extra: m,
    })
}

fn tool_from_wire(mut m: Map<String, Value>) -> Result<Tool, ApiError> {
    let ty = type_of(&m);
    let is_custom = (ty.is_none() || ty == Some("custom")) && m.contains_key("input_schema");
    if !is_custom {
        return Ok(Tool {
            kind: ToolKind::Opaque {
                format: FMT,
                value: Value::Object(m),
            },
            cache_control: None,
            extra: Map::new(),
        });
    }
    let name = require_string(&mut m, "name", "Tool")?;
    let description = take_string(&mut m, "description");
    let parameters = m.remove("input_schema");
    let strict = match m.remove("strict") {
        Some(Value::Bool(b)) => Some(b),
        Some(Value::Null) | None => None,
        Some(other) => {
            m.insert("strict".into(), other);
            None
        }
    };
    let cache_control = m.remove("cache_control").filter(|v| !v.is_null());
    Ok(Tool {
        kind: ToolKind::Function(FunctionTool {
            name,
            description,
            parameters,
            strict,
            extra: Map::new(),
        }),
        cache_control,
        extra: m,
    })
}

type ToolChoiceIr = (Option<ToolChoice>, Option<bool>);

fn tool_choice_from_wire(m: &Map<String, Value>) -> Option<ToolChoiceIr> {
    let parallel = match m.get("disable_parallel_tool_use") {
        None => None,
        Some(Value::Bool(b)) => Some(!b),
        Some(_) => return None,
    };
    let (tc, keys): (ToolChoice, &[&str]) = match m.get("type")?.as_str()? {
        "auto" => (ToolChoice::Auto, &["type", "disable_parallel_tool_use"]),
        "any" => (ToolChoice::Required, &["type", "disable_parallel_tool_use"]),
        "none" => (ToolChoice::None, &["type"]),
        "tool" => (
            ToolChoice::Tool {
                name: m.get("name")?.as_str()?.to_owned(),
            },
            &["type", "name", "disable_parallel_tool_use"],
        ),
        _ => return None,
    };
    only_keys(m, keys).then_some((Some(tc), parallel))
}

fn response_format_from_wire(v: Value) -> Result<ResponseFormat, Value> {
    match &v {
        Value::Object(o)
            if o.get("type").and_then(Value::as_str) == Some("json_schema")
                && o.contains_key("schema")
                && only_keys(o, &["type", "schema"]) =>
        {
            let schema = o.get("schema").cloned().unwrap_or(Value::Null);
            Ok(ResponseFormat::JsonSchema {
                name: None,
                description: None,
                schema,
                strict: None,
            })
        }
        _ => Err(v),
    }
}

// ---------------------------------------------------------------------------------------------
// Anfrage: IR → Wire
// ---------------------------------------------------------------------------------------------

/// Serialisiert eine IR-Anfrage als Anthropic-Request-Body.
pub fn encode_request(req: ChatRequest, losses: &mut Losses) -> Result<Vec<u8>, ApiError> {
    let wire = request_to_wire(req, losses)?;
    serde_json::to_vec(&wire).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// Übersetzt eine IR-Anfrage in den Anthropic-Wire-Typ.
pub fn request_to_wire(req: ChatRequest, losses: &mut Losses) -> Result<MessagesRequest, ApiError> {
    let k = req.keeps_extra_for(FMT);
    let mut extra = keep(req.extra, k, losses, "Anfrage");

    let (system, messages) = messages_to_wire(req.messages, k, losses)?;

    let mut tools = Vec::new();
    for t in req.tools {
        if let Some(t) = tool_to_wire(t, k, losses) {
            tools.push(t);
        }
    }

    let mut tool_choice = req.tool_choice.map(|tc| {
        let mut m = Map::new();
        match tc {
            ToolChoice::Auto => {
                m.insert("type".into(), json!("auto"));
            }
            ToolChoice::None => {
                m.insert("type".into(), json!("none"));
            }
            ToolChoice::Required => {
                m.insert("type".into(), json!("any"));
            }
            ToolChoice::Tool { name } => {
                m.insert("type".into(), json!("tool"));
                m.insert("name".into(), Value::String(name));
            }
        }
        m
    });
    if let Some(parallel) = req.parallel_tool_calls {
        let tc = tool_choice.get_or_insert_with(|| {
            let mut m = Map::new();
            m.insert("type".into(), json!("auto"));
            m
        });
        if tc.get("type").and_then(Value::as_str) != Some("none") {
            tc.insert("disable_parallel_tool_use".into(), Value::Bool(!parallel));
        }
    }
    if tool_choice.is_none()
        && let Some(Value::Object(tc)) = extra.remove("tool_choice")
    {
        tool_choice = Some(tc);
    }

    let s = req.sampling;
    if s.frequency_penalty.is_some() || s.presence_penalty.is_some() || s.seed.is_some() {
        losses
            .note("frequency_penalty/presence_penalty/seed werden von Anthropic nicht unterstützt");
    }

    let mut output_config = match extra.remove("output_config") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let mut thinking = None;
    if let Some(r) = req.reasoning {
        if let Some(e) = r.effort {
            let effort = match e.as_str() {
                "minimal" | "none" => {
                    losses.note(format!("reasoning_effort '{e}' auf 'low' abgebildet"));
                    "low".to_owned()
                }
                _ => e,
            };
            output_config.insert("effort".into(), Value::String(effort));
        }
        if let Some(t) = r.thinking {
            let mut m = keep(t.extra, k, losses, "thinking");
            m.insert("type".into(), Value::String(t.mode));
            if let Some(b) = t.budget_tokens {
                m.insert("budget_tokens".into(), Value::from(b));
            }
            if let Some(d) = t.display {
                m.insert("display".into(), Value::String(d));
            }
            thinking = Some(m);
        }
    }
    if thinking.is_none()
        && let Some(Value::Object(t)) = extra.remove("thinking")
    {
        thinking = Some(t);
    }
    match req.response_format {
        None | Some(ResponseFormat::Text) => {}
        Some(ResponseFormat::JsonObject) => {
            losses.note("json_object als JSON-Schema {\"type\":\"object\"} abgebildet");
            output_config.insert(
                "format".into(),
                json!({"type": "json_schema", "schema": {"type": "object"}}),
            );
        }
        Some(ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        }) => {
            if name.is_some() || description.is_some() || strict.is_some() {
                losses.note("Name/Beschreibung/strict des JSON-Schemas verworfen");
            }
            output_config.insert(
                "format".into(),
                json!({"type": "json_schema", "schema": schema}),
            );
        }
    }

    let metadata = match (req.user, req.metadata) {
        (None, None) => None,
        (user, meta) => {
            let mut m = match meta {
                Some(m) if k => m,
                Some(m) => {
                    losses.extra("metadata", &m);
                    Map::new()
                }
                None => Map::new(),
            };
            if let Some(u) = user {
                m.insert("user_id".into(), Value::String(u));
            }
            (!m.is_empty() || k).then_some(m)
        }
    };

    if let Some(so) = req.stream_options {
        losses.extra("stream_options", &so.extra);
    }

    let max_tokens = match req.max_tokens {
        Some(m) => m,
        None => {
            losses.note(format!(
                "max_tokens fehlte, Standard {DEFAULT_MAX_TOKENS} gesetzt"
            ));
            DEFAULT_MAX_TOKENS
        }
    };

    Ok(MessagesRequest {
        model: req.model,
        messages,
        max_tokens: Some(max_tokens),
        system,
        tools: (!tools.is_empty()).then_some(tools),
        tool_choice,
        temperature: s.temperature,
        top_p: s.top_p,
        top_k: s.top_k,
        stop_sequences: (!req.stop.is_empty()).then_some(req.stop),
        thinking,
        output_config: (!output_config.is_empty()).then_some(output_config),
        metadata,
        stream: req.stream,
        extra,
    })
}

type SystemAndMessages = (
    Option<TextOrParts<Map<String, Value>>>,
    Vec<AnthropicMessage>,
);

fn messages_to_wire(
    messages: Vec<Message>,
    k: bool,
    losses: &mut Losses,
) -> Result<SystemAndMessages, ApiError> {
    let mut iter = messages.into_iter().peekable();
    let mut leading = Vec::new();
    while let Some(m) = iter.next_if(|m| matches!(m.role, Role::System | Role::Developer)) {
        if m.name.is_some() {
            losses.note("name einer Systemnachricht verworfen");
        }
        losses.extra("Systemnachricht", &m.extra);
        leading.push(m.content);
    }
    let system = match leading.len() {
        0 => None,
        1 => leading
            .pop()
            .map(|c| content_to_wire(c, k, losses))
            .transpose()?,
        _ => {
            let mut blocks = Vec::new();
            for c in leading {
                for p in c.into_parts() {
                    if let Some(b) = part_to_wire(p, k, losses)? {
                        blocks.push(b);
                    }
                }
            }
            Some(TextOrParts::Parts(blocks))
        }
    };

    let mut out: Vec<AnthropicMessage> = Vec::new();
    let mut last_was_tool = false;
    for m in iter {
        if m.name.is_some() {
            losses.note("name einer Nachricht verworfen");
        }
        let extra = keep(m.extra, k, losses, "Nachricht");
        let role = match m.role {
            Role::System | Role::Developer => "system",
            Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
        };
        let content = content_to_wire(m.content, k, losses)?;
        if m.role == Role::Tool
            && last_was_tool
            && let Some(prev) = out.last_mut()
        {
            let mut blocks =
                match std::mem::replace(&mut prev.content, TextOrParts::Parts(Vec::new())) {
                    TextOrParts::Text(t) => vec![text_block(t)],
                    TextOrParts::Parts(p) => p,
                };
            match content {
                TextOrParts::Text(t) => blocks.push(text_block(t)),
                TextOrParts::Parts(p) => blocks.extend(p),
            }
            prev.content = TextOrParts::Parts(blocks);
            prev.extra.extend(extra);
            continue;
        }
        last_was_tool = m.role == Role::Tool;
        out.push(AnthropicMessage {
            role: role.to_owned(),
            content,
            extra,
        });
    }
    Ok((system, out))
}

fn text_block(t: String) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("type".into(), json!("text"));
    m.insert("text".into(), Value::String(t));
    m
}

fn content_to_wire(
    c: Content,
    k: bool,
    losses: &mut Losses,
) -> Result<TextOrParts<Map<String, Value>>, ApiError> {
    Ok(match c {
        Content::Text(t) => TextOrParts::Text(t),
        Content::Parts(parts) => {
            let mut blocks = Vec::with_capacity(parts.len());
            for p in parts {
                if let Some(b) = part_to_wire(p, k, losses)? {
                    blocks.push(b);
                }
            }
            TextOrParts::Parts(blocks)
        }
    })
}

fn source_to_wire(
    s: MediaSource,
    default_media: &str,
    k: bool,
    losses: &mut Losses,
) -> Option<Value> {
    Some(match s {
        MediaSource::Base64 { media_type, data } => json!({
            "type": "base64",
            "media_type": media_type.unwrap_or_else(|| default_media.to_owned()),
            "data": data,
        }),
        MediaSource::Url { url } => json!({"type": "url", "url": url}),
        MediaSource::FileId { file_id } => json!({"type": "file", "file_id": file_id}),
        MediaSource::Text { media_type, data } => {
            json!({"type": "text", "media_type": media_type, "data": data})
        }
        MediaSource::Other { value } if k => value,
        MediaSource::Other { .. } => {
            losses.note("unbekannte Medienquelle verworfen");
            return None;
        }
    })
}

/// Tool-Argumente (JSON-Text) als Objekt für `tool_use.input`.
pub(crate) fn arguments_to_input(arguments: &str) -> Result<Value, ApiError> {
    if arguments.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(arguments).map_err(|e| {
        ApiError::invalid_request(format!("Tool-Argumente sind kein gültiges JSON: {e}"))
    })
}

fn part_to_wire(
    p: Part,
    k: bool,
    losses: &mut Losses,
) -> Result<Option<Map<String, Value>>, ApiError> {
    let mut m = keep(p.extra, k, losses, "Inhaltsteil");
    match p.kind {
        PartKind::Text { text } | PartKind::Refusal { text } => {
            m.insert("type".into(), json!("text"));
            m.insert("text".into(), Value::String(text));
        }
        PartKind::Image { source, detail } => {
            if detail.is_some() {
                losses.note("detail eines Bildes verworfen");
            }
            let Some(src) = source_to_wire(source, "image/png", k, losses) else {
                return Ok(None);
            };
            m.insert("type".into(), json!("image"));
            m.insert("source".into(), src);
        }
        PartKind::File { source, filename } => {
            let Some(src) = source_to_wire(source, "application/pdf", k, losses) else {
                return Ok(None);
            };
            m.insert("type".into(), json!("document"));
            m.insert("source".into(), src);
            if let Some(t) = filename {
                m.insert("title".into(), Value::String(t));
            }
        }
        PartKind::Audio { .. } => {
            losses.note("Audio-Eingaben werden von Anthropic nicht unterstützt");
            return Ok(None);
        }
        PartKind::ToolCall {
            id,
            name,
            arguments,
        } => {
            m.insert("type".into(), json!("tool_use"));
            m.insert("id".into(), Value::String(id));
            m.insert("name".into(), Value::String(name));
            m.insert("input".into(), arguments_to_input(&arguments)?);
        }
        PartKind::ToolResult {
            tool_call_id,
            content,
            is_error,
        } => {
            m.insert("type".into(), json!("tool_result"));
            m.insert("tool_use_id".into(), Value::String(tool_call_id));
            match content_to_wire(content, k, losses)? {
                TextOrParts::Text(t) => {
                    m.insert("content".into(), Value::String(t));
                }
                TextOrParts::Parts(blocks) if blocks.is_empty() => {}
                TextOrParts::Parts(blocks) => {
                    m.insert(
                        "content".into(),
                        Value::Array(blocks.into_iter().map(Value::Object).collect()),
                    );
                }
            }
            if let Some(e) = is_error {
                m.insert("is_error".into(), Value::Bool(e));
            }
        }
        PartKind::Reasoning {
            text,
            signature: Some(sig),
        } => {
            m.insert("type".into(), json!("thinking"));
            m.insert("thinking".into(), Value::String(text));
            m.insert("signature".into(), Value::String(sig));
        }
        PartKind::Reasoning {
            signature: None, ..
        } => {
            losses.note("Reasoning ohne Signatur verworfen (Anthropic verlangt eine Signatur)");
            return Ok(None);
        }
        PartKind::RedactedReasoning { data } => {
            m.insert("type".into(), json!("redacted_thinking"));
            m.insert("data".into(), Value::String(data));
        }
        PartKind::Opaque { format, value } if format == FMT && k => {
            return Ok(value.as_object().cloned());
        }
        PartKind::Opaque { .. } => {
            losses.note("unbekannter Inhaltsteil eines anderen Formats verworfen");
            return Ok(None);
        }
    }
    if let Some(cc) = p.cache_control {
        m.insert("cache_control".into(), cc);
    }
    Ok(Some(m))
}

fn tool_to_wire(t: Tool, k: bool, losses: &mut Losses) -> Option<Map<String, Value>> {
    match t.kind {
        ToolKind::Function(f) => {
            losses.extra("Tool-Funktion", &f.extra);
            let mut m = keep(t.extra, k, losses, "Tool");
            m.insert("name".into(), Value::String(f.name));
            if let Some(d) = f.description {
                m.insert("description".into(), Value::String(d));
            }
            m.insert(
                "input_schema".into(),
                f.parameters
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            );
            if let Some(s) = f.strict {
                m.insert("strict".into(), Value::Bool(s));
            }
            if let Some(cc) = t.cache_control {
                m.insert("cache_control".into(), cc);
            }
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
    let wire: MessagesResponse = serde_json::from_slice(body)?;
    response_from_wire(wire)
}

/// Übersetzt eine Anthropic-Antwort in die IR.
pub fn response_from_wire(w: MessagesResponse) -> Result<ChatResponse, ApiError> {
    let parts = w
        .content
        .into_iter()
        .map(part_from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChatResponse {
        id: w.id,
        model: w.model,
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: Content::Parts(parts),
                name: None,
                extra: Map::new(),
            },
            stop_reason: w.stop_reason.as_deref().map(stop_reason_from_wire),
            stop_sequence: w.stop_sequence,
            extra: Map::new(),
        }],
        usage: Some(usage_from_wire(w.usage)),
        extra: w.extra,
        source: Some(FMT),
    })
}

/// Anthropic `stop_reason` → IR.
pub(crate) fn stop_reason_from_wire(r: &str) -> StopReason {
    match r {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        "tool_use" => StopReason::ToolUse,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        other => StopReason::Other(other.to_owned()),
    }
}

/// IR → Anthropic `stop_reason`.
pub(crate) fn stop_reason_to_wire(r: &StopReason) -> String {
    match r {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::PauseTurn => "pause_turn",
        StopReason::Refusal | StopReason::ContentFilter => "refusal",
        StopReason::Other(s) => s,
    }
    .to_owned()
}

/// Anthropic-Usage → IR (Eingabe inkl. Cache).
pub(crate) fn usage_from_wire(u: WireUsage) -> Usage {
    let input = u
        .input_tokens
        .saturating_add(u.cache_read_input_tokens.unwrap_or(0))
        .saturating_add(u.cache_creation_input_tokens.unwrap_or(0));
    Usage {
        input_tokens: input,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_input_tokens,
        cache_write_tokens: u.cache_creation_input_tokens,
        reasoning_tokens: None,
        images: None,
        audio_ms: None,
        characters: None,
        extra: u.extra,
    }
}

/// IR-Usage → Anthropic.
pub(crate) fn usage_to_wire(u: Usage, k: bool, losses: &mut Losses) -> WireUsage {
    let cached = u
        .cache_read_tokens
        .unwrap_or(0)
        .saturating_add(u.cache_write_tokens.unwrap_or(0));
    WireUsage {
        input_tokens: u.input_tokens.saturating_sub(cached),
        output_tokens: u.output_tokens,
        cache_creation_input_tokens: u.cache_write_tokens,
        cache_read_input_tokens: u.cache_read_tokens,
        extra: keep(u.extra, k, losses, "usage"),
    }
}

/// Serialisiert eine IR-Antwort als Anthropic-Response-Body.
pub fn encode_response(resp: ChatResponse, losses: &mut Losses) -> Result<Vec<u8>, ApiError> {
    let wire = response_to_wire(resp, losses)?;
    serde_json::to_vec(&wire).map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))
}

/// Übersetzt eine IR-Antwort in den Anthropic-Wire-Typ.
pub fn response_to_wire(
    resp: ChatResponse,
    losses: &mut Losses,
) -> Result<MessagesResponse, ApiError> {
    let k = resp.source == Some(FMT);
    if resp.choices.len() > 1 {
        losses.note("weitere Antwortvarianten (n > 1) verworfen");
    }
    let mut content = Vec::new();
    let mut stop_reason = None;
    let mut stop_sequence = None;
    if let Some(c) = resp.choices.into_iter().next() {
        losses.extra("choices[]", &c.extra);
        losses.extra("message", &c.message.extra);
        for p in c.message.content.into_parts() {
            if let Some(b) = part_to_wire(p, k, losses)? {
                content.push(b);
            }
        }
        stop_reason = c.stop_reason.as_ref().map(stop_reason_to_wire);
        stop_sequence = c.stop_sequence;
    }
    Ok(MessagesResponse {
        id: resp.id,
        kind: "message".to_owned(),
        role: "assistant".to_owned(),
        model: resp.model,
        content,
        stop_reason,
        stop_sequence,
        usage: resp
            .usage
            .map(|u| usage_to_wire(u, k, losses))
            .unwrap_or_default(),
        extra: keep(resp.extra, k, losses, "Antwort"),
    })
}

// ---------------------------------------------------------------------------------------------
// Modelle und Fehler
// ---------------------------------------------------------------------------------------------

/// Unix-Sekunden → RFC 3339 (UTC).
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Algorithmus „civil_from_days“ (H. Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `GET /v1/models` im Anthropic-Format.
pub fn encode_models(models: &[ModelInfo]) -> ModelList {
    let data: Vec<Model> = models
        .iter()
        .map(|m| Model {
            kind: "model".to_owned(),
            id: m.id.clone(),
            display_name: m.display_name.clone().unwrap_or_else(|| m.id.clone()),
            created_at: rfc3339(m.created),
        })
        .collect();
    ModelList {
        has_more: false,
        first_id: data.first().map(|m| m.id.clone()),
        last_id: data.last().map(|m| m.id.clone()),
        data,
    }
}

/// Anthropic-`type` zu einer Fehlerklasse.
pub fn error_type(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest | ErrorKind::MethodNotAllowed => "invalid_request_error",
        ErrorKind::Authentication => "authentication_error",
        ErrorKind::PermissionDenied => "permission_error",
        ErrorKind::NotFound => "not_found_error",
        ErrorKind::RequestTooLarge => "request_too_large",
        ErrorKind::RateLimit => "rate_limit_error",
        ErrorKind::Overloaded => "overloaded_error",
        ErrorKind::Timeout => "timeout_error",
        ErrorKind::Internal | ErrorKind::NotImplemented | ErrorKind::BadGateway => "api_error",
    }
}

/// HTTP-Status im Anthropic-Format (Überlastung = 529).
pub fn error_status(kind: ErrorKind) -> u16 {
    match kind {
        ErrorKind::Overloaded => 529,
        k => k.status(),
    }
}

/// Fehler als Anthropic-Fehlerobjekt.
pub fn encode_error(err: &ApiError, request_id: Option<&str>) -> ErrorBody {
    let message = match &err.param {
        Some(p) if !err.message.starts_with(p.as_str()) => format!("{p}: {}", err.message),
        _ => err.message.clone(),
    };
    ErrorBody {
        kind: "error".to_owned(),
        error: ErrorObject {
            kind: error_type(err.kind).to_owned(),
            message,
        },
        request_id: request_id.map(str::to_owned),
    }
}

/// Parst einen Anthropic-Fehler-Body (Upstream); die Klasse folgt dem HTTP-Status.
pub fn decode_error(status: u16, body: &[u8]) -> ApiError {
    let kind = ErrorKind::from_status(status);
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(b) => ApiError::new(kind, b.error.message).with_code(b.error.kind),
        Err(_) => ApiError::new(kind, String::from_utf8_lossy(body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_763_000_000), "2025-11-13T02:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
