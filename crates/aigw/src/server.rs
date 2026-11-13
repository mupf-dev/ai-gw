// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Listener der Daten- und Steuerebene.
//!
//! Standard: zwei Listener (`server.data.listen`, `server.control.listen`). Sind beide
//! Adressen gleich, läuft ein Listener mit beiden Routern. Beide Server fahren auf dasselbe
//! Signal hin geordnet herunter (laufende Anfragen und Streams werden abgeschlossen).
//!
//! Proben auf beiden Listenern: `/healthz` (Prozess lebt) und `/readyz` (F15, AIGW-75):
//! `200 {"status":"ready"}` im Normalfall, `200 {"status":"degraded"}`, wenn der
//! Konfigurations-Snapshot veraltet ist (länger als `snapshot.stale_after_secs` nicht mit der
//! Datenbank abgleichbar) oder aus dem Platten-Cache stammt. Der Gateway bedient Anfragen in
//! beiden Fällen weiter (letzter bekannter Stand); `degraded` ist ein Signal für Monitoring und
//! UI, kein Grund, die Instanz aus dem Load Balancer zu nehmen.

use std::future::{Future, IntoFuture as _};
use std::io;
use std::net::SocketAddr;

use aigw_core::config::ServerConfig;
use aigw_core::snapshot::SnapshotHandle;
use axum::extract::State;
use axum::routing::get;
use axum::serve::ListenerExt as _;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Gebundene Listener.
#[derive(Debug)]
pub(crate) struct Listeners {
    /// Datenebene (bei gemeinsamer Adresse auch Steuerebene).
    pub(crate) data: TcpListener,
    /// Steuerebene, `None` bei gemeinsamer Adresse.
    pub(crate) control: Option<TcpListener>,
}

impl Listeners {
    /// Bindet die konfigurierten Adressen.
    pub(crate) async fn bind(cfg: &ServerConfig) -> io::Result<Self> {
        let data = TcpListener::bind(cfg.data.listen).await?;
        let control = if cfg.is_combined() {
            None
        } else {
            Some(TcpListener::bind(cfg.control.listen).await?)
        };
        Ok(Self { data, control })
    }

    /// Tatsächliche Adressen (Daten, Steuerung).
    pub(crate) fn local_addrs(&self) -> io::Result<(SocketAddr, SocketAddr)> {
        let data = self.data.local_addr()?;
        let control = match &self.control {
            Some(c) => c.local_addr()?,
            None => data,
        };
        Ok((data, control))
    }
}

/// Router der Steuerebene (`control` = Admin-API, -UI, `/metrics` …) plus optional die Proben
/// `/healthz` und `/readyz`.
fn control_router(control: Router, probes: Option<&SnapshotHandle>) -> Router {
    match probes {
        Some(snapshot) => control
            .route("/healthz", get(aigw_proxy::healthz))
            .merge(readyz_router(snapshot.clone())),
        None => control,
    }
}

/// `/readyz` (F15).
pub(crate) fn readyz_router(snapshot: SnapshotHandle) -> Router {
    Router::new()
        .route("/readyz", get(readyz))
        .with_state(snapshot)
}

async fn readyz(State(snapshot): State<SnapshotHandle>) -> Json<serde_json::Value> {
    let st = snapshot.status();
    Json(serde_json::json!({
        "status": if st.stale { "degraded" } else { "ready" },
        "snapshot": {
            "version": st.version,
            "ageSeconds": st.age.as_secs(),
            "source": st.source.as_str(),
            "stale": st.stale,
            "failing": st.failing,
        },
    }))
}

/// Betreibt die Listener, bis `shutdown` fertig ist; wartet dann auf laufende Anfragen.
///
/// `data_router` ist der Router der Datenebene (`aigw_proxy::router(…)`, ggf. mit `/mcp`),
/// `control` der vollständige Router der Steuerebene (Admin-API/-UI aus
/// `aigw_admin::router_with(…)`, zusammengeführt mit `/metrics`). `snapshot` speist `/readyz`.
pub(crate) async fn run(
    listeners: Listeners,
    data_router: Router,
    control_plane: Router,
    snapshot: SnapshotHandle,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let data_router = data_router.merge(readyz_router(snapshot.clone()));
    // TCP_NODELAY: SSE-Chunks sofort senden statt auf ACKs zu warten (Nagle, Q4).
    let data_listener = listeners.data.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    match listeners.control {
        None => {
            // `/healthz` und `/readyz` liefert bereits der Daten-Router.
            let app = data_router.merge(control_router(control_plane, None));
            axum::serve(data_listener, app)
                .with_graceful_shutdown(shutdown)
                .await
        }
        Some(control) => {
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                shutdown.await;
                let _ = tx.send(true);
            });
            let wait = |mut rx: watch::Receiver<bool>| async move {
                let _ = rx.wait_for(|stop| *stop).await;
            };
            let data = axum::serve(data_listener, data_router)
                .with_graceful_shutdown(wait(rx.clone()))
                .into_future();
            let ctrl = axum::serve(control, control_router(control_plane, Some(&snapshot)))
                .with_graceful_shutdown(wait(rx))
                .into_future();
            tokio::try_join!(data, ctrl).map(|_| ())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::config::{ControlListenerConfig, DataListenerConfig};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::StatusCode;
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    async fn get_status(addr: SocketAddr, path: &str) -> StatusCode {
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let res = client
            .get(format!("http://{addr}{path}").parse().unwrap())
            .await
            .unwrap();
        let status = res.status();
        let _ = res.into_body().collect().await;
        status
    }

    fn cfg(data: &str, control: &str) -> ServerConfig {
        ServerConfig {
            data: DataListenerConfig {
                listen: data.parse().unwrap(),
                ..DataListenerConfig::default()
            },
            control: ControlListenerConfig {
                listen: control.parse().unwrap(),
            },
        }
    }

    #[tokio::test]
    async fn separate_listeners_split_routes_and_shut_down() {
        // Port 0 auf zwei Loopback-Adressen → zwei getrennte Listener.
        let listeners = Listeners::bind(&cfg("127.0.0.1:0", "127.0.0.2:0"))
            .await
            .unwrap();
        assert!(listeners.control.is_some());
        let (data, control) = listeners.local_addrs().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(run(
            listeners,
            aigw_proxy::router(aigw_proxy::ProxyState::mock()),
            aigw_admin::router(),
            SnapshotHandle::default(),
            async move {
                let _ = rx.await;
            },
        ));

        assert_eq!(get_status(data, "/healthz").await, StatusCode::OK);
        assert_eq!(get_status(data, "/v1/models").await, StatusCode::OK);
        assert_eq!(
            get_status(data, "/admin/api/v1/version").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get_status(control, "/healthz").await, StatusCode::OK);
        assert_eq!(get_status(data, "/readyz").await, StatusCode::OK);
        assert_eq!(get_status(control, "/readyz").await, StatusCode::OK);
        assert_eq!(
            get_status(control, "/admin/api/v1/version").await,
            StatusCode::OK
        );
        assert_eq!(
            get_status(control, "/v1/models").await,
            StatusCode::NOT_FOUND
        );

        tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("Shutdown hängt")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn same_address_serves_both_routers() {
        let listeners = Listeners::bind(&cfg("127.0.0.1:0", "127.0.0.1:0"))
            .await
            .unwrap();
        assert!(listeners.control.is_none());
        let (data, control) = listeners.local_addrs().unwrap();
        assert_eq!(data, control);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(run(
            listeners,
            aigw_proxy::router(aigw_proxy::ProxyState::mock()),
            aigw_admin::router(),
            SnapshotHandle::default(),
            async move {
                let _ = rx.await;
            },
        ));
        assert_eq!(get_status(data, "/healthz").await, StatusCode::OK);
        assert_eq!(get_status(data, "/readyz").await, StatusCode::OK);
        assert_eq!(get_status(data, "/v1/models").await, StatusCode::OK);
        assert_eq!(
            get_status(data, "/admin/api/v1/version").await,
            StatusCode::OK
        );
        tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .expect("Shutdown hängt")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn readyz_reports_degraded_for_cache_and_stale() {
        async fn body(h: &SnapshotHandle) -> serde_json::Value {
            let Json(v) = readyz(State(h.clone())).await;
            v
        }
        let h = SnapshotHandle::default();
        let v = body(&h).await;
        assert_eq!(v["status"], "ready");
        assert_eq!(v["snapshot"]["source"], "database");
        assert_eq!(v["snapshot"]["stale"], false);

        h.set_stale_after(std::time::Duration::ZERO);
        h.mark_failing();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let v = body(&h).await;
        assert_eq!(v["status"], "degraded", "{v}");
        assert_eq!(v["snapshot"]["failing"], true);

        let c = SnapshotHandle::from_cache(
            aigw_core::snapshot::Snapshot {
                version: 9,
                ..Default::default()
            },
            std::time::SystemTime::now(),
        );
        let v = body(&c).await;
        assert_eq!(v["status"], "degraded");
        assert_eq!(v["snapshot"]["source"], "cache");
        assert_eq!(v["snapshot"]["version"], 9);
    }
}
