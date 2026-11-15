// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Vorberechnete Plugin-Ketten je (Org, Team, Key, Modellgruppe) aus dem Snapshot.
//!
//! **Aufbau** ([`PluginIndex::build`], einmal je `config_version`):
//! 1. Jede Bindung wird einmal gebaut: Kettensteuerung abtrennen, Plugin-Konfiguration
//!    validieren, Instanz erzeugen. Ungültige Bindungen werden geloggt und übersprungen –
//!    mit `failMode: closed` (bzw. [`IndexOptions::invalid_binding`]) sperren sie stattdessen
//!    die betroffenen Ketten (`500 plugin_config_invalid`).
//! 2. Für jede vorkommende Kombination „Org/Team/Key mit eigenen Bindungen“ (Präfix) und jede
//!    Modellgruppe mit Bindungen wird die Kette einmal zusammengesetzt: Ebenen in der Folge
//!    global → Organisation → Team → Key → Modellgruppe, `inherit: override|disable` wirkt auf
//!    Bindungen desselben Plugins der äußeren Ebenen (außer `locked`), danach Sortierung nach
//!    `order`, Ebene, Name.
//!
//! **Lookup** ([`PluginIndex::chain`]): Key-ID → Präfix, Modellgruppe → Spalte, zwei
//! Hash-Zugriffe, ein `Arc`-Klon. Ohne Bindungen ist es ein Vergleich.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use aigw_core::resource::{Id, PluginBinding, ScopeLevel};
use aigw_core::snapshot::{KeyCtx, Snapshot, SnapshotHandle};
use arc_swap::ArcSwap;
use serde_json::{Map, Value};

use crate::binding::{self, FailMode, Inherit};
use crate::builtin::Broken;
use crate::chain::{Chain, Entry};
use crate::plugin::Plugin;
use crate::registry::{BuildCtx, Registry};

/// Optionen des Index-Aufbaus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOptions {
    /// Umgang mit ungültigen Bindungen ohne eigenes `failMode`: `open` = überspringen
    /// (Standard), `closed` = betroffene Ketten sperren.
    pub invalid_binding: FailMode,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            invalid_binding: FailMode::Open,
        }
    }
}

fn level_rank(level: ScopeLevel) -> u8 {
    match level {
        ScopeLevel::Global => 0,
        ScopeLevel::Organization => 1,
        ScopeLevel::Team => 2,
        ScopeLevel::ApiKey => 3,
        ScopeLevel::ModelGroup => 4,
    }
}

/// Eine gebaute Bindung.
struct Built {
    name: String,
    plugin: String,
    level: ScopeLevel,
    order: i32,
    inherit: Inherit,
    locked: bool,
    /// `None` bei `inherit: disable`.
    entry: Option<Arc<Entry>>,
}

/// Wiederverwendbare Plugin-Instanzen (Bindungsname → Konfiguration + Instanz), damit ein
/// Reload unveränderte Bindungen nicht neu baut (Regex, Verbindungspools …).
///
/// Der dritte Wert ist der Fingerabdruck der Fabrik ([`crate::PluginFactory::fingerprint`],
/// z. B. SHA-256 eines WASM-Moduls): ändert er sich, wird trotz gleicher Konfiguration neu
/// gebaut.
type Reuse = HashMap<String, (String, Map<String, Value>, String, Arc<dyn Plugin>)>;

/// Präfix-Signatur: Org/Team/Key-Namen, soweit sie eigene Bindungen haben.
type Sig = (Option<String>, Option<String>, Option<String>);

/// Vorberechnete Ketten eines Snapshots.
pub struct PluginIndex {
    version: i64,
    bindings: usize,
    /// Key-ID → Präfix (nur Keys mit Präfix ≠ 0).
    key_prefix: HashMap<Id, usize>,
    /// Modellgruppe (kanonischer Name) → Spalte (≥ 1; 0 = ohne Modellgruppen-Bindungen).
    group_slot: HashMap<String, usize>,
    /// `[Präfix][Spalte]`.
    chains: Vec<Box<[Arc<Chain>]>>,
    reuse: Reuse,
}

impl fmt::Debug for PluginIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginIndex")
            .field("version", &self.version)
            .field("bindings", &self.bindings)
            .field("prefixes", &self.chains.len())
            .field("model_groups", &self.group_slot.len())
            .finish()
    }
}

impl Default for PluginIndex {
    fn default() -> Self {
        Self::with_global(Chain::empty())
    }
}

impl PluginIndex {
    /// Index, der jeder Anfrage dieselbe Kette gibt (Tests, Betrieb ohne Snapshot).
    pub fn with_global(chain: Arc<Chain>) -> Self {
        Self {
            version: 0,
            bindings: chain.entries().len(),
            key_prefix: HashMap::new(),
            group_slot: HashMap::new(),
            chains: vec![vec![chain].into_boxed_slice()],
            reuse: Reuse::new(),
        }
    }

    /// `config_version` des Snapshots.
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Anzahl wirksamer (gebauter) Bindungen.
    pub fn bindings(&self) -> usize {
        self.bindings
    }

    /// Wirksame Kette für Key und Modellgruppe (kanonischer Name).
    #[inline]
    pub fn chain(&self, key: Option<&KeyCtx>, model_group: Option<&str>) -> Arc<Chain> {
        let prefix = match key {
            Some(k) if !self.key_prefix.is_empty() => {
                self.key_prefix.get(&k.key.id).copied().unwrap_or(0)
            }
            _ => 0,
        };
        let slot = match model_group {
            Some(g) if !self.group_slot.is_empty() => self.group_slot.get(g).copied().unwrap_or(0),
            _ => 0,
        };
        let row = &self.chains[prefix.min(self.chains.len() - 1)];
        Arc::clone(&row[slot.min(row.len() - 1)])
    }

    /// Baut den Index aus `snap` (ohne Wiederverwendung).
    pub fn build(snap: &Snapshot, registry: &Registry, opts: IndexOptions) -> Self {
        Self::build_reusing(snap, registry, opts, &Reuse::new())
    }

    fn build_reusing(
        snap: &Snapshot,
        registry: &Registry,
        opts: IndexOptions,
        prev: &Reuse,
    ) -> Self {
        let mut reuse = Reuse::new();
        let mut global = Vec::new();
        let mut by: [HashMap<String, Vec<Built>>; 4] = Default::default();
        let mut count = 0usize;
        for b in &snap.plugin_bindings {
            if !b.spec.enabled {
                continue;
            }
            let Some(built) = build_binding(snap, b, registry, opts, prev, &mut reuse) else {
                continue;
            };
            count += usize::from(built.entry.is_some());
            match (b.spec.scope.level, &b.spec.scope.target) {
                (ScopeLevel::Global, _) => global.push(built),
                (level, Some(t)) => by[usize::from(level_rank(level)) - 1]
                    .entry(t.clone())
                    .or_default()
                    .push(built),
                (level, None) => tracing::error!(
                    binding = %b.metadata.name,
                    level = level.as_str(),
                    "Plugin-Bindung ohne Ziel – übersprungen"
                ),
            }
        }
        let sort =
            |v: &mut Vec<Built>| v.sort_by(|a, b| (a.order, &a.name).cmp(&(b.order, &b.name)));
        sort(&mut global);
        for m in &mut by {
            m.values_mut().for_each(sort);
        }
        let [orgs, teams, keys, groups] = &by;

        // Präfixe aus den Keys des Snapshots.
        let mut sigs: HashMap<Sig, usize> = HashMap::new();
        let mut prefixes: Vec<Sig> = vec![(None, None, None)];
        sigs.insert((None, None, None), 0);
        let mut key_prefix = HashMap::new();
        if !(orgs.is_empty() && teams.is_empty() && keys.is_empty()) {
            for k in snap.keys.values() {
                let pick = |name: Option<&str>, m: &HashMap<String, Vec<Built>>| {
                    name.filter(|n| m.contains_key(*n)).map(str::to_owned)
                };
                let sig = (
                    pick(
                        k.organization.as_ref().map(|o| o.metadata.name.as_str()),
                        orgs,
                    ),
                    pick(k.team.as_ref().map(|t| t.metadata.name.as_str()), teams),
                    pick(Some(k.key.metadata.name.as_str()), keys),
                );
                if sig == (None, None, None) {
                    continue;
                }
                let next = prefixes.len();
                let p = *sigs.entry(sig.clone()).or_insert_with(|| {
                    prefixes.push(sig);
                    next
                });
                key_prefix.insert(k.key.id, p);
            }
        }

        let mut group_names: Vec<&String> = groups.keys().collect();
        group_names.sort();
        let group_slot: HashMap<String, usize> = group_names
            .iter()
            .enumerate()
            .map(|(i, g)| ((*g).clone(), i + 1))
            .collect();

        let empty = Vec::new();
        let chains = prefixes
            .iter()
            .map(|(o, t, k)| {
                let base = [Some(&global), get(o, orgs), get(t, teams), get(k, keys)];
                std::iter::once(compose(&base, &empty))
                    .chain(
                        group_names
                            .iter()
                            .map(|g| compose(&base, groups.get(*g).unwrap_or(&empty))),
                    )
                    .collect::<Box<[_]>>()
            })
            .collect();
        Self {
            version: snap.version,
            bindings: count,
            key_prefix,
            group_slot,
            chains,
            reuse,
        }
    }
}

fn build_binding(
    snap: &Snapshot,
    b: &PluginBinding,
    registry: &Registry,
    opts: IndexOptions,
    prev: &Reuse,
    reuse: &mut Reuse,
) -> Option<Built> {
    let name = &b.metadata.name;
    let plugin = &b.spec.plugin;
    let split = binding::split_spec(&b.spec);
    let factory = registry.get(plugin);
    let invalid = |reason: String, fail_mode: Option<FailMode>| -> Option<Built> {
        let mode = fail_mode.unwrap_or(opts.invalid_binding);
        tracing::error!(
            binding = %name,
            plugin = %plugin,
            error = %reason,
            fail_mode = mode.as_str(),
            "Plugin-Bindung ungültig – {}",
            match mode {
                FailMode::Open => "übersprungen",
                FailMode::Closed => "betroffene Ketten gesperrt",
            }
        );
        (mode == FailMode::Closed).then(|| Built {
            name: name.clone(),
            plugin: plugin.clone(),
            level: b.spec.scope.level,
            order: b.spec.order,
            inherit: Inherit::Append,
            locked: true,
            entry: Some(Arc::new(Entry {
                level: b.spec.scope.level,
                order: b.spec.order,
                ..Entry::new(name.as_str(), Broken::plugin(name))
                    .with_metrics(Arc::clone(registry.metrics()))
            })),
        })
    };
    let control = match split.control {
        Ok(c) => c,
        Err(e) => return invalid(e, split.fail_mode),
    };
    let Some(factory) = factory else {
        return invalid(format!("unbekanntes Plugin {plugin:?}"), control.fail_mode);
    };
    let mut built = Built {
        name: name.clone(),
        plugin: plugin.clone(),
        level: b.spec.scope.level,
        order: b.spec.order,
        inherit: control.inherit,
        locked: control.locked,
        entry: None,
    };
    if control.inherit == Inherit::Disable {
        return Some(built);
    }
    let timeout = control
        .timeout
        .unwrap_or_else(|| factory.default_timeout())
        .min(Duration::from_secs(60));
    // Die Ebene gehört zum Fingerabdruck: sie entscheidet über private Ziele (S-22).
    let fingerprint = format!(
        "{}|{}",
        b.spec.scope.level.as_str(),
        factory.fingerprint(snap, &split.plugin_config)
    );
    let instance = match prev.get(name) {
        Some((p, c, f, inst)) if p == plugin && *c == b.spec.config && *f == fingerprint => {
            Arc::clone(inst)
        }
        _ => match factory.build_in(
            &BuildCtx {
                snapshot: snap,
                timeout,
                level: b.spec.scope.level,
            },
            &split.plugin_config,
        ) {
            Ok(inst) => inst,
            Err(e) => return invalid(format!("Konfiguration: {e}"), control.fail_mode),
        },
    };
    reuse.insert(
        name.clone(),
        (
            plugin.clone(),
            b.spec.config.clone(),
            fingerprint,
            Arc::clone(&instance),
        ),
    );
    built.entry = Some(Arc::new(Entry {
        plugin: instance,
        binding: Arc::from(name.as_str()),
        level: b.spec.scope.level,
        order: b.spec.order,
        fail_mode: control
            .fail_mode
            .unwrap_or_else(|| factory.default_fail_mode()),
        timeout,
        output_reject: control.output_reject,
        metrics: Arc::clone(registry.metrics()),
    }));
    Some(built)
}

fn get<'m>(name: &Option<String>, m: &'m HashMap<String, Vec<Built>>) -> Option<&'m Vec<Built>> {
    name.as_ref().and_then(|n| m.get(n))
}

/// Setzt eine Kette aus den Ebenen `base` (global, Org, Team, Key) und `group` zusammen.
fn compose(base: &[Option<&Vec<Built>>; 4], group: &[Built]) -> Arc<Chain> {
    let mut acc: Vec<&Built> = Vec::new();
    let levels = base.iter().map(|l| l.map_or(&[][..], |v| v.as_slice()));
    for level in levels.chain(std::iter::once(group)) {
        for b in level {
            match b.inherit {
                Inherit::Append => {
                    if b.entry.is_some() {
                        acc.push(b);
                    }
                }
                Inherit::Override | Inherit::Disable => {
                    let outer = |a: &&Built| {
                        a.plugin == b.plugin && level_rank(a.level) < level_rank(b.level)
                    };
                    if acc.iter().any(|a| outer(a) && a.locked) {
                        tracing::debug!(
                            binding = %b.name,
                            plugin = %b.plugin,
                            "inherit: {:?} wirkungslos – geerbte Bindung ist gesperrt (locked)",
                            b.inherit
                        );
                        continue;
                    }
                    acc.retain(|a| !outer(a));
                    if b.inherit == Inherit::Override {
                        acc.push(b);
                    }
                }
            }
        }
    }
    if acc.is_empty() {
        return Chain::empty();
    }
    acc.sort_by(|a, b| {
        (a.order, level_rank(a.level), &a.name).cmp(&(b.order, level_rank(b.level), &b.name))
    });
    Arc::new(Chain::new(
        acc.into_iter().filter_map(|b| b.entry.clone()).collect(),
    ))
}

/// Aktueller [`PluginIndex`] mit Hot Reload (lock-freies Lesen, `ArcSwap`).
pub struct PluginChains {
    current: ArcSwap<PluginIndex>,
    registry: Registry,
    opts: IndexOptions,
    /// Serialisiert Neuaufbauten (Hintergrund-Task und Anfragen mit neuerem Snapshot).
    rebuild: Mutex<()>,
    /// Feste Kette ([`PluginChains::fixed`]): Snapshots ändern sie nicht.
    pinned: bool,
}

impl fmt::Debug for PluginChains {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginChains")
            .field("current", &*self.current.load())
            .field("registry", &self.registry)
            .field("opts", &self.opts)
            .finish()
    }
}

impl Default for PluginChains {
    fn default() -> Self {
        Self::new(Registry::with_builtin(), IndexOptions::default())
    }
}

impl PluginChains {
    /// Ohne Bindungen; neue Indizes mit `registry` und `opts`.
    pub fn new(registry: Registry, opts: IndexOptions) -> Self {
        Self {
            current: ArcSwap::from_pointee(PluginIndex::default()),
            registry,
            opts,
            rebuild: Mutex::new(()),
            pinned: false,
        }
    }

    /// Feste Kette für alle Anfragen (Tests, Betrieb ohne Snapshot). Sie gilt unabhängig vom
    /// Snapshot der Anfrage; [`PluginChains::rebuild`] und [`PluginChains::spawn`] lassen sie
    /// unverändert.
    pub fn fixed(chain: Chain) -> Self {
        let me = Self {
            pinned: true,
            ..Self::default()
        };
        me.current
            .store(Arc::new(PluginIndex::with_global(Arc::new(chain))));
        me
    }

    /// Registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Fehlermodus-Zähler (`aigw_plugin_failmode_triggered_total`, F148) für `/metrics`.
    pub fn metrics(&self) -> Arc<crate::PluginMetrics> {
        Arc::clone(self.registry.metrics())
    }

    /// Aktueller Index.
    pub fn index(&self) -> Arc<PluginIndex> {
        self.current.load_full()
    }

    /// Wirksame Kette einer Anfrage.
    ///
    /// `snap` ist der Snapshot der Anfrage (aus der Schlüsselprüfung). Ist er neuer als der
    /// Index, wird der Index sofort aus ihm gebaut (einmal je Version), damit Kette und
    /// Key/Team/Modellgruppe nie aus verschiedenen Ständen stammen.
    pub fn chain_for(
        &self,
        snap: Option<&Snapshot>,
        key: Option<&KeyCtx>,
        model_group: Option<&str>,
    ) -> Arc<Chain> {
        let idx = self.current.load();
        if let Some(s) = snap
            && s.version > idx.version
            && !self.pinned
        {
            drop(idx);
            return self.rebuild(s).chain(key, model_group);
        }
        idx.chain(key, model_group)
    }

    /// Baut den Index aus `snap` neu (falls `snap` neuer ist) und gibt den aktuellen zurück.
    pub fn rebuild(&self, snap: &Snapshot) -> Arc<PluginIndex> {
        let _guard = self.rebuild.lock().unwrap_or_else(PoisonError::into_inner);
        let cur = self.current.load_full();
        if self.pinned || (snap.version <= cur.version && cur.version != 0) {
            return cur;
        }
        let idx = Arc::new(PluginIndex::build_reusing(
            snap,
            &self.registry,
            self.opts,
            &cur.reuse,
        ));
        tracing::info!(
            config_version = idx.version,
            bindings = idx.bindings,
            prefixes = idx.chains.len(),
            model_groups = idx.group_slot.len(),
            "Plugin-Ketten aufgebaut"
        );
        self.current.store(Arc::clone(&idx));
        idx
    }

    /// Baut den Index aus dem aktuellen Snapshot und startet eine Task, die ihn bei jeder
    /// neuen `config_version` neu baut. Die Task endet mit dem zurückgegebenen
    /// [`ChainsTask`].
    pub fn spawn(self: &Arc<Self>, handle: SnapshotHandle) -> ChainsTask {
        let mut rx = handle.subscribe();
        self.rebuild(&handle.load());
        let me = Arc::clone(self);
        ChainsTask(tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                me.rebuild(&handle.load());
            }
        }))
    }
}

/// Hintergrund-Task von [`PluginChains::spawn`]; wird beim Fallenlassen beendet.
#[derive(Debug)]
pub struct ChainsTask(tokio::task::JoinHandle<()>);

impl Drop for ChainsTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}
