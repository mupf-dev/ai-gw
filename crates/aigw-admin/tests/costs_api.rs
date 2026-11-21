// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integrationstests Kostenanalyse (AIGW-117) mit echtem Server und echter Datenbank:
//! Kennzahlen mit Vorperiode und Aufschlüsselung (Spalten der Migration 0013), Ersparnisse,
//! Plugin-Kosten, Kostentreiber je Dimension, Budget-Prognose und Burn-down,
//! Chargeback-Export (CSV/JSON, Kostenstelle, Labels, Abschnitte) sowie die Geltungsbereiche
//! je Rolle (wie Analytics).
//!
//! Läuft gegen SQLite (In-Memory) und – mit `AIGW_TEST_POSTGRES_URL` bzw. `scripts/check.sh`
//! mit `AIGW_TEST_POSTGRES=docker` – gegen Postgres.

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
use aigw_admin::costs::{CostsState, router as costs_router};
use aigw_billing::{BudgetKey, BudgetLevel, Calendar};
use aigw_core::config::AdminAuthConfig;
use aigw_core::resource::{
    Amount, BudgetPeriod, DeploymentSpec, Id, ModelGroupSpec, ObjectMeta, new_id,
};
use aigw_core::types::ProviderKind;
use aigw_store::Store;
use aigw_store::observe::{RollupCounts, RollupKey};
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

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap()
}

/// Antwort roh: Status, Content-Type, Content-Disposition, Body.
async fn raw(
    addr: SocketAddr,
    token: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, String, String, String) {
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
    let h = |n: &str| {
        res.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let (ct, cd) = (h("content-type"), h("content-disposition"));
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, ct, cd, String::from_utf8(bytes.to_vec()).unwrap())
}

struct Env {
    addr: SocketAddr,
    admin: String,
}

impl Env {
    async fn get(&self, token: &str, path: &str) -> (StatusCode, Value) {
        let (s, _, _, b) = raw(self.addr, token, Method::GET, path, None).await;
        (s, serde_json::from_str(&b).unwrap_or(Value::Null))
    }

    async fn ok(&self, token: &str, path: &str) -> Value {
        let (s, v) = self.get(token, path).await;
        assert_eq!(s, StatusCode::OK, "{path}: {v}");
        v
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, _, _, b) = raw(self.addr, &self.admin, Method::POST, path, Some(body)).await;
        assert_eq!(s, StatusCode::CREATED, "{path}: {b}");
        serde_json::from_str(&b).unwrap()
    }

    async fn admin_key(&self, name: &str, roles: Value) -> String {
        let v = self
            .post("/admin-api-keys", json!({"name": name, "roles": roles}))
            .await;
        v["secret"].as_str().unwrap().to_owned()
    }
}

fn amount(nanos: i64) -> Value {
    serde_json::to_value(Amount::from_nanos(nanos)).unwrap()
}

fn nanos(v: &Value) -> i64 {
    v.as_str()
        .unwrap_or_else(|| panic!("kein Betrag: {v}"))
        .parse::<Amount>()
        .unwrap()
        .nanos()
}

/// Testdaten je Minute und Key: Kosten aufgeschlüsselt, Plugin-Anteil, Cache.
fn counts(weight: i64, m: i64, scale: i64) -> RollupCounts {
    let req = (1 + (m % 2) + weight) * scale;
    let (input, read, output, plugin, other) = (3 * req, req, 5 * req, 2 * req, 1);
    RollupCounts {
        requests: req,
        errors: i64::from(m % 10 == 0),
        cache_hits: i64::from(m % 4 == 0),
        input_tokens: 100 * req,
        output_tokens: 50 * req,
        cached_tokens: 20 * req,
        cost: input + read + output + plugin + other,
        cost_input: input,
        cost_cache_read: read,
        cost_output: output,
        cache_read_saved: 4 * req,
        plugin_cost: plugin,
        plugin_input_tokens: 7 * req,
        plugin_output_tokens: req,
        ..RollupCounts::default()
    }
}

struct K {
    id: String,
    team: String,
    org: String,
    group: &'static str,
    deployment: String,
    weight: i64,
}

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
    let costs = costs_router(CostsState {
        store: store.clone(),
        auth: Arc::clone(&state.auth),
        calendar: Arc::new(Calendar::utc()),
        rollups: true,
        currency: "EUR".into(),
        cost_center_key: "costCenter".into(),
    });
    let app = aigw_admin::router_with(state).merge(costs);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let env = Env {
        addr,
        admin: secret,
    };

    // --- Mandanten, Kostenstellen, Labels, Budgets -----------------------------------------
    let acme = env
        .post(
            "/organizations",
            json!({"metadata": {"name": "acme"}, "spec": {"metadata": {"costCenter": "CC-ACME"}}}),
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
    for (t, spec) in [
        (
            "platform",
            json!({"organization": "acme", "metadata": {"costCenter": "CC-100"},
                   "budget": {"amount": "0.000001", "period": "month", "warnAtPercent": [80]}}),
        ),
        ("research", json!({"organization": "acme"})),
        (
            "ops",
            json!({"organization": "beta", "metadata": {"costCenter": "=HACK()"},
                   "budget": {"amount": "1000", "period": "month"}}),
        ),
    ] {
        let labels = if t == "platform" {
            json!({"env": "prod"})
        } else {
            json!({})
        };
        let v = env
            .post(
                "/teams",
                json!({"metadata": {"name": t, "labels": labels}, "spec": spec}),
            )
            .await;
        team_ids.insert(t, v["id"].as_str().unwrap().to_owned());
    }
    let mut key_ids = BTreeMap::new();
    for (name, spec) in [
        (
            "k-plat",
            json!({"organization": "acme", "team": "platform", "metadata": {"costCenter": "CC-K1"}}),
        ),
        ("k-res", json!({"organization": "acme", "team": "research"})),
        ("k-ops", json!({"organization": "beta", "team": "ops"})),
        ("k-none", json!({"organization": "acme"})),
    ] {
        let v = env
            .post(
                "/api-keys",
                json!({"metadata": {"name": name}, "spec": spec}),
            )
            .await;
        key_ids.insert(name, v["key"]["id"].as_str().unwrap().to_owned());
    }
    let keys = [
        K {
            id: key_ids["k-plat"].clone(),
            team: team_ids["platform"].clone(),
            org: acme.clone(),
            group: "gpt-4o",
            deployment: d_mock.clone(),
            weight: 0,
        },
        K {
            id: key_ids["k-res"].clone(),
            team: team_ids["research"].clone(),
            org: acme.clone(),
            group: "gpt-4o",
            deployment: d_mock.clone(),
            weight: 1,
        },
        K {
            id: key_ids["k-ops"].clone(),
            team: team_ids["ops"].clone(),
            org: beta.clone(),
            group: "claude",
            deployment: d_oll.clone(),
            weight: 2,
        },
        K {
            id: key_ids["k-none"].clone(),
            team: String::new(),
            org: acme.clone(),
            group: "claude",
            deployment: d_oll.clone(),
            weight: 3,
        },
    ];

    // --- Rollups: Vorperiode [T0−3h, T0) halb so viel, Zeitraum [T0, T0+3h) -----------------
    let mut data: BTreeMap<RollupKey, RollupCounts> = BTreeMap::new();
    for (start, scale) in [(T0 - 3 * HOUR, 1), (T0, 2)] {
        let mut batch = Vec::new();
        for m in 0..180 {
            for k in &keys {
                let rk = RollupKey {
                    bucket_start: start + m * MIN,
                    api_key_id: k.id.clone(),
                    team_id: k.team.clone(),
                    organization_id: k.org.clone(),
                    model_group: k.group.to_owned(),
                    deployment_id: k.deployment.clone(),
                    error_class: String::new(),
                };
                let c = counts(k.weight, m, scale);
                data.entry(rk.clone()).or_default().merge(&c);
                batch.push((rk, c));
            }
        }
        store.write_request_batch(&[], &[], &batch).await.unwrap();
    }
    let all = |_: &RollupKey| true;
    let cur = sum(&data, T0, T0 + 3 * HOUR, all);
    let prev = sum(&data, T0 - 3 * HOUR, T0, all);
    let range = format!("from={}&to={}", ts(T0), ts(T0 + 3 * HOUR));
    let a = env.admin.clone();

    // --- Kennzahlen ------------------------------------------------------------------------
    let v = env.ok(&a, &format!("/costs/summary?{range}")).await;
    assert_eq!(v["currency"], "EUR");
    assert_eq!(v["previousFrom"], ts(T0 - 3 * HOUR));
    assert_eq!(v["total"]["amount"], amount(cur.cost));
    assert_eq!(v["total"]["previous"], amount(prev.cost));
    assert_eq!(v["total"]["delta"], amount(cur.cost - prev.cost));
    let dp = v["total"]["deltaPercent"].as_f64().unwrap();
    assert!((dp - (cur.cost - prev.cost) as f64 / prev.cost as f64 * 100.0).abs() < 1e-9);
    assert_eq!(v["requests"], cur.requests);
    assert_eq!(v["previousRequests"], prev.requests);
    assert_eq!(
        nanos(&v["costPerRequest"]["amount"]),
        cur.cost / cur.requests
    );
    assert_eq!(
        nanos(&v["costPer1kInputTokens"]),
        (cur.cost_input + cur.cost_cache_read) * 1000 / cur.input_tokens
    );
    assert_eq!(
        nanos(&v["costPer1kOutputTokens"]),
        cur.cost_output * 1000 / cur.output_tokens
    );
    let parts: BTreeMap<&str, i64> = v["breakdown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["part"].as_str().unwrap(), nanos(&p["amount"])))
        .collect();
    assert_eq!(parts["input"], cur.cost_input);
    assert_eq!(parts["cache_read"], cur.cost_cache_read);
    assert_eq!(parts["cache_write"], 0);
    assert_eq!(parts["output"], cur.cost_output);
    assert_eq!(parts["plugins"], cur.plugin_cost);
    assert_eq!(parts.values().sum::<i64>(), cur.cost, "Teile = Gesamt");
    assert_eq!(v["savings"]["providerCache"], amount(cur.cache_read_saved));
    assert_eq!(v["savings"]["providerCacheTokens"], cur.cached_tokens);
    assert_eq!(v["savings"]["gatewayCacheHits"], cur.cache_hits);
    // Schätzung je Modellgruppe: Treffer × (Kosten ohne Plugins) / bezahlte Anfragen.
    let mut est = 0_i128;
    for g in ["gpt-4o", "claude"] {
        let c = sum(&data, T0, T0 + 3 * HOUR, |k| k.model_group == g);
        let paid = c.requests - c.cache_hits - c.errors;
        est += i128::from(c.cost - c.plugin_cost) * i128::from(c.cache_hits) / i128::from(paid);
    }
    assert_eq!(
        nanos(&v["savings"]["gatewayCacheEstimate"]),
        i64::try_from(est).unwrap()
    );
    assert_eq!(v["savings"]["gatewayCacheHitsUnestimated"], 0);
    assert_eq!(v["plugins"]["amount"], amount(cur.plugin_cost));
    assert_eq!(v["plugins"]["inputTokens"], cur.plugin_input_tokens);
    assert_eq!(v["plugins"]["outputTokens"], cur.plugin_output_tokens);

    // Filter wie Analytics (Name oder ID), Anbieter über Deployments.
    let v = env
        .ok(&a, &format!("/costs/summary?{range}&provider=ollama"))
        .await;
    let c = sum(&data, T0, T0 + 3 * HOUR, |k| k.deployment_id == d_oll);
    assert_eq!(v["total"]["amount"], amount(c.cost));
    let (s, _) = env
        .get(&a, &format!("/costs/summary?{range}&team=unbekannt"))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // --- Kostentreiber ---------------------------------------------------------------------
    let v = env
        .ok(&a, &format!("/costs/breakdown?dimension=team&{range}"))
        .await;
    assert_eq!(v["total"]["amount"], amount(cur.cost));
    let items = v["items"].as_array().unwrap();
    // Gewichte: k-none (ohne Team, 3) > ops (2) > research (1) > platform (0).
    let names: Vec<&str> = items
        .iter()
        .map(|i| i["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, ["", "ops", "research", "platform"]);
    let plat = sum(&data, T0, T0 + 3 * HOUR, |k| {
        k.team_id == team_ids["platform"]
    });
    let plat_prev = sum(&data, T0 - 3 * HOUR, T0, |k| {
        k.team_id == team_ids["platform"]
    });
    let p = &items[3];
    assert_eq!(p["key"], team_ids["platform"].as_str());
    assert_eq!(p["cost"]["amount"], amount(plat.cost));
    assert_eq!(p["cost"]["previous"], amount(plat_prev.cost));
    assert_eq!(p["cost"]["delta"], amount(plat.cost - plat_prev.cost));
    assert_eq!(p["requests"], plat.requests);
    assert_eq!(p["pluginCost"], amount(plat.plugin_cost));
    assert!((p["share"].as_f64().unwrap() - plat.cost as f64 / cur.cost as f64).abs() < 1e-12);
    let v = env
        .ok(
            &a,
            &format!("/costs/breakdown?dimension=provider&limit=1&{range}"),
        )
        .await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["key"], "ollama");
    let (s, _) = env
        .get(
            &a,
            &format!("/costs/breakdown?dimension=error_class&{range}"),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // --- Chargeback-Export -----------------------------------------------------------------
    let wide = format!("from={}&to={}", ts(T0 - 3 * HOUR), ts(T0 + 3 * HOUR));
    let (s, ct, cd, csv) = raw(
        addr,
        &a,
        Method::GET,
        &format!("/costs/export?dimension=team&granularity=day&{wide}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{csv}");
    assert!(ct.starts_with("text/csv"), "{ct}");
    assert!(
        cd.contains("attachment") && cd.contains("aigw-kosten-team-2026-10-31-2026-11-01.csv"),
        "{cd}"
    );
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(
        lines[0],
        "period_start,period_end,dimension,key,name,organization,team,cost_center,label.env,meta.costCenter,requests,input_tokens,output_tokens,cached_tokens,cost,plugin_cost,currency"
    );
    // 2 Tage (31.10. 21:00–24:00 und 1.11. 0:00–3:00) × 4 Gruppen (inkl. ohne Team).
    assert_eq!(lines.len(), 1 + 8, "{csv}");
    let row = lines
        .iter()
        .find(|l| l.starts_with(&format!("{},", ts(T0))) && l.contains(",platform,"))
        .unwrap();
    let cols: Vec<&str> = row.split(',').collect();
    assert_eq!(cols[1], ts(T0 + 3 * HOUR));
    assert_eq!(cols[2], "team");
    assert_eq!(cols[5], "acme");
    assert_eq!(cols[7], "CC-100");
    assert_eq!(cols[8], "prod");
    assert_eq!(cols[10], plat.requests.to_string());
    assert_eq!(cols[14], Amount::from_nanos(plat.cost).to_string());
    assert_eq!(cols[16], "EUR");
    // Formel-Injektion in Metadaten wird entschärft.
    assert!(csv.contains(",'=HACK(),"), "{csv}");

    // Keys: Kostenstelle vom Key, sonst Team, sonst Organisation.
    let (_, _, _, csv) = raw(
        addr,
        &a,
        Method::GET,
        &format!("/costs/export?dimension=key&granularity=total&{range}"),
        None,
    )
    .await;
    let cc: BTreeMap<String, String> = csv
        .lines()
        .skip(1)
        .map(|l| {
            let c: Vec<&str> = l.split(',').collect();
            (c[4].to_owned(), c[7].to_owned())
        })
        .collect();
    assert_eq!(cc["k-plat"], "CC-K1");
    assert_eq!(cc["k-ops"], "'=HACK()");
    assert_eq!(cc["k-none"], "CC-ACME");
    assert_eq!(cc["k-res"], "CC-ACME");

    // JSON, Abschnitt gesamt.
    let (s, ct, _, body) = raw(
        addr,
        &a,
        Method::GET,
        &format!("/costs/export?format=json&dimension=model_group&granularity=total&{range}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(ct.starts_with("application/json"));
    let j: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(j["currency"], "EUR");
    assert_eq!(j["dimension"], "model_group");
    let rows = j["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let total: i64 = rows.iter().map(|r| nanos(&r["cost"])).sum();
    assert_eq!(total, cur.cost);
    assert_eq!(rows[0]["key"], "claude");
    let (s, _, _, _) = raw(
        addr,
        &a,
        Method::GET,
        &format!("/costs/export?format=xlsx&{range}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

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
    let v = env.ok(&v_plat, &format!("/costs/summary?{range}")).await;
    assert_eq!(v["total"]["amount"], amount(plat.cost));
    let acme_c = sum(&data, T0, T0 + 3 * HOUR, |k| k.organization_id == acme);
    let v = env.ok(&oa_acme, &format!("/costs/summary?{range}")).await;
    assert_eq!(v["total"]["amount"], amount(acme_c.cost));
    let v = env
        .ok(
            &oa_acme,
            &format!("/costs/breakdown?dimension=team&{range}"),
        )
        .await;
    let names: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["name"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(names, ["", "research", "platform"]);
    for path in [
        format!("/costs/summary?{range}&team=research"),
        format!("/costs/breakdown?dimension=key&{range}&key=k-ops"),
        format!("/costs/export?{range}&team=ops"),
    ] {
        let (s, _) = env.get(&v_plat, &path).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{path}");
    }
    let (s, _) = env
        .get(&oa_acme, &format!("/costs/summary?{range}&team=ops"))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Export des Team-Betrachters: nur das eigene Team, Labels nur sichtbarer Objekte.
    let (_, _, _, csv) = raw(
        addr,
        &v_plat,
        Method::GET,
        &format!("/costs/export?dimension=team&granularity=total&{range}"),
        None,
    )
    .await;
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 2, "{csv}");
    assert!(lines[1].contains(",platform,"));
    assert!(!csv.contains("HACK"));
    let (s, _) = env.get("falsch", &format!("/costs/summary?{range}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // --- Budgets mit Prognose --------------------------------------------------------------
    // Verbrauch im laufenden Monat (budget_usage) und Kosten der letzten Tage (Rollups).
    let now = now_ms();
    let cal = Calendar::utc();
    let month = cal.period(BudgetPeriod::Month, now);
    let tid = |t: &str| team_ids[t].parse::<Id>().unwrap();
    store
        .sync_budget_usage(
            &[
                (
                    BudgetKey {
                        level: BudgetLevel::Team,
                        target: tid("platform"),
                        period: BudgetPeriod::Month,
                        period_start_ms: month.start_ms,
                    },
                    500,
                ),
                (
                    BudgetKey {
                        level: BudgetLevel::Team,
                        target: tid("ops"),
                        period: BudgetPeriod::Month,
                        period_start_ms: month.start_ms,
                    },
                    10,
                ),
            ],
            &[],
            now,
        )
        .await
        .unwrap();
    let recent = (now - 2 * MIN).max(month.start_ms) / MIN * MIN;
    let rk = RollupKey {
        bucket_start: recent,
        api_key_id: keys[0].id.clone(),
        team_id: team_ids["platform"].clone(),
        organization_id: acme.clone(),
        model_group: "gpt-4o".into(),
        deployment_id: d_mock.clone(),
        error_class: String::new(),
    };
    store
        .write_request_batch(
            &[],
            &[],
            &[(
                rk,
                RollupCounts {
                    requests: 1,
                    cost: 700,
                    ..RollupCounts::default()
                },
            )],
        )
        .await
        .unwrap();
    let v = env.ok(&a, "/costs/budgets").await;
    assert_eq!(v["currency"], "EUR");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let by = |n: &str| {
        items
            .iter()
            .find(|i| i["budget"]["targetName"] == n)
            .unwrap()
            .clone()
    };
    let p = by("platform");
    assert_eq!(p["budget"]["used"], amount(500));
    assert_eq!(p["forecast"]["method"], "average");
    assert_eq!(p["forecast"]["lookbackDays"], 7);
    // 700 Nano in 7 Tagen = 100 je Tag.
    assert_eq!(p["forecast"]["dailyRate"], amount(100));
    // Budget 1000 Nano: Überschreitung erwartet (oder schon erreicht bei Monatsende).
    assert!(
        p["forecast"]["willExceed"].as_bool().unwrap()
            || month.end_ms.unwrap() - now < 6 * 86_400_000
    );
    assert!(p["burnDown"].as_array().unwrap().is_empty());
    let o = by("ops");
    assert_eq!(o["health"], "ok");
    assert_eq!(o["forecast"]["dailyRate"], amount(0));
    assert_eq!(o["forecast"]["willExceed"], false);
    assert_eq!(o["forecast"]["projected"], amount(10));

    // Lineare Hochrechnung und Burn-down für ein Ziel.
    let v = env
        .ok(
            &a,
            "/costs/budgets?level=team&target=platform&method=linear",
        )
        .await;
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["forecast"]["method"], "linear");
    assert!(items[0]["forecast"].get("lookbackDays").is_none());
    let burn = items[0]["burnDown"].as_array().unwrap();
    let spent: Vec<i64> = burn
        .iter()
        .filter_map(|b| b.get("spent").map(nanos))
        .collect();
    assert!(!spent.is_empty());
    // 700 von eben plus die Testdaten, falls sie im laufenden Monat liegen.
    let in_month = sum(&data, month.start_ms, now, |k| {
        k.team_id == team_ids["platform"]
    });
    assert_eq!(*spent.last().unwrap(), 700 + in_month.cost, "{burn:?}");
    assert!(spent.windows(2).all(|w| w[0] <= w[1]), "kumuliert");
    // Nach jetzt nur Prognose, nie beides.
    assert!(
        burn.iter()
            .all(|b| b.get("spent").is_some() != b.get("projected").is_some())
    );

    // Geltungsbereich: Team-Betrachter sieht nur das eigene Budget.
    let v = env.ok(&v_plat, "/costs/budgets").await;
    let names: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["budget"]["targetName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["platform"]);
    for q in [
        "lookbackDays=0",
        "lookbackDays=91",
        "target=platform",
        "method=magic",
    ] {
        let (s, _) = env.get(&a, &format!("/costs/budgets?{q}")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{q}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_in_memory() {
    scenario(Store::connect("sqlite::memory:").await.unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres() {
    let Ok(base) = std::env::var("AIGW_TEST_POSTGRES_URL") else {
        eprintln!("AIGW_TEST_POSTGRES_URL nicht gesetzt – Postgres-Test übersprungen");
        return;
    };
    let db = format!("aigw_costs_test_{}", new_id().simple());
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
