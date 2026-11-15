// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Client-Sessions des Gateways (`Mcp-Session-Id`).
//!
//! Eine Session entsteht mit `initialize`, ist an den API-Key gebunden (eine fremde Session-ID
//! nützt ohne denselben Key nichts) und an den Endpunkt (`/mcp` bzw. `/mcp/{server}`). Sie hält
//! laufende Anfragen (für `notifications/cancelled`), die Zuordnung Ressourcen-URI → Server
//! (aggregierter Endpunkt) und einen Kanal für Benachrichtigungen an `GET`-Ströme. Sessions
//! liegen im Speicher der Instanz (Load-Balancer: Sticky Sessions, siehe Doku).
//!
//! Grenzen (S-28): `max_sessions` je Instanz, zusätzlich `max_sessions_per_key` und
//! `max_sessions_per_team`, damit ein einzelner Key bzw. ein Team die Instanz nicht für alle
//! anderen sperrt. Der Benachrichtigungskanal ist begrenzt (`notification_buffer`); langsame
//! `GET`-Ströme verlieren die ältesten Meldungen (mit Log), die Session bleibt bestehen.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aigw_core::resource::Id;
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::jsonrpc::{INTERNAL_ERROR, RequestId, RpcError};

/// Geltungsbereich einer Session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `/mcp`: alle freigegebenen Server, Namen mit Namensraum.
    Aggregate,
    /// `/mcp/{server}`: ein Server, Namen wie beim Upstream.
    Server(String),
}

/// Eine Client-Session.
pub struct Session {
    /// Session-ID (zufällig, 256 Bit, hex).
    pub id: String,
    /// API-Key, an den die Session gebunden ist.
    pub key: Option<Id>,
    /// Team des Keys (Grenze `max_sessions_per_team`).
    pub team: Option<Id>,
    /// Endpunkt.
    pub scope: Scope,
    /// Ausgehandelte Protokollversion.
    pub protocol_version: &'static str,
    last_seen: Mutex<Instant>,
    inflight: Mutex<HashMap<RequestId, oneshot::Sender<()>>>,
    resource_routes: Mutex<HashMap<String, String>>,
    notify: broadcast::Sender<Value>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("scope", &self.scope)
            .field("protocol_version", &self.protocol_version)
            .finish_non_exhaustive()
    }
}

/// Laufende Anfrage; entfernt sich beim Fallenlassen.
#[derive(Debug)]
pub struct Inflight {
    session: Arc<Session>,
    id: RequestId,
    /// Wird ausgelöst, wenn der Client die Anfrage abbricht.
    pub cancelled: oneshot::Receiver<()>,
}

impl Drop for Inflight {
    fn drop(&mut self) {
        if let Ok(mut m) = self.session.inflight.lock() {
            m.remove(&self.id);
        }
    }
}

impl Session {
    fn touch(&self) {
        if let Ok(mut t) = self.last_seen.lock() {
            *t = Instant::now();
        }
    }

    fn idle_for(&self) -> Duration {
        self.last_seen
            .lock()
            .map(|t| t.elapsed())
            .unwrap_or_default()
    }

    /// Registriert eine laufende Anfrage (für `notifications/cancelled`).
    pub fn begin(self: &Arc<Self>, id: RequestId) -> Inflight {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut m) = self.inflight.lock() {
            m.insert(id.clone(), tx);
        }
        Inflight {
            session: Arc::clone(self),
            id,
            cancelled: rx,
        }
    }

    /// Bricht eine laufende Anfrage ab (`false`, wenn sie nicht (mehr) läuft).
    pub fn cancel(&self, id: &RequestId) -> bool {
        let tx = self.inflight.lock().ok().and_then(|mut m| m.remove(id));
        tx.is_some_and(|tx| tx.send(()).is_ok())
    }

    /// Merkt sich, welcher Server eine Ressource geliefert hat.
    pub fn set_resource_routes(&self, routes: impl IntoIterator<Item = (String, String)>) {
        if let Ok(mut m) = self.resource_routes.lock() {
            m.extend(routes);
            // Begrenzen (sehr große Listen): ältere Einträge nicht weiter aufheben.
            if m.len() > 100_000 {
                m.clear();
            }
        }
    }

    /// Server einer zuvor gelisteten Ressource.
    pub fn resource_route(&self, uri: &str) -> Option<String> {
        self.resource_routes.lock().ok()?.get(uri).cloned()
    }

    /// Benachrichtigungen für `GET`-Ströme abonnieren.
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.notify.subscribe()
    }

    /// Benachrichtigung an alle `GET`-Ströme dieser Session.
    pub fn notify(&self, msg: Value) {
        let _ = self.notify.send(msg);
    }
}

/// Alle Sessions dieser Instanz.
#[derive(Debug)]
pub struct Sessions {
    map: Mutex<HashMap<String, Arc<Session>>>,
    max: usize,
    per_key: usize,
    per_team: usize,
    buffer: usize,
    idle: Duration,
}

/// Grenzen für [`Sessions`] (S-28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// Höchstzahl je Instanz.
    pub max: usize,
    /// Höchstzahl je API-Key.
    pub per_key: usize,
    /// Höchstzahl je Team.
    pub per_team: usize,
    /// Puffer des Benachrichtigungskanals je Session (Nachrichten).
    pub notification_buffer: usize,
}

impl SessionLimits {
    /// Nur eine Gesamtgrenze (ohne Grenzen je Key/Team), Puffer 64.
    pub fn total(max: usize) -> Self {
        Self {
            max,
            per_key: usize::MAX,
            per_team: usize::MAX,
            notification_buffer: 64,
        }
    }
}

fn random_id() -> Result<String, RpcError> {
    let mut b = [0u8; 32];
    aws_lc_rs::rand::fill(&mut b)
        .map_err(|_| RpcError::new(INTERNAL_ERROR, "Zufallsquelle nicht verfügbar"))?;
    Ok(b.iter().fold(String::with_capacity(64), |mut s, x| {
        use std::fmt::Write as _;
        let _ = write!(s, "{x:02x}");
        s
    }))
}

impl Sessions {
    /// Leere Verwaltung, nur mit Gesamtgrenze.
    pub fn new(max: usize, idle: Duration) -> Self {
        Self::with_limits(SessionLimits::total(max), idle)
    }

    /// Leere Verwaltung mit Grenzen je Instanz, Key und Team.
    pub fn with_limits(limits: SessionLimits, idle: Duration) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max: limits.max.max(1),
            per_key: limits.per_key.max(1),
            per_team: limits.per_team.max(1),
            buffer: limits.notification_buffer.clamp(1, 65_536),
            idle,
        }
    }

    /// Legt eine Session an. Fehler bei Erreichen von `max_sessions` (`503`) bzw. der Grenze
    /// je Key/Team (`429`, Code `too_many_sessions`); abgelaufene Sessions werden vorher
    /// entfernt.
    pub fn create(
        &self,
        key: Option<Id>,
        team: Option<Id>,
        scope: Scope,
        protocol_version: &'static str,
    ) -> Result<Arc<Session>, RpcError> {
        let mut map = self
            .map
            .lock()
            .map_err(|_| RpcError::new(INTERNAL_ERROR, "Sessions gesperrt"))?;
        let count = |map: &HashMap<String, Arc<Session>>| {
            let (mut k, mut t) = (0usize, 0usize);
            for s in map.values() {
                k += usize::from(key.is_some() && s.key == key);
                t += usize::from(team.is_some() && s.team == team);
            }
            (k, t)
        };
        let over = |map: &HashMap<String, Arc<Session>>| {
            let (k, t) = count(map);
            map.len() >= self.max || k >= self.per_key || t >= self.per_team
        };
        if over(&map) {
            // Zuerst abgelaufene entfernen.
            let idle = self.idle;
            map.retain(|_, s| s.idle_for() < idle);
            if map.len() >= self.max {
                return Err(RpcError::gateway(
                    crate::jsonrpc::RATE_LIMITED,
                    503,
                    "too_many_sessions",
                    "Zu viele MCP-Sessions auf dieser Instanz",
                ));
            }
            let (k, t) = count(&map);
            if k >= self.per_key {
                return Err(RpcError::gateway(
                    crate::jsonrpc::RATE_LIMITED,
                    429,
                    "too_many_sessions",
                    format!(
                        "Zu viele MCP-Sessions für diesen API-Key (höchstens {}); nicht mehr \
                         benötigte Sessions mit DELETE beenden",
                        self.per_key
                    ),
                ));
            }
            if t >= self.per_team {
                return Err(RpcError::gateway(
                    crate::jsonrpc::RATE_LIMITED,
                    429,
                    "too_many_sessions",
                    format!(
                        "Zu viele MCP-Sessions für dieses Team (höchstens {})",
                        self.per_team
                    ),
                ));
            }
        }
        let (notify, _) = broadcast::channel(self.buffer);
        let s = Arc::new(Session {
            id: random_id()?,
            key,
            team,
            scope,
            protocol_version,
            last_seen: Mutex::new(Instant::now()),
            inflight: Mutex::new(HashMap::new()),
            resource_routes: Mutex::new(HashMap::new()),
            notify,
        });
        map.insert(s.id.clone(), Arc::clone(&s));
        Ok(s)
    }

    /// Session zu einer ID (markiert sie als benutzt).
    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        let s = self.map.lock().ok()?.get(id).cloned()?;
        if s.idle_for() >= self.idle {
            self.remove(id);
            return None;
        }
        s.touch();
        Some(s)
    }

    /// Beendet eine Session.
    pub fn remove(&self, id: &str) -> bool {
        self.map.lock().is_ok_and(|mut m| m.remove(id).is_some())
    }

    /// Entfernt abgelaufene Sessions; liefert deren Anzahl.
    pub fn evict_idle(&self) -> usize {
        let Ok(mut m) = self.map.lock() else {
            return 0;
        };
        let before = m.len();
        let idle = self.idle;
        m.retain(|_, s| s.idle_for() < idle);
        before - m.len()
    }

    /// Benachrichtigung an alle Sessions.
    pub fn broadcast(&self, msg: &Value) {
        let all: Vec<Arc<Session>> = self
            .map
            .lock()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        for s in all {
            s.notify(msg.clone());
        }
    }

    /// Anzahl Sessions.
    pub fn len(&self) -> usize {
        self.map.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Keine Sessions?
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_cancel() {
        let s = Sessions::new(2, Duration::from_secs(60));
        let a = s
            .create(None, None, Scope::Aggregate, "2025-06-18")
            .unwrap();
        assert_eq!(a.id.len(), 64);
        let b = s
            .create(None, None, Scope::Server("x".into()), "2025-06-18")
            .unwrap();
        assert_ne!(a.id, b.id);
        assert!(
            s.create(None, None, Scope::Aggregate, "2025-06-18")
                .is_err()
        );
        assert!(s.get(&a.id).is_some());
        let mut f = a.begin(RequestId::Num(7));
        assert!(a.cancel(&RequestId::Num(7)));
        assert!(f.cancelled.try_recv().is_ok());
        assert!(!a.cancel(&RequestId::Num(7)));
        assert!(s.remove(&b.id));
        assert!(s.get(&b.id).is_none());
        let short = Sessions::new(10, Duration::ZERO);
        let c = short
            .create(None, None, Scope::Aggregate, "2025-06-18")
            .unwrap();
        assert!(short.get(&c.id).is_none());
    }

    /// S-28: Grenzen je Key und Team; andere Keys bleiben unberührt; Beenden gibt frei.
    #[test]
    fn per_key_and_team_limits() {
        let s = Sessions::with_limits(
            SessionLimits {
                max: 100,
                per_key: 2,
                per_team: 3,
                notification_buffer: 4,
            },
            Duration::from_secs(60),
        );
        let (k1, k2, k3, t) = (
            aigw_core::resource::new_id(),
            aigw_core::resource::new_id(),
            aigw_core::resource::new_id(),
            aigw_core::resource::new_id(),
        );
        let v = "2025-06-18";
        let a = s.create(Some(k1), Some(t), Scope::Aggregate, v).unwrap();
        s.create(Some(k1), Some(t), Scope::Aggregate, v).unwrap();
        let e = s
            .create(Some(k1), Some(t), Scope::Aggregate, v)
            .unwrap_err();
        assert_eq!(e.status(), Some(429));
        assert!(e.message.contains("API-Key"));
        s.create(Some(k2), Some(t), Scope::Aggregate, v).unwrap();
        let e = s
            .create(Some(k2), Some(t), Scope::Aggregate, v)
            .unwrap_err();
        assert!(e.message.contains("Team"));
        // Anderes Team, anderer Key: frei.
        s.create(Some(k3), None, Scope::Aggregate, v).unwrap();
        assert!(s.remove(&a.id));
        s.create(Some(k1), Some(t), Scope::Aggregate, v).unwrap();
        assert_eq!(s.len(), 4);
    }

    /// S-28: Der Benachrichtigungskanal ist begrenzt – ein langsamer Empfänger verliert die
    /// ältesten Meldungen, der Speicher wächst nicht.
    #[test]
    fn notification_channel_is_bounded() {
        let s = Sessions::with_limits(
            SessionLimits {
                notification_buffer: 2,
                ..SessionLimits::total(10)
            },
            Duration::from_secs(60),
        );
        let a = s
            .create(None, None, Scope::Aggregate, "2025-06-18")
            .unwrap();
        let mut rx = a.subscribe();
        for i in 0..10 {
            a.notify(serde_json::json!(i));
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(8))
        ));
        assert_eq!(rx.try_recv().unwrap(), serde_json::json!(8));
    }
}
