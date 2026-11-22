// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Azure AI Foundry gegen lokale Fake-Server: Claude über den Anthropic-kompatiblen Endpunkt
//! (`{endpoint}/v1/messages`) mit Entra-ID-Bearer-Token statt `x-api-key`, Managed Identity in
//! Azure Container Apps (App-Service-Protokoll `IDENTITY_ENDPOINT`/`IDENTITY_HEADER` mit
//! benutzerzugewiesener Identität), gemeinsame Token-Quelle mit Azure OpenAI, Fehlerklassen.
//! Keine externen Aufrufe, keine echten Zugangsdaten.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::config::AdminConfig;
use aigw_core::ir::{ChatRequest, ErrorClass, Message, Role};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, EntraAuth, EntraCredentialKind, JsonObject,
    ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::upstream::UpstreamPolicy;
use aigw_outbound::{AddressGuard, OutboundClient, StaticResolver};
use aigw_providers::anthropic::AnthropicAdapter;
use aigw_providers::entra::EntraEnv;
use aigw_providers::openai::OpenAiAdapter;
use aigw_providers::{Timeouts, Upstream, UpstreamClient, with_credential_auth};
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

const TENANT: &str = "72f988bf-86f1-41af-91ab-2d7cd011db47";
const CLIENT: &str = "11111111-2222-3333-4444-555555555555";
const UAMI: &str = "99999999-8888-7777-6666-555555555555";

// ---------------------------------------------------------------------------------------------
// Hilfen
// ---------------------------------------------------------------------------------------------

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap()
}

fn form(s: &str) -> HashMap<String, String> {
    s.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_owned(), percent_decode(v)))
        .collect()
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
            metadata: ObjectMeta::named("foundry"),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider,
                endpoint: Some(endpoint.into()),
                provider_model: model.into(),
                credential: (!creds.is_empty()).then(|| "c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
            },
        },
        credential: (!creds.is_empty()).then(|| {
            Arc::new(CredentialData(
                creds
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), SecretString::new(*v)))
                    .collect::<BTreeMap<_, _>>(),
            ))
        }),
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        messages: vec![Message::text(Role::User, "Hallo?")],
        max_tokens: Some(16),
        stream: Some(false),
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

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Client mit SSRF-Regeln wie im Betrieb: nur `foundry.test` (→ 127.0.0.1) ist für die
/// Modelle freigeschaltet; `localhost` (Container-Apps-Identitätsendpunkt) ist nur über die
/// gezielte Ausnahme der Managed Identity erreichbar.
fn guarded_client() -> UpstreamClient {
    let policy = UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["foundry.test".into()],
        allow_private_upstreams: vec!["foundry.test".into()],
        ..AdminConfig::default()
    })
    .unwrap();
    let outbound = OutboundClient::builder(AddressGuard::policy(policy))
        .resolver(Arc::new(
            StaticResolver::new()
                .with("foundry.test", [IpAddr::V4(Ipv4Addr::LOCALHOST)])
                .with("localhost", [IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        ))
        .build()
        .unwrap();
    UpstreamClient::from_outbound(outbound)
}

// ---------------------------------------------------------------------------------------------
// Fake-Entra, Fake-Container-Apps-Identität, Fake-Foundry
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Fake {
    token_requests: AtomicUsize,
    /// 0 = Erfolg, sonst HTTP-Status der Token-Antwort.
    token_status: AtomicU16,
    /// 0 = Erfolg, sonst HTTP-Status der Foundry-Antwort.
    model_status: AtomicU16,
    /// Abfragen an den Identitätsendpunkt.
    mi: Mutex<Vec<String>>,
    /// (Pfad, Header, Body) der Modell-Anfragen.
    calls: Mutex<Vec<(String, HeaderMap, Value)>>,
}

fn token_reply(fake: &Fake, prefix: &str, expires: Value) -> Response {
    let n = fake.token_requests.fetch_add(1, Ordering::SeqCst) + 1;
    let status = fake.token_status.load(Ordering::SeqCst);
    if status != 0 {
        return (
            StatusCode::from_u16(status).unwrap(),
            json!({"error": "invalid_client", "error_description": "AADSTS7000215"}).to_string(),
        )
            .into_response();
    }
    let mut body = json!({"access_token": format!("{prefix}-{n}"), "token_type": "Bearer"});
    body.as_object_mut()
        .unwrap()
        .extend(expires.as_object().unwrap().clone());
    ([("content-type", "application/json")], body.to_string()).into_response()
}

async fn token_handler(
    State(fake): State<Arc<Fake>>,
    Path(tenant): Path<String>,
    body: String,
) -> Response {
    assert_eq!(tenant, TENANT);
    let f = form(&body);
    assert_eq!(f["grant_type"], "client_credentials");
    assert_eq!(f["client_id"], CLIENT);
    assert_eq!(f["scope"], "https://cognitiveservices.azure.com/.default");
    token_reply(&fake, "entra-tok", json!({"expires_in": 3600}))
}

/// Container Apps: `GET {IDENTITY_ENDPOINT}?api-version=2019-08-01&resource=…&client_id=…`
/// mit `X-IDENTITY-HEADER`; Antwort mit `expires_on` (Unix-Sekunden als Zeichenkette).
async fn aca_identity_handler(
    State(fake): State<Arc<Fake>>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if headers
        .get("x-identity-header")
        .is_none_or(|h| h != "aca-hdr-geheim")
    {
        return (StatusCode::UNAUTHORIZED, "{}").into_response();
    }
    assert!(headers.get("metadata").is_none());
    fake.mi.lock().unwrap().push(q.unwrap_or_default());
    let on = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    token_reply(
        &fake,
        "aca-tok",
        json!({"expires_on": on.to_string(), "resource": "https://cognitiveservices.azure.com",
               "client_id": UAMI}),
    )
}

async fn messages_handler(
    State(fake): State<Arc<Fake>>,
    uri: http::Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap();
    fake.calls
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), headers, body.clone()));
    let status = fake.model_status.load(Ordering::SeqCst);
    if status != 0 {
        return (
            StatusCode::from_u16(status).unwrap(),
            [("content-type", "application/json")],
            json!({"type": "error", "error": {"type": "authentication_error",
                   "message": "Principal does not have access"}})
            .to_string(),
        )
            .into_response();
    }
    (
        [("content-type", "application/json")],
        json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": body["model"],
            "content": [{"type": "text", "text": "Hallo Welt"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 2}
        })
        .to_string(),
    )
        .into_response()
}

async fn chat_completions_handler(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    fake.calls
        .lock()
        .unwrap()
        .push(("/openai".into(), headers, Value::Null));
    (
        [("content-type", "application/json")],
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-5.4-mini",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hallo Welt"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        })
        .to_string(),
    )
        .into_response()
}

async fn env() -> (SocketAddr, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route("/msi/token", get(aca_identity_handler))
        .route("/anthropic/v1/messages", post(messages_handler))
        .route(
            "/openai/deployments/gpt-5.4-mini/chat/completions",
            post(chat_completions_handler),
        )
        .with_state(fake.clone());
    (serve(app).await, fake)
}

fn header<'a>(h: &'a HeaderMap, k: &str) -> Option<&'a str> {
    h.get(k).map(|v| v.to_str().unwrap())
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn claude_on_foundry_with_client_secret_sends_bearer_instead_of_api_key() {
    let (addr, fake) = env().await;
    let creds = [
        ("tenantId", TENANT),
        ("clientId", CLIENT),
        ("clientSecret", "sp-geheim"),
    ];
    let d = deployment(
        ProviderKind::Anthropic,
        &format!("http://{addr}/anthropic"),
        "claude-sonnet-5-5",
        &creds,
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}")}),
    );
    let a = AnthropicAdapter::from_deployment(&d).unwrap();
    assert_eq!(
        a.uri().to_string(),
        format!("http://{addr}/anthropic/v1/messages")
    );
    assert!(!format!("{a:?}").contains("sp-geheim"));
    let up = Upstream::new(
        "foundry",
        Arc::new(a.clone()),
        UpstreamClient::unguarded().unwrap(),
        fast(),
    );
    let r = up.chat(request()).await.unwrap();
    assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 1, "Token-Cache");

    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    let (path, h, body) = &calls[0];
    assert_eq!(path, "/anthropic/v1/messages");
    assert_eq!(header(h, "authorization"), Some("Bearer entra-tok-1"));
    assert!(h.get("x-api-key").is_none(), "kein x-api-key bei entraId");
    assert_eq!(header(h, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(body["model"], "claude-sonnet-5-5");
    assert_eq!(body["max_tokens"], 16);

    // Endpunkt mit /v1 (wie in manchen SDK-Beispielen) ergibt dieselbe URL.
    let d = deployment(
        ProviderKind::Anthropic,
        &format!("http://{addr}/anthropic/v1/"),
        "claude-sonnet-5-5",
        &creds,
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}")}),
    );
    assert_eq!(
        AnthropicAdapter::from_deployment(&d)
            .unwrap()
            .uri()
            .to_string(),
        format!("http://{addr}/anthropic/v1/messages")
    );
}

#[tokio::test]
async fn container_apps_managed_identity_shared_by_gpt_and_claude() {
    let (addr, fake) = env().await;
    // Container Apps: IDENTITY_ENDPOINT zeigt auf localhost (eigener Port) – nur dieser Host ist
    // von den SSRF-Regeln ausgenommen.
    let env = EntraEnv {
        identity_endpoint: Some(format!("http://localhost:{}/msi/token", addr.port())),
        identity_header: Some(SecretString::new("aca-hdr-geheim")),
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let mi = json!({"auth": "entraId", "entraCredential": "managedIdentity",
                    "entraClientId": UAMI});
    let base = format!("http://foundry.test:{}", addr.port());

    let mut s = mi.clone();
    s["apiVersion"] = json!("2025-04-01-preview");
    let gpt = deployment(ProviderKind::AzureOpenAi, &base, "gpt-5.4-mini", &[], s);
    let gpt = OpenAiAdapter::from_deployment_with_entra_env(&gpt, &env).unwrap();
    let sonnet = deployment(
        ProviderKind::Anthropic,
        &format!("{base}/anthropic"),
        "claude-sonnet-5-5",
        &[],
        mi.clone(),
    );
    let sonnet = AnthropicAdapter::from_deployment_with_entra_env(&sonnet, &env).unwrap();
    let mut s = mi.clone();
    s["reasoningMode"] = json!("effort");
    let opus = deployment(
        ProviderKind::Anthropic,
        &format!("{base}/anthropic"),
        "claude-opus-5-5",
        &[],
        s,
    );
    let opus = AnthropicAdapter::from_deployment_with_entra_env(&opus, &env).unwrap();
    assert!(!format!("{sonnet:?}").contains("aca-hdr-geheim"));
    // Gleiche Identität, gleicher Scope → eine Token-Quelle für alle drei Deployments.
    assert!(Arc::ptr_eq(
        gpt.entra_source().unwrap(),
        sonnet.entra_source().unwrap()
    ));
    assert!(Arc::ptr_eq(
        sonnet.entra_source().unwrap(),
        opus.entra_source().unwrap()
    ));

    let client = guarded_client();
    let up = |name: &str, p: Arc<dyn aigw_providers::Provider>| {
        Upstream::new(name, p, client.clone(), fast())
    };
    let r = up("gpt", Arc::new(gpt)).chat(request()).await.unwrap();
    assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    let r = up("sonnet", Arc::new(sonnet))
        .chat(request())
        .await
        .unwrap();
    assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    let r = up("opus", Arc::new(opus)).chat(request()).await.unwrap();
    assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 1);

    let q = form(&fake.mi.lock().unwrap()[0]);
    assert_eq!(q["api-version"], "2019-08-01");
    assert_eq!(q["resource"], "https://cognitiveservices.azure.com");
    assert_eq!(q["client_id"], UAMI);

    let calls = fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    for (path, h, _) in &calls {
        assert_eq!(
            header(h, "authorization"),
            Some("Bearer aca-tok-1"),
            "{path}"
        );
        assert!(h.get("x-api-key").is_none() && h.get("api-key").is_none());
    }
    assert_eq!(calls[2].2["model"], "claude-opus-5-5");

    // Ohne die Ausnahme bleibt localhost gesperrt: ein Modell-Endpunkt auf localhost wird nicht
    // erreicht (nur der Identitätsendpunkt ist freigeschaltet).
    let d = deployment(
        ProviderKind::Anthropic,
        &format!("http://localhost:{}/anthropic", addr.port()),
        "claude-sonnet-5-5",
        &[],
        mi,
    );
    let a = AnthropicAdapter::from_deployment_with_entra_env(&d, &env).unwrap();
    let e = up("blocked", Arc::new(a))
        .chat(request())
        .await
        .unwrap_err();
    assert_ne!(e.class, ErrorClass::Auth);
    assert_eq!(fake.calls.lock().unwrap().len(), 3);
}

/// AIGW-84: Die Anmeldung steht an den Zugangsdaten (`ProviderCredential.spec.entra`), die
/// Deployments verweisen nur noch auf `credential` – GPT und Claude teilen sich die Token-Quelle.
#[tokio::test]
async fn managed_identity_from_credential_entra() {
    let (addr, fake) = env().await;
    let env = EntraEnv {
        identity_endpoint: Some(format!("http://localhost:{}/msi/token", addr.port())),
        identity_header: Some(SecretString::new("aca-hdr-geheim")),
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let entra = EntraAuth {
        credential: EntraCredentialKind::ManagedIdentity,
        tenant_id: None,
        client_id: Some(UAMI.into()),
        authority: None,
        scope: None,
        send_x5c: None,
    };
    let base = format!("http://foundry.test:{}", addr.port());
    // Deployment mit Zugangsdaten ohne Werte (Managed Identity speichert kein Geheimnis).
    let with_cred = |provider, endpoint: String, model: &str, settings: Value| {
        let mut d = deployment(provider, &endpoint, model, &[], settings);
        d.deployment.spec.credential = Some("foundry-mi".into());
        d.credential = Some(Arc::new(CredentialData(BTreeMap::new())));
        with_credential_auth(Some(&entra), std::borrow::Cow::Owned(d)).into_owned()
    };
    let gpt = with_cred(
        ProviderKind::AzureOpenAi,
        base.clone(),
        "gpt-5.4-mini",
        json!({"apiVersion": "2025-04-01-preview"}),
    );
    assert_eq!(gpt.deployment.spec.settings["auth"], "entraId");
    assert_eq!(gpt.deployment.spec.settings["entraClientId"], UAMI);
    let gpt = OpenAiAdapter::from_deployment_with_entra_env(&gpt, &env).unwrap();
    let claude = with_cred(
        ProviderKind::Anthropic,
        format!("{base}/anthropic"),
        "claude-sonnet-5-5",
        json!({}),
    );
    let claude = AnthropicAdapter::from_deployment_with_entra_env(&claude, &env).unwrap();
    assert!(Arc::ptr_eq(
        gpt.entra_source().unwrap(),
        claude.entra_source().unwrap()
    ));
    let client = guarded_client();
    for (name, p) in [
        ("gpt", Arc::new(gpt) as Arc<dyn aigw_providers::Provider>),
        ("claude", Arc::new(claude)),
    ] {
        let r = Upstream::new(name, p, client.clone(), fast())
            .chat(request())
            .await
            .unwrap();
        assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    }
    assert_eq!(fake.token_requests.load(Ordering::SeqCst), 1);
    let q = form(&fake.mi.lock().unwrap()[0]);
    assert_eq!(q["client_id"], UAMI);
    for (path, h, _) in fake.calls.lock().unwrap().iter() {
        assert_eq!(
            header(h, "authorization"),
            Some("Bearer aca-tok-1"),
            "{path}"
        );
    }
    // Eigene Einstellungen des Deployments haben Vorrang (hier: API-Key statt Entra).
    let own = deployment(
        ProviderKind::AzureOpenAi,
        &base,
        "gpt",
        &[("apiKey", "k")],
        json!({"apiVersion": "2025-04-01-preview", "auth": "apiKey"}),
    );
    let merged = with_credential_auth(Some(&entra), std::borrow::Cow::Borrowed(&own));
    assert!(matches!(merged, std::borrow::Cow::Borrowed(_)));
}

#[tokio::test]
async fn foundry_errors_are_classified() {
    let (addr, fake) = env().await;
    let creds = [
        ("tenantId", TENANT),
        ("clientId", CLIENT),
        ("clientSecret", "anderes-geheimnis"),
    ];
    let d = deployment(
        ProviderKind::Anthropic,
        &format!("http://{addr}/anthropic"),
        "claude-sonnet-5-5",
        &creds,
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}")}),
    );
    // Token-Abruf abgelehnt → Auth, nicht retrybar, Modell wird nicht angefragt.
    fake.token_status.store(401, Ordering::SeqCst);
    let a = AnthropicAdapter::from_deployment(&d).unwrap();
    let up = Upstream::new(
        "foundry",
        Arc::new(a),
        UpstreamClient::unguarded().unwrap(),
        fast(),
    );
    let e = up.chat(request()).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Auth);
    assert!(!e.retryable);
    assert!(fake.calls.lock().unwrap().is_empty());

    // Token gültig, aber der Identität fehlt die Rolle → Auth vom Modell-Endpunkt.
    fake.token_status.store(0, Ordering::SeqCst);
    fake.model_status.store(401, Ordering::SeqCst);
    let d = deployment(
        ProviderKind::Anthropic,
        &format!("http://{addr}/anthropic"),
        "claude-sonnet-5-5",
        &[
            ("tenantId", TENANT),
            ("clientId", CLIENT),
            ("clientSecret", "drittes-geheimnis"),
        ],
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}")}),
    );
    let up = Upstream::new(
        "foundry2",
        Arc::new(AnthropicAdapter::from_deployment(&d).unwrap()),
        UpstreamClient::unguarded().unwrap(),
        fast(),
    );
    let e = up.chat(request()).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Auth);
    assert!(!e.error.message.contains("Principal"), "Meldung verborgen");
}

#[test]
fn configuration_errors() {
    let ep = "https://res.services.ai.azure.com/anthropic";
    type Case<'a> = (Vec<(&'a str, &'a str)>, Value, &'a str);
    let cases: Vec<Case<'_>> = vec![
        // apiKey (Standard) ohne Schlüssel.
        (vec![], json!({}), "apiKey"),
        (vec![("apiKey", "k")], json!({"auth": "magie"}), "unbekannt"),
        (vec![], json!({"auth": "entraId"}), "entraCredential"),
        (
            vec![],
            json!({"auth": "entraId", "entraCredential": "managedIdentity",
                   "entraScope": "https://vault.azure.net/.default"}),
            "entraScope",
        ),
    ];
    for (creds, settings, needle) in cases {
        let d = deployment(
            ProviderKind::Anthropic,
            ep,
            "claude",
            &creds,
            settings.clone(),
        );
        let e = AnthropicAdapter::from_deployment_with_entra_env(&d, &EntraEnv::default())
            .unwrap_err()
            .to_string();
        assert!(e.contains(needle), "{settings}: {e}");
    }
    // entraId ohne endpoint: kein Token an api.anthropic.com.
    let mut d = deployment(
        ProviderKind::Anthropic,
        ep,
        "claude",
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
    );
    d.deployment.spec.endpoint = None;
    let allowed = EntraEnv {
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let e = AnthropicAdapter::from_deployment_with_entra_env(&d, &allowed)
        .unwrap_err()
        .to_string();
    assert!(e.contains("endpoint"), "{e}");
    // apiKey bleibt Standard und setzt keine Token-Quelle.
    let d = deployment(
        ProviderKind::Anthropic,
        ep,
        "claude",
        &[("apiKey", "k")],
        json!({}),
    );
    assert!(
        AnthropicAdapter::from_deployment(&d)
            .unwrap()
            .entra_source()
            .is_none()
    );
}
