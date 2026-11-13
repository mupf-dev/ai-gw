// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aigw_core::resource::{
    self as r, ApiKeySpec, ApiKeyStatus, LimitMetric, LimitSpec, LimitWindow, ModelGroupSpec,
    Object, ObjectMeta, OrganizationSpec, TeamSpec, new_id,
};
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx};
use aigw_counters::{LocalCounters, Window};

use super::plan::{Level, plan};
use super::*;

fn limit(metric: LimitMetric, n: u64, window: LimitWindow, group: Option<&str>) -> LimitSpec {
    LimitSpec {
        metric,
        limit: n,
        window,
        model_group: group.map(str::to_owned),
    }
}

fn group(name: &str, aliases: &[&str], limits: Vec<LimitSpec>) -> Arc<ModelGroupCtx> {
    Arc::new(ModelGroupCtx {
        group: Object {
            id: new_id(),
            metadata: ObjectMeta::named(name),
            spec: ModelGroupSpec {
                aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
                limits,
                ..ModelGroupSpec::default()
            },
        },
        deployments: Vec::new(),
    })
}

fn key(
    key_limits: Vec<LimitSpec>,
    team_limits: Vec<LimitSpec>,
    org_limits: Vec<LimitSpec>,
) -> Arc<KeyCtx> {
    Arc::new(KeyCtx {
        key: r::ApiKey {
            id: new_id(),
            metadata: ObjectMeta::named("k"),
            spec: ApiKeySpec {
                limits: key_limits,
                ..ApiKeySpec::default()
            },
            status: ApiKeyStatus {
                prefix: "x".into(),
                state: r::ApiKeyState::Active,
                last_used_at: None,
            },
        },
        team: Some(Arc::new(Object {
            id: new_id(),
            metadata: ObjectMeta::named("t"),
            spec: TeamSpec {
                organization: "o".into(),
                limits: team_limits,
                ..TeamSpec::default()
            },
        })),
        organization: Some(Arc::new(Object {
            id: new_id(),
            metadata: ObjectMeta::named("o"),
            spec: OrganizationSpec {
                limits: org_limits,
                ..OrganizationSpec::default()
            },
        })),
        expires_at: None,
    })
}

#[test]
fn plan_orders_by_metric_and_scopes_groups() {
    use LimitMetric as M;
    use LimitWindow as W;
    let g = group(
        "gpt-4o",
        &["gpt-4o-latest"],
        vec![limit(M::Tokens, 9, W::Minute, None)],
    );
    let k = key(
        vec![
            limit(M::Tokens, 100, W::Minute, None),
            limit(M::Requests, 10, W::Day, Some("gpt-4o-latest")),
            limit(M::Requests, 5, W::Minute, Some("claude")),
            limit(M::Concurrency, 1, W::Minute, None),
        ],
        vec![limit(M::OutputTokens, 50, W::Hour, None)],
        vec![limit(M::InputTokens, 70, W::Minute, None)],
    );
    let mut out = Vec::new();
    plan(Some(&k), Some(&g), &mut out);
    let got: Vec<_> = out
        .iter()
        .map(|p| (p.metric, p.level, p.window, p.limit))
        .collect();
    assert_eq!(
        got,
        [
            (Metric::Requests, Level::Key, Window::Day, 10),
            (Metric::Tokens, Level::Key, Window::Minute, 100),
            (Metric::Tokens, Level::ModelGroup, Window::Minute, 9),
            (Metric::InputTokens, Level::Organization, Window::Minute, 70),
            (Metric::OutputTokens, Level::Team, Window::Hour, 50),
        ]
    );
    // Gruppenbezogenes Limit unter kanonischem Namen, auch wenn ein Alias konfiguriert ist.
    assert_eq!(out[0].key, format!("key:{}:requests:gpt-4o", k.key.id));
    assert_eq!(out[2].key, format!("group:{}:tokens", g.group.id));

    // Ohne Snapshot-Gruppe entfallen gruppenbezogene Limits.
    let mut out = Vec::new();
    plan(Some(&k), None, &mut out);
    assert_eq!(out.len(), 3);
}

#[tokio::test]
async fn admit_is_atomic_across_levels_and_settles_parts() {
    use LimitMetric as M;
    use LimitWindow as W;
    let counters = Arc::new(LocalCounters::new());
    let limiter = Limiter::new(counters.clone(), None, &LimitsConfig::default());
    let k = key(
        vec![
            limit(M::Requests, 10, W::Minute, None),
            limit(M::Tokens, 1000, W::Minute, None),
            limit(M::InputTokens, 1000, W::Minute, None),
        ],
        vec![limit(M::Requests, 1, W::Minute, None)],
        vec![],
    );
    let auth = AuthContext {
        key_id: None,
        key: Some(Arc::clone(&k)),
        snapshot: None,
    };
    let est = TokenEstimate {
        input: 30,
        output: 100,
    };
    let a = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap();
    let id = k.key.id;
    assert_eq!(
        counters.current(&format!("key:{id}:tokens"), Window::Minute),
        130
    );
    assert!(a.parts.tokens.is_some() && a.parts.input.is_some() && a.parts.output.is_none());
    // Header: je Metrik das knappste Limit (Anfragen: Team mit Rest 0; Tokens: 1000 − 130).
    let mut h = axum::http::HeaderMap::new();
    a.headers()
        .apply(crate::ErrorFormat::OpenAi, &mut h, SystemTime::now());
    let get = |n: &str| h.get(n).unwrap().to_str().unwrap().to_owned();
    assert_eq!(get("x-ratelimit-limit-requests"), "1");
    assert_eq!(get("x-ratelimit-remaining-requests"), "0");
    assert_eq!(get("x-ratelimit-limit-tokens"), "1000");
    assert_eq!(get("x-ratelimit-remaining-tokens"), "870");
    assert!(get("x-ratelimit-reset-tokens").ends_with('s'));

    // Team-Limit erschöpft → nichts gebucht, auch nicht beim Key.
    let denied = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap_err();
    assert_eq!(
        denied.error.error.code.as_deref(),
        Some("rate_limit_exceeded")
    );
    assert!(denied.error.retry_after.is_some());
    assert!(!denied.headers.is_empty());
    assert_eq!(
        counters.current(&format!("key:{id}:requests"), Window::Minute),
        1
    );
    assert_eq!(
        counters.current(&format!("key:{id}:tokens"), Window::Minute),
        130
    );

    // Abschluss: Tokens auf die tatsächliche Usage, Eingabe auf die Eingabe.
    let mut rec = RequestRecord::new("r", aigw_core::wire::WireFormat::OpenAiChat, 200);
    rec.usage = Usage {
        input_tokens: 20,
        output_tokens: 5,
        ..Usage::default()
    };
    a.complete(&mut rec);
    for _ in 0..100 {
        if counters.current(&format!("key:{id}:tokens"), Window::Minute) == 25 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        counters.current(&format!("key:{id}:tokens"), Window::Minute),
        25
    );
    assert_eq!(
        counters.current(&format!("key:{id}:input-tokens"), Window::Minute),
        20
    );
    assert_eq!(
        counters.current(&format!("key:{id}:requests"), Window::Minute),
        1
    );
    assert_eq!(rec.cost, None, "ohne Snapshot kein Preis");
}

#[tokio::test]
async fn too_large_request_has_no_retry_after() {
    let limiter = Limiter::local();
    let k = key(
        vec![limit(LimitMetric::Tokens, 100, LimitWindow::Minute, None)],
        vec![],
        vec![],
    );
    let auth = AuthContext {
        key_id: None,
        key: Some(k),
        snapshot: None,
    };
    let est = TokenEstimate {
        input: 10,
        output: 4096,
    };
    let denied = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap_err();
    assert_eq!(denied.error.retry_after, None);
    assert!(denied.error.error.message.contains("max_tokens"));
}

/// Overhead je Anfrage mit `local`: `admit` (Plan + Reservierung von 5 Limits auf 4 Ebenen)
/// und `complete` (Abgleich als Task). Release-Messung:
/// `cargo test --release -p aigw-proxy --lib limits::tests::overhead -- --ignored --nocapture`.
#[test]
#[ignore = "Messung, nur im Release-Build aussagekräftig"]
#[allow(clippy::print_stdout)]
fn overhead() {
    use LimitMetric as M;
    use LimitWindow as W;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let big = u64::MAX / 4;
        let g = group("gpt-4o", &[], vec![limit(M::Tokens, big, W::Minute, None)]);
        let k = key(
            vec![
                limit(M::Requests, big, W::Minute, None),
                limit(M::Tokens, big, W::Minute, None),
            ],
            vec![limit(M::Tokens, big, W::Minute, None)],
            vec![limit(M::Requests, big, W::Day, None)],
        );
        let auth = AuthContext {
            key_id: None,
            key: Some(k),
            snapshot: None,
        };
        let limiter = Limiter::local();
        let none = Limiter::local();
        let empty = AuthContext::default();
        let est = TokenEstimate {
            input: 100,
            output: 1000,
        };
        let mut rec = RequestRecord::new("r", aigw_core::wire::WireFormat::OpenAiChat, 200);
        rec.usage = Usage {
            input_tokens: 90,
            output_tokens: 300,
            ..Usage::default()
        };
        let cases: [(&str, &Limiter, &AuthContext, Option<&Arc<ModelGroupCtx>>); 2] = [
            ("ohne Limits", &none, &empty, None),
            (
                "5 Limits (Key, Team, Org, Gruppe)",
                &limiter,
                &auth,
                Some(&g),
            ),
        ];
        for (name, l, a, grp) in cases {
            let n = 200_000u32;
            let start = std::time::Instant::now();
            for i in 0..n {
                let adm = l.admit(a, grp, est, SystemTime::now()).await.unwrap();
                adm.complete(&mut rec);
                if i % 1024 == 0 {
                    // Abgleich-Tasks laufen lassen (sonst wächst nur die Warteschlange).
                    tokio::task::yield_now().await;
                }
            }
            let per = start.elapsed() / n;
            println!("limits overhead {name}: {per:?} je Anfrage (admit + complete)");
        }
    });
}
