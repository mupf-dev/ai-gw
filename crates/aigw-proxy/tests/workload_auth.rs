// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! AIGW-115: Anmeldung der Datenebene mit Access-Tokens von Workload-/Agent-Identitäten
//! (Microsoft Entra Agent ID) gegen einen lokalen JWKS-Server: Zuordnung zum Key, `401`/`403`,
//! unveränderte Wirkung von Modellfreigabe und Limits, Attribution im Request-Log, API-Keys
//! funktionieren weiter.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use aigw_core::observe::{RequestObserver, RequestRecord};
use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, LimitMetric, LimitSpec, LimitWindow,
    ModelGroupSpec, Object, ObjectMeta, OrganizationSpec, new_id,
};
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx, Snapshot, SnapshotHandle};
use aigw_core::workload::{
    JwtAuthConfig, JwtIssuerConfig, JwtProfile, WorkloadIdentity, WorkloadTokenKind,
};
use aigw_jwt::testkit::{JwksServer, TestIssuer, entra_agent_claims};
use aigw_proxy::auth::hash_key;
use aigw_proxy::{ProxyState, SnapshotAuthenticator, WorkloadVerifier};
use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

const TID: &str = "aaaabbbb-0000-cccc-1111-dddd2222eeee";
const AGENT: &str = "11112222-bbbb-3333-cccc-4444dddd5555";
const LIMITED_AGENT: &str = "22223333-bbbb-3333-cccc-4444dddd5555";
const UNKNOWN_AGENT: &str = "99998888-bbbb-3333-cccc-4444dddd5555";
const AUD: &str = "api://ai-gw";
const SECRET: &str = "sk-aigw-agentkey";

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<RequestRecord>>);

impl RequestObserver for Capture {
    fn record(&self, record: RequestRecord) {
        self.0.lock().unwrap().push(record);
    }
}

fn obj<S>(name: &str, spec: S) -> Object<S> {
    Object {
        id: new_id(),
        metadata: ObjectMeta::named(name),
        spec,
    }
}

fn key(name: &str, spec: ApiKeySpec) -> Arc<KeyCtx> {
    Arc::new(KeyCtx {
        key: ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec,
            status: ApiKeyStatus {
                prefix: "sk-aigw-x".into(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        },
        team: None,
        organization: Some(Arc::new(obj("o", OrganizationSpec::default()))),
        expires_at: None,
    })
}

fn identity(subject: &str) -> WorkloadIdentity {
    WorkloadIdentity {
        issuer: "entra".into(),
        subject: subject.into(),
        description: Some("Test-Agent".into()),
    }
}

struct Env {
    addr: SocketAddr,
    issuer: TestIssuer,
    records: Arc<Capture>,
    agent_key_id: String,
}

async fn start() -> Env {
    let issuer = TestIssuer::rsa("kid-1");
    let jwks = JwksServer::start(issuer.jwks()).await;
    let verifier = WorkloadVerifier::new(&JwtAuthConfig {
        issuers: vec![JwtIssuerConfig {
            name: "entra".into(),
            profile: JwtProfile::Entra,
            tenant_id: Some(TID.into()),
            jwks_uri: Some(jwks.jwks_uri()),
            audiences: vec![AUD.into()],
            ..JwtIssuerConfig::default()
        }],
        ..JwtAuthConfig::default()
    })
    .unwrap()
    .map(Arc::new);

    let mut snap = Snapshot::default();
    for name in ["mock", "other"] {
        snap.model_groups.insert(
            name.into(),
            Arc::new(ModelGroupCtx {
                group: obj(name, ModelGroupSpec::default()),
                deployments: Vec::new(),
            }),
        );
    }
    let agent = key(
        "agent-key",
        ApiKeySpec {
            organization: "o".into(),
            allowed_models: vec!["mock".into()],
            workload_identities: vec![identity(AGENT)],
            ..ApiKeySpec::default()
        },
    );
    let agent_key_id = agent.key.id.to_string();
    let limited = key(
        "limited-key",
        ApiKeySpec {
            organization: "o".into(),
            limits: vec![LimitSpec {
                metric: LimitMetric::Requests,
                limit: 1,
                window: LimitWindow::Minute,
                model_group: None,
            }],
            workload_identities: vec![identity(LIMITED_AGENT)],
            ..ApiKeySpec::default()
        },
    );
    snap.keys.insert(hash_key(SECRET), agent);
    snap.keys.insert(hash_key("sk-aigw-limited"), limited);
    snap.rebuild_workload_index();
    let snapshot = SnapshotHandle::new(snap);

    let records = Arc::new(Capture::default());
    let state = ProxyState {
        auth: Arc::new(SnapshotAuthenticator::new(snapshot).with_jwt(verifier)),
        observer: Arc::clone(&records) as Arc<dyn RequestObserver>,
        ..ProxyState::mock()
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, aigw_proxy::router(state))
            .await
            .unwrap();
    });
    Env {
        addr,
        issuer,
        records,
        agent_key_id,
    }
}

impl Env {
    fn token(&self, agent: &str) -> String {
        self.issuer.sign(&entra_agent_claims(TID, agent, AUD))
    }

    async fn post(&self, path: &str, bearer: &str, body: Value) -> (StatusCode, Value) {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<bytes::Bytes>>();
        let mut req = hyper::Request::post(format!("http://{}{path}", self.addr))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {bearer}"));
        if path == "/v1/messages" {
            req = req.header("anthropic-version", "2023-06-01");
        }
        let res = client
            .request(
                req.body(Full::new(serde_json::to_vec(&body).unwrap().into()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn chat(&self, bearer: &str, model: &str) -> (StatusCode, Value) {
        self.post(
            "/v1/chat/completions",
            bearer,
            json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await
    }

    fn last(&self) -> RequestRecord {
        self.records.0.lock().unwrap().last().cloned().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_token_maps_to_key_and_is_attributed() {
    let env = start().await;
    let token = env.token(AGENT);
    let (status, body) = env.chat(&token, "mock").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rec = env.last();
    assert_eq!(
        rec.api_key_id.map(|i| i.to_string()),
        Some(env.agent_key_id.clone())
    );
    let w = rec.workload.expect("Attribution fehlt");
    assert_eq!(w.issuer, "entra");
    assert_eq!(w.subject, AGENT);
    assert_eq!(w.kind, WorkloadTokenKind::App);
    assert!(w.agent);
    assert_eq!(
        w.blueprint.as_deref(),
        Some("30cf4c22-9985-4ef7-8756-91cc888176bd")
    );

    // Anthropic-Eingang mit demselben Token.
    let (status, body) = env
        .post(
            "/v1/messages",
            &token,
            json!({"model": "mock", "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(env.last().workload.unwrap().subject, AGENT);

    // Der API-Key desselben Keys funktioniert weiter (ohne Workload-Attribution).
    let (status, _) = env.chat(SECRET, "mock").await;
    assert_eq!(status, StatusCode::OK);
    assert!(env.last().workload.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_rules_apply_unchanged() {
    let env = start().await;
    // Modellfreigabe des Keys gilt.
    let (status, body) = env.chat(&env.token(AGENT), "other").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "model_not_allowed");
    // Limits des Keys gelten (1 Anfrage/Minute).
    let t = env.token(LIMITED_AGENT);
    assert_eq!(env.chat(&t, "mock").await.0, StatusCode::OK);
    assert_eq!(env.chat(&t, "mock").await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_or_invalid_tokens_are_rejected() {
    let env = start().await;
    // Gültiges Token, aber Identität keinem Key zugeordnet → 403 ohne Details.
    let (status, body) = env.chat(&env.token(UNKNOWN_AGENT), "mock").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "identity_not_allowed");
    assert!(!body.to_string().contains(UNKNOWN_AGENT));

    // Falsche Zielgruppe → 401.
    let wrong_aud = env
        .issuer
        .sign(&entra_agent_claims(TID, AGENT, "api://anderes-api"));
    let (status, body) = env.chat(&wrong_aud, "mock").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "invalid_api_key");

    // Fremder Signaturschlüssel mit gleicher kid → 401.
    let forged = TestIssuer::rsa("kid-1").sign(&entra_agent_claims(TID, AGENT, AUD));
    assert_eq!(env.chat(&forged, "mock").await.0, StatusCode::UNAUTHORIZED);

    // Abgelaufen → 401.
    let mut c = entra_agent_claims(TID, AGENT, AUD);
    c["exp"] = json!(aigw_jwt::now_unix() - 3600);
    assert_eq!(
        env.chat(&env.issuer.sign(&c), "mock").await.0,
        StatusCode::UNAUTHORIZED
    );

    // Token nicht im Request-Log, auch nicht in Fehlerantworten.
    for r in env.records.0.lock().unwrap().iter() {
        assert!(!format!("{r:?}").contains(&forged));
    }
}
