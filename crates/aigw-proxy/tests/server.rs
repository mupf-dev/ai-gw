// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Integrationstests der Datenebene mit echtem Server (Port 0) und Mock-Anbieter.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::Arc;

use aigw_core::ir::{ApiError, ErrorKind};
use aigw_core::wire::sse::{SseEvent, SseParser};
use aigw_proxy::{ApiKey, AuthContext, Authenticator, ProxyConfig, ProxyState, REQUEST_ID_HEADER};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

async fn start(state: ProxyState) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    addr
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("kein JSON ({e}): {}", String::from_utf8_lossy(&self.body)))
    }

    fn sse(&self) -> Vec<SseEvent> {
        SseParser::new().push(&self.body)
    }

    fn request_id(&self) -> &str {
        self.headers
            .get(REQUEST_ID_HEADER)
            .expect("x-aigw-request-id fehlt")
            .to_str()
            .unwrap()
    }
}

async fn send(
    addr: SocketAddr,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: impl Into<Bytes>,
) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path}"));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = client
        .request(req.body(Full::new(body.into())).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

async fn post_json(addr: SocketAddr, path: &str, body: Value) -> Resp {
    send(
        addr,
        Method::POST,
        path,
        &[("content-type", "application/json")],
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

fn tool_def_openai() -> Value {
    json!([{"type": "function", "function": {"name": "lookup", "parameters": {"type": "object"}}}])
}

fn tool_def_anthropic() -> Value {
    json!([{"name": "lookup", "input_schema": {"type": "object"}}])
}

// ---------------------------------------------------------------------------------------------
// OpenAI
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn openai_non_streaming() {
    let addr = start(ProxyState::mock()).await;
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": [{"role": "user", "content": "Hallo Welt"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let id = r.request_id();
    assert_eq!(id.len(), 36);
    assert_eq!(&id[14..15], "7", "UUIDv7 erwartet: {id}");
    assert_eq!(r.headers["content-type"], "application/json");
    let v = r.json();
    assert_eq!(v["object"], "chat.completion");
    assert_eq!(v["model"], "mock");
    assert_eq!(v["choices"][0]["message"]["content"], "Echo: Hallo Welt");
    assert_eq!(v["choices"][0]["finish_reason"], "stop");
    assert_eq!(v["usage"]["completion_tokens"], 3);
    assert_eq!(
        v["usage"]["total_tokens"],
        v["usage"]["prompt_tokens"].as_u64().unwrap() + 3
    );
}

#[tokio::test]
async fn openai_streaming_with_usage_and_tool_call() {
    let addr = start(ProxyState::mock()).await;
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({
            "model": "mock",
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": tool_def_openai(),
            "messages": [{"role": "user", "content": "bitte tool nutzen"}]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let frames = r.sse();
    assert_eq!(frames.last().unwrap().data, "[DONE]");
    let chunks: Vec<Value> = frames[..frames.len() - 1]
        .iter()
        .map(|f| serde_json::from_str(&f.data).unwrap())
        .collect();
    assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk")
    );
    let text: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "Echo: bitte tool nutzen");
    let args: String = chunks
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&args).unwrap(),
        json!({"echo": "bitte tool nutzen"})
    );
    assert!(
        chunks
            .iter()
            .any(|c| c["choices"][0]["finish_reason"] == "tool_calls")
    );
    let last = chunks.last().unwrap();
    assert_eq!(last["choices"], json!([]));
    assert!(last["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn openai_streaming_without_usage() {
    let addr = start(ProxyState::mock()).await;
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock", "stream": true, "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&r.body).contains("\"usage\""));
}

// ---------------------------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn anthropic_non_streaming_with_thinking_and_tool() {
    let addr = start(ProxyState::mock()).await;
    let r = post_json(
        addr,
        "/v1/messages",
        json!({
            "model": "mock",
            "max_tokens": 100,
            "thinking": {"type": "adaptive"},
            "tools": tool_def_anthropic(),
            "system": "sei nett",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "tool bitte"}]}]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    let types: Vec<&str> = v["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(types, ["thinking", "text", "tool_use"]);
    assert_eq!(v["content"][0]["signature"], "mock-signature");
    assert_eq!(v["content"][1]["text"], "Echo: tool bitte");
    assert_eq!(v["content"][2]["input"], json!({"echo": "tool bitte"}));
    assert_eq!(v["stop_reason"], "tool_use");
    assert!(v["stop_sequence"].is_null());
    assert!(v["usage"]["input_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn anthropic_streaming() {
    let addr = start(ProxyState::mock()).await;
    let r = post_json(
        addr,
        "/v1/messages",
        json!({
            "model": "mock",
            "max_tokens": 100,
            "stream": true,
            "tools": tool_def_anthropic(),
            "messages": [{"role": "user", "content": "nutze tool"}]
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let frames = r.sse();
    let names: Vec<&str> = frames.iter().map(|f| f.event.as_deref().unwrap()).collect();
    assert_eq!(names[0], "message_start");
    assert_eq!(names[names.len() - 2], "message_delta");
    assert_eq!(names[names.len() - 1], "message_stop");
    // Jede Nutzlast trägt ihren Ereignisnamen als `type`.
    for f in &frames {
        let v: Value = serde_json::from_str(&f.data).unwrap();
        assert_eq!(v["type"], f.event.as_deref().unwrap());
    }
    let datas: Vec<Value> = frames
        .iter()
        .map(|f| serde_json::from_str(&f.data).unwrap())
        .collect();
    assert!(
        datas[0]["message"]["usage"]["input_tokens"]
            .as_u64()
            .unwrap()
            > 0
    );
    let text: String = datas
        .iter()
        .filter_map(|d| d["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "Echo: nutze tool");
    let json_parts: String = datas
        .iter()
        .filter_map(|d| d["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(
        serde_json::from_str::<Value>(&json_parts).unwrap(),
        json!({"echo": "nutze tool"})
    );
    let starts: Vec<&Value> = datas
        .iter()
        .filter(|d| d["type"] == "content_block_start")
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1]["index"], 1);
    assert_eq!(starts[1]["content_block"]["type"], "tool_use");
    let delta = &datas[datas.len() - 2];
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(delta["usage"]["output_tokens"], 3);
}

// ---------------------------------------------------------------------------------------------
// Fehlerformate
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn errors_use_ingress_format() {
    let addr = start(ProxyState::mock()).await;

    // OpenAI: ungültiges JSON.
    let r = send(addr, Method::POST, "/v1/chat/completions", &[], "{kaputt").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v = r.json();
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["error"]["code"], "invalid_json");
    assert!(v["error"].as_object().unwrap().contains_key("param"));
    r.request_id();

    // Anthropic: ungültiges JSON, request_id im Body = Header.
    let r = send(addr, Method::POST, "/v1/messages", &[], "{kaputt").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let v = r.json();
    assert_eq!(v["type"], "error");
    assert_eq!(v["error"]["type"], "invalid_request_error");
    assert_eq!(v["request_id"], r.request_id());

    // Anthropic: max_tokens fehlt.
    let r = post_json(
        addr,
        "/v1/messages",
        json!({"model": "mock", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("max_tokens")
    );

    // Leere Nachrichtenliste.
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": []}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["error"]["param"], "messages");

    // Anbieterfehler: OpenAI 503, Anthropic 529.
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock-error", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.json()["error"]["type"], "server_error");
    let r = post_json(
        addr,
        "/v1/messages",
        json!({"model": "mock-error", "max_tokens": 5, "stream": true,
               "messages": [{"role": "user", "content": "x"}]}),
    )
    .await;
    assert_eq!(r.status.as_u16(), 529);
    assert_eq!(r.json()["error"]["type"], "overloaded_error");

    // Unbekannte Pfade und Methoden.
    let r = send(addr, Method::POST, "/v1/embeddingz", &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"]["code"], "not_found");
    let r = send(addr, Method::POST, "/v1/messages/count_tokens", &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json()["error"]["type"], "not_found_error");
    let r = send(addr, Method::GET, "/v1/chat/completions", &[], "").await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.headers["allow"], "POST");
    assert_eq!(r.json()["error"]["code"], "method_not_allowed");
    let r = send(addr, Method::GET, "/v1/messages", &[], "").await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.json()["type"], "error");
}

#[tokio::test]
async fn mid_stream_errors_are_sse_events() {
    let addr = start(ProxyState::mock()).await;
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock-stream-error", "stream": true,
               "messages": [{"role": "user", "content": "a b c"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let frames = r.sse();
    let last: Value = serde_json::from_str(&frames.last().unwrap().data).unwrap();
    assert_eq!(last["error"]["type"], "server_error");

    let r = post_json(
        addr,
        "/v1/messages",
        json!({"model": "mock-stream-error", "max_tokens": 5, "stream": true,
               "messages": [{"role": "user", "content": "a b c"}]}),
    )
    .await;
    let frames = r.sse();
    let last = frames.last().unwrap();
    assert_eq!(last.event.as_deref(), Some("error"));
    assert!(
        !frames
            .iter()
            .any(|f| f.event.as_deref() == Some("message_stop"))
    );
}

#[tokio::test]
async fn body_limit_is_enforced_in_ingress_format() {
    let state = ProxyState {
        config: ProxyConfig {
            max_body_bytes: 1024,
            ..ProxyConfig::default()
        },
        ..ProxyState::mock()
    };
    let addr = start(state).await;
    let big = "x".repeat(2048);
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": [{"role": "user", "content": big}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.json()["error"]["code"], "request_too_large");
    let r = post_json(
        addr,
        "/v1/messages",
        json!({"model": "mock", "max_tokens": 5, "messages": [{"role": "user", "content": big}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.json()["error"]["type"], "request_too_large");
    // Kleine Anfragen gehen weiterhin durch.
    let r = post_json(
        addr,
        "/v1/chat/completions",
        json!({"model": "mock", "messages": [{"role": "user", "content": "klein"}]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------------------------
// Modelle, Auth, Health
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn models_in_both_formats() {
    let addr = start(ProxyState::mock()).await;
    let r = send(addr, Method::GET, "/v1/models", &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["object"], "list");
    assert_eq!(v["data"][0]["id"], "mock");
    assert_eq!(v["data"][0]["object"], "model");

    let r = send(
        addr,
        Method::GET,
        "/v1/models",
        &[("anthropic-version", "2023-06-01")],
        "",
    )
    .await;
    let v = r.json();
    assert_eq!(v["data"][0]["type"], "model");
    assert_eq!(v["data"][0]["created_at"], "2025-11-13T02:13:20Z");
    assert_eq!(v["has_more"], false);
    assert_eq!(v["first_id"], "mock");
}

#[derive(Debug)]
struct RequireKey;

impl Authenticator for RequireKey {
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        match key {
            Some(k) if k.secret == "sk-test" => Ok(AuthContext {
                key_id: Some("test".into()),
                ..AuthContext::default()
            }),
            Some(_) => Err(ApiError::new(
                ErrorKind::Authentication,
                "Schlüssel ungültig",
            )),
            None => Err(ApiError::new(ErrorKind::Authentication, "Schlüssel fehlt")),
        }
    }
}

#[tokio::test]
async fn auth_stub_is_consulted_with_all_header_variants() {
    let state = ProxyState {
        auth: Arc::new(RequireKey),
        ..ProxyState::mock()
    };
    let addr = start(state).await;
    let body =
        json!({"model": "mock", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]});
    let body = serde_json::to_vec(&body).unwrap();

    let r = send(
        addr,
        Method::POST,
        "/v1/chat/completions",
        &[],
        body.clone(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"]["type"], "authentication_error");
    let r = send(
        addr,
        Method::POST,
        "/v1/messages",
        &[("x-api-key", "falsch")],
        body.clone(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["error"]["type"], "authentication_error");

    for (h, v) in [
        ("authorization", "Bearer sk-test"),
        ("api-key", "sk-test"),
        ("x-api-key", "sk-test"),
    ] {
        let r = send(
            addr,
            Method::POST,
            "/v1/chat/completions",
            &[(h, v)],
            body.clone(),
        )
        .await;
        assert_eq!(r.status, StatusCode::OK, "{h}");
    }
    let r = send(addr, Method::GET, "/v1/models", &[], "").await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn healthz_has_request_id() {
    let addr = start(ProxyState::mock()).await;
    let r = send(addr, Method::GET, "/healthz", &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(&r.body[..], b"ok");
    r.request_id();
}
