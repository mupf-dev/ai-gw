// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Prozesslokales Zähler-Backend (Stub ohne Zeitfenster).

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{CounterBackend, CounterKey};

/// Zähler im Prozess-RAM. Lesen und Erhöhen bestehender Zähler kommen mit einem Read-Lock aus.
#[derive(Debug, Default)]
pub struct LocalCounters {
    map: RwLock<HashMap<String, AtomicU64>>,
}

impl LocalCounters {
    /// Erzeugt ein leeres Backend.
    pub fn new() -> Self {
        Self::default()
    }
}

fn poisoned() -> aigw_core::Error {
    aigw_core::Error::Internal("Zähler-Lock vergiftet".to_owned())
}

impl CounterBackend for LocalCounters {
    fn incr(&self, key: &CounterKey, delta: u64) -> aigw_core::Result<u64> {
        {
            let map = self.map.read().map_err(|_| poisoned())?;
            if let Some(c) = map.get(key) {
                return Ok(c.fetch_add(delta, Ordering::Relaxed).saturating_add(delta));
            }
        }
        let mut map = self.map.write().map_err(|_| poisoned())?;
        let c = map.entry(key.to_owned()).or_default();
        Ok(c.fetch_add(delta, Ordering::Relaxed).saturating_add(delta))
    }

    fn get(&self, key: &CounterKey) -> aigw_core::Result<u64> {
        let map = self.map.read().map_err(|_| poisoned())?;
        Ok(map.get(key).map_or(0, |c| c.load(Ordering::Relaxed)))
    }

    fn reset(&self, key: &CounterKey) -> aigw_core::Result<()> {
        let map = self.map.read().map_err(|_| poisoned())?;
        if let Some(c) = map.get(key) {
            c.store(0, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incr_get_reset() {
        let c = LocalCounters::new();
        assert_eq!(c.get("k").unwrap(), 0);
        assert_eq!(c.incr("k", 5).unwrap(), 5);
        assert_eq!(c.incr("k", 2).unwrap(), 7);
        assert_eq!(c.get("k").unwrap(), 7);
        c.reset("k").unwrap();
        assert_eq!(c.get("k").unwrap(), 0);
    }
}
