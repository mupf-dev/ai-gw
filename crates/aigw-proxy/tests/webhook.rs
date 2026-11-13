// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `webhook` (AIGW-61) im Hot Path gegen einen lokalen Fake-Webhook-Server: alle
//! Antworttypen in beiden Eingangsformaten, Antwort-Phase im gepufferten Modus,
//! Zeitüberschreitung mit `failMode: open/closed`, Signaturprüfung mit dem Referenz-Verifier,
//! gesperrte Modell-Umleitung, SSRF-Ablehnung.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::config::AdminConfig;
use aigw_core::ir::{ChatResponse, Choice, Message, Role, StopReason};
use aigw_core::upstream::UpstreamPolicy;
use aigw_core::wire::sse::SseParser;
use aigw_plugins::{Chain, Entry, FailMode, PluginChains, PluginFactory, builtin};
use aigw_proxy::ProxyState;
use aigw_webhook::{ClientOptions, WebhookClient, sign};
use axum::extract::{Path, State};
use axum::http::HeaderMap as AxHeaders;
use axum::routing::post as axpost;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

const SECRET: &str = "webhook-test-geheimnis";

// ---------------------------------------------------------------------------------------------
// Fake-Webhook-Server
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Seen {
    calls: Vec<(AxHeaders, Value)>,
    bad_signatures: usize,
}

type Shared = Arc<Mutex<Seen>>;

fn canned(text: &str) -> ChatResponse {
    ChatResponse {
        id: String::new(),
        model: String::new(),
        created: Some(1),
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, text),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: None,
        extra: Default::default(),
        source: None,
    }
}

fn last_user_text(payload: &Value) -> String {
    let req: aigw_core::ir::ChatRequest =
        serde_json::from_value(payload["request"].clone()).unwrap();
    req.last_user_text().unwrap_or_default()
}

async fn hook(
    State(seen): State<Shared>,
    Path(mode): Path<String>,
    headers: AxHeaders,
    body: Bytes,
) -> (StatusCode, String) {
    // Referenz-Verifier wie beim Empfänger.
    let sig = headers
        .get(sign::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let payload: Value = serde_json::from_slice(&body).unwrap();
    {
        let mut s = seen.lock().unwrap();
        if sign::verify(SECRET.as_bytes(), &sig, &body, now, sign::DEFAULT_TOLERANCE).is_err() {
            s.bad_signatures += 1;
        }
        s.calls.push((headers.clone(), payload.clone()));
    }
    let ok = |v: Value| (StatusCode::OK, v.to_string());
    match mode.as_str() {
        "continue" => ok(json!({"action": "continue"})),
        "empty" => (StatusCode::NO_CONTENT, String::new()),
        "request" => {
            let text = last_user_text(&payload);
            if text.contains("verboten") {
                ok(
                    json!({"action": "reject", "status": 403, "message": "Richtlinie",
                          "code": "policy_violation"}),
                )
            } else if text.contains("ping") {
                ok(json!({"action": "respond", "response": canned("pong vom Webhook")}))
            } else if text.contains("umleiten") {
                let mut req = payload["request"].clone();
                req["model"] = json!("anderes-modell");
                ok(json!({"action": "modify", "request": req}))
            } else {
                let mut req = payload["request"].clone();
                req["messages"] = json!([Message::text(Role::User, format!("geändert: {text}"))]);
                // Versuch, Upstream-Header einzuschleusen – muss ignoriert werden.
                req["hints"] = json!({"upstream_headers": [["x-evil", "1"]]});
                ok(json!({"action": "modify", "request": req}))
            }
        }
        "response" => {
            let resp: ChatResponse = serde_json::from_value(payload["response"].clone()).unwrap();
            let text = resp.choices[0].message.content.text();
            if text.contains("verboten") {
                return ok(
                    json!({"action": "reject", "status": 422, "message": "Ausgabe gesperrt"}),
                );
            }
            let mut out = payload["response"].clone();
            out["choices"][0]["message"] =
                serde_json::to_value(Message::text(Role::Assistant, text.to_uppercase())).unwrap();
            ok(json!({"action": "modify", "response": out}))
        }
        "slow" => {
            tokio::time::sleep(Duration::from_millis(600)).await;
            ok(json!({"action": "continue"}))
        }
        "error" => (StatusCode::BAD_GATEWAY, "kaputt".into()),
        "garbage" => ok(json!({"action": "explode"})),
        _ => (StatusCode::NOT_FOUND, String::new()),
    }
}

async fn fake_webhook() -> (SocketAddr, Shared) {
    let seen = Shared::default();
    let app = axum::Router::new()
        .route("/hook/{mode}", axpost(hook))
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, seen)
}

// ---------------------------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------------------------

fn local_policy() -> UpstreamPolicy {
    UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["127.0.0.1".into()],
        allow_private_upstreams: vec!["127.0.0.1".into()],
        ..AdminConfig::default()
    })
    .unwrap()
}

fn factory() -> builtin::webhook::Factory {
    // Geheimnis aus einer Datei (Umgebungsvariablen setzen bräuchte `unsafe`).
    builtin::webhook::Factory::new(
        WebhookClient::new(local_policy(), ClientOptions::default()).unwrap(),
        None,
    )
}

fn secret_file() -> String {
    let dir = std::env::temp_dir().join(format!("aigw-webhook-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("secret");
    std::fs::write(&p, format!("{SECRET}\n")).unwrap();
    p.display().to_string()
}

fn webhook_entry(
    addr: SocketAddr,
    mode: &str,
    phases: &[&str],
    fail: FailMode,
    timeout: Duration,
) -> Arc<Entry> {
    let plugin = factory()
        .build(&json!({
            "url": format!("http://{addr}/hook/{mode}"),
            "phases": phases,
            "secret": {"file": secret_file()},
            "includeHeaders": ["x-client-app"],
        }))
        .unwrap();
    Arc::new(
        Entry::new(format!("wh-{mode}"), plugin)
            .with_fail_mode(fail)
            .with_timeout(timeout),
    )
}

async fn gateway(entries: Vec<Arc<Entry>>) -> SocketAddr {
    let state = ProxyState {
        plugins: Arc::new(PluginChains::fixed(Chain::new(entries))),
        ..ProxyState::mock()
    };
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

    fn sse_text(&self) -> String {
        let mut out = String::new();
        for f in SseParser::new().push(&self.body) {
            let Ok(v) = serde_json::from_str::<Value>(&f.data) else {
                continue;
            };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                out.push_str(t);
            }
            if v["type"] == "content_block_delta"
                && let Some(t) = v["delta"]["text"].as_str()
            {
                out.push_str(t);
            }
        }
        out
    }

    /// Antworttext (OpenAI oder Anthropic, JSON).
    fn text(&self) -> String {
        let v = self.json();
        v["choices"][0]["message"]["content"]
            .as_str()
            .or_else(|| v["content"][0]["text"].as_str())
            .unwrap_or_default()
            .to_owned()
    }
}

async fn post(addr: SocketAddr, path: &str, body: Value) -> Resp {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-gateway-geheim")
        .header("x-api-key", "sk-gateway-geheim")
        .header("x-client-app", "testsuite")
        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
        .unwrap();
    let res = client.request(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

fn openai(text: &str, stream: bool) -> Value {
    json!({"model": "mock", "stream": stream,
           "messages": [{"role": "user", "content": text}]})
}

fn anthropic(text: &str, stream: bool) -> Value {
    json!({"model": "mock", "stream": stream, "max_tokens": 100,
           "messages": [{"role": "user", "content": text}]})
}

const OAI: &str = "/v1/chat/completions";
const ANT: &str = "/v1/messages";
const T: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn continue_payload_signature_and_no_secrets() {
    let (wh, seen) = fake_webhook().await;
    let gw = gateway(vec![webhook_entry(
        wh,
        "continue",
        &["request"],
        FailMode::Closed,
        T,
    )])
    .await;
    let r = post(gw, OAI, openai("hallo", false)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text(), "Echo: hallo");
    let r = post(gw, ANT, anthropic("hallo", false)).await;
    assert_eq!(r.text(), "Echo: hallo");

    let s = seen.lock().unwrap();
    assert_eq!(s.calls.len(), 2);
    assert_eq!(
        s.bad_signatures, 0,
        "Signatur muss mit dem Referenz-Verifier passen"
    );
    let (h, p) = &s.calls[0];
    assert_eq!(p["version"], 1);
    assert_eq!(p["phase"], "request");
    assert_eq!(p["ingress"], "open-ai-chat");
    assert_eq!(s.calls[1].1["ingress"], "anthropic-messages");
    assert_eq!(p["headers"]["x-client-app"], "testsuite");
    assert_eq!(
        h.get("x-aigw-request-id").unwrap().to_str().unwrap(),
        p["requestId"].as_str().unwrap()
    );
    assert!(h.get("x-aigw-delivery").is_some());
    // Keine Gateway-Keys, weder im Payload noch in den Headern.
    for (h, p) in &s.calls {
        assert!(!p.to_string().contains("sk-gateway-geheim"));
        assert!(h.get("authorization").is_none() && h.get("x-api-key").is_none());
    }
}

#[tokio::test]
async fn empty_reply_is_continue() {
    let (wh, _) = fake_webhook().await;
    let gw = gateway(vec![webhook_entry(
        wh,
        "empty",
        &["request"],
        FailMode::Closed,
        T,
    )])
    .await;
    let r = post(gw, OAI, openai("hallo", false)).await;
    assert_eq!(r.text(), "Echo: hallo");
}

#[tokio::test]
async fn modify_reject_respond_in_both_formats() {
    let (wh, _) = fake_webhook().await;
    let gw = gateway(vec![webhook_entry(
        wh,
        "request",
        &["request"],
        FailMode::Closed,
        T,
    )])
    .await;

    // modify
    let r = post(gw, OAI, openai("hallo", false)).await;
    assert_eq!(r.text(), "Echo: geändert: hallo");
    let r = post(gw, ANT, anthropic("hallo", false)).await;
    assert_eq!(r.text(), "Echo: geändert: hallo");
    let r = post(gw, OAI, openai("hallo", true)).await;
    assert_eq!(r.sse_text(), "Echo: geändert: hallo");
    let r = post(gw, ANT, anthropic("hallo", true)).await;
    assert_eq!(r.sse_text(), "Echo: geändert: hallo");

    // reject
    let r = post(gw, OAI, openai("das ist verboten", false)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["error"]["code"], "policy_violation");
    assert_eq!(r.json()["error"]["message"], "Richtlinie");
    let r = post(gw, ANT, anthropic("das ist verboten", true)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["type"], "error");
    assert_eq!(r.json()["error"]["type"], "permission_error");

    // respond (feste Antwort, kein Upstream)
    let r = post(gw, OAI, openai("ping", false)).await;
    assert_eq!(r.text(), "pong vom Webhook");
    assert!(r.headers.get("x-aigw-deployment").is_none());
    let r = post(gw, ANT, anthropic("ping", false)).await;
    assert_eq!(r.text(), "pong vom Webhook");
    let r = post(gw, OAI, openai("ping", true)).await;
    assert_eq!(r.sse_text(), "pong vom Webhook");
    let r = post(gw, ANT, anthropic("ping", true)).await;
    assert_eq!(r.sse_text(), "pong vom Webhook");
}

#[tokio::test]
async fn model_redirect_is_a_plugin_error() {
    let (wh, _) = fake_webhook().await;
    let gw = gateway(vec![webhook_entry(
        wh,
        "request",
        &["request"],
        FailMode::Closed,
        T,
    )])
    .await;
    let r = post(gw, OAI, openai("bitte umleiten", false)).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"]["code"], "plugin_error");

    let gw = gateway(vec![webhook_entry(
        wh,
        "request",
        &["request"],
        FailMode::Open,
        T,
    )])
    .await;
    let r = post(gw, ANT, anthropic("bitte umleiten", false)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text(), "Echo: bitte umleiten");
}

#[tokio::test]
async fn response_phase_is_buffered() {
    let (wh, seen) = fake_webhook().await;
    let gw = gateway(vec![webhook_entry(
        wh,
        "response",
        &["response"],
        FailMode::Closed,
        T,
    )])
    .await;
    let r = post(gw, OAI, openai("hallo", false)).await;
    assert_eq!(r.text(), "ECHO: HALLO");
    let r = post(gw, ANT, anthropic("hallo", false)).await;
    assert_eq!(r.text(), "ECHO: HALLO");
    let r = post(gw, OAI, openai("hallo", true)).await;
    assert_eq!(r.headers.get("x-aigw-buffered").unwrap(), "plugins");
    assert_eq!(r.sse_text(), "ECHO: HALLO");
    let r = post(gw, ANT, anthropic("hallo", true)).await;
    assert_eq!(r.sse_text(), "ECHO: HALLO");
    // Ausgabe sperren (422 bleibt eine 4xx-Ablehnung).
    let r = post(gw, OAI, openai("verboten", false)).await;
    assert!(r.status.is_client_error(), "{}", r.status);
    let r = post(gw, ANT, anthropic("verboten", true)).await;
    assert!(r.status.is_client_error(), "{}", r.status);
    assert_eq!(r.json()["type"], "error");

    let s = seen.lock().unwrap();
    assert!(s.calls.iter().all(|(_, p)| p["phase"] == "response"));
    assert_eq!(s.bad_signatures, 0);
}

#[tokio::test]
async fn timeout_fail_open_and_closed() {
    let (wh, _) = fake_webhook().await;
    let ms200 = Duration::from_millis(200);
    let closed = gateway(vec![webhook_entry(
        wh,
        "slow",
        &["request"],
        FailMode::Closed,
        ms200,
    )])
    .await;
    let started = std::time::Instant::now();
    let r = post(closed, OAI, openai("hallo", false)).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json()["error"]["code"], "plugin_error");
    assert!(
        started.elapsed() < Duration::from_millis(550),
        "{:?}",
        started.elapsed()
    );
    let r = post(closed, ANT, anthropic("hallo", false)).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);

    let open = gateway(vec![webhook_entry(
        wh,
        "slow",
        &["request"],
        FailMode::Open,
        ms200,
    )])
    .await;
    let r = post(open, OAI, openai("hallo", false)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.text(), "Echo: hallo");
    let r = post(open, ANT, anthropic("hallo", true)).await;
    assert_eq!(r.sse_text(), "Echo: hallo");
}

#[tokio::test]
async fn http_errors_and_invalid_replies_follow_fail_mode() {
    let (wh, _) = fake_webhook().await;
    for mode in ["error", "garbage"] {
        let gw = gateway(vec![webhook_entry(
            wh,
            mode,
            &["request"],
            FailMode::Closed,
            T,
        )])
        .await;
        let r = post(gw, OAI, openai("hallo", false)).await;
        assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{mode}");
        let gw = gateway(vec![webhook_entry(
            wh,
            mode,
            &["request"],
            FailMode::Open,
            T,
        )])
        .await;
        let r = post(gw, OAI, openai("hallo", false)).await;
        assert_eq!(r.text(), "Echo: hallo", "{mode}");
    }
}

#[tokio::test]
async fn ssrf_is_rejected() {
    // Standard-Fabrik (strenge Regeln): lokale/private Ziele, http, Metadaten-Endpunkt.
    let strict = builtin::webhook::Factory::default();
    for url in [
        "http://127.0.0.1:9/hook",
        "https://127.0.0.1/hook",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/hook",
        "https://localhost/hook",
        "https://ollama/hook",
        "https://hooks.internal/hook",
    ] {
        let Err(e) = strict.build(&json!({"url": url})) else {
            panic!("{url} angenommen")
        };
        assert!(e.starts_with("url:"), "{url}: {e}");
    }
    // Auch über die Registry (Admin-API / `aigw config validate`).
    let reg = aigw_plugins::Registry::with_builtin();
    let cfg = json!({"url": "https://10.0.0.1/x", "failMode": "open"});
    assert!(reg.validate("webhook", cfg.as_object().unwrap()).is_err());
    let cfg = json!({"url": "https://hooks.example.org/x", "timeoutMs": 150});
    assert!(reg.validate("webhook", cfg.as_object().unwrap()).is_ok());
}

// ---------------------------------------------------------------------------------------------
// Interne Meta-Schlüssel (`aigw.*`) verlassen das Gateway nie (Sicherheitsfix AIGW-37/61)
// ---------------------------------------------------------------------------------------------

/// Schreibt einen internen und einen externen Meta-Schlüssel.
struct MetaWriter;

impl aigw_plugins::Plugin for MetaWriter {
    fn name(&self) -> &'static str {
        "meta-writer"
    }
    fn phases(&self) -> aigw_plugins::Phases {
        aigw_plugins::Phases::REQUEST
    }
    fn on_request<'a>(
        &'a self,
        ctx: &'a mut aigw_plugins::ReqCtx<'_>,
    ) -> aigw_plugins::Hook<'a, aigw_plugins::Flow> {
        ctx.meta
            .insert("aigw.intern".into(), json!({"geheim": "nicht-raus"}));
        ctx.meta.insert("app.hint".into(), json!("darf-raus"));
        aigw_plugins::Hook::ok(aigw_plugins::Flow::Continue)
    }
}

#[tokio::test]
async fn internal_meta_never_leaves_the_gateway() {
    let (wh, seen) = fake_webhook().await;
    let pii = builtin::pii::Factory
        .build(&json!({"categories": {"email": "pseudonymize"}}))
        .unwrap();
    let plugin = factory()
        .build(&json!({
            "url": format!("http://{wh}/hook/continue"),
            "phases": ["request", "response"],
            "secret": {"file": secret_file()},
        }))
        .unwrap();
    let gw = gateway(vec![
        Arc::new(Entry::new("pii", pii).with_order(1)),
        Arc::new(Entry::new("meta", Arc::new(MetaWriter)).with_order(2)),
        Arc::new(Entry::new("wh", plugin).with_order(3)),
    ])
    .await;
    let r = post(gw, OAI, openai("Ich bin bob@example.com", false)).await;
    assert_eq!(r.status, StatusCode::OK);
    // Pseudonym in der Antwort zurückübersetzt (pii läuft in der Antwort-Phase nach dem Webhook).
    assert!(r.text().contains("bob@example.com"), "{}", r.text());

    let s = seen.lock().unwrap();
    assert_eq!(s.calls.len(), 2, "Anfrage- und Antwort-Phase");
    for (_, p) in &s.calls {
        let meta = p["meta"].as_object().expect("externe Meta-Sicht");
        assert_eq!(meta.get("app.hint"), Some(&json!("darf-raus")));
        assert!(
            meta.keys().all(|k| !k.starts_with("aigw.")),
            "interner Meta-Schlüssel im Webhook-Payload: {meta:?}"
        );
        let text = p.to_string();
        // Weder die Pseudonym-Zuordnung noch andere interne Daten.
        assert!(!text.contains("aigw."), "{text}");
        assert!(!text.contains("nicht-raus"), "{text}");
    }
    // Beide Phasen: der Webhook sieht nur den Platzhalter, nie den Originalwert – in der
    // Antwort-Phase laufen die Haken in umgekehrter Reihenfolge (Zwiebelprinzip, F167b),
    // `pii` übersetzt erst nach dem Webhook zurück.
    for (_, p) in &s.calls {
        let text = p.to_string();
        assert!(
            text.contains("[EMAIL_1]") && !text.contains("bob@example.com"),
            "{text}"
        );
    }
}

/// F167b (Sicherheit): `pii` (pseudonymize, order 10) → Webhook mit Antwort-Phase (order 20).
/// Der Webhook darf in keiner Phase, keinem Format und auch nicht im gepufferten Stream den
/// Originalwert sehen; der Client bekommt ihn zurück.
#[tokio::test]
async fn response_webhook_sees_only_pseudonyms() {
    let (wh, seen) = fake_webhook().await;
    let pii = builtin::pii::Factory
        .build(&json!({"categories": {"email": "pseudonymize", "phone": "pseudonymize"}}))
        .unwrap();
    let plugin = factory()
        .build(&json!({
            "url": format!("http://{wh}/hook/continue"),
            "phases": ["response"],
            "secret": {"file": secret_file()},
        }))
        .unwrap();
    let gw = gateway(vec![
        Arc::new(Entry::new("pii", pii).with_order(10)),
        Arc::new(Entry::new("audit", plugin).with_order(20)),
    ])
    .await;
    let secret = "Mail an bob@example.com oder +49 30 1234567";
    for stream in [false, true] {
        for (path, body) in [
            (OAI, openai(secret, stream)),
            (ANT, anthropic(secret, stream)),
        ] {
            let r = post(gw, path, body).await;
            assert_eq!(r.status, StatusCode::OK, "{path} {stream}");
            let text = if stream { r.sse_text() } else { r.text() };
            assert_eq!(text, format!("Echo: {secret}"), "{path} {stream}");
        }
    }
    let s = seen.lock().unwrap();
    assert_eq!(s.calls.len(), 4);
    for (_, p) in &s.calls {
        assert_eq!(p["phase"], "response");
        let text = p["response"].to_string();
        assert!(
            text.contains("[EMAIL_1]") && text.contains("[PHONE_1]"),
            "{text}"
        );
        let all = p.to_string();
        assert!(!all.contains("bob@example.com"), "{all}");
        assert!(!all.contains("1234567"), "{all}");
    }
}
