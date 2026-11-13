// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gegentest des OTLP-Exports gegen den offiziellen OpenTelemetry Collector (F103).
//!
//! Läuft nur mit `AIGW_TEST_OTELCOL_URL` (OTLP/HTTP-Basis-URL) und `AIGW_TEST_OTELCOL_DIR`
//! (Verzeichnis mit den file-Exportern `traces.json`/`logs.json`); `scripts/check.sh` startet
//! dafür mit `AIGW_TEST_OTELCOL=docker` einen `otel/opentelemetry-collector-contrib`-Container
//! in fester Version. Geprüft wird, was der Collector **dekodiert** hat (OTLP-JSON des
//! file-Exporters) – unabhängig vom eigenen Protobuf-Encoder: Ressource, Span-Baum
//! (Trace-/Parent-IDs, Art), GenAI-Attribute, Log-Einträge mit `event_name`.
//!
//! Single-Thread-Runtime, damit der Test-Subscriber (`set_default`, thread-lokal) auch für die
//! Server-Tasks gilt.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    missing_docs
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::{ObserveConfig, OtlpConfig, OtlpTracesConfig};
use aigw_core::ir::Usage;
use aigw_core::observe::RequestRecord;
use aigw_core::resource::{
    CredentialData, Deployment, DeploymentSpec, JsonObject, ObjectMeta, SecretString, new_id,
};
use aigw_core::snapshot::DeploymentCtx;
use aigw_core::types::ProviderKind;
use aigw_core::wire::WireFormat;
use aigw_observe::ObserveBuilder;
use aigw_observe::otel::{OtlpLogExporter, OtlpMetrics, test_subscriber};
use aigw_providers::{Registry, Upstream, UpstreamClient};
use aigw_proxy::{ProxyState, StaticRoutes};
use axum::Router;
use axum::routing::post;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

/// Collector-URL und Ausgabeverzeichnis, falls der Gegentest aktiv ist.
fn collector() -> Option<(String, PathBuf)> {
    let url = std::env::var("AIGW_TEST_OTELCOL_URL").ok()?;
    let dir = std::env::var("AIGW_TEST_OTELCOL_DIR").ok()?;
    Some((url, PathBuf::from(dir)))
}

fn otlp_cfg(url: &str) -> OtlpConfig {
    OtlpConfig {
        endpoint: url.to_owned(),
        service_name: "aigw-otelcol-test".into(),
        traces: OtlpTracesConfig {
            enabled: true,
            flush_interval_ms: 50,
            ..OtlpTracesConfig::default()
        },
        ..OtlpConfig::default()
    }
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn fake_openai() -> SocketAddr {
    serve(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                [("content-type", "application/json")],
                json!({"id": "c1", "object": "chat.completion", "created": 1, "model": "up-model",
                       "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hallo"},
                                    "finish_reason": "stop"}],
                       "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}})
                .to_string(),
            )
        }),
    ))
    .await
}

fn deployment(endpoint: &str) -> DeploymentCtx {
    DeploymentCtx {
        deployment: Deployment {
            id: new_id(),
            metadata: ObjectMeta::named("dep-ok"),
            spec: DeploymentSpec {
                model_group: "gruppe".into(),
                provider: ProviderKind::OpenAi,
                endpoint: Some(endpoint.into()),
                provider_model: "gpt-up".into(),
                credential: Some("c".into()),
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings: JsonObject::new(),
            },
        },
        credential: Some(Arc::new(CredentialData(BTreeMap::from([(
            "apiKey".to_owned(),
            SecretString::new("sk-upstream"),
        )])))),
    }
}

/// Liest alle JSON-Zeilen der Datei `name` und sammelt `(Ressource, Eintrag)` unter
/// `resource_key`/`scope_key`/`items_key`.
fn read_items(dir: &std::path::Path, name: &str, keys: [&str; 3]) -> Vec<(Value, Value)> {
    let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for r in v[keys[0]].as_array().into_iter().flatten() {
            for sc in r[keys[1]].as_array().into_iter().flatten() {
                for item in sc[keys[2]].as_array().into_iter().flatten() {
                    out.push((r["resource"].clone(), item.clone()));
                }
            }
        }
    }
    out
}

/// Attribute (`[{key, value: {stringValue|intValue|boolValue|doubleValue}}]`) als Map.
fn attrs(v: &Value) -> BTreeMap<String, Value> {
    v["attributes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let val = &a["value"];
            let x = val
                .get("stringValue")
                .or_else(|| val.get("intValue"))
                .or_else(|| val.get("boolValue"))
                .or_else(|| val.get("doubleValue"))
                .cloned()
                .unwrap_or(Value::Null);
            (a["key"].as_str().unwrap_or_default().to_owned(), x)
        })
        .collect()
}

fn s(m: &BTreeMap<String, Value>, k: &str) -> String {
    match m.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => panic!("Attribut {k} fehlt: {m:?}"),
    }
}

/// Wartet, bis `pred` auf den gelesenen Einträgen erfüllt ist (file-Exporter schreibt verzögert).
async fn wait_for(
    dir: &std::path::Path,
    name: &str,
    keys: [&str; 3],
    pred: impl Fn(&[(Value, Value)]) -> bool,
) -> Vec<(Value, Value)> {
    for _ in 0..100 {
        let items = read_items(dir, name, keys);
        if pred(&items) {
            return items;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Collector hat {name} nicht rechtzeitig geschrieben");
}

#[tokio::test]
async fn traces_arrive_at_official_collector() {
    let Some((url, dir)) = collector() else {
        eprintln!("AIGW_TEST_OTELCOL_URL/AIGW_TEST_OTELCOL_DIR nicht gesetzt – übersprungen");
        return;
    };
    let (subscriber, activator) = test_subscriber();
    let _guard = tracing::subscriber::set_default(subscriber);
    let metrics = Arc::new(OtlpMetrics::default());
    let exporter = activator
        .activate(&otlp_cfg(&url), Arc::clone(&metrics))
        .unwrap();

    let up = fake_openai().await;
    let upstream = Upstream::from_deployment(
        &deployment(&format!("http://{up}/v1")),
        &Registry::with_builtin(),
        &UpstreamClient::unguarded().unwrap(),
    )
    .unwrap();
    let mut routes = StaticRoutes::new();
    routes.insert("ok", Arc::new(upstream));
    let gw = serve(aigw_proxy::router(ProxyState::with_routes(Arc::new(
        routes,
    ))))
    .await;

    // Eindeutiger Trace je Lauf (die Datei des Collectors sammelt über Läufe hinweg).
    let trace_id = uuid::Uuid::now_v7().simple().to_string();
    let parent_id = "00f067aa0ba902b7";
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let req = hyper::Request::post(format!("http://{gw}/v1/chat/completions"))
        .header("content-type", "application/json")
        .header("traceparent", format!("00-{trace_id}-{parent_id}-01"))
        .body(Full::new(Bytes::from(
            json!({"model": "ok", "max_tokens": 50, "messages": [{"role": "user", "content": "Hallo"}]})
                .to_string(),
        )))
        .unwrap();
    let resp = client.request(req).await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.into_body().collect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(exporter.shutdown(Duration::from_secs(5)).await);
    assert!(
        metrics.spans_exported() >= 4,
        "Spans an den Collector übergeben"
    );

    let keys = ["resourceSpans", "scopeSpans", "spans"];
    let items = wait_for(&dir, "traces.json", keys, |items| {
        items
            .iter()
            .filter(|(_, sp)| sp["traceId"] == trace_id.as_str())
            .count()
            >= 4
    })
    .await;
    let spans: Vec<&(Value, Value)> = items
        .iter()
        .filter(|(_, sp)| sp["traceId"] == trace_id.as_str())
        .collect();
    let (resource, root) = spans
        .iter()
        .find(|(_, sp)| sp["parentSpanId"] == parent_id)
        .map(|t| (&t.0, &t.1))
        .expect("Wurzel-Span mit eingehendem Eltern-Kontext");
    let res = attrs(resource);
    assert_eq!(s(&res, "service.name"), "aigw-otelcol-test");
    assert_eq!(s(&res, "telemetry.sdk.name"), "aigw");
    assert_eq!(root["name"], "chat ok");
    assert_eq!(root["kind"], 2, "SERVER");
    let ra = attrs(root);
    assert_eq!(s(&ra, "gen_ai.operation.name"), "chat");
    assert_eq!(s(&ra, "gen_ai.system"), "openai");
    assert_eq!(s(&ra, "gen_ai.provider.name"), "openai");
    assert_eq!(s(&ra, "gen_ai.request.model"), "ok");
    assert_eq!(s(&ra, "gen_ai.response.model"), "up-model");
    assert_eq!(s(&ra, "gen_ai.usage.input_tokens"), "11");
    assert_eq!(s(&ra, "gen_ai.usage.output_tokens"), "2");
    assert_eq!(s(&ra, "http.response.status_code"), "200");
    assert_eq!(s(&ra, "aigw.deployment"), "dep-ok");
    let start: u64 = root["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end: u64 = root["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(end >= start && start > 1_600_000_000_000_000_000);

    let root_id = root["spanId"].as_str().unwrap();
    let children: Vec<&Value> = spans
        .iter()
        .map(|(_, sp)| sp)
        .filter(|sp| sp["parentSpanId"] == root_id)
        .collect();
    let names: Vec<&str> = children.iter().filter_map(|c| c["name"].as_str()).collect();
    for n in ["auth", "route", "chat gpt-up"] {
        assert!(names.contains(&n), "Kind-Span {n} fehlt: {names:?}");
    }
    let attempt = children
        .iter()
        .find(|c| c["name"] == "chat gpt-up")
        .unwrap();
    assert_eq!(attempt["kind"], 3, "CLIENT");
    let aa = attrs(attempt);
    assert_eq!(s(&aa, "gen_ai.provider.name"), "openai");
    assert_eq!(s(&aa, "server.address"), "127.0.0.1");
    assert_eq!(s(&aa, "http.response.status_code"), "200");
}

#[tokio::test]
async fn logs_arrive_at_official_collector() {
    let Some((url, dir)) = collector() else {
        eprintln!("AIGW_TEST_OTELCOL_URL/AIGW_TEST_OTELCOL_DIR nicht gesetzt – übersprungen");
        return;
    };
    let metrics = Arc::new(OtlpMetrics::default());
    let exporter = OtlpLogExporter::new(&otlp_cfg(&url), Arc::clone(&metrics)).unwrap();
    let handle = ObserveBuilder::new(ObserveConfig {
        flush_interval_ms: 20,
        ..ObserveConfig::default()
    })
    .exporter(Arc::new(exporter))
    .start();
    let mut r = RequestRecord::new(new_id().to_string(), WireFormat::AnthropicMessages, 200);
    let id = r.request_id.clone();
    r.model_alias = Some("claude".into());
    r.model_group = Some("claude".into());
    r.provider = Some(ProviderKind::Anthropic);
    r.provider_model = Some("claude-x".into());
    r.duration = Duration::from_millis(321);
    r.usage = Usage {
        input_tokens: 100,
        output_tokens: 20,
        ..Usage::default()
    };
    handle.observer().request_started();
    handle.observer().record(r);
    assert!(handle.shutdown().await);
    assert_eq!(metrics.logs_exported(), 1);

    let keys = ["resourceLogs", "scopeLogs", "logRecords"];
    let find = |items: &[(Value, Value)]| {
        items
            .iter()
            .find(|(_, l)| attrs(l).get("aigw.request_id") == Some(&Value::String(id.clone())))
            .cloned()
    };
    let items = wait_for(&dir, "logs.json", keys, |items| find(items).is_some()).await;
    let (resource, log) = find(&items).unwrap();
    assert_eq!(s(&attrs(&resource), "service.name"), "aigw-otelcol-test");
    assert_eq!(log["eventName"], "aigw.request");
    assert_eq!(log["severityText"], "INFO");
    assert_eq!(log["severityNumber"], 9);
    let a = attrs(&log);
    assert_eq!(s(&a, "gen_ai.system"), "anthropic");
    assert_eq!(s(&a, "gen_ai.provider.name"), "anthropic");
    assert_eq!(s(&a, "gen_ai.request.model"), "claude");
    assert_eq!(s(&a, "gen_ai.usage.input_tokens"), "100");
    assert_eq!(s(&a, "aigw.latency_ms"), "321");
}
