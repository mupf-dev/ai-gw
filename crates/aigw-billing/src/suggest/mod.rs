// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preisvorschläge (AIGW-85): ermittelt je Deployment einen Preis aus
//!
//! 1. der **Azure Retail Prices API** ([`azure`]) für Azure-OpenAI- und Foundry-Deployments
//!    (Region und Bereitstellungsart aus `settings.pricing` bzw. Konfiguration),
//! 2. dem **Preiskatalog** ([`catalog`]) für direkte Anbieter (und als Ersatz, wenn Azure
//!    nichts liefert),
//!
//! und vergleicht ihn mit dem aktuell gültigen `Price` (gleiche Suche wie die Abrechnung:
//! Anbieter + `providerModel`, spezifisch vor generisch). Vorschläge ändern nichts – die
//! Admin-API legt erst auf ausdrücklichen Wunsch neue Preisversionen an. **Manuelle Preise
//! haben Vorrang:** ein Preis ohne Label [`LABEL_SOURCE`] gilt als manuell und wird nur mit
//! ausdrücklicher Bestätigung ersetzt.
//!
//! Netzwerkzugriff nur beim Abruf von Vorschlägen bzw. beim periodischen Abgleich
//! (`check_interval_secs`), über [`aigw_outbound`] mit fest erlaubtem Host.

pub mod azure;
pub mod catalog;
pub mod decimal;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use aigw_core::config::PriceSuggestionsConfig;
use aigw_core::resource::{DeploymentSpec, Object, PriceSpec, TokenPrices};
use aigw_core::types::ProviderKind;
use aigw_outbound::{FetchLimits, OutboundClient};
use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;

pub use azure::{AzureRetail, AzureDeploymentType};
pub use catalog::{Catalog, CatalogMatch};

/// Label einer übernommenen Preisversion: Quelle (`azure-retail` | `catalog`). Preise ohne
/// dieses Label gelten als manuell gepflegt.
pub const LABEL_SOURCE: &str = "aigw.io/price-source";
/// Label: Azure-Region der übernommenen Preisversion.
pub const LABEL_REGION: &str = "aigw.io/price-region";
/// Label: Azure-Bereitstellungsart der übernommenen Preisversion.
pub const LABEL_DEPLOYMENT_TYPE: &str = "aigw.io/price-deployment-type";
/// Label: Deployment, für das der Vorschlag übernommen wurde.
pub const LABEL_DEPLOYMENT: &str = "aigw.io/price-deployment";

/// Größte Katalogdatei (URL).
const MAX_CATALOG_BYTES: usize = 32 * 1024 * 1024;
/// Nach einem Fehler beim Katalogabruf frühestens so spät erneut versuchen.
const CATALOG_BACKOFF: Duration = Duration::from_secs(300);

/// Quelle eines Vorschlags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PriceSuggestionSource {
    /// Azure Retail Prices API.
    AzureRetail,
    /// Preiskatalog (mitgeliefert oder per URL).
    Catalog,
}

impl PriceSuggestionSource {
    /// Bezeichner (Label-Wert).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AzureRetail => "azure-retail",
            Self::Catalog => "catalog",
        }
    }
}

/// Vergleich Vorschlag ↔ aktueller Preis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PriceSuggestionStatus {
    /// Kein Preis hinterlegt (Kosten = 0), Vorschlag vorhanden.
    Missing,
    /// Aktueller Preis entspricht dem Vorschlag.
    Unchanged,
    /// Aktueller Preis weicht ab (bei manuellem Preis: nur mit Bestätigung ersetzen).
    Differs,
    /// Mehrere Deployments mit demselben Preisschlüssel (Anbieter + `providerModel`) haben
    /// unterschiedliche Vorschläge (z. B. andere Region) – nur eines übernehmen.
    Conflict,
    /// Keine Quelle kennt das Modell.
    NoSuggestion,
}

/// Aktuell gültige Preisversion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CurrentPriceVersion {
    /// Name der Preisversion.
    pub name: String,
    /// Manuell gepflegt (ohne Label `aigw.io/price-source`) – hat Vorrang.
    pub manual: bool,
    /// Preis.
    pub spec: PriceSpec,
}

/// Vorschlag für ein Deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceSuggestion {
    /// Deployment.
    pub deployment: String,
    /// Modellgruppe.
    pub model_group: String,
    /// Anbieter.
    pub provider: ProviderKind,
    /// Modell beim Anbieter (Preisschlüssel).
    pub provider_model: String,
    /// Deployment aktiv.
    pub enabled: bool,
    /// Quelle des Vorschlags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PriceSuggestionSource>,
    /// Azure-Region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Azure-Bereitstellungsart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_type: Option<AzureDeploymentType>,
    /// Gefundenes Modell (Katalogname bzw. Modellteil der Meter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    /// Verwendete Azure-Meter (`skuName`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub meters: Vec<String>,
    /// Vorgeschlagene Preisversion (`validFrom` = Gültigkeit laut Quelle, sonst jetzt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested: Option<PriceSpec>,
    /// Aktuell gültige Preisversion (gleiche Suche wie die Abrechnung).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<CurrentPriceVersion>,
    /// Vergleich.
    pub status: PriceSuggestionStatus,
    /// Abweichende Felder (z. B. `perMillionTokens.input`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub differences: Vec<String>,
    /// Hinweise (Ersatzquelle, Mehrdeutigkeit, fehlende Region …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Zustand einer Quelle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceSourceStatus {
    /// Quelle.
    pub source: PriceSuggestionSource,
    /// Azure-Region bzw. Herkunft des Katalogs (`builtin` oder URL).
    pub scope: String,
    /// Letzter erfolgreicher Abruf.
    #[serde(with = "time::serde::rfc3339::option", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub fetched_at: Option<OffsetDateTime>,
    /// Anzahl Meter bzw. Katalogmodelle.
    pub entries: usize,
    /// Stand des Katalogs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Fehler des letzten Abrufs (ältere Daten bleiben in Gebrauch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Ergebnis des letzten periodischen Abgleichs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceCheckSummary {
    /// Zeitpunkt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub at: OffsetDateTime,
    /// Deployments mit abweichendem Preis.
    pub differing: Vec<String>,
    /// Deployments ohne Preis, für die es einen Vorschlag gibt.
    pub missing: Vec<String>,
}

/// Alle Vorschläge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PriceSuggestionReport {
    /// Erzeugt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub generated_at: OffsetDateTime,
    /// Je Deployment.
    pub items: Vec<PriceSuggestion>,
    /// Zustand der Quellen.
    pub sources: Vec<PriceSourceStatus>,
    /// Letzter periodischer Abgleich (falls eingeschaltet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check: Option<PriceCheckSummary>,
}

/// Zuordnung aus `settings.pricing`.
#[derive(Debug, Default)]
struct PricingSettings<'a> {
    source: Option<&'a str>,
    region: Option<&'a str>,
    deployment_type: Option<&'a str>,
    model: Option<&'a str>,
    meter: Option<&'a str>,
}

impl<'a> PricingSettings<'a> {
    fn of(spec: &'a DeploymentSpec) -> Self {
        let Some(Value::Object(o)) = spec.settings.get("pricing") else {
            return Self::default();
        };
        let s = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        Self {
            source: s("source"),
            region: s("region"),
            deployment_type: s("deploymentType"),
            model: s("model"),
            meter: s("meter"),
        }
    }
}

#[derive(Debug)]
struct CatalogState {
    catalog: Arc<Catalog>,
    origin: String,
    fetched_at: Option<OffsetDateTime>,
    loaded: Option<Instant>,
    attempted: Option<Instant>,
    error: Option<String>,
}

#[derive(Debug)]
struct RemoteCatalog {
    client: OutboundClient,
    url: String,
    litellm: bool,
    refresh: Duration,
    lock: tokio::sync::Mutex<()>,
}

/// Erzeugt Preisvorschläge (eine Instanz je Prozess; Zwischenspeicher für Azure und Katalog).
#[derive(Debug)]
pub struct Suggester {
    azure: Option<AzureRetail>,
    default_region: Option<String>,
    default_type: AzureDeploymentType,
    catalog: RwLock<CatalogState>,
    remote: Option<RemoteCatalog>,
    last_check: Mutex<Option<PriceCheckSummary>>,
}

impl Suggester {
    /// Aus `billing.price_suggestions`. `client` sind die SSRF-Regeln für ausgehende
    /// Verbindungen; die Hosts von `azure.api_url` und `catalog.url` stammen aus der
    /// Startkonfiguration und werden ausgenommen ([`OutboundClient::with_exempt_host`]).
    pub fn from_config(cfg: &PriceSuggestionsConfig, client: &OutboundClient) -> Result<Self, String> {
        let default_type = AzureDeploymentType::parse(&cfg.azure.default_deployment_type).ok_or_else(|| {
            format!(
                "billing.price_suggestions.azure.default_deployment_type {:?}: global, data-zone oder regional",
                cfg.azure.default_deployment_type
            )
        })?;
        if let Some(r) = &cfg.azure.default_region
            && !azure::valid_region(r)
        {
            return Err(format!(
                "billing.price_suggestions.azure.default_region {r:?}: Azure-Region klein ohne Leerzeichen"
            ));
        }
        let azure = if cfg.azure.enabled {
            let host = url::Url::parse(&cfg.azure.api_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .ok_or("billing.price_suggestions.azure.api_url: URL mit Host erwartet")?;
            let c = client
                .with_exempt_host(&host)
                .map_err(|e| format!("billing.price_suggestions.azure: {e}"))?;
            Some(
                AzureRetail::new(
                    c,
                    &cfg.azure.api_url,
                    &cfg.azure.service_name,
                    Duration::from_secs(cfg.azure.cache_ttl_secs.max(60)),
                    Duration::from_secs(cfg.azure.timeout_secs.clamp(1, 300)),
                )
                .map_err(|e| format!("billing.price_suggestions.azure.{e}"))?,
            )
        } else {
            None
        };
        let litellm = match cfg.catalog.format.as_str() {
            "aigw" => false,
            "litellm" => true,
            f => {
                return Err(format!(
                    "billing.price_suggestions.catalog.format {f:?}: aigw oder litellm"
                ));
            }
        };
        let remote = match cfg.catalog.url.as_deref().filter(|u| !u.trim().is_empty()) {
            None => None,
            Some(u) => {
                let parsed = url::Url::parse(u)
                    .map_err(|e| format!("billing.price_suggestions.catalog.url: {e}"))?;
                if !matches!(parsed.scheme(), "https" | "http") {
                    return Err("billing.price_suggestions.catalog.url: http(s) erwartet".into());
                }
                let host = parsed
                    .host_str()
                    .ok_or("billing.price_suggestions.catalog.url: Host fehlt")?;
                Some(RemoteCatalog {
                    client: client
                        .with_exempt_host(host)
                        .map_err(|e| format!("billing.price_suggestions.catalog: {e}"))?,
                    url: u.to_owned(),
                    litellm,
                    refresh: Duration::from_secs(cfg.catalog.refresh_secs.max(60)),
                    lock: tokio::sync::Mutex::new(()),
                })
            }
        };
        let builtin = Catalog::builtin().map_err(|e| format!("mitgelieferter Preiskatalog: {e}"))?;
        Ok(Self {
            azure,
            default_region: cfg.azure.default_region.clone(),
            default_type,
            catalog: RwLock::new(CatalogState {
                catalog: Arc::new(builtin),
                origin: "builtin".into(),
                fetched_at: None,
                loaded: None,
                attempted: None,
                error: None,
            }),
            remote,
            last_check: Mutex::new(None),
        })
    }

    /// Aktueller Katalog; lädt ihn bei Bedarf von der URL nach (Fehler: bisheriger Katalog).
    async fn catalog(&self, refresh: bool) -> Arc<Catalog> {
        if let Some(r) = &self.remote {
            let due = {
                let st = self.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                let stale = st.loaded.is_none_or(|t| t.elapsed() >= r.refresh);
                let backoff = st.attempted.is_some_and(|t| t.elapsed() < CATALOG_BACKOFF)
                    && st.error.is_some();
                (stale || refresh) && !(backoff && !refresh)
            };
            if due {
                let _g = r.lock.lock().await;
                let res = fetch_catalog(r).await;
                let mut st = self.catalog.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                st.attempted = Some(Instant::now());
                match res {
                    Ok(c) => {
                        st.catalog = Arc::new(c);
                        st.origin.clone_from(&r.url);
                        st.fetched_at = Some(OffsetDateTime::now_utc());
                        st.loaded = Some(Instant::now());
                        st.error = None;
                    }
                    Err(e) => {
                        tracing::warn!(url = %r.url, error = %e, "Preiskatalog: Abruf fehlgeschlagen");
                        st.error = Some(e);
                    }
                }
            }
        }
        let st = self.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(&st.catalog)
    }

    fn catalog_status(&self) -> PriceSourceStatus {
        let st = self.catalog.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        PriceSourceStatus {
            source: PriceSuggestionSource::Catalog,
            scope: st.origin.clone(),
            fetched_at: st.fetched_at,
            entries: st.catalog.len(),
            version: Some(st.catalog.version.clone()),
            error: st.error.clone(),
        }
    }

    /// Letzter periodischer Abgleich.
    pub fn last_check(&self) -> Option<PriceCheckSummary> {
        self.last_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Vorschläge für `deployments` im Vergleich zu `prices` zum Zeitpunkt `now`.
    /// `refresh` erzwingt neue Abrufe (Azure, Katalog-URL).
    pub async fn report(
        &self,
        deployments: &[Object<DeploymentSpec>],
        prices: &[Object<PriceSpec>],
        now: OffsetDateTime,
        refresh: bool,
    ) -> PriceSuggestionReport {
        let catalog = self.catalog(refresh).await;
        // Azure-Regionen einmal je Region abrufen.
        let plans: Vec<Plan<'_>> = deployments.iter().map(|d| self.plan(d)).collect();
        let mut regions: BTreeMap<String, azure::RegionPrices> = BTreeMap::new();
        if let Some(az) = &self.azure {
            let wanted: BTreeSet<&str> = plans
                .iter()
                .filter_map(|p| p.azure.as_ref().and_then(|a| a.region.as_deref()))
                .collect();
            for r in wanted {
                regions.insert(r.to_owned(), az.region(r, refresh).await);
            }
        }
        let mut items: Vec<PriceSuggestion> = deployments
            .iter()
            .zip(&plans)
            .map(|(d, p)| self.suggest_one(d, p, &catalog, &regions, prices, now))
            .collect();
        mark_conflicts(&mut items);
        let mut sources: Vec<PriceSourceStatus> = regions
            .iter()
            .map(|(r, rp)| PriceSourceStatus {
                source: PriceSuggestionSource::AzureRetail,
                scope: r.clone(),
                fetched_at: rp.fetched_at,
                entries: rp.meters.len(),
                version: None,
                error: rp.error.clone(),
            })
            .collect();
        sources.push(self.catalog_status());
        PriceSuggestionReport {
            generated_at: now,
            items,
            sources,
            last_check: self.last_check(),
        }
    }

    /// Periodischer Abgleich: Warnung im Log bei neu abweichenden bzw. fehlenden Preisen.
    pub async fn check(
        &self,
        deployments: &[Object<DeploymentSpec>],
        prices: &[Object<PriceSpec>],
    ) -> PriceCheckSummary {
        let now = OffsetDateTime::now_utc();
        let report = self.report(deployments, prices, now, false).await;
        let pick = |s: PriceSuggestionStatus| -> Vec<String> {
            report
                .items
                .iter()
                .filter(|i| i.status == s)
                .map(|i| i.deployment.clone())
                .collect()
        };
        let summary = PriceCheckSummary {
            at: now,
            differing: pick(PriceSuggestionStatus::Differs),
            missing: pick(PriceSuggestionStatus::Missing),
        };
        let previous = self.last_check();
        let before: BTreeSet<&String> = previous
            .iter()
            .flat_map(|p| p.differing.iter().chain(&p.missing))
            .collect();
        for i in report.items.iter().filter(|i| {
            matches!(i.status, PriceSuggestionStatus::Differs | PriceSuggestionStatus::Missing)
                && !before.contains(&i.deployment)
        }) {
            tracing::warn!(
                deployment = %i.deployment,
                model = %i.provider_model,
                status = ?i.status,
                source = i.source.map(PriceSuggestionSource::as_str),
                differences = %i.differences.join(","),
                manual = i.current.as_ref().is_some_and(|c| c.manual),
                "Preisabgleich: Preis weicht vom Vorschlag ab bzw. fehlt"
            );
        }
        *self.last_check.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(summary.clone());
        summary
    }

    /// Welche Quellen für ein Deployment in Frage kommen.
    fn plan<'a>(&self, d: &'a Object<DeploymentSpec>) -> Plan<'a> {
        let spec = &d.spec;
        let ps = PricingSettings::of(spec);
        let mut notes = Vec::new();
        let source = ps.source.unwrap_or("auto");
        let endpoint = spec.endpoint.as_deref().unwrap_or_default();
        let azure_like = spec.provider == ProviderKind::AzureOpenAi || azure::is_azure_host(endpoint);
        let use_azure = match source {
            "azure" => true,
            "auto" => azure_like,
            _ => false,
        };
        let azure = if use_azure && spec.provider != ProviderKind::Mock {
            if self.azure.is_none() {
                notes.push("Azure Retail Prices ist ausgeschaltet (billing.price_suggestions.azure.enabled)".into());
                None
            } else {
                let region = ps
                    .region
                    .map(str::to_owned)
                    .or_else(|| azure::region_from_endpoint(endpoint))
                    .or_else(|| self.default_region.clone());
                if region.is_none() {
                    notes.push(
                        "Azure-Region unbekannt – settings.pricing.region setzen oder \
                         billing.price_suggestions.azure.default_region"
                            .into(),
                    );
                }
                let dtype = ps
                    .deployment_type
                    .and_then(AzureDeploymentType::parse)
                    .unwrap_or(self.default_type);
                let model = ps.meter.or(ps.model).unwrap_or(&spec.provider_model);
                Some(AzurePlan {
                    region,
                    dtype,
                    tokens: azure::model_tokens(model),
                    model: model.to_owned(),
                })
            }
        } else {
            None
        };
        Plan {
            source,
            azure,
            catalog_model: ps.model.unwrap_or(&spec.provider_model),
            notes,
        }
    }

    fn suggest_one(
        &self,
        d: &Object<DeploymentSpec>,
        plan: &Plan<'_>,
        catalog: &Catalog,
        regions: &BTreeMap<String, azure::RegionPrices>,
        prices: &[Object<PriceSpec>],
        now: OffsetDateTime,
    ) -> PriceSuggestion {
        let spec = &d.spec;
        let mut out = PriceSuggestion {
            deployment: d.metadata.name.clone(),
            model_group: spec.model_group.clone(),
            provider: spec.provider,
            provider_model: spec.provider_model.clone(),
            enabled: spec.enabled,
            source: None,
            region: None,
            deployment_type: None,
            matched: None,
            meters: Vec::new(),
            suggested: None,
            current: current_price(prices, spec.provider, &spec.provider_model, now),
            status: PriceSuggestionStatus::NoSuggestion,
            differences: Vec::new(),
            notes: plan.notes.clone(),
        };
        let price = |tokens: TokenPrices, valid_from: Option<OffsetDateTime>| PriceSpec {
            provider: Some(spec.provider),
            model: spec.provider_model.clone(),
            valid_from: valid_from.unwrap_or(now).min(now),
            per_million_tokens: tokens,
            per_request: None,
            per_image: None,
            per_audio_minute: None,
            per_million_characters: None,
        };
        if spec.provider == ProviderKind::Mock {
            out.notes.push("Mock-Anbieter: keine Kosten".into());
        } else if plan.source == "none" {
            out.notes.push("settings.pricing.source: none".into());
        } else {
            if let Some(az) = &plan.azure {
                out.region.clone_from(&az.region);
                out.deployment_type = Some(az.dtype);
                if let Some(rp) = az.region.as_ref().and_then(|r| regions.get(r)) {
                    match azure::lookup(&rp.meters, &az.tokens, az.dtype) {
                        Some(m) => {
                            out.source = Some(PriceSuggestionSource::AzureRetail);
                            out.matched = Some(match &m.version {
                                Some(v) => format!("{} ({v})", az.model),
                                None => az.model.clone(),
                            });
                            if !m.other_versions.is_empty() {
                                out.notes.push(format!(
                                    "Weitere Versionen in Azure: {} – Version über \
                                     settings.pricing.model festlegen (z. B. {}-JJJJ-MM-TT)",
                                    m.other_versions.join(", "),
                                    az.model
                                ));
                            }
                            out.meters = m.meters;
                            out.suggested = Some(price(m.prices, Some(m.valid_from)));
                        }
                        None if rp.meters.is_empty() => out.notes.push(format!(
                            "Azure Retail Prices für {} nicht verfügbar{}",
                            az.region.as_deref().unwrap_or_default(),
                            rp.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                        )),
                        None => out.notes.push(format!(
                            "Kein Azure-Meter für {:?} ({}) in {} – settings.pricing.meter \
                             nennt den Modellteil des skuName",
                            az.model,
                            az.dtype.as_str(),
                            az.region.as_deref().unwrap_or_default()
                        )),
                    }
                }
            }
            if out.suggested.is_none() && plan.source != "azure" {
                // Azure OpenAI ohne Treffer: OpenAI-Listenpreis als Näherung.
                let provider = match spec.provider {
                    ProviderKind::AzureOpenAi => ProviderKind::OpenAi,
                    p => p,
                };
                if let Some((e, how)) = catalog.lookup(provider, plan.catalog_model) {
                    out.source = Some(PriceSuggestionSource::Catalog);
                    out.matched = Some(format!("{}/{}", provider.as_str(), e.model));
                    if how == CatalogMatch::Base {
                        out.notes.push(format!(
                            "Katalogpreis des Basismodells {} (ohne Versionsanhang)",
                            e.model
                        ));
                    }
                    if provider != spec.provider {
                        out.notes.push(
                            "OpenAI-Listenpreis als Näherung – Azure-Preis nicht gefunden".into(),
                        );
                    }
                    let mut p = price(e.per_million_tokens, e.valid_from_at());
                    p.per_request = e.per_request;
                    p.per_image = e.per_image;
                    p.per_audio_minute = e.per_audio_minute;
                    p.per_million_characters = e.per_million_characters;
                    out.suggested = Some(p);
                }
            }
        }
        if out.current.is_none() {
            out.notes.push("Kein Preis hinterlegt – Kosten werden mit 0 gebucht".into());
        }
        match (&out.suggested, &out.current) {
            (None, _) => out.status = PriceSuggestionStatus::NoSuggestion,
            (Some(_), None) => out.status = PriceSuggestionStatus::Missing,
            (Some(s), Some(c)) => {
                out.differences = differences(s, &c.spec);
                out.status = if out.differences.is_empty() {
                    PriceSuggestionStatus::Unchanged
                } else {
                    PriceSuggestionStatus::Differs
                };
            }
        }
        out
    }
}

#[derive(Debug)]
struct AzurePlan {
    region: Option<String>,
    dtype: AzureDeploymentType,
    tokens: Vec<String>,
    model: String,
}

#[derive(Debug)]
struct Plan<'a> {
    source: &'a str,
    azure: Option<AzurePlan>,
    catalog_model: &'a str,
    notes: Vec<String>,
}

async fn fetch_catalog(r: &RemoteCatalog) -> Result<Catalog, String> {
    let req = http::Request::get(r.url.as_str())
        .header(http::header::ACCEPT, "application/json, application/yaml, text/plain")
        .header(http::header::USER_AGENT, concat!("aigw/", env!("CARGO_PKG_VERSION")))
        .body(Bytes::new())
        .map_err(|e| e.to_string())?;
    let reply = r
        .client
        .fetch(req, &FetchLimits::new(Duration::from_secs(60), MAX_CATALOG_BYTES))
        .await
        .map_err(|e| e.to_string())?;
    if !reply.status.is_success() {
        return Err(format!("HTTP {}", reply.status.as_u16()));
    }
    let text = std::str::from_utf8(&reply.body).map_err(|_| "kein UTF-8".to_owned())?;
    if r.litellm {
        Catalog::parse_litellm(text)
    } else {
        Catalog::parse_aigw(text)
    }
}

/// Zum Zeitpunkt `at` gültiger Preis (wie `Snapshot::price_at`: spezifisch vor generisch).
pub fn current_price(
    prices: &[Object<PriceSpec>],
    provider: ProviderKind,
    model: &str,
    at: OffsetDateTime,
) -> Option<CurrentPriceVersion> {
    let pick = |p: Option<ProviderKind>| {
        prices
            .iter()
            .filter(|o| o.spec.provider == p && o.spec.model == model && o.spec.valid_from <= at)
            .max_by_key(|o| o.spec.valid_from)
    };
    pick(Some(provider)).or_else(|| pick(None)).map(|o| CurrentPriceVersion {
        name: o.metadata.name.clone(),
        manual: !o.metadata.labels.contains_key(LABEL_SOURCE),
        spec: o.spec.clone(),
    })
}

/// Abweichende Preisfelder (ohne `validFrom`, Anbieter, Modell).
pub fn differences(a: &PriceSpec, b: &PriceSpec) -> Vec<String> {
    let (x, y) = (&a.per_million_tokens, &b.per_million_tokens);
    let fields = [
        ("perMillionTokens.input", x.input, y.input),
        ("perMillionTokens.cachedInput", x.cached_input, y.cached_input),
        ("perMillionTokens.cacheWrite", x.cache_write, y.cache_write),
        ("perMillionTokens.output", x.output, y.output),
        ("perMillionTokens.reasoning", x.reasoning, y.reasoning),
        ("perMillionTokens.audioInput", x.audio_input, y.audio_input),
        ("perMillionTokens.audioOutput", x.audio_output, y.audio_output),
        ("perMillionTokens.imageInput", x.image_input, y.image_input),
        ("perRequest", a.per_request, b.per_request),
        ("perImage", a.per_image, b.per_image),
        ("perAudioMinute", a.per_audio_minute, b.per_audio_minute),
        ("perMillionCharacters", a.per_million_characters, b.per_million_characters),
    ];
    fields
        .into_iter()
        .filter(|(_, l, r)| l != r)
        .map(|(n, _, _)| n.to_owned())
        .collect()
}

/// Markiert Vorschläge als Konflikt, wenn Deployments mit demselben Preisschlüssel
/// unterschiedliche Preise vorschlagen.
fn mark_conflicts(items: &mut [PriceSuggestion]) {
    let mut by_key: HashMap<(ProviderKind, String), Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.suggested.is_some() {
            by_key
                .entry((it.provider, it.provider_model.clone()))
                .or_default()
                .push(i);
        }
    }
    for idx in by_key.values().filter(|v| v.len() > 1) {
        let first = items[idx[0]].suggested.clone();
        let differ = idx.iter().any(|&i| match (&items[i].suggested, &first) {
            (Some(s), Some(f)) => !differences(s, f).is_empty(),
            _ => false,
        });
        if differ {
            let names: Vec<String> = idx.iter().map(|&i| items[i].deployment.clone()).collect();
            for &i in idx {
                items[i].status = PriceSuggestionStatus::Conflict;
                items[i].notes.push(format!(
                    "Gleicher Preisschlüssel ({}/{}) mit abweichenden Vorschlägen: {} – \
                     Preise gelten je Anbieter und providerModel, nur einen übernehmen",
                    items[i].provider.as_str(),
                    items[i].provider_model,
                    names.join(", ")
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::resource::ObjectMeta;

    fn dep(name: &str, provider: ProviderKind, model: &str, settings: Value) -> Object<DeploymentSpec> {
        let Value::Object(settings) = settings else { unreachable!() };
        Object {
            id: aigw_core::resource::Id::default(),
            metadata: ObjectMeta::named(name),
            spec: DeploymentSpec {
                model_group: "g".into(),
                provider,
                endpoint: None,
                provider_model: model.into(),
                credential: None,
                weight: 1,
                priority: 0,
                capacity: Default::default(),
                enabled: true,
                settings,
            },
        }
    }

    fn suggester() -> Suggester {
        let mut cfg = PriceSuggestionsConfig::default();
        cfg.azure.enabled = false;
        Suggester::from_config(&cfg, &OutboundClient::new(aigw_outbound::AddressGuard::Trusted).unwrap())
            .unwrap()
    }

    #[tokio::test]
    async fn catalog_suggestions_and_status() {
        let s = suggester();
        let now = OffsetDateTime::now_utc();
        let deps = vec![
            dep("a", ProviderKind::OpenAi, "gpt-5.4-mini", serde_json::json!({})),
            dep("b", ProviderKind::Anthropic, "claude-sonnet-5-5", serde_json::json!({})),
            dep("c", ProviderKind::OpenAiCompatible, "x", serde_json::json!({})),
            dep("d", ProviderKind::Mock, "mock", serde_json::json!({})),
            dep("e", ProviderKind::OpenAi, "gpt-5.4-mini", serde_json::json!({"pricing": {"source": "none"}})),
        ];
        let manual = Object {
            id: aigw_core::resource::Id::default(),
            metadata: ObjectMeta::named("manual"),
            spec: PriceSpec {
                provider: Some(ProviderKind::Anthropic),
                model: "claude-sonnet-5-5".into(),
                valid_from: OffsetDateTime::UNIX_EPOCH,
                per_million_tokens: TokenPrices {
                    input: Some("3".parse().unwrap()),
                    ..Default::default()
                },
                per_request: None,
                per_image: None,
                per_audio_minute: None,
                per_million_characters: None,
            },
        };
        let r = s.report(&deps, &[manual], now, false).await;
        let st: Vec<_> = r.items.iter().map(|i| i.status).collect();
        assert_eq!(
            st,
            [
                PriceSuggestionStatus::Missing,
                PriceSuggestionStatus::Differs,
                PriceSuggestionStatus::NoSuggestion,
                PriceSuggestionStatus::NoSuggestion,
                PriceSuggestionStatus::NoSuggestion,
            ]
        );
        assert_eq!(r.items[0].source, Some(PriceSuggestionSource::Catalog));
        assert!(r.items[1].current.as_ref().is_some_and(|c| c.manual));
        assert!(r.items[1].differences.contains(&"perMillionTokens.input".to_owned()));
        assert!(r.items[2].notes.iter().any(|n| n.contains("Kosten werden mit 0")));
        assert_eq!(r.sources.len(), 1);
    }

    #[tokio::test]
    async fn azure_without_region_falls_back_to_openai_catalog() {
        let mut cfg = PriceSuggestionsConfig::default();
        cfg.azure.api_url = "http://127.0.0.1:9/api/retail/prices".into();
        let s = Suggester::from_config(&cfg, &OutboundClient::new(aigw_outbound::AddressGuard::Trusted).unwrap())
            .unwrap();
        let deps = vec![dep("az", ProviderKind::AzureOpenAi, "gpt-5.4-mini", serde_json::json!({}))];
        let r = s.report(&deps, &[], OffsetDateTime::now_utc(), false).await;
        let i = &r.items[0];
        assert_eq!(i.source, Some(PriceSuggestionSource::Catalog));
        assert!(i.notes.iter().any(|n| n.contains("Region unbekannt")));
        assert!(i.notes.iter().any(|n| n.contains("Näherung")));
    }

    #[test]
    fn conflicts_are_marked() {
        let mk = |d: &str, input: &str| PriceSuggestion {
            deployment: d.into(),
            model_group: "g".into(),
            provider: ProviderKind::AzureOpenAi,
            provider_model: "m".into(),
            enabled: true,
            source: Some(PriceSuggestionSource::AzureRetail),
            region: None,
            deployment_type: None,
            matched: None,
            meters: Vec::new(),
            suggested: Some(PriceSpec {
                provider: Some(ProviderKind::AzureOpenAi),
                model: "m".into(),
                valid_from: OffsetDateTime::UNIX_EPOCH,
                per_million_tokens: TokenPrices {
                    input: Some(input.parse().unwrap()),
                    ..Default::default()
                },
                per_request: None,
                per_image: None,
                per_audio_minute: None,
                per_million_characters: None,
            }),
            current: None,
            status: PriceSuggestionStatus::Missing,
            differences: Vec::new(),
            notes: Vec::new(),
        };
        let mut same = vec![mk("a", "1"), mk("b", "1")];
        mark_conflicts(&mut same);
        assert!(same.iter().all(|i| i.status == PriceSuggestionStatus::Missing));
        let mut diff = vec![mk("a", "1"), mk("b", "1.1")];
        mark_conflicts(&mut diff);
        assert!(diff.iter().all(|i| i.status == PriceSuggestionStatus::Conflict));
    }
}
