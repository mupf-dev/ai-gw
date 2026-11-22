// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Integrationstests Dashboards und Log-Explorer (AIGW-46) mit echtem Server und echter
//! Datenbank: Aggregation über Minuten- und Stunden-Rollups (inkl. angebrochener Stunden),
//! Perzentile, Top-N + „other“, Anbieter-Gruppierung, Vorperiode, Geltungsbereiche je Rolle,
//! Payload-Berechtigung (nur admin/auditor), Cursor-Pagination und Filter des Log-Explorers.
//!
//! Läuft gegen SQLite (In-Memory und Datei) und – mit `AIGW_TEST_POSTGRES_URL` bzw.
//! `scripts/check.sh` mit `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use aigw_admin::AdminState;
use aigw_admin::analytics::{AnalyticsState, router as analytics_router};
use aigw_core::config::AdminAuthConfig;
use aigw_core::resource::{Amount, DeploymentSpec, ModelGroupSpec, ObjectMeta, new_id};
use aigw_core::types::ProviderKind;
use aigw_store::Store;
use aigw_store::analytics::histogram_quantile;
use aigw_store::observe::{
    AttemptRow, GuardrailRow, PayloadRow, RequestLogRow, RollupCounts, RollupKey,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

/// 2026-11-01T00:00:00Z
const T0: i64 = 1_793_491_200_000;
const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

fn ts(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

async fn call(
    addr: SocketAddr,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
    let mut req = Request::builder()
        .method(method)
        .uri(format!("http://{addr}/admin/api/v1{path}"))
        .header("authorization", format!("Bearer {token}"));
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Bytes::from(serde_json::to_vec(&b).unwrap())
        }
        None => Bytes::new(),
    };
    let res = client
        .request(req.body(Full::new(body)).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, v)
}

struct Env {
    addr: SocketAddr,
    admin: String,
}

impl Env {
    async fn get(&self, token: &str, path: &str) -> (StatusCode, Value) {
        call(self.addr, token, Method::GET, path, None).await
    }

    async fn ok(&self, token: &str, path: &str) -> Value {
        let (s, v) = self.get(token, path).await;
        assert_eq!(s, StatusCode::OK, "{path}: {v}");
        v
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = call(self.addr, &self.admin, Method::POST, path, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{path}: {v}");
        v
    }

    /// Admin-API-Key mit den Rollen; liefert das Secret.
    async fn admin_key(&self, name: &str, roles: Value) -> String {
        let v = self
            .post("/admin-api-keys", json!({"name": name, "roles": roles}))
            .await;
        v["secret"].as_str().unwrap().to_owned()
    }
}

/// Ein Key der Testdaten.
#[derive(Clone)]
struct K {
    id: String,
    team: String,
    org: String,
    group: &'static str,
    deployment: String,
    weight: i64,
    lat_bucket: usize,
}

fn counts(k: &K, m: i64) -> RollupCounts {
    let req = 1 + (m % 3) + k.weight;
    let mut c = RollupCounts {
        requests: req,
        errors: i64::from(m % 5 == 0),
        cache_hits: req / 2,
        input_tokens: 10 * req,
        output_tokens: 5 * req,
        cost: req * 1_000_000,
        latency_ms_sum: 100 * req,
        ..RollupCounts::default()
    };
    c.latency_hist[k.lat_bucket] = req;
    if k.weight == 0 {
        c.ttft_hist[1] = req;
        c.ttft_count = req;
    }
    c
}

/// Erwartete Summe über die Testdaten.
fn sum(
    data: &BTreeMap<RollupKey, RollupCounts>,
    from: i64,
    to: i64,
    keep: impl Fn(&RollupKey) -> bool,
) -> RollupCounts {
    let mut c = RollupCounts::default();
    for (k, v) in data {
        if k.bucket_start >= from && k.bucket_start < to && keep(k) {
            c.merge(v);
        }
    }
    c
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("keine Zahl: {v}"))
}

fn amount(nanos: i64) -> Value {
    serde_json::to_value(Amount::from_nanos(nanos)).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn log(
    id: &str,
    at: i64,
    k: Option<&K>,
    status: i64,
    error: Option<&str>,
    stream: bool,
    cache: Option<&str>,
) -> RequestLogRow {
    RequestLogRow {
        id: id.to_owned(),
        at,
        endpoint: "openai-chat".into(),
        api_key_id: k.map(|k| k.id.clone()),
        team_id: k.and_then(|k| (!k.team.is_empty()).then(|| k.team.clone())),
        organization_id: k.and_then(|k| (!k.org.is_empty()).then(|| k.org.clone())),
        model_requested: k.map(|k| k.group.to_owned()),
        model_group: k.map(|k| k.group.to_owned()),
        deployment_id: k.map(|k| k.deployment.clone()),
        provider: k.map(|k| {
            if k.group == "gpt-4o" {
                "mock".to_owned()
            } else {
                "ollama".to_owned()
            }
        }),
        status,
        error_code: error.map(str::to_owned),
        input_tokens: 10,
        output_tokens: 5,
        cost: 2_500_000,
        latency_ms: 120,
        ttft_ms: stream.then_some(40),
        cache: cache.map(str::to_owned),
        stream,
        ..RequestLogRow::default()
    }
}

async fn scenario(store: Store) {
    store.migrate().await.unwrap();
    for g in ["gpt-4o", "claude"] {
        store
            .create("test", ObjectMeta::named(g), ModelGroupSpec::default())
            .await
            .unwrap();
    }
    let dep = |group: &str, provider: ProviderKind, model: &str| DeploymentSpec {
        model_group: group.to_owned(),
        provider,
        endpoint: None,
        provider_model: model.to_owned(),
        credential: None,
        weight: 1,
        priority: 0,
        capacity: aigw_core::resource::Capacity::default(),
        enabled: true,
        settings: aigw_core::resource::JsonObject::default(),
    };
    let d_mock = store
        .create(
            "test",
            ObjectMeta::named("gpt-4o-mock"),
            dep("gpt-4o", ProviderKind::Mock, "mock"),
        )
        .await
        .unwrap()
        .id
        .to_string();
    let d_oll = store
        .create(
            "test",
            ObjectMeta::named("claude-ollama"),
            dep("claude", ProviderKind::Ollama, "llama3.1"),
        )
        .await
        .unwrap()
        .id
        .to_string();

    let (token, secret) = aigw_admin::BootstrapToken::generate().unwrap();
    let state = AdminState::from_config(
        store.clone(),
        &AdminAuthConfig::default(),
        Some(token),
        None,
    )
    .unwrap();
    let analytics = analytics_router(AnalyticsState {
        store: store.clone(),
        auth: Arc::clone(&state.auth),
        // Tagesschritte in der Abrechnungszeitzone (F143).
        calendar: Arc::new(aigw_billing::Calendar::new("Europe/Berlin").unwrap()),
        rollups: true,
    });
    let app = aigw_admin::router_with(state).merge(analytics);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let env = Env {
        addr,
        admin: secret,
    };

    // --- Mandanten und Keys -------------------------------------------------------------------
    let acme = env
        .post(
            "/organizations",
            json!({"metadata": {"name": "acme"}, "spec": {}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let beta = env
        .post(
            "/organizations",
            json!({"metadata": {"name": "beta"}, "spec": {}}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut team_ids = BTreeMap::new();
    for (t, o) in [("platform", "acme"), ("research", "acme"), ("ops", "beta")] {
        let v = env
            .post(
                "/teams",
                json!({"metadata": {"name": t}, "spec": {"organization": o}}),
            )
            .await;
        team_ids.insert(t, v["id"].as_str().unwrap().to_owned());
    }
    let key = |name: &str, team: Option<&str>| {
        // F3: Organisation Pflicht (mit Team die des Teams).
        let body = match team {
            Some(t) => {
                let org = if t == "ops" { "beta" } else { "acme" };
                json!({"metadata": {"name": name}, "spec": {"organization": org, "team": t}})
            }
            None => json!({"metadata": {"name": name}, "spec": {"organization": "acme"}}),
        };
        (name.to_owned(), body)
    };
    let mut key_ids = BTreeMap::new();
    for (name, body) in [
        key("k-plat", Some("platform")),
        key("k-res", Some("research")),
        key("k-ops", Some("ops")),
        key("k-none", None),
    ] {
        let v = env.post("/api-keys", body).await;
        key_ids.insert(name, v["key"]["id"].as_str().unwrap().to_owned());
    }
    let mk = |name: &str,
              team: &str,
              org: &str,
              group: &'static str,
              deployment: &str,
              weight: i64,
              lat_bucket: usize| K {
        id: key_ids[name].clone(),
        team: team.to_owned(),
        org: org.to_owned(),
        group,
        deployment: deployment.to_owned(),
        weight,
        lat_bucket,
    };
    let k_plat = mk(
        "k-plat",
        &team_ids["platform"],
        &acme,
        "gpt-4o",
        &d_mock,
        0,
        2,
    );
    let k_res = mk(
        "k-res",
        &team_ids["research"],
        &acme,
        "gpt-4o",
        &d_mock,
        1,
        4,
    );
    let k_ops = mk("k-ops", &team_ids["ops"], &beta, "claude", &d_oll, 2, 4);
    // Key ohne Team, aber (F3) mit Organisation: für Org-Rollen sichtbar (F146).
    let k_none = mk("k-none", "", &acme, "claude", &d_oll, 3, 4);
    let keys = [&k_plat, &k_res, &k_ops, &k_none];

    // --- Rollups: 3 Stunden, je Minute und Key; Minute 5 doppelt (additiver UPSERT) ---------
    let mut data: BTreeMap<RollupKey, RollupCounts> = BTreeMap::new();
    for hour in 0..3 {
        let mut batch = Vec::new();
        for m in hour * 60..(hour + 1) * 60 {
            for k in keys {
                let rk = RollupKey {
                    bucket_start: T0 + m * MIN,
                    api_key_id: k.id.clone(),
                    team_id: k.team.clone(),
                    organization_id: k.org.clone(),
                    model_group: k.group.to_owned(),
                    deployment_id: k.deployment.clone(),
                    error_class: String::new(),
                };
                let c = counts(k, m);
                data.entry(rk.clone()).or_default().merge(&c);
                batch.push((rk, c));
            }
        }
        store.write_request_batch(&[], &[], &batch).await.unwrap();
    }
    let again: Vec<_> = data
        .iter()
        .filter(|(k, _)| k.bucket_start == T0 + 5 * MIN)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    store.write_request_batch(&[], &[], &again).await.unwrap();
    for (k, v) in again {
        data.get_mut(&k).unwrap().merge(&v);
    }
    // F47: Fehlerklasse als weitere Schlüsselspalte (eigene Zeilen neben '' = ohne Fehler).
    let timeouts = RollupKey {
        bucket_start: T0 + 10 * MIN,
        error_class: "timeout".into(),
        ..data
            .keys()
            .find(|k| k.api_key_id == k_plat.id)
            .unwrap()
            .clone()
    };
    let tc = RollupCounts {
        requests: 2,
        errors: 2,
        latency_ms_sum: 60_000,
        ..RollupCounts::default()
    };
    store
        .write_request_batch(&[], &[], &[(timeouts.clone(), tc.clone())])
        .await
        .unwrap();
    data.insert(timeouts, tc);
    let all = |_: &RollupKey| true;

    // --- Zeitreihe: Stunden-Rollups --------------------------------------------------------
    let a = &env.admin;
    let v = env
        .ok(
            a,
            &format!(
                "/analytics/timeseries?metric=requests&step=1h&from={}&to={}",
                ts(T0),
                ts(T0 + 3 * HOUR)
            ),
        )
        .await;
    assert_eq!(v["step"], "1h");
    assert_eq!(v["stepSeconds"], 3600);
    assert_eq!(v["unit"], "count");
    let s = &v["series"][0];
    assert_eq!(s["key"], "total");
    let pts = s["points"].as_array().unwrap();
    assert_eq!(pts.len(), 3);
    for (i, p) in pts.iter().enumerate() {
        let h = T0 + i64::try_from(i).unwrap() * HOUR;
        assert_eq!(p["t"], ts(h));
        assert_eq!(num(&p["v"]), sum(&data, h, h + HOUR, all).requests as f64);
    }
    assert_eq!(
        num(&s["total"]),
        sum(&data, T0, T0 + 3 * HOUR, all).requests as f64
    );

    // Minuten-Rollups, 5-Minuten-Schritte, nicht auf Stunden ausgerichtet
    let v = env
        .ok(
            a,
            &format!(
                "/analytics/timeseries?metric=tokens&step=5m&from={}&to={}",
                ts(T0 + 10 * MIN),
                ts(T0 + 70 * MIN)
            ),
        )
        .await;
    let pts = v["series"][0]["points"].as_array().unwrap();
    assert_eq!(pts.len(), 12);
    for (i, p) in pts.iter().enumerate() {
        let b = T0 + 10 * MIN + i64::try_from(i).unwrap() * 5 * MIN;
        let c = sum(&data, b, b + 5 * MIN, all);
        assert_eq!(num(&p["v"]), (c.input_tokens + c.output_tokens) as f64);
    }

    // automatische Schrittwahl: 24 h → 15m (F141), 7 Tage → 1h, 60 Tage → 1d
    for (range, step) in [
        (6 * HOUR, "5m"),
        (24 * HOUR, "15m"),
        (7 * 24 * HOUR, "1h"),
        (60 * 24 * HOUR, "1d"),
    ] {
        let v = env
            .ok(
                a,
                &format!(
                    "/analytics/timeseries?metric=requests&from={}&to={}",
                    ts(T0),
                    ts(T0 + range)
                ),
            )
            .await;
        assert_eq!(v["step"], step);
    }
    // Tagessumme (Stunden-Rollups, Schritt 1d) = alles
    let v = env
        .ok(
            a,
            &format!(
                "/analytics/timeseries?metric=cost&step=1d&from={}&to={}",
                ts(T0),
                ts(T0 + 2 * 24 * HOUR)
            ),
        )
        .await;
    let total_cost = sum(&data, T0, T0 + 3 * HOUR, all).cost;
    assert_eq!(v["unit"], "currency");
    // F143: Tage beginnen um Mitternacht in Berlin (Winterzeit: 23:00 UTC des Vortags)
    assert_eq!(v["timezone"], "Europe/Berlin");
    assert_eq!(v["rollups"], true);
    assert_eq!(v["series"][0]["points"][0]["t"], ts(T0 - HOUR));
    assert_eq!(v["series"][0]["points"][1]["t"], ts(T0 + 23 * HOUR));
    assert_eq!(v["series"][0]["points"][0]["amount"], amount(total_cost));
    assert_eq!(v["series"][0]["points"][1]["amount"], amount(0));
    assert_eq!(v["series"][0]["totalAmount"], amount(total_cost));

    // --- KPIs: Bereich über Minuten-Rand + ganze Stunden, Vorperiode -----------------------
    let (from, to) = (T0 + 30 * MIN, T0 + 150 * MIN);
    let v = env
        .ok(
            a,
            &format!("/analytics/summary?from={}&to={}", ts(from), ts(to)),
        )
        .await;
    let cur = sum(&data, from, to, all);
    let prev = sum(&data, from - (to - from), from, all);
    assert_eq!(v["previousFrom"], ts(from - (to - from)));
    assert_eq!(num(&v["requests"]["value"]), cur.requests as f64);
    assert_eq!(num(&v["requests"]["previous"]), prev.requests as f64);
    let delta = (cur.requests - prev.requests) as f64 / prev.requests as f64 * 100.0;
    assert!((num(&v["requests"]["deltaPercent"]) - delta).abs() < 1e-9);
    assert_eq!(v["cost"]["amount"], amount(cur.cost));
    assert_eq!(v["cost"]["previousAmount"], amount(prev.cost));
    assert_eq!(
        num(&v["tokens"]["value"]),
        (cur.input_tokens + cur.output_tokens) as f64
    );
    assert!(
        (num(&v["errorRate"]["value"]) - cur.errors as f64 / cur.requests as f64).abs() < 1e-12
    );
    assert!(
        (num(&v["cacheHitRate"]["value"]) - cur.cache_hits as f64 / cur.requests as f64).abs()
            < 1e-12
    );
    let p95 = histogram_quantile(&cur.latency_hist, 0.95).unwrap();
    assert!((num(&v["latencyP95Ms"]["value"]) - p95).abs() < 1e-9);
    // F144: Quoten zusätzlich in Prozentpunkten
    let er = |c: &RollupCounts| c.errors as f64 / c.requests as f64;
    assert!((num(&v["errorRate"]["deltaPoints"]) - (er(&cur) - er(&prev)) * 100.0).abs() < 1e-9);
    assert!(v["requests"].get("deltaPoints").is_none());
    // Vorperiode ohne Daten → Delta null
    let v = env
        .ok(
            a,
            &format!(
                "/analytics/summary?from={}&to={}",
                ts(T0),
                ts(T0 + 3 * HOUR)
            ),
        )
        .await;
    assert_eq!(v["requests"]["previous"], 0.0);
    assert!(v["requests"]["deltaPercent"].is_null());
    assert!(v["latencyP95Ms"]["previous"].is_null());

    // --- Gruppierung: Top-N + other, Perzentile, Anbieter ----------------------------------
    let range3 = format!("from={}&to={}", ts(T0), ts(T0 + 3 * HOUR));
    let v = env
        .ok(
            a,
            &format!("/analytics/timeseries?metric=requests&step=1h&groupBy=team&limit=2&{range3}"),
        )
        .await;
    let series = v["series"].as_array().unwrap();
    let keys_out: Vec<&str> = series.iter().map(|s| s["key"].as_str().unwrap()).collect();
    // Rang nach Anfragen: ohne Team (k-none) > ops > research > platform
    assert_eq!(keys_out, ["", team_ids["ops"].as_str(), "__other__"]);
    assert_eq!(series[1]["name"], "ops");
    assert!(series[0].get("name").is_none());
    assert_eq!(series[2]["other"], true);
    let other = sum(&data, T0, T0 + 3 * HOUR, |k| {
        k.team_id == team_ids["platform"] || k.team_id == team_ids["research"]
    });
    assert_eq!(num(&series[2]["total"]), other.requests as f64);
    let first_hour_other = sum(&data, T0, T0 + HOUR, |k| {
        k.team_id == team_ids["platform"] || k.team_id == team_ids["research"]
    });
    assert_eq!(
        num(&series[2]["points"][0]["v"]),
        first_hour_other.requests as f64
    );

    let v = env
        .ok(
            a,
            &format!(
                "/analytics/timeseries?metric=latency_p95&groupBy=model_group&step=1h&{range3}"
            ),
        )
        .await;
    for s in v["series"].as_array().unwrap() {
        let g = s["key"].as_str().unwrap().to_owned();
        let c = sum(&data, T0, T0 + 3 * HOUR, |k| k.model_group == g);
        let want = histogram_quantile(&c.latency_hist, 0.95).unwrap();
        assert!((num(&s["total"]) - want).abs() < 1e-9, "{g}");
        assert_eq!(s["name"], g.as_str());
    }
    // nur k-plat hat TTFT (Bucket 25–50 ms) → p95 = 48,75
    let v = env
        .ok(
            a,
            &format!("/analytics/timeseries?metric=ttft_p95&step=1h&key=k-plat&{range3}"),
        )
        .await;
    assert_eq!(num(&v["series"][0]["total"]), 48.75);
    let v = env
        .ok(
            a,
            &format!("/analytics/timeseries?metric=ttft_p95&step=1h&key=k-ops&{range3}"),
        )
        .await;
    assert!(v["series"][0]["total"].is_null());

    // F47: Gruppierung und Filter nach Fehlerklasse
    let v = env
        .ok(
            a,
            &format!("/analytics/timeseries?metric=errors&groupBy=error_class&step=1h&{range3}"),
        )
        .await;
    let by_class: BTreeMap<String, f64> = v["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["key"].as_str().unwrap().to_owned(), num(&s["total"])))
        .collect();
    assert_eq!(by_class["timeout"], 2.0);
    assert_eq!(
        by_class[""],
        sum(&data, T0, T0 + 3 * HOUR, |k| k.error_class.is_empty()).errors as f64
    );
    let v = env
        .ok(
            a,
            &format!("/analytics/summary?errorClass=timeout&{range3}"),
        )
        .await;
    assert_eq!(num(&v["requests"]["value"]), 2.0);
    let v = env
        .ok(
            a,
            &format!("/analytics/top?dimension=error_class&metric=errors&{range3}"),
        )
        .await;
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["key"] == "timeout")
    );

    let v = env
        .ok(
            a,
            &format!("/analytics/timeseries?metric=requests&groupBy=provider&step=1h&{range3}"),
        )
        .await;
    let prov: BTreeMap<String, f64> = v["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["key"].as_str().unwrap().to_owned(), num(&s["total"])))
        .collect();
    assert_eq!(
        prov["mock"],
        sum(&data, T0, T0 + 3 * HOUR, |k| k.deployment_id == d_mock).requests as f64
    );
    assert_eq!(
        prov["ollama"],
        sum(&data, T0, T0 + 3 * HOUR, |k| k.deployment_id == d_oll).requests as f64
    );
    // Filter nach Anbieter
    let v = env
        .ok(a, &format!("/analytics/summary?provider=ollama&{range3}"))
        .await;
    assert_eq!(num(&v["requests"]["value"]), prov["ollama"]);

    // --- Rangliste ------------------------------------------------------------------------
    let v = env
        .ok(
            a,
            &format!("/analytics/top?dimension=key&metric=cost&limit=2&{range3}"),
        )
        .await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["name"], "k-none");
    assert_eq!(items[1]["name"], "k-ops");
    let c_none = sum(&data, T0, T0 + 3 * HOUR, |k| k.api_key_id == k_none.id);
    assert_eq!(items[0]["amount"], amount(c_none.cost));
    assert_eq!(items[0]["requests"], c_none.requests);
    let share = c_none.cost as f64 / sum(&data, T0, T0 + 3 * HOUR, all).cost as f64;
    assert!((num(&items[0]["share"]) - share).abs() < 1e-12);
    let v = env
        .ok(
            a,
            &format!("/analytics/top?dimension=deployment&metric=latency_p99&{range3}"),
        )
        .await;
    assert_eq!(v["items"][0]["name"], "claude-ollama");
    assert!(v["items"][0].get("share").is_none());

    // --- Fehler ----------------------------------------------------------------------------
    for (path, status) in [
        (
            "/analytics/timeseries?metric=requests&foo=1",
            StatusCode::BAD_REQUEST,
        ),
        ("/analytics/timeseries?metric=nope", StatusCode::BAD_REQUEST),
        ("/analytics/timeseries", StatusCode::BAD_REQUEST),
        ("/analytics/summary?from=gestern", StatusCode::BAD_REQUEST),
        (
            "/analytics/top?dimension=key&limit=0",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/analytics/summary?team=gibt-es-nicht",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("/request-logs?cursor=xyz", StatusCode::BAD_REQUEST),
        ("/request-logs?status=7xx", StatusCode::BAD_REQUEST),
    ] {
        let (s, v) = env.get(a, path).await;
        assert_eq!(s, status, "{path}: {v}");
    }
    let (s, _) = env
        .get(
            a,
            &format!(
                "/analytics/timeseries?metric=requests&step=1m&from={}&to={}",
                ts(T0),
                ts(T0 + 7 * 24 * HOUR)
            ),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "zu viele Punkte");

    // --- Geltungsbereiche ------------------------------------------------------------------
    let v_plat = env
        .admin_key(
            "v-plat",
            json!([{"role": "viewer", "organization": "acme", "team": "platform"}]),
        )
        .await;
    let oa_acme = env
        .admin_key(
            "oa-acme",
            json!([{"role": "org-admin", "organization": "acme"}]),
        )
        .await;
    let viewer = env.admin_key("view", json!([{"role": "viewer"}])).await;
    let auditor = env.admin_key("aud", json!([{"role": "auditor"}])).await;
    let aud_plat = env
        .admin_key(
            "aud-plat",
            json!([{"role": "auditor", "organization": "acme", "team": "platform"}]),
        )
        .await;

    let req_of = |v: &Value| num(&v["requests"]["value"]);
    let plat = sum(&data, T0, T0 + 3 * HOUR, |k| {
        k.team_id == team_ids["platform"]
    })
    .requests;
    let acme_sum = sum(&data, T0, T0 + 3 * HOUR, |k| k.organization_id == acme).requests;
    let total = sum(&data, T0, T0 + 3 * HOUR, all).requests;
    let summary = format!("/analytics/summary?{range3}");
    assert_eq!(req_of(&env.ok(&v_plat, &summary).await), plat as f64);
    assert_eq!(req_of(&env.ok(&oa_acme, &summary).await), acme_sum as f64);
    assert_eq!(req_of(&env.ok(&viewer, &summary).await), total as f64);
    // Organisation als Filter schneidet nur
    assert_eq!(
        req_of(
            &env.ok(&v_plat, &format!("{summary}&organization=acme"))
                .await
        ),
        plat as f64
    );
    // ausdrücklich fremdes Team bzw. fremder Key → 403
    for path in [
        format!("{summary}&team=research"),
        format!("{summary}&key=k-none"),
        format!("{summary}&key=k-res"),
    ] {
        let (s, _) = env.get(&v_plat, &path).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
    }
    let (s, _) = env.get(&oa_acme, &format!("{summary}&team=ops")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(
        req_of(&env.ok(&oa_acme, &format!("{summary}&team=research")).await),
        sum(&data, T0, T0 + 3 * HOUR, |k| k.team_id
            == team_ids["research"])
        .requests as f64
    );
    let v = env
        .ok(&oa_acme, &format!("/analytics/top?dimension=team&{range3}"))
        .await;
    let names: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap_or(""))
        .collect();
    // F146: Keys ohne Team der eigenen Organisation (k-none) sind für org-admin sichtbar
    assert_eq!(names, ["", "research", "platform"]);
    assert_eq!(
        req_of(&env.ok(&oa_acme, &format!("{summary}&key=k-none")).await),
        sum(&data, T0, T0 + 3 * HOUR, |k| k.api_key_id == k_none.id).requests as f64
    );
    let v = env
        .ok(
            &v_plat,
            &format!("/analytics/timeseries?metric=requests&groupBy=key&step=1h&{range3}"),
        )
        .await;
    assert_eq!(v["series"].as_array().unwrap().len(), 1);
    assert_eq!(v["series"][0]["name"], "k-plat");

    // --- Log-Explorer ----------------------------------------------------------------------
    let ids: Vec<String> = (0..7).map(|_| new_id().to_string()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    let logs = vec![
        log(
            &sorted[0],
            T0 + MIN,
            Some(&k_plat),
            200,
            None,
            true,
            Some("hit-exact"),
        ),
        log(
            &sorted[1],
            T0 + 2 * MIN,
            Some(&k_res),
            429,
            Some("rate_limited"),
            false,
            None,
        ),
        log(
            &sorted[2],
            T0 + 3 * MIN,
            Some(&k_ops),
            200,
            None,
            false,
            Some("miss"),
        ),
        // gleicher Zeitpunkt: Reihenfolge nach ID
        log(
            &sorted[3],
            T0 + 4 * MIN,
            Some(&k_plat),
            500,
            Some("upstream_error"),
            true,
            None,
        ),
        log(
            &sorted[4],
            T0 + 4 * MIN,
            Some(&k_plat),
            200,
            None,
            false,
            None,
        ),
        log(
            &sorted[5],
            T0 + 5 * MIN,
            Some(&k_none),
            200,
            None,
            false,
            None,
        ),
        log(
            &sorted[6],
            T0 + 6 * MIN,
            None,
            401,
            Some("invalid_api_key"),
            false,
            None,
        ),
    ];
    let mut logs = logs;
    logs[2].client_request_id = Some("client-abc-1".into());
    // F47, F100, F147, F161, F191
    logs[0].client_ip = Some("10.1.2.3".into());
    logs[0].guardrails = vec![GuardrailRow {
        plugin: "pii".into(),
        category: "email".into(),
        action: "mask".into(),
        count: 2,
    }];
    logs[1].error_class = Some("rate_limit".into());
    logs[3].error_class = Some("server_error".into());
    logs[2].trace_id = Some("4bf92f3577b34da6a3ce929d0e0e4736".into());
    logs[4].cache = Some("cache-embedding".into());
    logs[4].parent_request_id = Some(sorted[3].clone());
    logs[0].attempts = vec![
        AttemptRow {
            model_group: Some("gpt-4o".into()),
            deployment: "gpt-4o-mock".into(),
            deployment_id: Some(d_mock.clone()),
            error: Some("rate_limit".into()),
            upstream_status: Some(429),
            duration_ms: 12,
        },
        AttemptRow {
            model_group: Some("gpt-4o".into()),
            deployment: "gpt-4o-mock".into(),
            deployment_id: Some(d_mock.clone()),
            error: None,
            upstream_status: None,
            duration_ms: 80,
        },
    ];
    let payload = |id: &str| PayloadRow {
        id: id.to_owned(),
        at: T0,
        mode: "masked".into(),
        payload: json!({"request": {"bytes": 12, "body": "{\"x\":\"[EMAIL]\"}"}}),
    };
    let payloads = [payload(&sorted[0]), payload(&sorted[5])];
    let log_refs: Vec<&RequestLogRow> = logs.iter().collect();
    let pl_refs: Vec<&PayloadRow> = payloads.iter().collect();
    store
        .write_request_batch(&log_refs, &pl_refs, &[])
        .await
        .unwrap();

    // Pagination: 3 + 3 + 1, neueste zuerst, gleiche Zeit nach ID absteigend
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &cursor {
            Some(c) => format!("/request-logs?limit=3&cursor={c}"),
            None => "/request-logs?limit=3".to_owned(),
        };
        let v = env.ok(a, &path).await;
        pages += 1;
        for i in v["items"].as_array().unwrap() {
            seen.push(i["id"].as_str().unwrap().to_owned());
        }
        match v["nextCursor"].as_str() {
            Some(c) => cursor = Some(c.to_owned()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let want: Vec<String> = sorted.iter().rev().cloned().collect();
    assert_eq!(seen, want);

    let list_ids = |v: &Value| -> Vec<String> {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect()
    };
    for (q, want) in [
        ("status=error", vec![6, 3, 1]),
        ("status=4xx", vec![6, 1]),
        ("status=success", vec![5, 4, 2, 0]),
        ("status=429", vec![1]),
        ("errorCode=rate_limited,upstream_error", vec![3, 1]),
        ("team=platform", vec![4, 3, 0]),
        ("key=k-ops", vec![2]),
        ("organization=acme", vec![5, 4, 3, 1, 0]),
        ("modelGroup=claude", vec![5, 2]),
        ("deployment=gpt-4o-mock", vec![4, 3, 1, 0]),
        ("provider=ollama", vec![5, 2]),
        ("stream=true", vec![3, 0]),
        ("cache=hit", vec![0]),
        ("cache=miss", vec![2]),
        ("cache=none", vec![6, 5, 3, 1]),
        ("cache=cache-embedding", vec![4]),
        ("errorClass=rate_limit,server_error", vec![3, 1]),
        ("errorClass=timeout", vec![]),
    ] {
        let v = env.ok(a, &format!("/request-logs?{q}")).await;
        let w: Vec<String> = want.iter().map(|i: &usize| sorted[*i].clone()).collect();
        assert_eq!(list_ids(&v), w, "{q}");
    }
    let v = env
        .ok(a, &format!("/request-logs?requestId={}", sorted[2]))
        .await;
    assert_eq!(list_ids(&v), [sorted[2].clone()]);
    let v = env
        .ok(a, "/request-logs?clientRequestId=client-abc-1")
        .await;
    assert_eq!(list_ids(&v), [sorted[2].clone()]);
    assert_eq!(v["items"][0]["clientRequestId"], "client-abc-1");
    // F100: Trace-ID (Groß-/Kleinschreibung egal)
    let v = env
        .ok(a, "/request-logs?traceId=4BF92F3577B34DA6A3CE929D0E0E4736")
        .await;
    assert_eq!(list_ids(&v), [sorted[2].clone()]);
    assert_eq!(v["items"][0]["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    // F191: Folgeanfragen (Cache-Embedding) einer Anfrage
    let v = env
        .ok(a, &format!("/request-logs?parentRequestId={}", sorted[3]))
        .await;
    assert_eq!(list_ids(&v), [sorted[4].clone()]);
    assert_eq!(v["items"][0]["parentRequestId"], sorted[3].as_str());
    assert_eq!(v["items"][0]["cache"], "cache-embedding");
    let v = env
        .ok(a, &format!("/request-logs?requestId={}", sorted[1]))
        .await;
    assert_eq!(v["items"][0]["errorClass"], "rate_limit");
    let v = env
        .ok(
            a,
            &format!(
                "/request-logs?from={}&to={}",
                ts(T0 + 2 * MIN),
                ts(T0 + 4 * MIN)
            ),
        )
        .await;
    assert_eq!(list_ids(&v), [sorted[2].clone(), sorted[1].clone()]);
    // Namen aufgelöst, Kosten als Dezimaltext, Payload-Kennzeichen
    let v = env
        .ok(a, &format!("/request-logs?requestId={}", sorted[0]))
        .await;
    let it = &v["items"][0];
    assert_eq!(it["apiKey"], "k-plat");
    assert_eq!(it["team"], "platform");
    assert_eq!(it["organization"], "acme");
    assert_eq!(it["deployment"], "gpt-4o-mock");
    assert_eq!(it["cost"], amount(2_500_000));
    assert_eq!(it["hasPayload"], true);
    assert_eq!(it["attemptCount"], 2);

    // Geltungsbereich im Log-Explorer
    let v = env.ok(&v_plat, "/request-logs").await;
    assert_eq!(
        list_ids(&v),
        [sorted[4].clone(), sorted[3].clone(), sorted[0].clone()]
    );
    let v = env.ok(&oa_acme, "/request-logs").await;
    assert_eq!(
        list_ids(&v).len(),
        5,
        "F146: inkl. Key ohne Team der Organisation"
    );
    // F147: clientIp nur für admin/auditor
    assert!(
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i.get("clientIp").is_none())
    );
    let v = env
        .ok(&viewer, &format!("/request-logs?requestId={}", sorted[0]))
        .await;
    assert!(v["items"][0].get("clientIp").is_none());
    for t in [&auditor, a] {
        let v = env
            .ok(t, &format!("/request-logs?requestId={}", sorted[0]))
            .await;
        assert_eq!(v["items"][0]["clientIp"], "10.1.2.3");
    }
    let (s, _) = env.get(&v_plat, "/request-logs?team=ops").await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Einzelanfrage: Versuche; Payload nur admin/auditor
    let detail = format!("/request-logs/{}", sorted[0]);
    let v = env.ok(&v_plat, &detail).await;
    assert_eq!(v["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(v["attempts"][0]["error"], "rate_limit");
    assert_eq!(v["attempts"][0]["upstreamStatus"], 429);
    assert_eq!(v["attempts"][1]["durationMs"], 80);
    assert_eq!(v["payloadStatus"], "forbidden");
    assert!(v.get("payload").is_none());
    assert_eq!(v["team"], "platform");
    assert!(v.get("clientIp").is_none(), "F147");
    // F161: Guardrail-Zähler
    assert_eq!(v["guardrails"][0]["plugin"], "pii");
    assert_eq!(v["guardrails"][0]["count"], 2);
    let (s, v) = env.get(&v_plat, &format!("{detail}/payload")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["code"], "forbidden");
    let (s, _) = env.get(&oa_acme, &format!("{detail}/payload")).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "org-admin darf keine Payloads");
    // F146: Einzelanfrage eines Keys ohne Team der eigenen Organisation
    let v = env
        .ok(&oa_acme, &format!("/request-logs/{}", sorted[5]))
        .await;
    assert_eq!(v["apiKey"], "k-none");
    assert_eq!(v["payloadStatus"], "forbidden");
    for t in [&aud_plat, &auditor, a] {
        let v = env.ok(t, &detail).await;
        assert_eq!(v["clientIp"], "10.1.2.3", "F147");
        assert_eq!(v["payloadStatus"], "available");
        assert_eq!(v["payload"]["mode"], "masked");
        assert_eq!(v["payload"]["payload"]["request"]["bytes"], 12);
        let v = env.ok(t, &format!("{detail}/payload")).await;
        assert_eq!(v["id"], sorted[0].as_str());
    }
    // ohne Payload
    let v = env.ok(a, &format!("/request-logs/{}", sorted[1])).await;
    assert_eq!(v["payloadStatus"], "not-stored");
    assert!(v["attempts"].as_array().unwrap().is_empty());
    let (s, _) = env
        .get(a, &format!("/request-logs/{}/payload", sorted[1]))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // fremdes Team bzw. ohne Team → 403 für eingeschränkte Rollen
    for id in [&sorted[1], &sorted[5], &sorted[6]] {
        let (s, _) = env.get(&v_plat, &format!("/request-logs/{id}")).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let (s, _) = env
        .get(&aud_plat, &format!("/request-logs/{}/payload", sorted[5]))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // globaler viewer: Metadaten ja, Payload nein
    let v = env
        .ok(&viewer, &format!("/request-logs/{}", sorted[5]))
        .await;
    assert_eq!(v["payloadStatus"], "forbidden");
    let v = env
        .ok(&auditor, &format!("/request-logs/{}", sorted[5]))
        .await;
    assert_eq!(v["payloadStatus"], "available");
    let (s, _) = env.get(a, "/request-logs/gibt-es-nicht").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // ohne Anmeldung
    let (s, _) = env.get("falsch", "/request-logs").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = env.get("falsch", "/analytics/summary").await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // --- Aufbewahrung: Stunden-Rollups gehen mit -------------------------------------------
    store.purge_rollups(T0 + HOUR + 30 * MIN).await.unwrap();
    let left = store.list_hourly_rollups(T0, T0 + 3 * HOUR).await.unwrap();
    assert!(left.iter().all(|(k, _)| k.bucket_start >= T0 + HOUR));
    assert!(left.iter().any(|(k, _)| k.bucket_start == T0 + HOUR));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_in_memory() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_file() {
    let dir = std::env::temp_dir().join(format!("aigw-analytics-test-{}", new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}/aigw.db?mode=rwc", dir.display());
    scenario(Store::connect(&url).await.unwrap()).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_analytics_test_{}", new_id().simple());
    let admin = sqlx::PgPool::connect(&base).await.unwrap();
    sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {db}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').expect("URL mit Datenbankpfad");
    let store = Store::connect(&format!("{prefix}/{db}")).await.unwrap();
    scenario(store.clone()).await;
    store.close().await;
    sqlx::query(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
        .execute(&admin)
        .await
        .unwrap();
}
