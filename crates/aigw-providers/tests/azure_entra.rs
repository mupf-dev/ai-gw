// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Azure OpenAI mit Entra ID (AIGW-82) gegen lokale Fake-Server: Client Credentials mit Secret
//! und Zertifikat (Signatur- und `x5t`-Prüfung am Fake-Token-Endpunkt), Workload Identity
//! (föderiertes Token aus Datei), Managed Identity (IMDS, App Service, Azure Arc mit
//! Challenge-Datei) mit gezielter SSRF-Ausnahme, verschlüsselte Zertifikatsschlüssel und `x5c`
//! (AIGW-94), Token-Cache je Credential, Erneuerung vor Ablauf, gebündelter Abruf,
//! Fehlerklassen. Keine externen Aufrufe, keine echten Zugangsdaten.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aigw_core::config::AdminConfig;
use aigw_core::ir::{ChatRequest, ErrorClass, Message, Role};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::upstream::UpstreamPolicy;
use aigw_outbound::{AddressGuard, OutboundClient, StaticResolver};
use aigw_providers::entra::{EntraEnv, EntraTokenSource};
use aigw_providers::openai::OpenAiAdapter;
use aigw_providers::{Timeouts, Upstream, UpstreamClient};
use aws_lc_rs::digest;
use aws_lc_rs::encoding::AsDer as _;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{
    KeyPair as _, RSA_PKCS1_2048_8192_SHA256, RsaKeyPair, UnparsedPublicKey,
};
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

const TENANT: &str = "72f988bf-86f1-41af-91ab-2d7cd011db47";
const CLIENT: &str = "11111111-2222-3333-4444-555555555555";
const SCOPE: &str = "https://cognitiveservices.azure.com/.default";

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

fn b64url(input: &[u8]) -> String {
    b64(input)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
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

fn azure(endpoint: &str, creds: &[(&str, &str)], settings: Value) -> DeploymentCtx {
    let Value::Object(settings) = settings else {
        panic!()
    };
    let settings: JsonObject = settings;
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named("aoai"),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider: ProviderKind::AzureOpenAi,
                endpoint: Some(endpoint.into()),
                provider_model: "gpt-4o".into(),
                credential: (!creds.is_empty()).then(|| "c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
                budget: None,
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

async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Fake-Entra + Fake-Azure-OpenAI
// ---------------------------------------------------------------------------------------------

struct Fake {
    expires_in: AtomicU64,
    /// 0 = Erfolg, sonst HTTP-Status der Token-Antwort.
    token_status: AtomicU64,
    /// Verzögerung des Token-Endpunkts (ms).
    delay_ms: AtomicU64,
    token_requests: AtomicUsize,
    /// Formulare der Token-Anfragen.
    forms: Mutex<Vec<HashMap<String, String>>>,
    /// (Abfrage, Header) der Managed-Identity-Anfragen.
    mi: Mutex<Vec<(String, HeaderMap)>>,
    /// Öffentlicher Schlüssel für Zertifikats-Assertions und erwartetes `x5t`.
    cert: Mutex<Option<(Vec<u8>, String)>>,
    /// `Authorization`/`api-key` der Modell-Anfragen.
    model_auth: Mutex<Vec<(Option<String>, Option<String>)>>,
    /// Azure Arc: `realm` der Challenge und erwarteter Schlüssel.
    arc: Mutex<Option<(String, String)>>,
    /// Azure Arc: `Authorization` jeder Anfrage (`None` = Challenge-Anfrage).
    arc_auth: Mutex<Vec<Option<String>>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            expires_in: AtomicU64::new(3600),
            token_status: AtomicU64::new(0),
            delay_ms: AtomicU64::new(0),
            token_requests: AtomicUsize::new(0),
            forms: Mutex::new(Vec::new()),
            mi: Mutex::new(Vec::new()),
            cert: Mutex::new(None),
            model_auth: Mutex::new(Vec::new()),
            arc: Mutex::new(None),
            arc_auth: Mutex::new(Vec::new()),
        })
    }

    fn token_requests(&self) -> usize {
        self.token_requests.load(Ordering::SeqCst)
    }

    async fn issue(&self, prefix: &str, as_string: bool) -> Response {
        let n = self.token_requests.fetch_add(1, Ordering::SeqCst) + 1;
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let status = self.token_status.load(Ordering::SeqCst);
        if status != 0 {
            return (
                StatusCode::from_u16(u16::try_from(status).unwrap()).unwrap(),
                json!({
                    "error": "invalid_client",
                    "error_description": "AADSTS7000215: Invalid client secret provided."
                })
                .to_string(),
            )
                .into_response();
        }
        let exp = self.expires_in.load(Ordering::SeqCst);
        let body = if as_string {
            json!({"access_token": format!("{prefix}-{n}"), "expires_in": exp.to_string(),
                   "token_type": "Bearer"})
        } else {
            json!({"access_token": format!("{prefix}-{n}"), "expires_in": exp,
                   "token_type": "Bearer"})
        };
        ([("content-type", "application/json")], body.to_string()).into_response()
    }
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
    if let Some(assertion) = f.get("client_assertion") {
        assert_eq!(
            f["client_assertion_type"],
            "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"
        );
        let cert = fake.cert.lock().unwrap().clone();
        if let Some((public, x5t)) = cert {
            let parts: Vec<&str> = assertion.split('.').collect();
            let pk = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, &public);
            assert!(
                pk.verify(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &b64url_decode(parts[2])
                )
                .is_ok(),
                "Assertion-Signatur ungültig"
            );
            let header: Value = serde_json::from_slice(&b64url_decode(parts[0])).unwrap();
            assert_eq!(header["alg"], "RS256");
            assert_eq!(header["x5t"], x5t);
            let claims: Value = serde_json::from_slice(&b64url_decode(parts[1])).unwrap();
            assert_eq!(claims["iss"], CLIENT);
            assert_eq!(claims["sub"], CLIENT);
            assert!(
                claims["aud"]
                    .as_str()
                    .unwrap()
                    .ends_with(&format!("/{TENANT}/oauth2/v2.0/token"))
            );
        }
    }
    fake.forms.lock().unwrap().push(f);
    fake.issue("entra-tok", false).await
}

async fn imds_handler(
    State(fake): State<Arc<Fake>>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_eq!(headers["metadata"], "true");
    fake.mi
        .lock()
        .unwrap()
        .push((q.unwrap_or_default(), headers));
    fake.issue("mi-tok", true).await
}

async fn app_service_handler(
    State(fake): State<Arc<Fake>>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if headers
        .get("x-identity-header")
        .is_none_or(|h| h != "hdr-geheim")
    {
        return (StatusCode::UNAUTHORIZED, "{}").into_response();
    }
    fake.mi
        .lock()
        .unwrap()
        .push((q.unwrap_or_default(), headers));
    fake.token_requests.fetch_add(1, Ordering::SeqCst);
    let on = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    (
        [("content-type", "application/json")],
        json!({"access_token": "as-tok-1", "expires_on": on.to_string(), "token_type": "Bearer"})
            .to_string(),
    )
        .into_response()
}

/// Azure-Arc-Agent: ohne `Authorization` 401 mit `WWW-Authenticate: Basic realm=<Datei>`, mit
/// dem Inhalt der Datei ein Token.
async fn arc_handler(
    State(fake): State<Arc<Fake>>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    assert_eq!(headers["metadata"], "true");
    let auth = headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_owned());
    fake.arc_auth.lock().unwrap().push(auth.clone());
    let (realm, key) = fake.arc.lock().unwrap().clone().unwrap();
    match auth {
        None => (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", format!("Basic realm={realm}"))],
            "",
        )
            .into_response(),
        Some(a) if a == format!("Basic {key}") => {
            fake.mi
                .lock()
                .unwrap()
                .push((q.unwrap_or_default(), headers));
            fake.issue("arc-tok", true).await
        }
        Some(_) => (StatusCode::UNAUTHORIZED, "{}").into_response(),
    }
}

async fn model_handler(State(fake): State<Arc<Fake>>, headers: HeaderMap) -> Response {
    let get = |k: &str| headers.get(k).map(|v| v.to_str().unwrap().to_owned());
    fake.model_auth
        .lock()
        .unwrap()
        .push((get("authorization"), get("api-key")));
    (
        [("content-type", "application/json")],
        json!({
            "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hallo Welt"},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
        })
        .to_string(),
    )
        .into_response()
}

async fn env() -> (SocketAddr, Arc<Fake>) {
    let fake = Fake::new();
    let app = Router::new()
        .route("/{tenant}/oauth2/v2.0/token", post(token_handler))
        .route("/metadata/identity/oauth2/token", get(imds_handler))
        .route("/msi/token", get(app_service_handler))
        .route("/arc/{case}", get(arc_handler))
        .route(
            "/openai/deployments/gpt-4o/chat/completions",
            post(model_handler),
        )
        .with_state(fake.clone());
    (serve(app).await, fake)
}

fn secret_creds(secret: &str) -> Vec<(&'static str, String)> {
    vec![
        ("tenantId", TENANT.to_owned()),
        ("clientId", CLIENT.to_owned()),
        ("clientSecret", secret.to_owned()),
    ]
}

fn secret_deployment(addr: SocketAddr, secret: &str) -> DeploymentCtx {
    let creds = secret_creds(secret);
    let creds: Vec<(&str, &str)> = creds.iter().map(|(k, v)| (*k, v.as_str())).collect();
    azure(
        &format!("http://{addr}"),
        &creds,
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}")}),
    )
}

fn upstream(adapter: &OpenAiAdapter) -> Upstream {
    Upstream::new(
        "aoai",
        Arc::new(adapter.clone()),
        UpstreamClient::unguarded().unwrap(),
        fast(),
    )
}

fn source(adapter: &OpenAiAdapter) -> &Arc<EntraTokenSource> {
    adapter.entra_source().unwrap()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn client_secret_bearer_and_shared_cache_per_credential() {
    let (addr, fake) = env().await;
    let a = OpenAiAdapter::from_deployment(&secret_deployment(addr, "geheim-1")).unwrap();
    let up = upstream(&a);
    for _ in 0..3 {
        let r = up.chat(request()).await.unwrap();
        assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    }
    // Zweites Deployment mit denselben Zugangsdaten (z. B. nach Konfigurationswechsel): gleiche
    // Token-Quelle, kein weiterer Abruf.
    let b = OpenAiAdapter::from_deployment(&secret_deployment(addr, "geheim-1")).unwrap();
    assert!(Arc::ptr_eq(source(&a), source(&b)));
    upstream(&b).chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 1);
    // Rotiertes Secret = neue Quelle.
    let c = OpenAiAdapter::from_deployment(&secret_deployment(addr, "geheim-2")).unwrap();
    assert!(!Arc::ptr_eq(source(&a), source(&c)));

    let f = fake.forms.lock().unwrap()[0].clone();
    assert_eq!(f["scope"], SCOPE);
    assert_eq!(f["client_secret"], "geheim-1");
    let auth = fake.model_auth.lock().unwrap().clone();
    assert!(
        auth.iter()
            .all(|(b, k)| b.as_deref() == Some("Bearer entra-tok-1") && k.is_none())
    );
    // Keine Geheimnisse/Tokens in Debug-Ausgaben.
    let dbg = format!("{a:?} {:?}", source(&a));
    assert!(
        !dbg.contains("geheim-1") && !dbg.contains("entra-tok"),
        "{dbg}"
    );
    assert!(dbg.contains("clientSecret"));
}

#[tokio::test]
async fn parallel_requests_share_one_fetch() {
    let (addr, fake) = env().await;
    fake.delay_ms.store(200, Ordering::SeqCst);
    let a = OpenAiAdapter::from_deployment(&secret_deployment(addr, "parallel")).unwrap();
    let up = upstream(&a);
    let rs = futures_util::future::join_all((0..10).map(|_| up.chat(request()))).await;
    assert!(rs.iter().all(Result::is_ok));
    assert_eq!(fake.token_requests(), 1);
    assert_eq!(source(&a).fetch_count(), 1);
}

#[tokio::test]
async fn refresh_ahead_of_expiry_and_after_expiry() {
    let (addr, fake) = env().await;
    // 200 s Laufzeit: gültig, aber im Vorlauf (300 s) → Erneuerung im Hintergrund.
    fake.expires_in.store(200, Ordering::SeqCst);
    let a = OpenAiAdapter::from_deployment(&secret_deployment(addr, "refresh")).unwrap();
    let up = upstream(&a);
    up.chat(request()).await.unwrap();
    fake.expires_in.store(3600, Ordering::SeqCst);
    up.chat(request()).await.unwrap();
    wait_for(|| fake.token_requests() >= 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    up.chat(request()).await.unwrap();
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 2);
    assert_eq!(
        fake.model_auth.lock().unwrap().last().unwrap().0.as_deref(),
        Some("Bearer entra-tok-2")
    );

    // Abgelaufenes Token: die nächste Anfrage holt synchron ein neues.
    let (addr, fake) = env().await;
    fake.expires_in.store(1, Ordering::SeqCst);
    let a = OpenAiAdapter::from_deployment(&secret_deployment(addr, "expired")).unwrap();
    let up = upstream(&a);
    up.chat(request()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 2);
    assert_eq!(
        fake.model_auth.lock().unwrap().last().unwrap().0.as_deref(),
        Some("Bearer entra-tok-2")
    );
}

#[tokio::test]
async fn token_errors_are_classified_and_shared() {
    // Abgelehnte Zugangsdaten: Auth, nicht retrybar, keine Modell-Anfrage, kein Secret im Fehler.
    let (addr, fake) = env().await;
    fake.token_status.store(401, Ordering::SeqCst);
    fake.delay_ms.store(150, Ordering::SeqCst);
    let a = OpenAiAdapter::from_deployment(&secret_deployment(addr, "falsch")).unwrap();
    let up = upstream(&a);
    let rs = futures_util::future::join_all((0..5).map(|_| up.chat(request()))).await;
    // Parallel wartende Anfragen teilen sich Abruf und Fehler.
    assert_eq!(fake.token_requests(), 1);
    for r in rs {
        let e = r.unwrap_err();
        assert_eq!(e.class, ErrorClass::Auth);
        assert!(!e.retryable);
        assert_eq!(e.upstream_status, Some(401));
        assert_eq!(e.error.status(), 502);
        assert_eq!(
            e.error.code.as_deref(),
            Some("upstream_authentication_error")
        );
        assert!(!e.error.message.contains("falsch"));
        assert!(!e.error.message.contains("AADSTS"));
    }
    assert!(fake.model_auth.lock().unwrap().is_empty());
    // Spätere Anfrage versucht es erneut.
    fake.delay_ms.store(0, Ordering::SeqCst);
    fake.token_status.store(0, Ordering::SeqCst);
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 2);

    // Token-Endpunkt gestört: retrybar (Fallback auf andere Deployments möglich).
    for (status, class) in [
        (503, ErrorClass::ServerError),
        (429, ErrorClass::RateLimit),
        (400, ErrorClass::Auth),
    ] {
        let (addr, fake) = env().await;
        fake.token_status.store(status, Ordering::SeqCst);
        let a = OpenAiAdapter::from_deployment(&secret_deployment(addr, &format!("s-{status}")))
            .unwrap();
        let e = upstream(&a).chat(request()).await.unwrap_err();
        assert_eq!(e.class, class, "{status}");
        assert_eq!(e.retryable, class != ErrorClass::Auth, "{status}");
    }

    // Nicht erreichbar: Verbindungsfehler (retrybar).
    let d = azure(
        "http://127.0.0.1:9",
        &[
            ("tenantId", TENANT),
            ("clientId", CLIENT),
            ("clientSecret", "x"),
        ],
        json!({"auth": "entraId", "entraAuthority": "http://127.0.0.1:9"}),
    );
    let a = OpenAiAdapter::from_deployment(&d).unwrap();
    let e = upstream(&a).chat(request()).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Connect);
    assert!(e.retryable);
}

#[tokio::test]
async fn client_certificate_assertion() {
    let (addr, fake) = env().await;
    let kp = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
    let der = kp.as_der().unwrap();
    let cert = vec![0x30u8, 0x03, 0x02, 0x01, 0x2a];
    let x5t = b64url(digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &cert).as_ref());
    *fake.cert.lock().unwrap() = Some((kp.public_key().as_ref().to_vec(), x5t));
    let pem = format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n\
         -----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        b64(&cert),
        b64(der.as_ref())
    );
    let d = azure(
        &format!("http://{addr}"),
        &[
            ("tenantId", TENANT),
            ("clientId", CLIENT),
            ("clientCertificate", &pem),
        ],
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}/")}),
    );
    let a = OpenAiAdapter::from_deployment(&d).unwrap();
    assert_eq!(source(&a).fetcher().method_name(), "clientCertificate");
    upstream(&a).chat(request()).await.unwrap();
    let f = fake.forms.lock().unwrap()[0].clone();
    assert!(f.contains_key("client_assertion"));
    assert!(!f.contains_key("client_secret"));
    assert!(!format!("{a:?}").contains("PRIVATE"));
}

fn temp_file(name: &str, content: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aigw-entra-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("token");
    std::fs::write(&p, content).unwrap();
    p
}

#[tokio::test]
async fn workload_identity_reads_federated_token_each_fetch() {
    let (addr, fake) = env().await;
    fake.expires_in.store(1, Ordering::SeqCst);
    let file = temp_file("wi", "föderiert-1\n");
    let env = EntraEnv {
        tenant_id: Some(TENANT.into()),
        client_id: Some(CLIENT.into()),
        federated_token_file: Some(file.clone()),
        authority_host: Some(format!("http://{addr}")),
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let d = azure(
        &format!("http://{addr}"),
        &[],
        json!({"auth": "entraId", "entraCredential": "workloadIdentity"}),
    );
    let a = OpenAiAdapter::from_deployment_with_entra_env(&d, &env).unwrap();
    let up = upstream(&a);
    up.chat(request()).await.unwrap();
    std::fs::write(&file, "föderiert-2").unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    up.chat(request()).await.unwrap();
    let forms = fake.forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 2);
    assert_eq!(forms[0]["client_assertion"], "föderiert-1");
    assert_eq!(forms[1]["client_assertion"], "föderiert-2");
    assert_eq!(forms[0]["scope"], SCOPE);

    // Fehlende Datei: Auth-Fehler.
    std::fs::remove_file(&file).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let e = up.chat(request()).await.unwrap_err();
    assert_eq!(e.class, ErrorClass::Auth);

    // Authority bei Workload Identity nur aus der Umgebung; Scope eingeschränkt.
    for settings in [
        json!({"auth": "entraId", "entraCredential": "workloadIdentity",
               "entraAuthority": "https://evil.example"}),
        json!({"auth": "entraId", "entraCredential": "workloadIdentity",
               "entraScope": "https://management.azure.com/.default"}),
    ] {
        let d = azure("https://res.openai.azure.com", &[], settings);
        assert!(OpenAiAdapter::from_deployment_with_entra_env(&d, &env).is_err());
    }
    let d = azure(
        "https://res.openai.azure.com",
        &[],
        json!({"auth": "entraId", "entraCredential": "workloadIdentity"}),
    );
    let allowed = EntraEnv {
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let e = OpenAiAdapter::from_deployment_with_entra_env(&d, &allowed).unwrap_err();
    assert!(e.to_string().contains("AZURE_FEDERATED_TOKEN_FILE"), "{e}");
}

/// AIGW-90: Workload/Managed Identity nur mit `upstream.entra.ambient_identity: true` – sonst
/// Fehler beim Aufbau mit dem Schlüssel in der Meldung; Service Principals sind nicht betroffen.
#[test]
fn ambient_identity_requires_opt_in() {
    for method in ["workloadIdentity", "managedIdentity"] {
        let d = azure(
            "https://res.openai.azure.com",
            &[],
            json!({"auth": "entraId", "entraCredential": method}),
        );
        let off = EntraEnv {
            federated_token_file: Some("/var/run/token".into()),
            tenant_id: Some(TENANT.into()),
            client_id: Some(CLIENT.into()),
            ..EntraEnv::default()
        };
        let e = OpenAiAdapter::from_deployment_with_entra_env(&d, &off)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("upstream.entra.ambient_identity") && e.contains(method),
            "{method}: {e}"
        );
        let on = EntraEnv {
            ambient_identity: true,
            ..off
        };
        OpenAiAdapter::from_deployment_with_entra_env(&d, &on).unwrap();
    }
    // Prozessweite Freigabe: Standard aus.
    assert!(!aigw_providers::entra::ambient_identity());
    let sp = azure(
        "https://res.openai.azure.com",
        &[
            ("tenantId", TENANT),
            ("clientId", CLIENT),
            ("clientSecret", "geheim"),
        ],
        json!({"auth": "entraId"}),
    );
    OpenAiAdapter::from_deployment_with_entra_env(&sp, &EntraEnv::default()).unwrap();
}

/// Client wie im Gateway: strenge SSRF-Regeln, nur der Name `aoai.test` (→ 127.0.0.1) ist für
/// das Modell freigeschaltet. Der IMDS-Fake auf `127.0.0.1` ist nur über die gezielte Ausnahme
/// der Managed Identity erreichbar.
fn guarded_client() -> UpstreamClient {
    let policy = UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["aoai.test".into()],
        allow_private_upstreams: vec!["aoai.test".into()],
        ..AdminConfig::default()
    })
    .unwrap();
    let outbound = OutboundClient::builder(AddressGuard::policy(policy))
        .resolver(Arc::new(
            StaticResolver::new().with("aoai.test", [IpAddr::V4(Ipv4Addr::LOCALHOST)]),
        ))
        .build()
        .unwrap();
    UpstreamClient::from_outbound(outbound)
}

#[tokio::test]
async fn managed_identity_imds_with_targeted_ssrf_exemption() {
    let (addr, fake) = env().await;
    let env = EntraEnv {
        imds_endpoint: Some(format!("http://{addr}/metadata/identity/oauth2/token")),
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let model = format!("http://aoai.test:{}", addr.port());
    // Benutzerzugewiesene Identität über settings.entraClientId.
    let d = azure(
        &model,
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity", "entraClientId": CLIENT}),
    );
    let a = OpenAiAdapter::from_deployment_with_entra_env(&d, &env).unwrap();
    let up = Upstream::new("aoai", Arc::new(a.clone()), guarded_client(), fast());
    let r = up.chat(request()).await.unwrap();
    assert_eq!(r.choices[0].message.content.text(), "Hallo Welt");
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 1);
    let (q, _) = fake.mi.lock().unwrap()[0].clone();
    let q = form(&q);
    assert_eq!(q["api-version"], "2018-02-01");
    assert_eq!(q["resource"], "https://cognitiveservices.azure.com");
    assert_eq!(q["client_id"], CLIENT);
    assert_eq!(
        fake.model_auth.lock().unwrap()[0].0.as_deref(),
        Some("Bearer mi-tok-1")
    );

    // Ohne Ausnahme wäre der Metadaten-Endpunkt gesperrt: derselbe Client erreicht 127.0.0.1
    // für das Modell nicht.
    let d = azure(
        &format!("http://{addr}"),
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
    );
    let b = OpenAiAdapter::from_deployment_with_entra_env(&d, &env).unwrap();
    let up = Upstream::new("aoai2", Arc::new(b), guarded_client(), fast());
    let e = up.chat(request()).await.unwrap_err();
    assert_ne!(
        e.class,
        ErrorClass::Auth,
        "Token klappt, Modell-Ziel bleibt gesperrt"
    );
    assert_eq!(fake.token_requests(), 2);
    // Nur die beiden Anfragen des ersten Deployments kamen beim Modell an.
    assert_eq!(fake.model_auth.lock().unwrap().len(), 2);

    // Metadatendienst noch nicht bereit (404) → retrybar.
    let env404 = EntraEnv {
        imds_endpoint: Some(format!("http://{addr}/gibt-es-nicht")),
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let d = azure(
        &model,
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity",
               "entraClientId": "andere-id"}),
    );
    let c = OpenAiAdapter::from_deployment_with_entra_env(&d, &env404).unwrap();
    let e = Upstream::new("aoai3", Arc::new(c), guarded_client(), fast())
        .chat(request())
        .await
        .unwrap_err();
    assert_eq!(e.class, ErrorClass::ServerError);
    assert!(e.retryable);
}

#[tokio::test]
async fn managed_identity_app_service() {
    let (addr, fake) = env().await;
    let env = EntraEnv {
        identity_endpoint: Some(format!("http://{addr}/msi/token")),
        identity_header: Some(SecretString::new("hdr-geheim")),
        ambient_identity: true,
        ..EntraEnv::default()
    };
    let d = azure(
        &format!("http://aoai.test:{}", addr.port()),
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
    );
    let a = OpenAiAdapter::from_deployment_with_entra_env(&d, &env).unwrap();
    assert!(!format!("{a:?}").contains("hdr-geheim"));
    let up = Upstream::new("aoai", Arc::new(a), guarded_client(), fast());
    up.chat(request()).await.unwrap();
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 1);
    let (q, _) = fake.mi.lock().unwrap()[0].clone();
    let q = form(&q);
    assert_eq!(q["api-version"], "2019-08-01");
    assert!(!q.contains_key("client_id"));
    assert_eq!(
        fake.model_auth.lock().unwrap()[0].0.as_deref(),
        Some("Bearer as-tok-1")
    );
}

#[test]
fn configuration_errors() {
    let ep = "https://res.openai.azure.com";
    let full = [
        ("tenantId", TENANT),
        ("clientId", CLIENT),
        ("clientSecret", "x"),
    ];
    type Case<'a> = (Vec<(&'a str, &'a str)>, Value, &'a str);
    let cases: Vec<Case<'_>> = vec![
        (vec![], json!({"auth": "entraId"}), "entraCredential"),
        (
            full.to_vec(),
            json!({"auth": "entraId", "entraCredential": "zauberei"}),
            "unbekannt",
        ),
        (
            vec![("clientId", CLIENT), ("clientSecret", "x")],
            json!({"auth": "entraId"}),
            "tenantId",
        ),
        (
            vec![
                ("tenantId", "../evil"),
                ("clientId", CLIENT),
                ("clientSecret", "x"),
            ],
            json!({"auth": "entraId"}),
            "tenantId",
        ),
        (
            full.to_vec(),
            json!({"auth": "entraId", "entraAuthority": "https://login.microsoftonline.us/x?y"}),
            "entraAuthority",
        ),
        (
            vec![
                ("tenantId", TENANT),
                ("clientId", CLIENT),
                ("clientCertificate", "kein pem"),
            ],
            json!({"auth": "entraId"}),
            "clientCertificate",
        ),
        (
            vec![],
            json!({"auth": "entraId", "entraCredential": "managedIdentity",
                   "entraScope": "https://vault.azure.net/.default"}),
            "entraScope",
        ),
    ];
    for (creds, settings, needle) in cases {
        let d = azure(ep, &creds, settings.clone());
        let e = OpenAiAdapter::from_deployment_with_entra_env(&d, &EntraEnv::default())
            .unwrap_err()
            .to_string();
        assert!(e.contains(needle), "{settings}: {e}");
    }
    // Sovereign Cloud: eigene Authority, eigener Scope.
    let d = azure(
        "https://res.openai.azure.us",
        &full,
        json!({"auth": "entraId", "entraAuthority": "https://login.microsoftonline.us",
               "entraScope": "https://cognitiveservices.azure.us/.default"}),
    );
    let a = OpenAiAdapter::from_deployment(&d).unwrap();
    assert_eq!(
        a.entra_source().unwrap().fetcher().token_url().to_string(),
        format!("https://login.microsoftonline.us/{TENANT}/oauth2/v2.0/token")
    );
}

// ---------------------------------------------------------------------------------------------
// AIGW-94: verschlüsselte Zertifikatsschlüssel, x5c, Azure Arc
// ---------------------------------------------------------------------------------------------

/// Testschlüssel (nur für Tests, mit OpenSSL 3 erzeugt, Passwort `test-passwort`):
/// `client-aes256-sha256.testpem` = Blatt + zweites Zertifikat + `openssl pkcs8 -topk8 -v2
/// aes-256-cbc -v2prf hmacWithSHA256`; `client-aes128-sha1.testpem` = `-v2 aes-128-cbc -v2prf
/// hmacWithSHA1`; `client-pbes1.testpem` = `-v1 PBE-SHA1-3DES`; `client-legacy-openssl.testpem` =
/// `openssl rsa -aes256 -traditional`; `public-key.der` = öffentlicher Schlüssel (PKCS#1).
const FIX_AES256: &str = include_str!("fixtures/entra/client-aes256-sha256.testpem");
const FIX_AES128: &str = include_str!("fixtures/entra/client-aes128-sha1.testpem");
const FIX_PBES1: &str = include_str!("fixtures/entra/client-pbes1.testpem");
const FIX_LEGACY: &str = include_str!("fixtures/entra/client-legacy-openssl.testpem");
const FIX_PUBLIC: &[u8] = include_bytes!("fixtures/entra/public-key.der");
const FIX_PASSWORD: &str = "test-passwort";

/// DER aller `CERTIFICATE`-Blöcke.
fn fixture_certs(pem: &str) -> Vec<Vec<u8>> {
    pem.split("-----BEGIN CERTIFICATE-----")
        .skip(1)
        .map(|b| {
            let body = &b[..b.find("-----END CERTIFICATE-----").unwrap()];
            b64_decode(body)
        })
        .collect()
}

fn b64_decode(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    b64url_decode(
        clean
            .trim_end_matches('=')
            .replace('+', "-")
            .replace('/', "_")
            .as_str(),
    )
}

fn cert_deployment(
    addr: SocketAddr,
    pem: &str,
    password: Option<&str>,
    x5c: bool,
) -> DeploymentCtx {
    let mut creds = vec![
        ("tenantId", TENANT),
        ("clientId", CLIENT),
        ("clientCertificate", pem),
    ];
    if let Some(p) = password {
        creds.push(("clientCertificatePassword", p));
    }
    azure(
        &format!("http://{addr}"),
        &creds,
        json!({"auth": "entraId", "entraAuthority": format!("http://{addr}"),
               "entraSendX5c": x5c}),
    )
}

#[tokio::test]
async fn encrypted_client_certificate_with_x5c() {
    let (addr, fake) = env().await;
    let certs = fixture_certs(FIX_AES256);
    assert_eq!(certs.len(), 2);
    let x5t = b64url(digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &certs[0]).as_ref());
    *fake.cert.lock().unwrap() = Some((FIX_PUBLIC.to_vec(), x5t));

    // AES-256/HMAC-SHA256 mit x5c (Kette in PEM-Reihenfolge, Standard-Base64).
    let a = OpenAiAdapter::from_deployment(&cert_deployment(
        addr,
        FIX_AES256,
        Some(FIX_PASSWORD),
        true,
    ))
    .unwrap();
    assert!(!format!("{a:?}").contains(FIX_PASSWORD));
    upstream(&a).chat(request()).await.unwrap();
    let f = fake.forms.lock().unwrap()[0].clone();
    let header: Value = serde_json::from_slice(&b64url_decode(
        f["client_assertion"].split('.').next().unwrap(),
    ))
    .unwrap();
    assert_eq!(header["x5c"], json!([b64(&certs[0]), b64(&certs[1])]));

    // AES-128/HMAC-SHA1 (PRF-Standardwert) ohne x5c.
    let b = OpenAiAdapter::from_deployment(&cert_deployment(
        addr,
        FIX_AES128,
        Some(FIX_PASSWORD),
        false,
    ))
    .unwrap();
    upstream(&b).chat(request()).await.unwrap();
    let f = fake.forms.lock().unwrap()[1].clone();
    let header: Value = serde_json::from_slice(&b64url_decode(
        f["client_assertion"].split('.').next().unwrap(),
    ))
    .unwrap();
    assert!(header.get("x5c").is_none(), "{header}");
    // Signaturen hat der Fake-Token-Endpunkt geprüft; beide Tokens kamen beim Modell an.
    assert_eq!(fake.model_auth.lock().unwrap().len(), 2);
}

#[test]
fn encrypted_client_certificate_errors_without_secrets() {
    let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let err = |pem: &str, pw: Option<&str>| {
        OpenAiAdapter::from_deployment(&cert_deployment(addr, pem, pw, false))
            .unwrap_err()
            .to_string()
    };
    let e = err(FIX_AES256, None);
    assert!(e.contains("clientCertificatePassword"), "{e}");
    let e = err(FIX_AES256, Some("falsches-passwort-xyz"));
    assert!(e.contains("Passwort falsch"), "{e}");
    assert!(!e.contains("falsches-passwort-xyz"), "{e}");
    let e = err(FIX_PBES1, Some(FIX_PASSWORD));
    assert!(
        e.contains("nicht unterstützt") && e.contains("PBES1"),
        "{e}"
    );
    assert!(!e.contains(FIX_PASSWORD), "{e}");
    let e = err(FIX_LEGACY, None);
    assert!(e.contains("pkcs8"), "{e}");
    // Passwort bei anderer Anmeldung: Konfigurationsfehler.
    let d = azure(
        "https://res.openai.azure.com",
        &[
            ("tenantId", TENANT),
            ("clientId", CLIENT),
            ("clientSecret", "x"),
            ("clientCertificatePassword", "pw"),
        ],
        json!({"auth": "entraId", "entraCredential": "clientSecret"}),
    );
    let e = OpenAiAdapter::from_deployment(&d).unwrap_err().to_string();
    assert!(e.contains("clientCertificatePassword"), "{e}");
}

fn arc_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aigw-arc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tokens")).unwrap();
    dir
}

fn arc_env(addr: SocketAddr, dir: &std::path::Path) -> EntraEnv {
    EntraEnv {
        identity_endpoint: Some(format!("http://{addr}/arc/token")),
        arc_imds_endpoint: Some("http://localhost:40342".into()),
        arc_token_dir: Some(dir.join("tokens")),
        ambient_identity: true,
        ..EntraEnv::default()
    }
}

#[tokio::test]
async fn managed_identity_azure_arc_challenge() {
    let (addr, fake) = env().await;
    let dir = arc_dir("ok");
    let key_file = dir.join("tokens").join("abc.key");
    std::fs::write(&key_file, "arc-geheim-42").unwrap();
    *fake.arc.lock().unwrap() = Some((key_file.display().to_string(), "arc-geheim-42".into()));
    let d = azure(
        &format!("http://aoai.test:{}", addr.port()),
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
    );
    let a = OpenAiAdapter::from_deployment_with_entra_env(&d, &arc_env(addr, &dir)).unwrap();
    // Strenger Client: der Arc-Endpunkt (127.0.0.1) nur über die gezielte Ausnahme.
    let up = Upstream::new("aoai", Arc::new(a), guarded_client(), fast());
    up.chat(request()).await.unwrap();
    up.chat(request()).await.unwrap();
    assert_eq!(fake.token_requests(), 1);
    assert_eq!(
        *fake.arc_auth.lock().unwrap(),
        vec![None, Some("Basic arc-geheim-42".to_owned())]
    );
    let (q, _) = fake.mi.lock().unwrap()[0].clone();
    let q = form(&q);
    assert_eq!(q["api-version"], "2020-06-01");
    assert_eq!(q["resource"], "https://cognitiveservices.azure.com");
    assert_eq!(
        fake.model_auth.lock().unwrap()[0].0.as_deref(),
        Some("Bearer arc-tok-1")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn azure_arc_rejects_key_files_outside_the_agent_directory() {
    let (addr, fake) = env().await;
    let dir = arc_dir("bad");
    let outside = dir.join("anderswo.key");
    std::fs::write(&outside, "fremd").unwrap();
    let inside_txt = dir.join("tokens").join("x.txt");
    std::fs::write(&inside_txt, "fremd").unwrap();
    for (i, realm) in [
        outside.display().to_string(),
        dir.join("tokens")
            .join("..")
            .join("anderswo.key")
            .display()
            .to_string(),
        inside_txt.display().to_string(),
        "abc.key".to_owned(),
        dir.join("tokens").join("fehlt.key").display().to_string(),
    ]
    .into_iter()
    .enumerate()
    {
        *fake.arc.lock().unwrap() = Some((realm.clone(), "fremd".into()));
        // Eigener Endpunktpfad je Fall = eigene Token-Quelle (kein geteilter Fehler).
        let d = azure(
            &format!("http://aoai.test:{}", addr.port()),
            &[],
            json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
        );
        let env = EntraEnv {
            identity_endpoint: Some(format!("http://{addr}/arc/fall-{i}")),
            ..arc_env(addr, &dir)
        };
        let a = OpenAiAdapter::from_deployment_with_entra_env(&d, &env).unwrap();
        let e = Upstream::new("aoai", Arc::new(a), guarded_client(), fast())
            .chat(request())
            .await
            .unwrap_err();
        assert_eq!(e.class, ErrorClass::Auth, "{realm}");
        assert!(
            !e.error.message.contains("fremd"),
            "{realm}: {}",
            e.error.message
        );
    }
    // Nie mit dem Inhalt einer fremden Datei nachgefragt.
    assert!(fake.arc_auth.lock().unwrap().iter().all(Option::is_none));
    assert_eq!(fake.token_requests(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn azure_arc_configuration() {
    let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let dir = std::env::temp_dir();
    // Benutzerzugewiesene Identität gibt es unter Arc nicht.
    let d = azure(
        "https://res.openai.azure.com",
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity", "entraClientId": CLIENT}),
    );
    let e = OpenAiAdapter::from_deployment_with_entra_env(&d, &arc_env(addr, &dir))
        .unwrap_err()
        .to_string();
    assert!(e.contains("Azure Arc"), "{e}");
    // Ohne Freigabe (upstream.entra.ambient_identity) auch unter Arc nicht.
    let off = EntraEnv {
        ambient_identity: false,
        ..arc_env(addr, &dir)
    };
    let d = azure(
        "https://res.openai.azure.com",
        &[],
        json!({"auth": "entraId", "entraCredential": "managedIdentity"}),
    );
    let e = OpenAiAdapter::from_deployment_with_entra_env(&d, &off)
        .unwrap_err()
        .to_string();
    assert!(e.contains("upstream.entra.ambient_identity"), "{e}");
    // App Service hat Vorrang, wenn IDENTITY_HEADER gesetzt ist.
    let both = EntraEnv {
        identity_header: Some(SecretString::new("h")),
        ..arc_env(addr, &dir)
    };
    let a = OpenAiAdapter::from_deployment_with_entra_env(&d, &both).unwrap();
    assert_eq!(
        a.entra_source().unwrap().fetcher().method_name(),
        "managedIdentity"
    );
}
