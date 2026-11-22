// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Gemeinsame Redis-Verbindung (exakte zweite Ebene und Vektor-Backend `redis`).
//!
//! `ConnectionManager` (multiplext, verbindet selbst neu) wird bei Bedarf aufgebaut; schlägt
//! das fehl, wird höchstens alle 5 s ein neuer Versuch unternommen – dazwischen gilt Redis als
//! nicht verfügbar (Cache-Miss, keine Wartezeit im Hot Path).

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use redis::aio::{ConnectionManager, ConnectionManagerConfig};

const RETRY: Duration = Duration::from_secs(5);

/// Redis-Verbindung mit verzögertem Aufbau.
pub struct RedisConn {
    client: redis::Client,
    timeout: Duration,
    conn: Mutex<Option<ConnectionManager>>,
    last_attempt: Mutex<Option<Instant>>,
    connecting: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for RedisConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisConn")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl RedisConn {
    /// Neue Verbindung (noch nicht aufgebaut). `timeout` gilt für Aufbau und Antworten.
    pub fn new(url: &str, timeout: Duration) -> Result<Self, String> {
        let client = redis::Client::open(url).map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            timeout,
            conn: Mutex::new(None),
            last_attempt: Mutex::new(None),
            connecting: tokio::sync::Mutex::new(()),
        })
    }

    /// Zeitlimit.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Verbindung (aufbauen, falls nötig und kein Versuch in den letzten 5 s).
    pub async fn get(&self) -> Result<ConnectionManager, String> {
        if let Some(c) = self
            .conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Ok(c);
        }
        let _g = self.connecting.lock().await;
        if let Some(c) = self
            .conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        {
            return Ok(c);
        }
        {
            let mut last = self
                .last_attempt
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if last.is_some_and(|t| t.elapsed() < RETRY) {
                return Err("Redis nicht verfügbar (nächster Versuch in Kürze)".into());
            }
            *last = Some(Instant::now());
        }
        let cfg = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(self.timeout))
            .set_response_timeout(Some(self.timeout))
            .set_number_of_retries(1);
        let fut = ConnectionManager::new_with_config(self.client.clone(), cfg);
        let c = tokio::time::timeout(self.timeout.max(Duration::from_millis(500)), fut)
            .await
            .map_err(|_| "Zeitüberschreitung beim Verbinden".to_owned())?
            .map_err(|e| e.to_string())?;
        *self.conn.lock().unwrap_or_else(PoisonError::into_inner) = Some(c.clone());
        Ok(c)
    }

    /// Führt `cmd` mit Zeitlimit aus.
    pub async fn query<T: redis::FromRedisValue>(&self, cmd: &redis::Cmd) -> Result<T, String> {
        let mut c = self.get().await?;
        tokio::time::timeout(self.timeout, cmd.query_async::<T>(&mut c))
            .await
            .map_err(|_| "Zeitüberschreitung".to_owned())?
            .map_err(|e| e.to_string())
    }

    /// Führt eine Pipeline mit Zeitlimit aus.
    pub async fn pipe<T: redis::FromRedisValue>(&self, p: &redis::Pipeline) -> Result<T, String> {
        let mut c = self.get().await?;
        tokio::time::timeout(self.timeout, p.query_async::<T>(&mut c))
            .await
            .map_err(|_| "Zeitüberschreitung".to_owned())?
            .map_err(|e| e.to_string())
    }
}
