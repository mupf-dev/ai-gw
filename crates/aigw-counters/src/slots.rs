// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Gleichzeitigkeits-Limits (`concurrency`, S-12): Zähler **ohne Zeitfenster**.
//!
//! Eine Anfrage belegt je Limit einen Platz ([`SlotLease`]) und gibt ihn frei, sobald die
//! Lease fallen gelassen wird – nach dem letzten Byte, bei Abbruch des Clients oder bei einem
//! Fehler (Drop-Semantik, nie vergessen). Mehrere Limits (Key, Team, Organisation,
//! Modellgruppe) werden wie bei [`crate::CounterBackend::reserve`] **atomar** belegt: alle
//! oder keines.
//!
//! - `local` und `postgres-sync`: [`LocalSlots`] im Prozess-RAM (exakt je Instanz; bei
//!   `postgres-sync` gilt das Limit damit **je Instanz**).
//! - `redis`: sortierte Mengen je Schlüssel (`Mitglied = Instanz:Lease`, Score = Ablaufzeit),
//!   Belegen/Freigeben per Lua-Skript; laufende Leases werden im Abstand `slot_ttl / 3`
//!   verlängert. Stürzt eine Instanz ab, verfallen ihre Plätze nach `slot_ttl`. Bei Ausfall
//!   von Redis: lokaler Rückfall wie bei den Fenster-Zählern.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

/// Ein zu belegendes Gleichzeitigkeits-Limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotCheck<'a> {
    /// Schlüssel, z. B. `key:<id>:concurrency`.
    pub key: &'a str,
    /// Höchstzahl gleichzeitig belegter Plätze.
    pub limit: u64,
}

impl<'a> SlotCheck<'a> {
    /// Neues Limit.
    pub const fn new(key: &'a str, limit: u64) -> Self {
        Self { key, limit }
    }
}

/// Ablehnung: ein Limit ist voll belegt. Es wurde nichts belegt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRejection {
    /// Index des vollen Limits in den übergebenen [`SlotCheck`]s.
    pub index: usize,
    /// Dessen Schlüssel.
    pub key: String,
    /// Dessen Limit.
    pub limit: u64,
    /// Belegte Plätze vor der (abgelehnten) Anfrage.
    pub in_flight: u64,
}

impl fmt::Display for SlotRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Gleichzeitigkeits-Limit {} für {:?} erreicht ({} belegt)",
            self.limit, self.key, self.in_flight
        )
    }
}

type Release = Box<dyn FnOnce(Vec<String>) + Send + Sync>;

/// Belegte Plätze; werden beim Fallenlassen freigegeben.
pub struct SlotLease {
    keys: Vec<String>,
    release: Option<Release>,
}

impl fmt::Debug for SlotLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotLease")
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

impl SlotLease {
    /// Leere Lease (keine Limits).
    pub fn empty() -> Self {
        Self {
            keys: Vec::new(),
            release: None,
        }
    }

    pub(crate) fn new(keys: Vec<String>, release: Release) -> Self {
        Self {
            keys,
            release: Some(release),
        }
    }

    /// Belegte Schlüssel.
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// `true` ohne belegte Plätze.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some(r) = self.release.take() {
            r(std::mem::take(&mut self.keys));
        }
    }
}

/// Doppelte Schlüssel (gleiches Limit zweimal) zusammenfassen: das engere Limit gilt.
pub(crate) fn distinct<'a>(checks: &[SlotCheck<'a>]) -> Vec<(usize, SlotCheck<'a>)> {
    let mut out: Vec<(usize, SlotCheck<'a>)> = Vec::with_capacity(checks.len());
    for (i, c) in checks.iter().enumerate() {
        match out.iter_mut().find(|(_, o)| o.key == c.key) {
            Some((j, o)) => {
                if c.limit < o.limit {
                    *o = *c;
                    *j = i;
                }
            }
            None => out.push((i, *c)),
        }
    }
    out
}

/// Gleichzeitigkeits-Zähler im Prozess-RAM (exakt je Instanz).
#[derive(Debug, Default)]
pub struct LocalSlots {
    map: Mutex<HashMap<String, u64>>,
}

impl LocalSlots {
    /// Leere Zähler.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Belegt je Limit einen Platz (alle oder keines).
    pub fn acquire(self: &Arc<Self>, checks: &[SlotCheck<'_>]) -> Result<SlotLease, SlotRejection> {
        if checks.is_empty() {
            return Ok(SlotLease::empty());
        }
        let checks = distinct(checks);
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        for (i, c) in &checks {
            let used = map.get(c.key).copied().unwrap_or(0);
            if used >= c.limit {
                return Err(SlotRejection {
                    index: *i,
                    key: c.key.to_owned(),
                    limit: c.limit,
                    in_flight: used,
                });
            }
        }
        let mut keys = Vec::with_capacity(checks.len());
        for (_, c) in &checks {
            *map.entry(c.key.to_owned()).or_insert(0) += 1;
            keys.push(c.key.to_owned());
        }
        drop(map);
        let me = Arc::clone(self);
        Ok(SlotLease::new(keys, Box::new(move |k| me.release(&k))))
    }

    /// Gibt je Schlüssel einen Platz frei.
    pub(crate) fn release(&self, keys: &[String]) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        for k in keys {
            if let Some(v) = map.get_mut(k) {
                *v = v.saturating_sub(1);
                if *v == 0 {
                    map.remove(k);
                }
            }
        }
    }

    /// Belegte Plätze eines Schlüssels.
    pub fn in_flight(&self, key: &str) -> u64 {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Anzahl Schlüssel mit belegten Plätzen.
    pub fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// `true`, wenn kein Platz belegt ist.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn atomic_and_released_on_drop() {
        let s = LocalSlots::new();
        let a = s
            .acquire(&[SlotCheck::new("k", 2), SlotCheck::new("t", 1)])
            .unwrap();
        assert_eq!((s.in_flight("k"), s.in_flight("t")), (1, 1));
        // Team voll → nichts belegt (auch nicht der Key).
        let e = s
            .acquire(&[SlotCheck::new("k", 2), SlotCheck::new("t", 1)])
            .unwrap_err();
        assert_eq!((e.index, e.limit, e.in_flight), (1, 1, 1));
        assert_eq!(s.in_flight("k"), 1);
        drop(a);
        assert!(s.is_empty());
    }

    #[test]
    fn duplicate_keys_use_tightest_limit_once() {
        let s = LocalSlots::new();
        let a = s
            .acquire(&[SlotCheck::new("k", 5), SlotCheck::new("k", 1)])
            .unwrap();
        assert_eq!(s.in_flight("k"), 1);
        let e = s
            .acquire(&[SlotCheck::new("k", 5), SlotCheck::new("k", 1)])
            .unwrap_err();
        assert_eq!((e.index, e.limit), (1, 1));
        drop(a);
        assert_eq!(s.in_flight("k"), 0);
    }

    #[test]
    fn zero_limit_rejects() {
        let s = LocalSlots::new();
        assert!(s.acquire(&[SlotCheck::new("k", 0)]).is_err());
        assert!(s.acquire(&[]).unwrap().is_empty());
    }
}
