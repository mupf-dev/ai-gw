// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Listener der Daten- und Steuerebene.
//!
//! Standard: zwei Listener (`server.data.listen`, `server.control.listen`). Sind beide
//! Adressen gleich, läuft ein Listener mit beiden Routern. Beide Server fahren auf dasselbe
//! Signal hin geordnet herunter (laufende Anfragen und Streams werden abgeschlossen).

use std::future::{Future, IntoFuture as _};
use std::io;
use std::net::SocketAddr;

use aigw_core::config::ServerConfig;
use aigw_proxy::ProxyState;
use axum::Router;
use axum::routing::get;
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

/// Router der Steuerebene: Admin-API, Admin-UI und optional `/healthz`.
fn control_router(with_healthz: bool) -> Router {
    let r = aigw_admin::router();
    if with_healthz {
        r.route("/healthz", get(aigw_proxy::healthz))
    } else {
        r
    }
}

/// Betreibt die Listener, bis `shutdown` fertig ist; wartet dann auf laufende Anfragen.
pub(crate) async fn run(
    listeners: Listeners,
    state: ProxyState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let data_router = aigw_proxy::router(state);
    match listeners.control {
        None => {
            // `/healthz` liefert bereits der Daten-Router.
            let app = data_router.merge(control_router(false));
            axum::serve(listeners.data, app)
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
            let data = axum::serve(listeners.data, data_router)
                .with_graceful_shutdown(wait(rx.clone()))
                .into_future();
            let ctrl = axum::serve(control, control_router(true))
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
        let task = tokio::spawn(run(listeners, ProxyState::mock(), async move {
            let _ = rx.await;
        }));

        assert_eq!(get_status(data, "/healthz").await, StatusCode::OK);
        assert_eq!(get_status(data, "/v1/models").await, StatusCode::OK);
        assert_eq!(
            get_status(data, "/admin/api/v1/version").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get_status(control, "/healthz").await, StatusCode::OK);
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
        let task = tokio::spawn(run(listeners, ProxyState::mock(), async move {
            let _ = rx.await;
        }));
        assert_eq!(get_status(data, "/healthz").await, StatusCode::OK);
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
}
