// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Backend `redis` gegen einen echten Redis/Valkey-Server.
//!
//! Läuft nur, wenn `AIGW_TEST_REDIS_URL` gesetzt ist (`scripts/check.sh` mit
//! `AIGW_TEST_REDIS=docker`). Jeder Test verwendet ein eigenes Schlüsselpräfix; Ausfälle
//! simuliert ein eigener TCP-Proxy je Test ([`Outage`]), nicht `CLIENT PAUSE` am gemeinsamen
//! Server.
//! Der Rückfalltest ohne Server läuft immer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unused_must_use
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aigw_counters::{
    CounterBackend, CounterError, Health, LimitCheck, RedisCounters, RedisOptions, Window,
};

/// Ausfall-Simulation (AIGW-94): ein TCP-Proxy vor Redis, der auf Kommando alle Daten
/// zurückhält (wie `CLIENT PAUSE ALL` – Verbindungen bleiben offen, Befehle hängen) und sie
/// beim Freigeben in Reihenfolge zustellt. Anders als eine Pause mit fester Dauer endet der
/// Ausfall erst, wenn der Test es sagt – das Ergebnis hängt nicht von der Last der Maschine
/// ab, und andere Tests am selben Server bleiben unberührt.
struct Outage {
    url: String,
    frozen: tokio::sync::watch::Sender<bool>,
}

impl Outage {
    async fn start(redis_url: &str) -> Self {
        let rest = redis_url
            .strip_prefix("redis://")
            .expect("AIGW_TEST_REDIS_URL: redis://host:port/db");
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (userinfo, target) = match authority.rsplit_once('@') {
            Some((u, t)) => (format!("{u}@"), t.to_owned()),
            None => (String::new(), authority.to_owned()),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (frozen, _) = tokio::sync::watch::channel(false);
        let tx = frozen.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = tokio::net::TcpStream::connect(&target).await else {
                    continue;
                };
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                tokio::spawn(pump(cr, sw, tx.subscribe()));
                tokio::spawn(pump(sr, cw, tx.subscribe()));
            }
        });
        Self {
            url: format!("redis://{userinfo}{addr}/{path}"),
            frozen,
        }
    }

    fn freeze(&self) {
        self.frozen.send_replace(true);
    }

    fn thaw(&self) {
        self.frozen.send_replace(false);
    }
}

/// Leitet Bytes weiter; solange `frozen`, bleiben gelesene Daten liegen.
async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    mut frozen: tokio::sync::watch::Receiver<bool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if frozen.wait_for(|f| !*f).await.is_err() {
            return;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

/// Optionen für die Ausfalltests: das Zeitlimit löst den Rückfall aus, solange der Proxy
/// zurückhält; danach muss es auch auf einer stark belasteten Maschine reichen (CI, parallele
/// Builds). Es bestimmt nur die Testdauer, nicht das Ergebnis.
fn outage_opts() -> RedisOptions {
    RedisOptions {
        key_prefix: format!("aigwtest:{}:", uuid::Uuid::now_v7().simple()),
        timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(10),
        reconnect_interval: Duration::from_millis(100),
        ..RedisOptions::default()
    }
}

/// Wartet (höchstens 30 s) auf `cond`.
async fn eventually(mut cond: impl FnMut() -> bool) {
    for _ in 0..300 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn url() -> Option<String> {
    let url = std::env::var("AIGW_TEST_REDIS_URL").ok();
    if url.is_none() {
        eprintln!("AIGW_TEST_REDIS_URL nicht gesetzt – Redis-Test übersprungen");
    }
    url
}

fn opts(prefix: &str) -> RedisOptions {
    RedisOptions {
        key_prefix: prefix.to_owned(),
        timeout: Duration::from_secs(2),
        ..RedisOptions::default()
    }
}

async fn pair(url: &str) -> (Arc<RedisCounters>, Arc<RedisCounters>) {
    let prefix = format!("aigwtest:{}:", uuid::Uuid::now_v7().simple());
    let a = RedisCounters::connect(url, opts(&prefix)).await.unwrap();
    let b = RedisCounters::connect(url, opts(&prefix)).await.unwrap();
    assert!(!a.is_degraded(), "{:?}", a.health());
    (a, b)
}

#[tokio::test]
async fn shared_exact_counts_and_settle() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    let lim = |amount| [LimitCheck::new("team:x", Window::Hour, 100, amount)];

    let ra = a.reserve(&lim(60)).await.unwrap();
    assert!(!ra.is_local_fallback());
    assert_eq!(ra.quotas()[0].remaining, 40);
    let rej = b.reserve(&lim(41)).await.unwrap_err();
    assert_eq!(rej.used, 60);
    assert!(rej.retry_after.is_some());
    b.reserve(&lim(40)).await.unwrap();
    assert_eq!(a.get("team:x", Window::Hour).await, 100);

    a.settle(ra, 20).await;
    assert_eq!(b.get("team:x", Window::Hour).await, 60);
    let rb = b.reserve(&lim(10)).await.unwrap();
    b.settle(rb, 25).await; // mehr als reserviert
    assert_eq!(a.get("team:x", Window::Hour).await, 85); // 20 + 40 + 25

    let never = a.reserve(&lim(101)).await.unwrap_err();
    assert_eq!(never.retry_after, None);
}

#[tokio::test]
async fn multi_key_all_or_nothing() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    a.reserve(&[LimitCheck::new("team", Window::Hour, 100, 95)])
        .await
        .unwrap();
    let checks = |amount| {
        [
            LimitCheck::new("key", Window::Hour, 1_000, amount),
            LimitCheck::new("team", Window::Hour, 100, amount),
            LimitCheck::fixed("org:req", Window::Day, 1_000, 1),
        ]
    };
    let rej = b.reserve(&checks(10)).await.unwrap_err();
    assert_eq!(rej.index, 1);
    assert_eq!(b.get("key", Window::Hour).await, 0);
    assert_eq!(b.get("org:req", Window::Day).await, 0);

    let r = b.reserve(&checks(5)).await.unwrap();
    assert_eq!(r.tightest().unwrap().0, 1);
    b.settle(r, 2).await; // org:req ist fixed und bleibt bei 1
    assert_eq!(a.get("key", Window::Hour).await, 2);
    assert_eq!(a.get("team", Window::Hour).await, 97);
    assert_eq!(a.get("org:req", Window::Day).await, 1);

    // Doppelter Schlüssel in einem Aufruf zählt kumulativ.
    let dup = [
        LimitCheck::new("dup", Window::Hour, 10, 6),
        LimitCheck::new("dup", Window::Hour, 10, 6),
    ];
    assert!(a.reserve(&dup).await.is_err());
    assert_eq!(a.get("dup", Window::Hour).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_tasks_two_instances_never_exceed_limit() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    let granted = Arc::new(AtomicU64::new(0));
    let mut tasks = Vec::new();
    for t in 0..64u64 {
        let c = if t % 2 == 0 { a.clone() } else { b.clone() };
        let granted = granted.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..100u64 {
                let key = format!("key{}", t % 8);
                let amount = 1 + (t + i) % 5;
                let checks = [
                    LimitCheck::new(&key, Window::Hour, 1_000, amount),
                    LimitCheck::new("team", Window::Hour, 3_000, amount),
                ];
                if let Ok(r) = c.reserve(&checks).await {
                    granted.fetch_add(amount, Ordering::Relaxed);
                    assert!(!r.is_local_fallback());
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let team = a.get("team", Window::Hour).await;
    assert_eq!(team, granted.load(Ordering::Relaxed));
    assert!(team <= 3_000, "Limit überschritten: {team}");
    assert!(team > 3_000 - 5, "Limit nicht ausgeschöpft: {team}");
}

#[tokio::test]
async fn falls_back_and_recovers() {
    let Some(url) = url() else { return };
    let outage = Outage::start(&url).await;
    let c = RedisCounters::connect(&outage.url, outage_opts())
        .await
        .unwrap();
    assert!(!c.is_degraded());

    // Server hängt → Zeitüberschreitung → lokaler Rückfall.
    outage.freeze();
    let r = c
        .reserve(&[LimitCheck::new("k", Window::Minute, 10, 3)])
        .await
        .unwrap();
    assert!(r.is_local_fallback());
    assert!(matches!(c.health(), Health::Degraded(_)));
    c.settle(r, 1).await; // lokal gebucht → lokal abgeglichen
    // Im Rückfall direkt lokal (ohne erneutes Warten auf Redis).
    let r2 = c
        .reserve(&[LimitCheck::new("k2", Window::Minute, 10, 4)])
        .await
        .unwrap();
    assert!(r2.is_local_fallback());
    c.settle(r2, 2).await;
    assert!(c.is_degraded(), "Ausfall dauert an: {:?}", c.health());

    // Server antwortet wieder → der Monitor verbindet wieder.
    outage.thaw();
    eventually(|| !c.is_degraded()).await;
    assert!(!c.is_degraded(), "{:?}", c.health());
    // F28: der lokal gebuchte Verbrauch wird nach der Rückkehr in Redis nachgetragen
    // ("k2" nur lokal; "k" lief in die Zeitüberschreitung und kann doppelt stehen).
    c.replay_now().await.unwrap();
    assert_eq!(c.get("k2", Window::Minute).await, 2);
    assert!(c.stats().replayed >= 1, "{:?}", c.stats());
    assert_eq!(c.stats().pending, 0);
    let r = c
        .reserve(&[LimitCheck::new("k", Window::Minute, 10, 3)])
        .await
        .unwrap();
    assert!(!r.is_local_fallback());
}

/// F28: Abgleiche von Redis-Reservierungen während eines Ausfalls werden zurückgestellt und
/// nachgetragen, lokal gebuchter Verbrauch ebenso; abgelaufene Buckets werden verworfen.
#[tokio::test]
async fn deferred_settles_replayed_after_outage() {
    let Some(url) = url() else { return };
    let outage = Outage::start(&url).await;
    let c = RedisCounters::connect(&outage.url, outage_opts())
        .await
        .unwrap();
    let hour = |n| [LimitCheck::new("h", Window::Hour, 1_000, n)];
    // Kurzer fester Zeitraum, der während des Ausfalls endet (Delta wird verworfen).
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    };
    let short_end = now() + 3_000;
    let short = Window::period(short_end - 4_000, short_end);
    let sec = |n| [LimitCheck::new("s", short, 1_000, n)];
    let rh = c.reserve(&hour(100)).await.unwrap();
    let rs = c.reserve(&sec(100)).await.unwrap();
    assert!(!rh.is_local_fallback() && !rs.is_local_fallback());
    assert!(now() < short_end, "Vorbereitung zu langsam");

    outage.freeze();
    // Auslöser des Rückfalls auf eigenem Schlüssel (das Skript kann später doch noch laufen).
    let t = c
        .reserve(&[LimitCheck::new("trigger", Window::Hour, 1_000, 1)])
        .await
        .unwrap();
    assert!(t.is_local_fallback() && c.is_degraded());
    // im Ausfall: lokal gebucht (+5) und zurückgestellte Abgleiche (−60 bzw. −90)
    let local = c.reserve(&hour(5)).await.unwrap();
    assert!(local.is_local_fallback());
    c.settle(rh, 40).await;
    c.settle(rs, 10).await;
    assert_eq!(c.lost_settles(), 2);
    assert!(c.stats().pending >= 2, "{:?}", c.stats());

    // Der Ausfall dauert, bis der kurze Zeitraum sicher vorbei ist.
    while now() < short_end + 200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(c.is_degraded());

    // Rückkehr: der Monitor verbindet neu und trägt nach.
    outage.thaw();
    eventually(|| {
        let st = c.stats();
        !c.is_degraded() && st.pending == 0 && st.replayed >= 2
    })
    .await;
    let st = c.stats();
    assert!(!c.is_degraded(), "{:?}", c.health());
    assert_eq!(st.pending, 0, "{st:?}");
    assert!(
        st.dropped >= 1,
        "Delta des abgelaufenen Zeitraums verworfen: {st:?}"
    );
    // Stunde: 100 − 60 (zurückgestellt) + 5 (lokal gebucht) = 45
    assert_eq!(c.get("h", Window::Hour).await, 45);
}

/// F25: feste Zeiträume (Kalender) in Redis: kein Gleiten, Ablauf nach dem Ende.
#[tokio::test]
async fn period_windows_exact_and_expire() {
    let Some(url) = url() else { return };
    let (a, b) = pair(&url).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let start = now - 10_000;
    let p = Window::period(start, now + 50_000);
    let key = format!("key:x:requests@calendar-day:{start}");
    let lim = |n| [LimitCheck::new(&key, p, 10, n)];
    let r = a.reserve(&lim(6)).await.unwrap();
    let reset = r.quotas()[0].reset_after;
    assert!(
        reset > Duration::from_secs(45) && reset <= Duration::from_secs(51),
        "{reset:?}"
    );
    let rej = b.reserve(&lim(5)).await.unwrap_err();
    assert!(rej.retry_after.unwrap() > Duration::from_secs(45));
    assert_eq!(rej.used, 6);
    a.settle(r, 2).await;
    assert_eq!(b.get(&key, p).await, 2);
    b.reserve(&lim(8)).await.unwrap();
    assert!(a.reserve(&lim(1)).await.is_err());
    // abgelaufener Zeitraum: 0
    let old = Window::period(start - 100_000, start - 50_000);
    assert_eq!(a.get(&key, old).await, 0);
}

/// F27: Server im Cluster-Modus → klare Fehlermeldung beim Start (Fake-Server, kein Redis
/// nötig).
#[tokio::test]
async fn cluster_mode_is_rejected_at_start() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    let Ok(n) = s.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    // Je RESP-Befehl (Zeile `*<n>`) eine Antwort – der Client pipelinet.
                    let req = String::from_utf8_lossy(&buf[..n]).to_ascii_uppercase();
                    let mut reply = String::new();
                    for cmd in req.split("\r\n*").filter(|c| !c.trim().is_empty()) {
                        if cmd.contains("INFO") {
                            let body = "# Cluster\r\ncluster_enabled:1\r\n";
                            reply.push_str(&format!("${}\r\n{body}\r\n", body.len()));
                        } else {
                            reply.push_str("+OK\r\n");
                        }
                    }
                    if s.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    let err = RedisCounters::connect(
        &format!("redis://{addr}/0"),
        RedisOptions {
            connect_timeout: Duration::from_secs(2),
            ..RedisOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CounterError::RedisCluster), "{err}");
    assert!(err.to_string().contains("Cluster"));
}

#[tokio::test]
async fn unreachable_redis_counts_locally() {
    // Braucht keinen Server: Port 1 ist nicht erreichbar.
    let c = RedisCounters::connect(
        "redis://127.0.0.1:1/0",
        RedisOptions {
            connect_timeout: Duration::from_millis(300),
            ..RedisOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(c.is_degraded());
    assert!(matches!(c.health(), Health::Degraded(_)));
    let r = c
        .reserve(&[LimitCheck::new("k", Window::Minute, 10, 7)])
        .await
        .unwrap();
    assert!(r.is_local_fallback());
    assert!(
        c.reserve(&[LimitCheck::new("k", Window::Minute, 10, 4)])
            .await
            .is_err()
    );
    c.settle(r, 2).await;
    assert_eq!(c.get("k", Window::Minute).await, 2);
    assert!(
        RedisCounters::connect("kein-url", RedisOptions::default())
            .await
            .is_err()
    );
}
