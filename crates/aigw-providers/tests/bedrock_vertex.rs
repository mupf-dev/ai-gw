// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Ende-zu-Ende-Tests der Adapter Bedrock (AIGW-18) und Vertex/Gemini (AIGW-19) gegen lokale
//! Fake-Server: SigV4-Prüfung auf Serverseite, Eventstream in ungünstig geschnittenen
//! Stücken, Service-Account-JWT mit Signaturprüfung am Fake-Token-Endpunkt, Token-Cache und
//! -Erneuerung, SSE, Fehlerklassen. Keine externen Aufrufe, keine echten Zugangsdaten
//! (der RSA-Schlüssel wird je Lauf erzeugt).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::ir::{ChatRequest, ErrorClass, Message, Role, StopReason, StreamEvent};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_providers::bedrock::eventstream::{HeaderValue as EsValue, Message as EsMessage};
use aigw_providers::bedrock::sigv4;
use aigw_providers::vertex::VertexAdapter;
use aigw_providers::{Registry, Timeouts, Upstream, UpstreamClient};
use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{
    KeyPair as _, RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey,
};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use futures_util::StreamExt as _;
use serde_json::{Value, json};

const AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

// ---------------------------------------------------------------------------------------------
// Hilfen
// ---------------------------------------------------------------------------------------------

fn b64(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in input.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn b64url_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => panic!("kein base64url"),
        };
        acc = ((acc & 0xff) << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn deployment(
    provider: ProviderKind,
    endpoint: &str,
    model: &str,
    creds: &[(&str, &str)],
    settings: Value,
) -> DeploymentCtx {
    let Value::Object(settings) = settings else {
        panic!()
    };
    let settings: JsonObject = settings;
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named(format!("dep-{model}")),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider,
                endpoint: Some(endpoint.into()),
                provider_model: model.into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
                budget: None,
            },
        },
        credential: Some(Arc::new(CredentialData(
            creds
                .iter()
                .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
                .collect::<BTreeMap<_, _>>(),
        ))),
    }
}

fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        messages: vec![
            Message::text(Role::System, "Sei knapp."),
            Message::text(Role::User, "Hallo?"),
        ],
        max_tokens: Some(64),
        stream: Some(stream),
        ..ChatRequest::default()
    }
}

fn fast() -> Timeouts {
    Timeouts {
        first_byte: Duration::from_secs(5),
        idle: Duration::from_millis(500),
        total: Duration::from_secs(10),
        ..Timeouts::default()
    }
}

async fn collect(up: &Upstream, req: ChatRequest) -> Vec<StreamEvent> {
    up.chat_stream(req).await.unwrap().collect().await
}

fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Antwort-Body in kleinen, ungünstig geschnittenen Stücken (über Frame-Grenzen hinweg).
fn chunked(content_type: &'static str, bytes: Vec<u8>, step: usize) -> Response {
    let chunks: Vec<Bytes> = bytes.chunks(step).map(Bytes::copy_from_slice).collect();
    let s = futures_util::stream::iter(chunks).then(|c| async move {
        tokio::task::yield_now().await;
        Ok::<_, std::io::Error>(c)
    });
    ([("content-type", content_type)], Body::from_stream(s)).into_response()
}

// ---------------------------------------------------------------------------------------------
// Fake-Bedrock
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct BedrockFake {
    /// (Pfad, Signatur gültig, Body)
    seen: Mutex<Vec<(String, bool, Value)>>,
}

/// Prüft die SigV4-Signatur so, wie AWS es tut: kanonische Anfrage aus den empfangenen
/// Headern (laut `SignedHeaders`), Pfad wie empfangen, SHA-256 des Bodys.
fn verify_sigv4(uri: &Uri, headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(rest) = auth.strip_prefix("AWS4-HMAC-SHA256 Credential=") else {
        return false;
    };
    let mut parts = rest.split(", ");
    let cred = parts.next().unwrap_or_default();
    let signed = parts
        .next()
        .and_then(|s| s.strip_prefix("SignedHeaders="))
        .unwrap_or_default();
    let sig = parts
        .next()
        .and_then(|s| s.strip_prefix("Signature="))
        .unwrap_or_default();
    let scope: Vec<&str> = cred.splitn(2, '/').collect();
    let [_, scope] = scope.as_slice() else {
        return false;
    };
    let fields: Vec<&str> = scope.split('/').collect();
    let (date, region, service) = (fields[0], fields[1], fields[2]);
    let list: Vec<(&str, &str)> = signed
        .split(';')
        .map(|n| {
            (
                n,
                headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or(""),
            )
        })
        .collect();
    let (canonical, _) = sigv4::canonical_request(
        "POST",
        uri.path(),
        uri.query().unwrap_or(""),
        &list,
        &sigv4::sha256_hex(body),
    );
    let amz_date = headers["x-amz-date"].to_str().unwrap();
    let sts = sigv4::string_to_sign(amz_date, scope, &canonical);
    let key = sigv4::signing_key(AWS_SECRET, date, region, service);
    sigv4::signature(&key, &sts) == sig && service == "bedrock" && region == "eu-central-1"
}

fn es_event(ty: &str, payload: Value) -> Vec<u8> {
    EsMessage {
        headers: vec![
            (":event-type".into(), EsValue::String(ty.into())),
            (
                ":content-type".into(),
                EsValue::String("application/json".into()),
            ),
            (":message-type".into(), EsValue::String("event".into())),
        ],
        payload: payload.to_string().into_bytes(),
    }
    .encode()
}

async fn bedrock_handler(
    State(fake): State<Arc<BedrockFake>>,
    Path((model, action)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ok = verify_sigv4(&uri, &headers, &body);
    let json: Value = serde_json::from_slice(&body).unwrap();
    fake.seen
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), ok, json.clone()));
    if !ok {
        return (
            StatusCode::FORBIDDEN,
            [("x-amzn-errortype", "InvalidSignatureException")],
            json!({"message": "The request signature we calculated does not match"}).to_string(),
        )
            .into_response();
    }
    let stream = action == "converse-stream";
    match model.as_str() {
        "throttled" => (
            StatusCode::TOO_MANY_REQUESTS,
            [(
                "x-amzn-errortype",
                "ThrottlingException:http://internal.amazon.com/coral/com.amazon.bedrock/",
            )],
            json!({"message": "Too many requests, please wait before trying again."}).to_string(),
        )
            .into_response(),
        "too-long" => (
            StatusCode::BAD_REQUEST,
            [("x-amzn-errortype", "ValidationException")],
            json!({"message": "Input is too long for requested model."}).to_string(),
        )
            .into_response(),
        "midstream" => {
            let mut b = es_event("messageStart", json!({"role": "assistant"}));
            b.extend(es_event(
                "contentBlockDelta",
                json!({"contentBlockIndex": 0, "delta": {"text": "a"}}),
            ));
            b.extend(
                EsMessage {
                    headers: vec![
                        (
                            ":exception-type".into(),
                            EsValue::String("modelStreamErrorException".into()),
                        ),
                        (":message-type".into(), EsValue::String("exception".into())),
                    ],
                    payload: json!({"message": "Model stream failed"})
                        .to_string()
                        .into_bytes(),
                }
                .encode(),
            );
            chunked("application/vnd.amazon.eventstream", b, 11)
        }
        _ if stream => {
            let mut b = es_event("messageStart", json!({"role": "assistant"}));
            for t in ["Hallo", " Welt"] {
                b.extend(es_event(
                    "contentBlockDelta",
                    json!({"contentBlockIndex": 0, "delta": {"text": t}, "p": "abcdefgh"}),
                ));
            }
            b.extend(es_event(
                "contentBlockStop",
                json!({"contentBlockIndex": 0}),
            ));
            b.extend(es_event("messageStop", json!({"stopReason": "end_turn"})));
            b.extend(es_event(
                "metadata",
                json!({"usage": {"inputTokens": 11, "outputTokens": 2, "totalTokens": 13}, "metrics": {"latencyMs": 5}}),
            ));
            chunked("application/vnd.amazon.eventstream", b, 7)
        }
        _ => (
            [("content-type", "application/json")],
            json!({
                "output": {"message": {"role": "assistant", "content": [{"text": "Hallo Welt"}]}},
                "stopReason": "end_turn",
                "usage": {"inputTokens": 11, "outputTokens": 2, "totalTokens": 13},
                "metrics": {"latencyMs": 5}
            })
            .to_string(),
        )
            .into_response(),
    }
}

async fn bedrock_env() -> (SocketAddr, Arc<BedrockFake>) {
    let fake = Arc::new(BedrockFake::default());
    let app = Router::new()
        .route("/model/{model}/{action}", post(bedrock_handler))
        .with_state(fake.clone());
    (serve(app).await, fake)
}

fn bedrock_upstream(addr: SocketAddr, model: &str) -> Upstream {
    let d = deployment(
        ProviderKind::Bedrock,
        &format!("http://{addr}"),
        model,
        &[
            ("accessKeyId", "AKIDEXAMPLE"),
            ("secretAccessKey", AWS_SECRET),
            ("sessionToken", "session-token-xyz"),
        ],
        json!({"region": "eu-central-1"}),
    );
    let client = UpstreamClient::unguarded().unwrap();
    let mut up = Upstream::from_deployment(&d, &Registry::with_builtin(), &client).unwrap();
    up = Upstream::new("bedrock", up.provider().clone(), client, fast());
    up
}

#[tokio::test]
async fn bedrock_converse_is_signed_and_translated() {
    let (addr, fake) = bedrock_env().await;
    let up = bedrock_upstream(addr, "anthropic.claude-3-haiku-20240307-v1:0");
    let resp = up.chat(request(false)).await.unwrap();
    assert_eq!(resp.choices[0].message.content.text(), "Hallo Welt");
    assert_eq!(resp.choices[0].stop_reason, Some(StopReason::EndTurn));
    assert_eq!(resp.usage.as_ref().unwrap().input_tokens, 11);

    let seen = fake.seen.lock().unwrap().clone();
    let (path, sig_ok, body) = &seen[0];
    assert!(sig_ok, "SigV4-Signatur ungültig");
    assert_eq!(
        path,
        "/model/anthropic.claude-3-haiku-20240307-v1%3A0/converse"
    );
    assert_eq!(body["system"], json!([{"text": "Sei knapp."}]));
    assert_eq!(body["messages"][0]["content"][0]["text"], "Hallo?");
    assert_eq!(body["inferenceConfig"]["maxTokens"], 64);
}

#[tokio::test]
async fn bedrock_converse_stream_eventstream() {
    let (addr, fake) = bedrock_env().await;
    let up = bedrock_upstream(addr, "amazon.nova-pro-v1:0");
    let ev = collect(&up, request(true)).await;
    assert!(matches!(&ev[0], StreamEvent::Start { model, .. } if model == "amazon.nova-pro-v1:0"));
    assert_eq!(text_of(&ev), "Hallo Welt");
    assert!(ev.contains(&StreamEvent::Stop {
        reason: StopReason::EndTurn,
        stop_sequence: None
    }));
    assert!(
        matches!(ev.last(), Some(StreamEvent::Usage(u)) if u.input_tokens == 11 && u.output_tokens == 2)
    );
    let seen = fake.seen.lock().unwrap();
    assert!(seen[0].1, "SigV4-Signatur ungültig");
    assert!(seen[0].0.ends_with("/converse-stream"));
}

#[tokio::test]
async fn bedrock_error_classes() {
    let (addr, _fake) = bedrock_env().await;
    let e = bedrock_upstream(addr, "throttled")
        .chat(request(false))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::RateLimit);
    assert!(e.retryable);
    assert_eq!(e.upstream_status, Some(429));

    let e = bedrock_upstream(addr, "too-long")
        .chat_stream(request(true))
        .await
        .err()
        .unwrap();
    assert_eq!(e.class, ErrorClass::ContextLength);
    assert_eq!(e.error.code.as_deref(), Some("context_length_exceeded"));

    let ev = collect(&bedrock_upstream(addr, "midstream"), request(true)).await;
    assert_eq!(text_of(&ev), "a");
    assert!(
        matches!(ev.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("modelStreamErrorException"))
    );

    // Falsches Secret → Fake lehnt die Signatur ab → Auth (502, ohne Anbietermeldung).
    let d = deployment(
        ProviderKind::Bedrock,
        &format!("http://{addr}"),
        "m",
        &[
            ("accessKeyId", "AKIDEXAMPLE"),
            ("secretAccessKey", "falsch"),
        ],
        json!({"region": "eu-central-1"}),
    );
    let up = Upstream::from_deployment(
        &d,
        &Registry::with_builtin(),
        &UpstreamClient::unguarded().unwrap(),
    )
    .unwrap();
    let e = up.chat(request(false)).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Auth);
    assert_eq!(e.error.status(), 502);
}

// ---------------------------------------------------------------------------------------------
// Fake-Google (Token-Endpunkt, Vertex AI, Gemini API)
// ---------------------------------------------------------------------------------------------

struct GoogleFake {
    public_key: Vec<u8>,
    token_url: Mutex<String>,
    /// `expires_in` der nächsten Token-Antworten.
    expires_in: AtomicU64,
    /// Token-Endpunkt antwortet mit diesem Status (0 = Erfolg).
    token_status: AtomicU64,
    token_requests: AtomicUsize,
    /// (Pfad inkl. Query, Authorization bzw. x-goog-api-key, Body)
    seen: Mutex<Vec<(String, String, Value)>>,
}

async fn token_handler(State(fake): State<Arc<GoogleFake>>, body: String) -> Response {
    let n = fake.token_requests.fetch_add(1, Ordering::SeqCst) + 1;
    let status = fake.token_status.load(Ordering::SeqCst);
    if status != 0 {
        return (
            StatusCode::from_u16(u16::try_from(status).unwrap()).unwrap(),
            json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."})
                .to_string(),
        )
            .into_response();
    }
    let mut grant = None;
    let mut assertion = None;
    for pair in body.split('&') {
        match pair.split_once('=') {
            Some(("grant_type", v)) => grant = Some(v.to_owned()),
            Some(("assertion", v)) => assertion = Some(v.to_owned()),
            _ => {}
        }
    }
    assert_eq!(
        grant.as_deref(),
        Some("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
    );
    let jwt = assertion.unwrap();
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let pk = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &fake.public_key);
    assert!(
        pk.verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &b64url_decode(parts[2])
        )
        .is_ok(),
        "JWT-Signatur ungültig"
    );
    let header: Value = serde_json::from_slice(&b64url_decode(parts[0])).unwrap();
    assert_eq!(header["alg"], "RS256");
    assert_eq!(header["kid"], "key-1");
    let claims: Value = serde_json::from_slice(&b64url_decode(parts[1])).unwrap();
    assert_eq!(claims["iss"], "gw@p1.iam.gserviceaccount.com");
    assert_eq!(
        claims["scope"],
        "https://www.googleapis.com/auth/cloud-platform"
    );
    assert_eq!(claims["aud"], *fake.token_url.lock().unwrap());
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        3600
    );
    (
        [("content-type", "application/json")],
        json!({"access_token": format!("tok-{n}"), "expires_in": fake.expires_in.load(Ordering::SeqCst), "token_type": "Bearer"})
            .to_string(),
    )
        .into_response()
}

fn gemini_chunk(text: &str) -> Value {
    json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}}],
           "responseId": "resp-1", "modelVersion": "gemini-2.5-flash-001"})
}

async fn model_handler(
    State(fake): State<Arc<GoogleFake>>,
    Path(model_action): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = headers
        .get("authorization")
        .or_else(|| headers.get("x-goog-api-key"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let json: Value = serde_json::from_slice(&body).unwrap();
    fake.seen
        .lock()
        .unwrap()
        .push((uri.to_string(), auth.clone(), json));
    if !(auth.starts_with("Bearer tok-") || auth == "AIza-test-key") {
        return (
            StatusCode::UNAUTHORIZED,
            json!({"error": {"code": 401, "message": "Request had invalid authentication credentials.", "status": "UNAUTHENTICATED"}}).to_string(),
        )
            .into_response();
    }
    let (model, action) = model_action.split_once(':').unwrap();
    match model {
        "blocked" => (
            [("content-type", "application/json")],
            json!({"promptFeedback": {"blockReason": "SAFETY"}, "usageMetadata": {"promptTokenCount": 3}})
                .to_string(),
        )
            .into_response(),
        "exhausted" => (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": {"code": 429, "message": "Resource has been exhausted (e.g. check quota).",
                   "status": "RESOURCE_EXHAUSTED",
                   "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "2s"}]}})
                .to_string(),
        )
            .into_response(),
        _ if action == "streamGenerateContent" => {
            assert_eq!(uri.query(), Some("alt=sse"));
            let mut b = String::new();
            for c in [
                gemini_chunk("Hallo"),
                gemini_chunk(" Welt"),
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": ""}]}, "finishReason": "STOP"}],
                       "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 2, "totalTokenCount": 13}}),
            ] {
                b.push_str(&format!("data: {c}\r\n\r\n"));
            }
            chunked("text/event-stream", b.into_bytes(), 9)
        }
        _ => (
            [("content-type", "application/json")],
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "Hallo Welt"}]}, "finishReason": "STOP"}],
                   "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 2, "totalTokenCount": 13},
                   "responseId": "resp-1", "modelVersion": "gemini-2.5-flash-001"})
                .to_string(),
        )
            .into_response(),
    }
}

struct Google {
    addr: SocketAddr,
    fake: Arc<GoogleFake>,
    sa_json: String,
}

async fn google_env() -> Google {
    let kp = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
    let der = kp.as_der().unwrap();
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        b64(der.as_ref())
    );
    let fake = Arc::new(GoogleFake {
        public_key: kp.public_key().as_ref().to_vec(),
        token_url: Mutex::new(String::new()),
        expires_in: AtomicU64::new(3600),
        token_status: AtomicU64::new(0),
        token_requests: AtomicUsize::new(0),
        seen: Mutex::new(Vec::new()),
    });
    let app = Router::new()
        .route("/token", post(token_handler))
        .route(
            "/v1/projects/p1/locations/europe-west4/publishers/google/models/{model_action}",
            post(model_handler),
        )
        .route("/v1beta/models/{model_action}", post(model_handler))
        .with_state(fake.clone());
    let addr = serve(app).await;
    let token_url = format!("http://{addr}/token");
    *fake.token_url.lock().unwrap() = token_url.clone();
    let sa_json = json!({
        "type": "service_account", "project_id": "p1", "private_key_id": "key-1",
        "private_key": pem, "client_email": "gw@p1.iam.gserviceaccount.com",
        "token_uri": token_url,
    })
    .to_string();
    Google {
        addr,
        fake,
        sa_json,
    }
}

impl Google {
    fn vertex(&self, model: &str) -> (VertexAdapter, Upstream) {
        let d = deployment(
            ProviderKind::Vertex,
            &format!("http://{}/v1", self.addr),
            model,
            &[("serviceAccountJson", &self.sa_json)],
            json!({"location": "europe-west4"}),
        );
        let adapter = VertexAdapter::from_deployment(&d).unwrap();
        let up = Upstream::new(
            "vertex",
            Arc::new(adapter.clone()),
            UpstreamClient::unguarded().unwrap(),
            fast(),
        );
        (adapter, up)
    }

    fn gemini(&self, model: &str) -> Upstream {
        let d = deployment(
            ProviderKind::Vertex,
            &format!("http://{}/v1beta", self.addr),
            model,
            &[("apiKey", "AIza-test-key")],
            json!({}),
        );
        Upstream::from_deployment(
            &d,
            &Registry::with_builtin(),
            &UpstreamClient::unguarded().unwrap(),
        )
        .unwrap()
    }

    fn token_requests(&self) -> usize {
        self.fake.token_requests.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn vertex_service_account_token_is_cached() {
    let g = google_env().await;
    let (adapter, up) = g.vertex("gemini-2.5-flash");
    for _ in 0..3 {
        let r = up.chat(request(false)).await.unwrap();
        assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    }
    // Parallele Anfragen ohne Token teilen sich einen Abruf.
    let (adapter2, up2) = g.vertex("gemini-2.5-flash");
    let rs = futures_util::future::join_all((0..5).map(|_| up2.chat(request(false)))).await;
    assert!(rs.iter().all(Result::is_ok));
    assert_eq!(g.token_requests(), 2);
    assert_eq!(adapter.service_account().unwrap().fetch_count(), 1);
    assert_eq!(adapter2.service_account().unwrap().fetch_count(), 1);

    let seen = g.fake.seen.lock().unwrap();
    assert_eq!(seen[0].1, "Bearer tok-1");
    assert!(
        seen[0]
            .0
            .ends_with("/models/gemini-2.5-flash:generateContent")
    );
    assert_eq!(
        seen[0].2["systemInstruction"]["parts"][0]["text"],
        "Sei knapp."
    );
    assert_eq!(seen[0].2["contents"][0]["parts"][0]["text"], "Hallo?");
    assert_eq!(seen[0].2["generationConfig"]["maxOutputTokens"], 64);
}

#[tokio::test]
async fn vertex_token_refresh_ahead_of_expiry() {
    let g = google_env().await;
    // 200 s Laufzeit: gültig, aber innerhalb des Vorlaufs (300 s) → Erneuerung im Hintergrund.
    g.fake.expires_in.store(200, Ordering::SeqCst);
    let (adapter, up) = g.vertex("gemini-2.5-flash");
    up.chat(request(false)).await.unwrap();
    assert_eq!(g.token_requests(), 1);
    g.fake.expires_in.store(3600, Ordering::SeqCst);
    up.chat(request(false)).await.unwrap();
    for _ in 0..200 {
        if g.token_requests() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(g.token_requests(), 2);
    // Antwort des Token-Endpunkts im Cache ablegen lassen.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Neues Token mit langer Laufzeit: keine weiteren Abrufe.
    up.chat(request(false)).await.unwrap();
    up.chat(request(false)).await.unwrap();
    assert_eq!(adapter.service_account().unwrap().fetch_count(), 2);
    assert_eq!(
        g.fake.seen.lock().unwrap().last().unwrap().1,
        "Bearer tok-2"
    );

    // Abgelaufenes Token: die nächste Anfrage holt synchron ein neues.
    let g = google_env().await;
    g.fake.expires_in.store(1, Ordering::SeqCst);
    let (_, up) = g.vertex("gemini-2.5-flash");
    up.chat(request(false)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    up.chat(request(false)).await.unwrap();
    assert_eq!(g.token_requests(), 2);
    assert_eq!(
        g.fake.seen.lock().unwrap().last().unwrap().1,
        "Bearer tok-2"
    );
}

#[tokio::test]
async fn vertex_stream_and_token_failure() {
    let g = google_env().await;
    let (_, up) = g.vertex("gemini-2.5-flash");
    let ev = collect(&up, request(true)).await;
    assert!(
        matches!(&ev[0], StreamEvent::Start { id, model, .. } if id == "resp-1" && model == "gemini-2.5-flash-001")
    );
    assert_eq!(text_of(&ev), "Hallo Welt");
    assert!(ev.contains(&StreamEvent::Stop {
        reason: StopReason::EndTurn,
        stop_sequence: None
    }));
    assert!(matches!(ev.last(), Some(StreamEvent::Usage(u)) if u.input_tokens == 11));

    // Token-Endpunkt lehnt ab → Auth-Fehler (502, nicht retrybar), keine Modell-Anfrage.
    let g = google_env().await;
    g.fake.token_status.store(400, Ordering::SeqCst);
    let (_, up) = g.vertex("gemini-2.5-flash");
    let e = up.chat(request(false)).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Auth);
    assert!(!e.retryable);
    assert_eq!(e.error.status(), 502);
    assert_eq!(
        e.error.code.as_deref(),
        Some("upstream_authentication_error")
    );
    assert!(g.fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn gemini_api_key_mode_and_error_classes() {
    let g = google_env().await;
    let up = g.gemini("gemini-2.5-flash");
    let r = up.chat(request(false)).await.unwrap();
    assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    let ev = collect(&up, request(true)).await;
    assert_eq!(text_of(&ev), "Hallo Welt");
    {
        let seen = g.fake.seen.lock().unwrap();
        assert_eq!(seen[0].1, "AIza-test-key");
        assert_eq!(seen[0].0, "/v1beta/models/gemini-2.5-flash:generateContent");
        assert_eq!(
            seen[1].0,
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
    }
    assert_eq!(g.token_requests(), 0);

    let e = g.gemini("blocked").chat(request(false)).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::ContentFilter);
    assert_eq!(e.error.code.as_deref(), Some("content_filter"));
    assert_eq!(e.error.status(), 400);

    let e = g
        .gemini("exhausted")
        .chat(request(false))
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::RateLimit);
    assert!(e.retryable);
    assert_eq!(e.retry_after, Some(Duration::from_secs(2)));
    assert_eq!(e.error.status(), 429);
}
