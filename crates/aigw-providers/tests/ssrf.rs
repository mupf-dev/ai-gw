// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! SSRF-Schutz des Anbieter-Clients nach der Namensauflösung (AIGW-72, F63): Ein Deployment,
//! dessen (öffentlich wirkender) Name auf eine private Adresse zeigt – etwa nach
//! DNS-Rebinding –, wird nicht verbunden; freigeschaltete Namen/Netze funktionieren.
//! Dazu `settings.caFile` (F48): eigene CA je Deployment, Fehler bei unlesbarer Datei.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use aigw_core::config::AdminConfig;
use aigw_core::ir::{ChatRequest, ErrorClass, Message, Role};
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::upstream::UpstreamPolicy;
use aigw_outbound::{AddressGuard, OutboundClient, StaticResolver};
use aigw_providers::{Registry, Upstream, UpstreamClient};
use axum::Router;
use axum::routing::post;
use serde_json::json;

const HOST: &str = "llm.example.com";

async fn fake_openai() -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = Arc::clone(&hits);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let h = Arc::clone(&h);
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "application/json")],
                    json!({"id": "c1", "object": "chat.completion", "created": 1, "model": "m",
                           "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                                        "finish_reason": "stop"}],
                           "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
                    .to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, hits)
}

fn deployment(port: u16, settings: serde_json::Value) -> DeploymentCtx {
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named("dep"),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider: ProviderKind::OpenAiCompatible,
                endpoint: Some(format!("http://{HOST}:{port}/v1")),
                provider_model: "m".into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings: settings
                    .as_object()
                    .cloned()
                    .unwrap_or_else(JsonObject::new),
                budget: None,
            },
        },
        credential: Some(Arc::new(CredentialData(BTreeMap::from([(
            "apiKey".to_owned(),
            SecretString::new("sk"),
        )])))),
    }
}

fn policy(private: &[&str]) -> UpstreamPolicy {
    UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec![HOST.into()],
        allow_private_upstreams: private.iter().map(|s| (*s).to_owned()).collect(),
        ..AdminConfig::default()
    })
    .unwrap()
}

/// Anbieter-Client, dessen Resolver `llm.example.com` auf 127.0.0.1 abbildet.
fn client(policy: UpstreamPolicy) -> UpstreamClient {
    UpstreamClient::from_outbound(
        OutboundClient::builder(AddressGuard::policy(policy))
            .resolver(Arc::new(
                StaticResolver::new().with(HOST, [IpAddr::V4(Ipv4Addr::LOCALHOST)]),
            ))
            .build()
            .unwrap(),
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "g".into(),
        messages: vec![Message::text(Role::User, "Hallo?")],
        ..ChatRequest::default()
    }
}

#[tokio::test]
async fn provider_rejects_name_resolving_to_private_address() {
    let (addr, hits) = fake_openai().await;
    let dep = deployment(addr.port(), json!({}));
    // Die URL-Prüfung (Admin-API/GitOps) sieht nur den Namen – sie lässt ihn durch.
    let strict = policy(&[]);
    strict
        .check_endpoint(&format!("http://{HOST}:{}/v1", addr.port()))
        .unwrap();
    let up = Upstream::from_deployment(&dep, &Registry::with_builtin(), &client(strict)).unwrap();
    let err = up.chat(request()).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Connect);
    assert_eq!(err.error.code.as_deref(), Some("upstream_address_blocked"));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "keine Verbindung zum Ziel");
}

#[tokio::test]
async fn provider_connects_to_allowed_hosts() {
    let (addr, hits) = fake_openai().await;
    let dep = deployment(addr.port(), json!({}));
    for allowed in [&[HOST][..], &["127.0.0.0/8"], &["*.example.com"]] {
        let up =
            Upstream::from_deployment(&dep, &Registry::with_builtin(), &client(policy(allowed)))
                .unwrap();
        let resp = up.chat(request()).await.unwrap();
        assert_eq!(resp.usage.unwrap().input_tokens, 1, "{allowed:?}");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn deployment_ca_file_must_be_readable() {
    let (addr, _) = fake_openai().await;
    let dep = deployment(addr.port(), json!({"caFile": "/nonexistent/aigw-ca.pem"}));
    // Ohne upstream.ca_dir ist caFile verboten (F177).
    let err = Upstream::from_deployment(&dep, &Registry::with_builtin(), &client(policy(&[HOST])))
        .unwrap_err()
        .to_string();
    assert!(err.contains("settings.caFile"), "{err}");
    assert!(err.contains("ca_dir"), "{err}");
    // Mit ca_dir: Datei außerhalb bzw. fehlend wird abgelehnt, Symlink hinaus ebenso.
    let root = std::env::temp_dir().join(format!("aigw-ssrf-ca-{}", std::process::id()));
    let dir = root.join("ca");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(root.join("outside.pem"), "x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("outside.pem"), dir.join("link.pem")).unwrap();
    let with_dir = client(policy(&[HOST])).with_ca_dir(Some(dir.clone()));
    for (path, needle) in [
        ("/nonexistent/aigw-ca.pem", "nicht im erlaubten Verzeichnis"),
        ("fehlt.pem", "fehlt.pem"),
        #[cfg(unix)]
        ("link.pem", "Symlink"),
    ] {
        let dep = deployment(addr.port(), json!({ "caFile": path }));
        let err = Upstream::from_deployment(&dep, &Registry::with_builtin(), &with_dir)
            .unwrap_err()
            .to_string();
        assert!(err.contains("settings.caFile"), "{err}");
        assert!(err.contains(needle), "{path}: {err}");
    }
    std::fs::remove_dir_all(&root).unwrap();
}
