// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Zähler-Backends für Limits und Budgets (Architektur §8, E4, AIGW-29).
//!
//! [`CounterBackend`] reserviert Kontingent in gleitenden Fenstern (Sliding Window mit zwei
//! gewichteten Buckets, siehe [`window`]) und gleicht Reservierungen später mit dem
//! tatsächlichen Verbrauch ab. Mehrere Limits (Key, Team, Organisation, Modellgruppe …) werden
//! in **einem** Aufruf atomar geprüft: Entweder werden alle gebucht oder keines.
//!
//! | Backend | Typ | Genauigkeit |
//! |---|---|---|
//! | `local` | [`LocalCounters`] | exakt je Instanz |
//! | `postgres-sync` | [`PgSyncCounters`] | lokal exakt, im Cluster ±Abgleichintervall |
//! | `redis` | [`RedisCounters`] | exakt im Cluster; bei Ausfall Rückfall auf lokal |
//!
//! Gleichzeitigkeits-Limits (`concurrency`) belegen Plätze ohne Zeitfenster
//! ([`CounterBackend::acquire_slots`], [`SlotLease`]).
//!
//! Auswahl über `counters.backend` mit [`from_config`]. Ausführlich:
//! `docs/konzept/05-zaehler.md`.

mod clock;
mod engine;
mod local;
mod pgsync;
mod redis_backend;
mod slots;
pub mod window;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use aigw_core::config::{Config, CounterBackendKind};

pub use clock::{Clock, ManualClock, SystemClock};
pub use local::LocalCounters;
pub use pgsync::{PgSyncCounters, PgSyncOptions, SyncReport};
pub use redis_backend::{RedisCounters, RedisOptions};
pub use slots::{LocalSlots, SlotCheck, SlotLease, SlotRejection};
pub use window::Window;

/// Ob eine Buchung beim Abgleich ([`CounterBackend::settle`]) korrigiert wird.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettleMode {
    /// Auf den tatsächlichen Verbrauch korrigieren (Token-Limits).
    #[default]
    Adjust,
    /// Unverändert lassen (z. B. Anfragezähler: eine Anfrage bleibt eine Anfrage).
    Keep,
}

/// Ein zu prüfendes Limit innerhalb einer Reservierung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitCheck<'a> {
    /// Zählerschlüssel, z. B. `key:<id>:tokens` (das Fenster gehört nicht hinein).
    pub key: &'a str,
    /// Fenster.
    pub window: Window,
    /// Höchstwert im gleitenden Fenster.
    pub limit: u64,
    /// Zu reservierende Menge (z. B. geschätzte Prompt-Tokens + `max_tokens`, oder 1 Anfrage).
    pub amount: u64,
    /// Verhalten beim Abgleich.
    pub settle: SettleMode,
}

impl<'a> LimitCheck<'a> {
    /// Limit mit [`SettleMode::Adjust`].
    pub const fn new(key: &'a str, window: Window, limit: u64, amount: u64) -> Self {
        Self {
            key,
            window,
            limit,
            amount,
            settle: SettleMode::Adjust,
        }
    }

    /// Wie [`LimitCheck::new`], aber ohne Korrektur beim Abgleich ([`SettleMode::Keep`]).
    pub const fn fixed(key: &'a str, window: Window, limit: u64, amount: u64) -> Self {
        Self {
            key,
            window,
            limit,
            amount,
            settle: SettleMode::Keep,
        }
    }
}

/// Kontingentstand eines Limits nach erfolgreicher Reservierung (für `x-ratelimit-*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// Limit.
    pub limit: u64,
    /// Verbleibend nach dieser Reservierung (geschätzt).
    pub remaining: u64,
    /// Zeit, bis der geschätzte Verbrauch ohne weitere Buchungen auf 0 gefallen ist.
    pub reset_after: Duration,
}

/// Wo eine Reservierung gebucht wurde (relevant für den Abgleich).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// Prozesslokale Engine.
    Local,
    /// Redis.
    Redis,
}

#[derive(Debug, Clone)]
pub(crate) struct Booked {
    pub(crate) key: Box<str>,
    pub(crate) window: Window,
    pub(crate) bucket: u64,
    pub(crate) amount: u64,
    pub(crate) settle: SettleMode,
}

/// Erfolgreiche Reservierung. Muss mit [`CounterBackend::settle`] abgeglichen werden; wird
/// sie ohne Abgleich verworfen, bleibt die reservierte Menge gebucht (konservativ).
#[must_use = "Reservierungen müssen mit settle() abgeglichen werden"]
#[derive(Debug, Clone)]
pub struct Reservation {
    pub(crate) booked: Vec<Booked>,
    pub(crate) quotas: Vec<Quota>,
    pub(crate) origin: Origin,
}

impl Reservation {
    pub(crate) fn empty(origin: Origin) -> Self {
        Self {
            booked: Vec::new(),
            quotas: Vec::new(),
            origin,
        }
    }

    /// Kontingentstände in der Reihenfolge der übergebenen [`LimitCheck`]s.
    pub fn quotas(&self) -> &[Quota] {
        &self.quotas
    }

    /// Das knappste Limit (kleinster Rest) – Kandidat für die `x-ratelimit-*`-Header.
    pub fn tightest(&self) -> Option<(usize, &Quota)> {
        self.quotas
            .iter()
            .enumerate()
            .min_by_key(|(_, q)| q.remaining)
    }

    /// `true`, wenn die Reservierung im lokalen Rückfall gebucht wurde (nur `redis`).
    pub fn is_local_fallback(&self) -> bool {
        self.origin == Origin::Local
    }

    /// Teilt die Reservierung wie [`Vec::split_off`]: `self` behält die Limits `[0, at)`, das
    /// Ergebnis enthält `[at, len)` (Indizes wie bei den übergebenen [`LimitCheck`]s).
    ///
    /// Damit lassen sich Limits, die in **einem** Aufruf atomar reserviert wurden, mit
    /// unterschiedlichem tatsächlichem Verbrauch abgleichen (z. B. Tokens gesamt gegen
    /// Eingabe-Tokens). Jeder Teil muss einzeln mit [`CounterBackend::settle`] abgeglichen
    /// werden.
    #[must_use = "Reservierungen müssen mit settle() abgeglichen werden"]
    pub fn split_off(&mut self, at: usize) -> Reservation {
        let at = at.min(self.booked.len());
        Reservation {
            booked: self.booked.split_off(at),
            quotas: self.quotas.split_off(at.min(self.quotas.len())),
            origin: self.origin,
        }
    }

    /// Anzahl der Limits in dieser Reservierung.
    pub fn len(&self) -> usize {
        self.booked.len()
    }

    /// `true`, wenn die Reservierung kein Limit enthält.
    pub fn is_empty(&self) -> bool {
        self.booked.is_empty()
    }
}

/// Ablehnung einer Reservierung. Es wurde nichts gebucht.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Index des maßgeblichen Limits in den übergebenen [`LimitCheck`]s (bei mehreren
    /// überschrittenen das mit der längsten Wartezeit).
    pub index: usize,
    /// Dessen Schlüssel.
    pub key: String,
    /// Dessen Fenster.
    pub window: Window,
    /// Dessen Limit.
    pub limit: u64,
    /// Geschätzter Verbrauch im Fenster vor der (abgelehnten) Reservierung.
    pub used: u64,
    /// Wartezeit, bis die Reservierung bei unverändertem Verbrauch passen würde
    /// (`Retry-After`). `None`: Menge größer als das Limit – passt nie.
    pub retry_after: Option<Duration>,
}

impl Rejection {
    /// Restkontingent (für `x-ratelimit-remaining-*`).
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Limit {} je {:?} für {:?} erreicht (verbraucht {})",
            self.limit, self.window, self.key, self.used
        )
    }
}

/// Zähler zum Nachtragen nach Ausfällen (Metriken `aigw_counter_replayed_total`,
/// `aigw_counter_replay_dropped_total`, `aigw_counter_replay_pending`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CounterStats {
    /// Nach einem Ausfall nachgetragene Deltas (`redis`, F28).
    pub replayed: u64,
    /// Verworfene Deltas (Bucket bereits abgelaufen oder Rückstau voll).
    pub dropped: u64,
    /// Noch nicht nachgetragene bzw. abgeglichene Deltas (Rückstau).
    pub pending: u64,
}

/// Betriebszustand eines Backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Normalbetrieb.
    Ok,
    /// Eingeschränkt (z. B. Redis/Postgres nicht erreichbar, lokaler Rückfall aktiv).
    Degraded(String),
}

/// Abstraktion über Zählerstände (E4).
///
/// Alle Methoden sind unfehlbar: Fällt ein externes Backend aus, zählt es lokal weiter
/// (siehe [`Health`]). Abgelehnt wird nur wegen erreichter Limits.
#[async_trait::async_trait]
pub trait CounterBackend: Send + Sync + fmt::Debug {
    /// Prüft alle `checks` und bucht sie, wenn **jedes** Limit die Menge noch zulässt
    /// (alle oder keines). Ein leeres `checks` ist immer erfolgreich.
    async fn reserve(&self, checks: &[LimitCheck<'_>]) -> Result<Reservation, Rejection>;

    /// Gleicht eine Reservierung mit dem tatsächlichen Verbrauch ab: Für jede Buchung mit
    /// [`SettleMode::Adjust`] wird `actual − amount` (±) im ursprünglichen Bucket verrechnet.
    /// Buchungen in Buckets, die nicht mehr zum Fenster zählen, bleiben unverändert.
    async fn settle(&self, reservation: Reservation, actual: u64);

    /// Geschätzter Verbrauch im gleitenden Fenster.
    async fn get(&self, key: &str, window: Window) -> u64;

    /// Belegt je Gleichzeitigkeits-Limit (`concurrency`, ohne Fenster) einen Platz – alle
    /// oder keines. Die Plätze bleiben belegt, bis die [`SlotLease`] fallen gelassen wird.
    async fn acquire_slots(&self, checks: &[SlotCheck<'_>]) -> Result<SlotLease, SlotRejection>;

    /// Belegte Plätze eines Gleichzeitigkeits-Schlüssels.
    async fn in_flight(&self, key: &str) -> u64;

    /// Art des Backends.
    fn kind(&self) -> CounterBackendKind;

    /// Betriebszustand.
    fn health(&self) -> Health {
        Health::Ok
    }

    /// Nachtragen nach Ausfällen (Standard: nichts).
    fn stats(&self) -> CounterStats {
        CounterStats::default()
    }
}

/// Fehler beim Aufbau eines Backends.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CounterError {
    /// `counters.backend: redis` ohne `counters.redis_url`.
    #[error("counters.backend = redis erfordert counters.redis_url")]
    MissingRedisUrl,
    /// `postgres-sync` ohne Postgres-Datenbank.
    #[error(
        "counters.backend = postgres-sync erfordert eine Postgres-Datenbank (store.url), nicht {0:?}"
    )]
    PostgresRequired(String),
    /// Ungültige Redis-URL o. Ä.
    #[error("Redis: {0}")]
    Redis(String),
    /// Redis im Cluster-Modus (F27).
    #[error(
        "counters.redis_url: Redis läuft im Cluster-Modus (cluster_enabled:1) – nicht unterstützt; \
         eine Einzelinstanz oder den Sentinel-Primary angeben"
    )]
    RedisCluster,
    /// Datenbankfehler.
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Laufzeit-Einstellungen der Backends. [`from_config`] übernimmt Abgleichintervall,
/// Redis-Poolgröße und -Zeitlimit aus `counters.*` (F26); der Rest (Uhr, Präfixe …) ist für
/// Tests und Benchmarks.
#[derive(Debug, Clone, Default)]
pub struct CounterOptions {
    /// Einstellungen für `postgres-sync`.
    pub pg: PgSyncOptions,
    /// Einstellungen für `redis`.
    pub redis: RedisOptions,
    /// Intervall der Speicherbereinigung abgelaufener Fenster (Standard 30 s).
    pub janitor_interval: Option<Duration>,
}

pub(crate) const DEFAULT_JANITOR: Duration = Duration::from_secs(30);

/// Baut das in `counters.backend` gewählte Backend. Muss innerhalb einer Tokio-Laufzeit
/// aufgerufen werden (Hintergrund-Tasks für Bereinigung, Abgleich und Wiederverbindung).
///
/// `postgres-sync` verwendet die Datenbank aus `store.url` (muss Postgres sein; die Tabelle
/// `counter_state` legt die Store-Migration an).
pub async fn from_config(
    cfg: &Config,
    opts: CounterOptions,
) -> Result<Arc<dyn CounterBackend>, CounterError> {
    let janitor = opts.janitor_interval.unwrap_or(DEFAULT_JANITOR);
    let mut opts = opts;
    let c = &cfg.counters;
    opts.pg.interval = Duration::from_millis(c.sync_interval_ms.max(1));
    opts.redis.pool_size = c.redis_pool_size.max(1);
    opts.redis.timeout = Duration::from_millis(c.redis_timeout_ms.max(1));
    Ok(match cfg.counters.backend {
        CounterBackendKind::Local => {
            let c = Arc::new(LocalCounters::new());
            c.spawn_janitor(janitor);
            c
        }
        CounterBackendKind::PostgresSync => {
            let url = &cfg.store.url;
            if !(url.starts_with("postgres:") || url.starts_with("postgresql:")) {
                return Err(CounterError::PostgresRequired(
                    url.split(':').next().unwrap_or_default().to_owned(),
                ));
            }
            PgSyncCounters::connect(url, opts.pg).await?
        }
        CounterBackendKind::Redis => {
            let url = cfg
                .counters
                .redis_url
                .as_deref()
                .ok_or(CounterError::MissingRedisUrl)?;
            RedisCounters::connect(url, opts.redis).await?
        }
    })
}
