// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aigw_core::resource::{
    self as r, ApiKeySpec, ApiKeyStatus, LimitMetric, LimitSpec, LimitWindow, ModelGroupSpec,
    Object, ObjectMeta, OrganizationSpec, TeamSpec, new_id,
};
use aigw_core::snapshot::{KeyCtx, ModelGroupCtx};
use aigw_counters::{LocalCounters, Window};

use super::plan::{Level, plan, plan_slots};
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
    plan(Some(&k), Some(&g), &Periods::default(), 0, &mut out);
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
    plan(Some(&k), None, &Periods::default(), 0, &mut out);
    assert_eq!(out.len(), 3);
}

#[test]
fn plan_calendar_windows_carry_period_in_key() {
    use LimitMetric as M;
    use LimitWindow as W;
    let g = group("g", &[], vec![limit(M::Requests, 3, W::CalendarDay, None)]);
    let k = key(
        vec![
            limit(M::Tokens, 100, W::CalendarMonth, None),
            limit(M::Tokens, 100, W::Days30, None),
        ],
        vec![limit(M::Tokens, 100, W::CalendarWeek, None)],
        Vec::new(),
    );
    // Berlin, Mittwoch 2026-11-11 10:00 UTC
    let now = 1_794_391_200_000u64;
    let periods = Periods::new(aigw_billing::Calendar::new("Europe/Berlin").unwrap());
    let mut out = Vec::new();
    plan(Some(&k), Some(&g), &periods, now, &mut out);
    let by = |l: Level, w: W| {
        out.iter()
            .find(|p| p.level == l && p.spec_window == w)
            .unwrap()
    };
    // Kalendermonat November in Berlin: 31.10. 23:00 UTC bis 30.11. 23:00 UTC
    let m = by(Level::Key, W::CalendarMonth);
    assert_eq!(
        m.window,
        Window::period(1_793_487_600_000, 1_796_079_600_000)
    );
    assert_eq!(
        m.key,
        format!("key:{}:tokens@calendar-month:1793487600000", k.key.id)
    );
    // gleitend: ohne Zeitraum im Schlüssel
    let d = by(Level::Key, W::Days30);
    assert_eq!(d.window, Window::Days30);
    assert_eq!(d.key, format!("key:{}:tokens", k.key.id));
    // Woche ab Montag 9.11. 00:00 MEZ = 8.11. 23:00 UTC
    let w = by(Level::Team, W::CalendarWeek);
    assert_eq!(
        w.window,
        Window::period(1_794_178_800_000, 1_794_783_600_000)
    );
    let day = by(Level::ModelGroup, W::CalendarDay);
    assert_eq!(
        day.window,
        Window::period(1_794_351_600_000, 1_794_438_000_000)
    );
    assert!(day.key.ends_with("@calendar-day:1794351600000"));
    // Zwischenspeicher: im selben Zeitraum identisch, danach der nächste
    assert_eq!(periods.window(W::CalendarDay, now + 3_600_000), day.window);
    assert_eq!(
        periods.window(W::CalendarDay, 1_794_438_000_000),
        Window::period(1_794_438_000_000, 1_794_524_400_000)
    );
}

#[tokio::test]
async fn calendar_limit_resets_at_period_end_and_reports_reset() {
    use LimitMetric as M;
    use LimitWindow as W;
    let clock = Arc::new(aigw_counters::ManualClock::new(0));
    let counters = Arc::new(LocalCounters::with_clock(clock.clone()));
    let limiter = Limiter::new(counters.clone(), None, &LimitsConfig::default());
    assert_eq!(limiter.timezone(), "UTC");
    let k = key(
        vec![limit(M::Requests, 2, W::CalendarDay, None)],
        Vec::new(),
        Vec::new(),
    );
    let auth = AuthContext {
        key: Some(k),
        ..AuthContext::default()
    };
    // 2026-11-13 22:00 UTC: noch 2 h bis Mitternacht
    let day = 1_794_614_400_000u64;
    let t = day - 2 * 3_600_000;
    clock.set(t);
    let at = |ms: u64| SystemTime::UNIX_EPOCH + Duration::from_millis(ms);
    let est = TokenEstimate::default();
    let a = limiter.admit(&auth, None, est, at(t)).await.unwrap();
    // Reset-Header = Ende des Kalendertags, nicht 24 h gleitend
    assert_eq!(a.headers(), &{
        let mut h = RateHeaders::default();
        h.set_requests_for_test(2, 1, Duration::from_millis(2 * 3_600_000));
        h
    });
    let _b = limiter.admit(&auth, None, est, at(t)).await.unwrap();
    clock.set(t + 1_000);
    let denied = limiter
        .admit(&auth, None, est, at(t + 1_000))
        .await
        .unwrap_err();
    assert_eq!(
        denied.error.retry_after,
        Some(Duration::from_millis(2 * 3_600_000 - 1_000))
    );
    assert!(denied.error.error.message.contains("Kalendertag"));
    // Nach Mitternacht (UTC) neuer Zeitraum
    clock.set(day + 1);
    assert!(limiter.admit(&auth, None, est, at(day + 1)).await.is_ok());
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
        workload: None,
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
    a.complete(&mut rec, &[]);
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

/// S-38: Pre-Plugins vergrößern die Eingabe – Differenz wird nachreserviert, abgelehnt,
/// wenn das Limit nicht reicht, und beim Abschluss zählt genau der tatsächliche Verbrauch.
#[tokio::test]
async fn plugin_grown_input_is_reserved_or_denied() {
    use LimitMetric as M;
    use LimitWindow as W;
    let counters = Arc::new(LocalCounters::new());
    let limiter = Limiter::new(counters.clone(), None, &LimitsConfig::default());
    let k = key(
        vec![
            limit(M::Tokens, 1000, W::Minute, None),
            limit(M::InputTokens, 300, W::Minute, None),
        ],
        vec![],
        vec![],
    );
    let id = k.key.id;
    let auth = AuthContext {
        key_id: None,
        key: Some(k),
        snapshot: None,
        workload: None,
    };
    let est = TokenEstimate {
        input: 50,
        output: 100,
    };
    let cur = |m: &str| counters.current(&format!("key:{id}:{m}"), Window::Minute);
    let mut a = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap();
    // Kleiner oder gleich: nichts zu tun.
    a.grow_input(40, SystemTime::now()).await.unwrap();
    assert_eq!(cur("input-tokens"), 50);
    // Gewachsen auf 200: +150 nachreserviert.
    a.grow_input(200, SystemTime::now()).await.unwrap();
    assert_eq!(cur("input-tokens"), 200);
    assert_eq!(cur("tokens"), 300);
    assert_eq!(a.input_estimate(), 200);

    // Zweite Anfrage: 50 passt noch, Wachstum auf 150 (+100) nicht mehr (200+50+100 > 300).
    let mut b = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap();
    let denied = b.grow_input(150, SystemTime::now()).await.unwrap_err();
    assert_eq!(
        denied.error.error.code.as_deref(),
        Some("rate_limit_exceeded")
    );
    assert_eq!(cur("input-tokens"), 250, "abgelehnt: nichts nachgebucht");
    drop(b);

    // Abschluss von a: tatsächlich 180 Eingabe + 10 Ausgabe.
    let mut rec = RequestRecord::new("r", aigw_core::wire::WireFormat::OpenAiChat, 200);
    rec.usage = Usage {
        input_tokens: 180,
        output_tokens: 10,
        ..Usage::default()
    };
    a.complete(&mut rec, &[]);
    // b (verworfen) bleibt konservativ mit 50/150 gebucht.
    for _ in 0..100 {
        if cur("input-tokens") == 230 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(cur("input-tokens"), 230);
    assert_eq!(cur("tokens"), 190 + 150);
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
        workload: None,
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

#[test]
fn plan_slots_levels_and_group_scope() {
    use LimitMetric as M;
    use LimitWindow as W;
    let g = group(
        "gpt-4o",
        &["latest"],
        vec![limit(M::Concurrency, 9, W::Minute, Some("egal"))],
    );
    let k = key(
        vec![
            limit(M::Concurrency, 2, W::Minute, None),
            limit(M::Concurrency, 1, W::Minute, Some("latest")),
            limit(M::Concurrency, 1, W::Minute, Some("claude")),
            limit(M::Requests, 5, W::Minute, None),
        ],
        vec![limit(M::Concurrency, 4, W::Second, None)],
        vec![],
    );
    let mut out = Vec::new();
    plan_slots(Some(&k), Some(&g), &mut out);
    let got: Vec<_> = out
        .iter()
        .map(|p| (p.key.clone(), p.limit, p.level))
        .collect();
    let (kid, tid) = (k.key.id, k.team.as_ref().unwrap().id);
    assert_eq!(
        got,
        [
            (format!("key:{kid}:concurrency"), 2, Level::Key),
            (format!("key:{kid}:concurrency:gpt-4o"), 1, Level::Key),
            (format!("team:{tid}:concurrency"), 4, Level::Team),
            (
                format!("group:{}:concurrency", g.group.id),
                9,
                Level::ModelGroup
            ),
        ]
    );
    // Fenster-Limits enthalten `concurrency` nicht.
    let mut win = Vec::new();
    plan(Some(&k), Some(&g), &Periods::default(), 0, &mut win);
    assert_eq!(win.len(), 1);
}

fn conc_auth(key_limit: u64, team_limit: u64) -> (AuthContext, Arc<KeyCtx>) {
    let k = key(
        vec![limit(
            LimitMetric::Concurrency,
            key_limit,
            LimitWindow::Minute,
            None,
        )],
        vec![limit(
            LimitMetric::Concurrency,
            team_limit,
            LimitWindow::Minute,
            None,
        )],
        vec![],
    );
    (
        AuthContext {
            key_id: None,
            key: Some(Arc::clone(&k)),
            snapshot: None,
            workload: None,
        },
        k,
    )
}

/// S-12: Plätze werden mit `complete` und beim Fallenlassen frei; Ablehnung `429` mit
/// `concurrency_limit_exceeded`, `Retry-After` und atomar über die Ebenen.
#[tokio::test]
async fn concurrency_admit_release_and_reject() {
    let counters = Arc::new(LocalCounters::new());
    let limiter = Limiter::new(counters.clone(), None, &LimitsConfig::default());
    let (auth, k) = conc_auth(2, 3);
    let kk = format!("key:{}:concurrency", k.key.id);
    let tk = format!("team:{}:concurrency", k.team.as_ref().unwrap().id);
    let now = SystemTime::now;
    let est = TokenEstimate::default();
    let a = limiter.admit(&auth, None, est, now()).await.unwrap();
    let b = limiter.admit(&auth, None, est, now()).await.unwrap();
    assert_eq!(counters.in_flight(&kk).await, 2);
    let d = limiter.admit(&auth, None, est, now()).await.unwrap_err();
    assert_eq!(
        d.error.error.code.as_deref(),
        Some("concurrency_limit_exceeded")
    );
    assert_eq!(d.error.retry_after, Some(Duration::from_secs(1)));
    assert!(d.error.error.message.contains("API-Key"));
    assert_eq!(counters.in_flight(&tk).await, 2, "Ablehnung belegt nichts");
    // Abschluss gibt frei.
    let mut rec = RequestRecord::new("r", aigw_core::wire::WireFormat::OpenAiChat, 200);
    a.complete(&mut rec, &[]);
    assert_eq!(counters.in_flight(&kk).await, 1);
    // Fallenlassen (Abbruch) gibt frei.
    drop(b);
    assert_eq!(counters.in_flight(&kk).await, 0);
    assert_eq!(counters.in_flight(&tk).await, 0);
}

/// S-12: Ein anderes Limit lehnt nach der Platzbelegung ab → Plätze sofort wieder frei.
#[tokio::test]
async fn concurrency_released_when_window_limit_rejects() {
    let counters = Arc::new(LocalCounters::new());
    let limiter = Limiter::new(counters.clone(), None, &LimitsConfig::default());
    let k = key(
        vec![
            limit(LimitMetric::Concurrency, 5, LimitWindow::Minute, None),
            limit(LimitMetric::Requests, 1, LimitWindow::Minute, None),
        ],
        vec![],
        vec![],
    );
    let auth = AuthContext {
        key_id: None,
        key: Some(Arc::clone(&k)),
        snapshot: None,
        workload: None,
    };
    let kk = format!("key:{}:concurrency", k.key.id);
    let est = TokenEstimate::default();
    let a = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap();
    let d = limiter
        .admit(&auth, None, est, SystemTime::now())
        .await
        .unwrap_err();
    assert_eq!(d.error.error.code.as_deref(), Some("rate_limit_exceeded"));
    assert_eq!(counters.in_flight(&kk).await, 1);
    drop(a);
    assert_eq!(counters.in_flight(&kk).await, 0);
}

/// S-12 Nebenläufigkeit: viele Tasks gleichzeitig, nie mehr als das Limit; abgebrochene Tasks
/// (abort mitten in der Anfrage) geben ihre Plätze frei.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrency_parallel_never_exceeds_and_abort_releases() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let counters = Arc::new(LocalCounters::new());
    let limiter = Arc::new(Limiter::new(
        counters.clone(),
        None,
        &LimitsConfig::default(),
    ));
    let (auth, k) = conc_auth(3, 100);
    let kk = format!("key:{}:concurrency", k.key.id);
    let auth = Arc::new(auth);
    let (cur, peak, ok) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let (l, a) = (Arc::clone(&limiter), Arc::clone(&auth));
        let (cur, peak, ok) = (Arc::clone(&cur), Arc::clone(&peak), Arc::clone(&ok));
        tasks.push(tokio::spawn(async move {
            for _ in 0..50 {
                if let Ok(adm) = l
                    .admit(&a, None, TokenEstimate::default(), SystemTime::now())
                    .await
                {
                    let n = cur.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    ok.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                    cur.fetch_sub(1, Ordering::SeqCst);
                    drop(adm);
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) <= 3);
    assert!(ok.load(Ordering::Relaxed) > 0);
    assert_eq!(counters.in_flight(&kk).await, 0);

    // Abbruch: Tasks halten Plätze und werden abgebrochen.
    let mut held = Vec::new();
    for _ in 0..3 {
        let (l, a) = (Arc::clone(&limiter), Arc::clone(&auth));
        held.push(tokio::spawn(async move {
            let _adm = l
                .admit(&a, None, TokenEstimate::default(), SystemTime::now())
                .await
                .unwrap();
            std::future::pending::<()>().await;
        }));
    }
    for _ in 0..200 {
        if counters.in_flight(&kk).await == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(counters.in_flight(&kk).await, 3);
    assert!(
        limiter
            .admit(&auth, None, TokenEstimate::default(), SystemTime::now())
            .await
            .is_err()
    );
    for h in &held {
        h.abort();
    }
    for h in held {
        let _ = h.await;
    }
    assert_eq!(counters.in_flight(&kk).await, 0);
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
            workload: None,
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
                adm.complete(&mut rec, &[]);
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
