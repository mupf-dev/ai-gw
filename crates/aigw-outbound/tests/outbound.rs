// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Outbound-Baustein gegen lokale Testserver: DNS-Rebinding (öffentlich wirkender Name →
//! private Adresse), IP-Literale, Freischaltung per Name/CIDR, Weiterleitungen, Größen- und
//! Zeitlimits, TLS-Varianten.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::AdminConfig;
use aigw_outbound::{
    AddressGuard, FetchLimits, OutboundClient, OutboundError, Redirects, StaticResolver,
    TlsOptions, UpstreamPolicy,
};
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use bytes::Bytes;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

async fn server() -> SocketAddr {
    let app = Router::new()
        .route("/ok", get(|| async { "hallo" }))
        .route(
            "/redirect",
            get(|| async {
                (
                    StatusCode::FOUND,
                    [("location", "http://169.254.169.254/latest/meta-data")],
                )
                    .into_response()
            }),
        )
        .route("/big", get(|| async { "x".repeat(10_000) }))
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "spät"
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn policy(private: &[&str]) -> UpstreamPolicy {
    UpstreamPolicy::from_config(&AdminConfig {
        allow_insecure_upstreams: vec!["*.example.com".into(), "127.0.0.1".into()],
        allow_private_upstreams: private.iter().map(|s| (*s).to_owned()).collect(),
        ..AdminConfig::default()
    })
    .unwrap()
}

/// Client, dessen Resolver `api.example.com` auf 127.0.0.1 abbildet (simuliertes Rebinding).
fn client(guard: AddressGuard) -> OutboundClient {
    OutboundClient::builder(guard)
        .resolver(Arc::new(
            StaticResolver::new().with("api.example.com", [LOCAL]),
        ))
        .build()
        .unwrap()
}

fn get_req(url: &str) -> http::Request<Bytes> {
    http::Request::get(url).body(Bytes::new()).unwrap()
}

fn limits() -> FetchLimits {
    FetchLimits::new(Duration::from_secs(5), 64 * 1024)
}

/// F187: Variante je Zeitlimit für den Verbindungsaufbau (eigener Pool, gleicher Filter).
#[tokio::test]
async fn connect_timeout_variants() {
    let addr = server().await;
    let base = client(AddressGuard::policy(policy(&["api.example.com"])));
    assert_eq!(base.connect_timeout(), Duration::from_secs(10));
    let fast = base
        .with_connect_timeout(Duration::from_millis(300))
        .unwrap();
    assert_eq!(fast.connect_timeout(), Duration::from_millis(300));
    // Gleiche Adressregeln wie die Basis: erlaubtes Ziel klappt, verbotenes nicht.
    let url = format!("http://api.example.com:{}/ok", addr.port());
    let r = fast.fetch(get_req(&url), &limits()).await.unwrap();
    assert_eq!(&r.body[..], b"hallo");
    let err = fast
        .fetch(get_req(&format!("http://{addr}/ok")), &limits())
        .await
        .unwrap_err();
    assert!(err.is_blocked(), "{err}");
    // Zurück zum Standardwert = Basis; CA-Varianten behalten das Zeitlimit.
    let back = fast.with_connect_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(back.connect_timeout(), Duration::from_secs(10));
    let tls = fast.with_tls(fast.tls().clone()).unwrap();
    assert_eq!(tls.connect_timeout(), Duration::from_millis(300));
    // Nicht routbares Ziel: das kurze Zeitlimit greift (statt 10 s).
    let trusted = client(AddressGuard::Trusted)
        .with_connect_timeout(Duration::from_millis(300))
        .unwrap();
    let started = std::time::Instant::now();
    let err = trusted
        .fetch(get_req("http://10.255.255.1:81/x"), &limits())
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(4), "{err}");
}

#[tokio::test]
async fn rebinding_to_private_address_is_blocked() {
    let addr = server().await;
    let url = format!("http://api.example.com:{}/ok", addr.port());
    let strict = client(AddressGuard::policy(policy(&[])));
    let err = strict.fetch(get_req(&url), &limits()).await.unwrap_err();
    assert!(err.is_blocked(), "{err}");
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("api.example.com"), "{err}");
}

#[tokio::test]
async fn allowed_name_or_network_connects() {
    let addr = server().await;
    let url = format!("http://api.example.com:{}/ok", addr.port());
    for allowed in [
        &["api.example.com"][..],
        &["127.0.0.0/8"],
        &["*.example.com"],
    ] {
        let c = client(AddressGuard::policy(policy(allowed)));
        let r = c.fetch(get_req(&url), &limits()).await.unwrap();
        assert_eq!(r.status, 200, "{allowed:?}");
        assert_eq!(&r.body[..], b"hallo");
    }
    // Ohne Filter (Startkonfiguration: OIDC, OTLP).
    let r = client(AddressGuard::Trusted)
        .fetch(get_req(&url), &limits())
        .await
        .unwrap();
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn ip_literals_bypass_dns_but_not_the_guard() {
    let addr = server().await;
    let url = format!("http://{addr}/ok");
    let strict = client(AddressGuard::policy(policy(&[])));
    let err = strict.fetch(get_req(&url), &limits()).await.unwrap_err();
    assert!(err.is_blocked(), "{err}");
    // Nie erlaubt, auch nicht mit Freischaltung: unspezifiziert/Multicast.
    let open = client(AddressGuard::policy(policy(&["0.0.0.0/0"])));
    let err = open
        .fetch(get_req("http://0.0.0.0:1/"), &limits())
        .await
        .unwrap_err();
    assert!(err.is_blocked(), "{err}");
    let ok = client(AddressGuard::policy(policy(&["127.0.0.1"])));
    assert_eq!(
        ok.fetch(get_req(&url), &limits()).await.unwrap().status,
        200
    );
    // Andere Schemata werden nie gesendet.
    let err = ok
        .fetch(get_req("ftp://127.0.0.1/"), &limits())
        .await
        .unwrap_err();
    assert!(matches!(err, OutboundError::Request(_)), "{err}");
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let addr = server().await;
    let c = client(AddressGuard::Trusted);
    let url = format!("http://{addr}/redirect");
    match c.fetch(get_req(&url), &limits()).await.unwrap_err() {
        OutboundError::Redirect { status, location } => {
            assert_eq!(status, 302);
            assert_eq!(
                location.as_deref(),
                Some("http://169.254.169.254/latest/meta-data")
            );
        }
        other => panic!("{other}"),
    }
    let mut l = limits();
    l.redirects = Redirects::Return;
    let r = c.fetch(get_req(&url), &l).await.unwrap();
    assert_eq!(r.status, 302);
}

#[tokio::test]
async fn size_and_time_limits() {
    let addr = server().await;
    let c = client(AddressGuard::Trusted);
    let err = c
        .fetch(
            get_req(&format!("http://{addr}/big")),
            &FetchLimits::new(Duration::from_secs(5), 1000),
        )
        .await
        .unwrap_err();
    assert_eq!(err, OutboundError::TooLarge(1000));
    let err = c
        .fetch(
            get_req(&format!("http://{addr}/slow")),
            &FetchLimits::new(Duration::from_millis(200), 1000),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, OutboundError::Timeout(_)), "{err}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn tls_variants_share_guard_and_report_bad_ca_files() {
    let c = client(AddressGuard::policy(policy(&[])));
    assert!(c.with_tls(TlsOptions::default()).is_ok());
    let err = c
        .with_ca_file(std::path::Path::new("/nonexistent/ca.pem"))
        .unwrap_err();
    assert!(matches!(err, OutboundError::Tls(_)), "{err}");
    // Die Variante behält den Adressfilter.
    let addr = server().await;
    let v = c
        .with_tls(TlsOptions {
            roots: aigw_outbound::RootsKind::Both,
            ca_files: Vec::new(),
        })
        .unwrap();
    let err = v
        .fetch(
            get_req(&format!("http://api.example.com:{}/ok", addr.port())),
            &limits(),
        )
        .await
        .unwrap_err();
    assert!(err.is_blocked(), "{err}");
}

/// AIGW-82: Ausnahme für genau einen fest verdrahteten Host (Azure IMDS für Managed Identity) –
/// alle anderen privaten Ziele bleiben gesperrt, die Basis-Regeln unverändert.
#[tokio::test]
async fn exempt_host_allows_only_that_host() {
    let addr = server().await;
    let strict = client(AddressGuard::policy(policy(&[])));
    let exempt = strict.with_exempt_host("127.0.0.1").unwrap();
    assert!(matches!(exempt.guard(), AddressGuard::Exempt { .. }));
    // IP-Literal: freigeschaltet.
    let r = exempt
        .fetch(get_req(&format!("http://{addr}/ok")), &limits())
        .await
        .unwrap();
    assert_eq!(&r.body[..], b"hallo");
    // Name, der auf dieselbe private Adresse zeigt: weiterhin gesperrt.
    let url = format!("http://api.example.com:{}/ok", addr.port());
    let err = exempt.fetch(get_req(&url), &limits()).await.unwrap_err();
    assert!(err.is_blocked(), "{err}");
    // Andere private Adressen: gesperrt.
    let err = exempt
        .fetch(get_req("http://10.0.0.1:1/"), &limits())
        .await
        .unwrap_err();
    assert!(err.is_blocked(), "{err}");
    // Der Ausgangs-Client bleibt streng.
    let err = strict
        .fetch(get_req(&format!("http://{addr}/ok")), &limits())
        .await
        .unwrap_err();
    assert!(err.is_blocked(), "{err}");
    // Freischaltung per Name gilt für den Namen (beliebige aufgelöste Adresse).
    let by_name = strict.with_exempt_host("API.example.com").unwrap();
    let r = by_name.fetch(get_req(&url), &limits()).await.unwrap();
    assert_eq!(r.status, 200);
}
