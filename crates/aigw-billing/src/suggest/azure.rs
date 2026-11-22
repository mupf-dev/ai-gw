// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Azure Retail Prices API (`https://prices.azure.com/api/retail/prices`, öffentlich, ohne
//! Anmeldung) für Azure-OpenAI- und Foundry-Deployments.
//!
//! Abfrage je Region: `serviceName eq '<service_name>' and armRegionName eq '<region>' and
//! type eq 'Consumption'`, `currencyCode=USD` (das Gateway rechnet in USD), Folgeseiten über
//! `NextPageLink` – nur auf demselben Host. Das Ergebnis liegt je Region im Zwischenspeicher
//! (`cache_ttl_secs`); schlägt eine Aktualisierung fehl, bleiben die alten Daten in Gebrauch.
//!
//! **Meter-Zuordnung.** Die `skuName`-Werte sind abgekürzt und uneinheitlich, z. B.
//! `5.4 mini cd Inp Gl`, `gpt-4o-mini-0718-Outp-regnl`, `o4-mini 0416 Batch Inp Data Zone`.
//! [`parse_meter`] zerlegt sie in Modell-Tokens, Bereitstellungsart (`Gl`/`glbl` → global,
//! `Dz`/`DZone`/`Data Zone` → data-zone, `regnl`/`rgnl` → regional) und Token-Art (`Inp`,
//! `Outp`/`Opt`, `cd`/`cached` + `Inp`, `Cd Wr` = Cache-Schreiben). Varianten wie Batch,
//! Priority Processing (`pp`), Fine-Tuning, Realtime/Audio, Long Context werden übergangen.
//! Passt die Heuristik nicht, nennt `settings.pricing.meter` den Modellteil des `skuName`
//! direkt (z. B. `Llama 4 Maverick 17B`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aigw_core::resource::{Amount, TokenPrices};
use aigw_outbound::{FetchLimits, OutboundClient};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use utoipa::ToSchema;

use super::decimal::scaled_amount;

/// Höchstzahl Seiten je Abfrage (je Seite bis 1 000 Einträge).
const MAX_PAGES: usize = 20;
/// Größte Antwortseite.
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// Bereitstellungsart eines Azure-Deployments (bestimmt die Meter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AzureDeploymentType {
    /// GlobalStandard.
    Global,
    /// DataZone Standard.
    DataZone,
    /// Standard (regional).
    Regional,
}

impl AzureDeploymentType {
    /// Aus `settings.pricing.deploymentType` bzw. der Konfiguration.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "global" => Some(Self::Global),
            "data-zone" => Some(Self::DataZone),
            "regional" => Some(Self::Regional),
            _ => None,
        }
    }

    /// Bezeichner (wie in `settings.pricing.deploymentType`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::DataZone => "data-zone",
            Self::Regional => "regional",
        }
    }
}

/// Token-Art eines Meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MeterKind {
    /// Eingabe.
    Input,
    /// Gecachte Eingabe (Lesen).
    CachedInput,
    /// Schreiben in den Prompt-Cache.
    CacheWrite,
    /// Ausgabe.
    Output,
}

/// Ein verwertbarer Meter (Preis je 1 Mio. Tokens).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meter {
    /// `skuName` (zur Anzeige).
    pub sku: String,
    /// Normalisierte Modell-Tokens.
    pub model: Vec<String>,
    /// Bereitstellungsart.
    pub deployment_type: AzureDeploymentType,
    /// Token-Art.
    pub kind: MeterKind,
    /// Preis je 1 Mio. Tokens.
    pub per_million: Amount,
    /// `effectiveStartDate`.
    pub effective: OffsetDateTime,
}

/// Eintrag der Retail-API (nur die genutzten Felder).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetailItem {
    /// Währung.
    #[serde(default)]
    pub currency_code: String,
    /// Preis je `unitOfMeasure`.
    pub retail_price: serde_json::Number,
    /// Region.
    #[serde(default)]
    pub arm_region_name: String,
    /// Gültig ab.
    #[serde(default)]
    pub effective_start_date: String,
    /// Produkt (z. B. `Azure OpenAI GPT5`).
    #[serde(default)]
    pub product_name: String,
    /// SKU (z. B. `5.4 mini Inp Gl`).
    #[serde(default)]
    pub sku_name: String,
    /// Meter (`skuName` plus Einheit, z. B. `… 1M Tokens`).
    #[serde(default)]
    pub meter_name: String,
    /// Einheit (`1K`, `1M`, `1/Hour` …).
    #[serde(default)]
    pub unit_of_measure: String,
    /// `Consumption`, `Reservation` …
    #[serde(default, rename = "type")]
    pub kind: String,
}

/// Eine Antwortseite.
#[derive(Debug, Clone, Deserialize)]
struct RetailPage {
    #[serde(rename = "Items", default)]
    items: Vec<RetailItem>,
    #[serde(rename = "NextPageLink", default)]
    next_page_link: Option<String>,
}

/// Tokens, die eine nicht unterstützte Variante kennzeichnen (Meter wird übergangen).
const EXCLUDED: &[&str] = &[
    "batch",
    "pp",
    "ft",
    "trng",
    "training",
    "longco",
    "lc",
    "rt",
    "realtime",
    "aud",
    "audio",
    "img",
    "image",
    "grader",
    "rft",
    "hosting",
    "fl",
    "flex",
    "sp",
    "tcrb",
    "transcribe",
    "tts",
    "l",
    "provisioned",
    "managed",
    "unit",
    "sora",
    "video",
    "ptu",
    "reservation",
];
/// Bedeutungslose Tokens (Standardpriorität, kurzer Kontext, Einheit).
const IGNORED: &[&str] = &["std", "shortco", "shco", "tokens", "token", "1m", "1k"];
/// Hersteller-/Familienpräfixe, die auf beiden Seiten entfallen.
const VENDOR: &[&str] = &[
    "gpt",
    "deepseek",
    "mistral",
    "microsoft",
    "meta",
    "instruct",
];

/// Zerlegt einen Namen in Tokens (klein, ohne Trenner).
fn tokens(s: &str) -> Vec<String> {
    s.to_ascii_lowercase()
        .split([' ', '-', '_', '/', ':', '@', ','])
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Vereinheitlicht Datumsangaben zu `MMTT` (`2024-07-18`, `20240718`, `0718`).
fn fold_dates(t: Vec<String>) -> Vec<String> {
    let is_year =
        |s: &str| s.len() == 4 && s.starts_with("20") && s.bytes().all(|b| b.is_ascii_digit());
    let is_2 = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
    let mut out = Vec::with_capacity(t.len());
    let mut i = 0;
    while i < t.len() {
        if is_year(&t[i]) && i + 2 < t.len() && is_2(&t[i + 1]) && is_2(&t[i + 2]) {
            out.push(format!("{}{}", t[i + 1], t[i + 2]));
            i += 3;
            continue;
        }
        let s = &t[i];
        if s.len() == 8 && s.starts_with("20") && s.bytes().all(|b| b.is_ascii_digit()) {
            out.push(s[4..].to_owned());
        } else {
            out.push(s.clone());
        }
        i += 1;
    }
    out
}

/// Modell-Tokens eines Modellnamens (`gpt-5.4-mini` → `["5.4", "mini"]`,
/// `gpt-4o-mini-2024-07-18` → `["4o", "mini", "0718"]`).
pub fn model_tokens(name: &str) -> Vec<String> {
    fold_dates(tokens(name))
        .into_iter()
        .filter(|t| !VENDOR.contains(&t.as_str()) && !IGNORED.contains(&t.as_str()))
        .collect()
}

/// Ist `t` eine vierstellige Versionskennung (`0718`)?
fn is_version(t: &str) -> bool {
    t.len() == 4 && t.bytes().all(|b| b.is_ascii_digit())
}

/// Zerlegt einen `skuName` in Modell, Bereitstellungsart und Token-Art; `None` für
/// Varianten ohne Gegenstück im Preisschema (Batch, Fine-Tuning, Realtime …) oder ohne
/// erkennbare Bereitstellungsart.
pub fn parse_sku(sku: &str) -> Option<(Vec<String>, AzureDeploymentType, MeterKind)> {
    let t = tokens(sku);
    let mut dtype = None;
    let (mut input, mut output, mut cached, mut write) = (false, false, false, false);
    let mut model = Vec::new();
    let mut i = 0;
    while i < t.len() {
        let s = t[i].as_str();
        let next = t.get(i + 1).map(String::as_str);
        match s {
            "data" if next == Some("zone") => {
                dtype = Some(AzureDeploymentType::DataZone);
                i += 1;
            }
            "gl" | "glbl" | "global" => dtype = Some(AzureDeploymentType::Global),
            "dz" | "dzone" | "dzn" => dtype = Some(AzureDeploymentType::DataZone),
            "regnl" | "rgnl" | "regional" | "rgn" => dtype = Some(AzureDeploymentType::Regional),
            "inp" | "input" | "inpt" | "in" => input = true,
            "outp" | "opt" | "out" | "output" | "outpt" => output = true,
            "cached" | "cchd" | "cd" | "cache" | "cachd" | "cch" => cached = true,
            "wr" | "write" => write = true,
            _ if EXCLUDED.contains(&s) => return None,
            _ if IGNORED.contains(&s) => {}
            _ => model.push(t[i].clone()),
        }
        i += 1;
    }
    let kind = match (input, output, cached, write) {
        (_, false, true, true) => MeterKind::CacheWrite,
        (_, false, true, false) => MeterKind::CachedInput,
        (_, true, false, false) => MeterKind::Output,
        // Ohne Angabe (z. B. Embeddings): Eingabe.
        (_, false, false, false) => MeterKind::Input,
        _ => return None,
    };
    let model: Vec<String> = fold_dates(model)
        .into_iter()
        .filter(|t| !VENDOR.contains(&t.as_str()))
        .collect();
    if model.is_empty() {
        return None;
    }
    Some((model, dtype?, kind))
}

/// Verwertbarer Meter aus einem Eintrag der Retail-API (Tokens, USD, Verbrauch).
pub fn parse_meter(item: &RetailItem) -> Option<Meter> {
    if !item.kind.eq_ignore_ascii_case("consumption")
        || (!item.currency_code.is_empty() && item.currency_code != "USD")
        || !item.meter_name.to_ascii_lowercase().ends_with("tokens")
    {
        return None;
    }
    let shift = match item.unit_of_measure.as_str() {
        "1K" => 3,
        "1M" => 0,
        "1" => 6,
        _ => return None,
    };
    let (model, deployment_type, kind) = parse_sku(&item.sku_name)?;
    let per_million = scaled_amount(&item.retail_price.to_string(), shift)?;
    let effective = OffsetDateTime::parse(
        &item.effective_start_date,
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    Some(Meter {
        sku: item.sku_name.clone(),
        model,
        deployment_type,
        kind,
        per_million,
        effective,
    })
}

/// Treffer für ein Modell in einer Region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureMatch {
    /// Preise je 1 Mio. Tokens.
    pub prices: TokenPrices,
    /// Verwendete Meter (`skuName`).
    pub meters: Vec<String>,
    /// Jüngstes `effectiveStartDate` der verwendeten Meter.
    pub valid_from: OffsetDateTime,
    /// Gewählte Version (`MMTT`), falls das Modell ohne Version angegeben war.
    pub version: Option<String>,
    /// Weitere Versionen, die ebenfalls gepasst hätten.
    pub other_versions: Vec<String>,
}

/// Sucht die Meter von `model` (Tokens aus [`model_tokens`]) mit Bereitstellungsart `dtype`.
///
/// Exakte Treffer gehen vor; sonst Meter mit zusätzlicher Versionskennung (`gpt-4o` →
/// `gpt 4o 1120`), dabei die Version mit dem jüngsten `effectiveStartDate`.
pub fn lookup(
    meters: &[Meter],
    model: &[String],
    dtype: AzureDeploymentType,
) -> Option<AzureMatch> {
    if model.is_empty() {
        return None;
    }
    let mut exact: Vec<&Meter> = Vec::new();
    let mut versioned: HashMap<&str, Vec<&Meter>> = HashMap::new();
    let model_has_version = model.last().is_some_and(|t| is_version(t));
    for m in meters.iter().filter(|m| m.deployment_type == dtype) {
        if m.model == model {
            exact.push(m);
        } else if !model_has_version
            && m.model.len() == model.len() + 1
            && m.model[..model.len()] == *model
            && is_version(&m.model[model.len()])
        {
            versioned.entry(&m.model[model.len()]).or_default().push(m);
        }
    }
    let (chosen, version, others) = if exact.is_empty() {
        let mut groups: Vec<(&str, Vec<&Meter>)> = versioned.into_iter().collect();
        // Jüngste Version zuerst (Gültigkeit, dann Kennung).
        groups.sort_by(|a, b| {
            let ea = a.1.iter().map(|m| m.effective).max();
            let eb = b.1.iter().map(|m| m.effective).max();
            eb.cmp(&ea).then_with(|| b.0.cmp(a.0))
        });
        let mut it = groups.into_iter();
        let (v, ms) = it.next()?;
        let mut others: Vec<String> = it.map(|(v, _)| v.to_owned()).collect();
        others.sort();
        (ms, Some(v.to_owned()), others)
    } else {
        (exact, None, Vec::new())
    };
    let mut prices = TokenPrices::default();
    let mut used: Vec<&Meter> = Vec::new();
    for kind in [
        MeterKind::Input,
        MeterKind::CachedInput,
        MeterKind::CacheWrite,
        MeterKind::Output,
    ] {
        // Mehrere Meter derselben Art: der jüngste gilt.
        let Some(m) = chosen
            .iter()
            .filter(|m| m.kind == kind)
            .max_by_key(|m| (m.effective, m.sku.clone()))
        else {
            continue;
        };
        let slot = match kind {
            MeterKind::Input => &mut prices.input,
            MeterKind::CachedInput => &mut prices.cached_input,
            MeterKind::CacheWrite => &mut prices.cache_write,
            MeterKind::Output => &mut prices.output,
        };
        *slot = Some(m.per_million);
        used.push(m);
    }
    // Ohne Eingabepreis kein brauchbarer Vorschlag.
    prices.input?;
    let valid_from = used.iter().map(|m| m.effective).max()?;
    Some(AzureMatch {
        prices,
        meters: used.iter().map(|m| m.sku.clone()).collect(),
        valid_from,
        version,
        other_versions: others,
    })
}

/// Region aus einem Azure-Endpunkt, soweit ablesbar: `https://<region>.api.cognitive.microsoft.com`
/// und serverlose Endpunkte `https://<name>.<region>.models.ai.azure.com`.
pub fn region_from_endpoint(endpoint: &str) -> Option<String> {
    let host = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, r)| r)
        .split(['/', ':', '?'])
        .next()?
        .to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let region = if host.ends_with(".api.cognitive.microsoft.com") && labels.len() == 5 {
        labels[0]
    } else if host.ends_with(".models.ai.azure.com") && labels.len() == 6 {
        labels[1]
    } else {
        return None;
    };
    valid_region(region).then(|| region.to_owned())
}

/// Ist `host` ein Azure-Endpunkt für Modelle (Azure OpenAI, Foundry)?
pub fn is_azure_host(endpoint: &str) -> bool {
    let host = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, r)| r)
        .split(['/', ':', '?'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        ".openai.azure.com",
        ".services.ai.azure.com",
        ".cognitiveservices.azure.com",
        ".models.ai.azure.com",
        ".inference.ai.azure.com",
        ".api.cognitive.microsoft.com",
    ]
    .iter()
    .any(|s| host.ends_with(s))
}

/// Gültiger Regionsname (`swedencentral`).
pub fn valid_region(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 64
        && r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Ergebnis einer Regionsabfrage.
#[derive(Debug, Clone)]
pub struct RegionPrices {
    /// Meter (leer, wenn nie erfolgreich geladen).
    pub meters: Arc<Vec<Meter>>,
    /// Zeitpunkt des letzten erfolgreichen Abrufs.
    pub fetched_at: Option<OffsetDateTime>,
    /// Fehler des letzten Abrufs (ältere Daten bleiben in Gebrauch).
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    meters: Arc<Vec<Meter>>,
    fetched_at: Option<OffsetDateTime>,
    loaded: Option<Instant>,
    attempted: Instant,
    error: Option<String>,
}

/// Client der Retail-API mit Zwischenspeicher je Region.
#[derive(Debug)]
pub struct AzureRetail {
    client: OutboundClient,
    base: url::Url,
    service_name: String,
    ttl: Duration,
    timeout: Duration,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

/// Nach einem Fehler frühestens so spät erneut versuchen.
const ERROR_BACKOFF: Duration = Duration::from_secs(60);

impl AzureRetail {
    /// Client für `api_url` (nur dieser Host, auch für Folgeseiten).
    pub fn new(
        client: OutboundClient,
        api_url: &str,
        service_name: &str,
        ttl: Duration,
        timeout: Duration,
    ) -> Result<Self, String> {
        let base = url::Url::parse(api_url).map_err(|e| format!("api_url: {e}"))?;
        if !matches!(base.scheme(), "https" | "http") || base.host_str().is_none() {
            return Err("api_url: http(s)-URL mit Host erwartet".into());
        }
        if service_name.trim().is_empty() || service_name.contains('\'') {
            return Err("service_name: nicht leer, ohne Apostroph".into());
        }
        Ok(Self {
            client,
            base,
            service_name: service_name.to_owned(),
            ttl,
            timeout,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Host der API (für die SSRF-Ausnahme).
    pub fn host(&self) -> &str {
        self.base.host_str().unwrap_or_default()
    }

    /// Meter einer Region (Zwischenspeicher; `refresh` erzwingt einen Abruf).
    pub async fn region(&self, region: &str, refresh: bool) -> RegionPrices {
        if !valid_region(region) {
            return RegionPrices {
                meters: Arc::new(Vec::new()),
                fetched_at: None,
                error: Some(format!("ungültige Region {region:?}")),
            };
        }
        // Ein Abruf je Region gleichzeitig: die Sperre gilt über den Abruf hinweg (selten,
        // nur Admin-Anfragen und der Abgleich).
        let mut cache = self.cache.lock().await;
        if let Some(e) = cache.get(region) {
            let fresh = e.loaded.is_some_and(|t| t.elapsed() < self.ttl);
            let backoff = e.error.is_some() && e.attempted.elapsed() < ERROR_BACKOFF;
            if (fresh && !refresh) || (backoff && !refresh) || (fresh && backoff) {
                return RegionPrices {
                    meters: Arc::clone(&e.meters),
                    fetched_at: e.fetched_at,
                    error: e.error.clone(),
                };
            }
        }
        let result = self.fetch(region).await;
        let entry = match (result, cache.remove(region)) {
            (Ok(meters), _) => CacheEntry {
                meters: Arc::new(meters),
                fetched_at: Some(OffsetDateTime::now_utc()),
                loaded: Some(Instant::now()),
                attempted: Instant::now(),
                error: None,
            },
            (Err(e), old) => {
                tracing::warn!(region, error = %e, "Azure Retail Prices: Abruf fehlgeschlagen");
                let (meters, fetched_at, loaded) = old
                    .map(|o| (o.meters, o.fetched_at, o.loaded))
                    .unwrap_or_else(|| (Arc::new(Vec::new()), None, None));
                CacheEntry {
                    meters,
                    fetched_at,
                    loaded,
                    attempted: Instant::now(),
                    error: Some(e),
                }
            }
        };
        let out = RegionPrices {
            meters: Arc::clone(&entry.meters),
            fetched_at: entry.fetched_at,
            error: entry.error.clone(),
        };
        cache.insert(region.to_owned(), entry);
        out
    }

    /// Erste Seite einer Region.
    fn first_url(&self, region: &str) -> url::Url {
        let mut u = self.base.clone();
        let filter = format!(
            "serviceName eq '{}' and armRegionName eq '{region}' and type eq 'Consumption'",
            self.service_name
        );
        u.query_pairs_mut()
            .append_pair("currencyCode", "USD")
            .append_pair("$filter", &filter);
        u
    }

    /// Folgeseite nur auf demselben Host (keine Weiterleitung an fremde Ziele).
    fn same_origin(&self, link: &str) -> Result<url::Url, String> {
        let u = url::Url::parse(link).map_err(|e| format!("NextPageLink ungültig: {e}"))?;
        if u.scheme() != self.base.scheme()
            || u.host_str() != self.base.host_str()
            || u.port_or_known_default() != self.base.port_or_known_default()
        {
            return Err(format!(
                "NextPageLink auf fremdem Host: {}",
                u.host_str().unwrap_or("")
            ));
        }
        Ok(u)
    }

    async fn fetch(&self, region: &str) -> Result<Vec<Meter>, String> {
        let mut url = self.first_url(region);
        let mut meters = Vec::new();
        let limits = FetchLimits::new(self.timeout, MAX_PAGE_BYTES);
        for _ in 0..MAX_PAGES {
            let req = http::Request::get(url.as_str())
                .header(http::header::ACCEPT, "application/json")
                .header(
                    http::header::USER_AGENT,
                    concat!("aigw/", env!("CARGO_PKG_VERSION")),
                )
                .body(Bytes::new())
                .map_err(|e| e.to_string())?;
            let reply = self
                .client
                .fetch(req, &limits)
                .await
                .map_err(|e| e.to_string())?;
            if !reply.status.is_success() {
                return Err(format!("HTTP {}", reply.status.as_u16()));
            }
            let page: RetailPage = serde_json::from_slice(&reply.body)
                .map_err(|e| format!("Antwort nicht lesbar: {e}"))?;
            meters.extend(page.items.iter().filter_map(parse_meter));
            match page.next_page_link.filter(|l| !l.is_empty()) {
                Some(link) => url = self.same_origin(&link)?,
                None => return Ok(meters),
            }
        }
        Err(format!("mehr als {MAX_PAGES} Seiten"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sku(s: &str) -> Option<(String, AzureDeploymentType, MeterKind)> {
        parse_sku(s).map(|(m, d, k)| (m.join(" "), d, k))
    }

    #[test]
    fn parses_sku_names() {
        use AzureDeploymentType::*;
        use MeterKind::*;
        assert_eq!(
            sku("5.4 mini Inp Gl"),
            Some(("5.4 mini".into(), Global, Input))
        );
        assert_eq!(
            sku("5.4 mini cd Inp Dz"),
            Some(("5.4 mini".into(), DataZone, CachedInput))
        );
        assert_eq!(
            sku("5.4 mini Opt Gl"),
            Some(("5.4 mini".into(), Global, Output))
        );
        assert_eq!(
            sku("gpt-4o-mini-0718-Outp-regnl"),
            Some(("4o mini 0718".into(), Regional, Output))
        );
        assert_eq!(
            sku("gpt 4o 0806 Outp Data Zone"),
            Some(("4o 0806".into(), DataZone, Output))
        );
        assert_eq!(
            sku("5.6 sol ShortCo Inp Std Gl"),
            Some(("5.6 sol".into(), Global, Input))
        );
        assert_eq!(
            sku("5.6 sol ShortCo Cd Wr Std Gl"),
            Some(("5.6 sol".into(), Global, CacheWrite))
        );
        assert_eq!(
            sku("V4.1 Flash cached Glbl"),
            Some(("v4.1 flash".into(), Global, CachedInput))
        );
        assert_eq!(
            sku("text embedding 3 small DZ"),
            Some(("text embedding 3 small".into(), DataZone, Input))
        );
        // Varianten ohne Gegenstück.
        assert_eq!(sku("5.4 mini Batch Inp Gl"), None);
        assert_eq!(sku("5.4 mini pp Inp Gl"), None);
        assert_eq!(sku("gpt-4o-rt-aud-0603 Outp glbl"), None);
        assert_eq!(sku("5.4 pro longco batch inp Dz"), None);
        assert_eq!(sku("Llama3.3 70b-IN-FT"), None);
        // Ohne Bereitstellungsart.
        assert_eq!(sku("Codestral Inp"), None);
    }

    #[test]
    fn model_token_normalisation() {
        assert_eq!(model_tokens("gpt-5.4-mini"), ["5.4", "mini"]);
        assert_eq!(
            model_tokens("gpt-4o-mini-2024-07-18"),
            ["4o", "mini", "0718"]
        );
        assert_eq!(model_tokens("o4-mini"), ["o4", "mini"]);
        assert_eq!(model_tokens("DeepSeek-V3.2"), ["v3.2"]);
        assert_eq!(model_tokens("Mistral-Large-3"), ["large", "3"]);
        assert_eq!(
            model_tokens("claude-sonnet-4-5-20250929"),
            ["claude", "sonnet", "4", "5", "0929"]
        );
    }

    fn meter(sku: &str, price: &str, date: &str) -> Meter {
        let (model, deployment_type, kind) = parse_sku(sku).unwrap();
        Meter {
            sku: sku.into(),
            model,
            deployment_type,
            kind,
            per_million: price.parse().unwrap(),
            effective: OffsetDateTime::parse(date, &time::format_description::well_known::Rfc3339)
                .unwrap(),
        }
    }

    #[test]
    fn lookup_prefers_exact_then_newest_version() {
        let ms = vec![
            meter("5.4 mini Inp Gl", "0.75", "2026-03-01T00:00:00Z"),
            meter("5.4 mini cd Inp Gl", "0.075", "2026-03-01T00:00:00Z"),
            meter("5.4 mini Opt Gl", "4.5", "2026-03-01T00:00:00Z"),
            meter("5.4 mini Inp Dz", "0.825", "2026-03-01T00:00:00Z"),
            meter("gpt 4o 0806 Inp glbl", "2.5", "2024-10-01T00:00:00Z"),
            meter("gpt 4o 0806 Outp glbl", "10", "2024-10-01T00:00:00Z"),
            meter("gpt 4o 1120 Inp glbl", "2.5", "2024-12-01T00:00:00Z"),
            meter("gpt 4o 1120 Outp glbl", "10", "2024-12-01T00:00:00Z"),
        ];
        let m = lookup(
            &ms,
            &model_tokens("gpt-5.4-mini"),
            AzureDeploymentType::Global,
        );
        let m = m.as_ref();
        assert_eq!(
            m.and_then(|m| m.prices.input)
                .map(|a| a.to_string())
                .as_deref(),
            Some("0.75")
        );
        assert_eq!(
            m.and_then(|m| m.prices.cached_input)
                .map(|a| a.to_string())
                .as_deref(),
            Some("0.075")
        );
        assert_eq!(
            m.and_then(|m| m.prices.output)
                .map(|a| a.to_string())
                .as_deref(),
            Some("4.5")
        );
        assert_eq!(m.map(|m| m.meters.len()), Some(3));
        let dz = lookup(
            &ms,
            &model_tokens("gpt-5.4-mini"),
            AzureDeploymentType::DataZone,
        );
        assert_eq!(
            dz.and_then(|m| m.prices.input)
                .map(|a| a.to_string())
                .as_deref(),
            Some("0.825")
        );
        let v = lookup(&ms, &model_tokens("gpt-4o"), AzureDeploymentType::Global);
        assert_eq!(v.as_ref().and_then(|m| m.version.as_deref()), Some("1120"));
        assert_eq!(v.map(|m| m.other_versions), Some(vec!["0806".to_owned()]));
        let pinned = lookup(
            &ms,
            &model_tokens("gpt-4o-2024-08-06"),
            AzureDeploymentType::Global,
        );
        assert_eq!(
            pinned.map(|m| m.meters[0].clone()).as_deref(),
            Some("gpt 4o 0806 Inp glbl")
        );
        assert!(lookup(&ms, &model_tokens("gpt-5.4"), AzureDeploymentType::Global).is_none());
        assert!(
            lookup(
                &ms,
                &model_tokens("gpt-5.4-mini"),
                AzureDeploymentType::Regional
            )
            .is_none()
        );
    }

    #[test]
    fn regions_and_hosts() {
        assert_eq!(
            region_from_endpoint("https://swedencentral.api.cognitive.microsoft.com/").as_deref(),
            Some("swedencentral")
        );
        assert_eq!(
            region_from_endpoint("https://Mistral-x.swedencentral.models.ai.azure.com").as_deref(),
            Some("swedencentral")
        );
        assert_eq!(region_from_endpoint("https://res.openai.azure.com"), None);
        assert!(is_azure_host("https://res.services.ai.azure.com/anthropic"));
        assert!(is_azure_host("https://res.openai.azure.com"));
        assert!(!is_azure_host("https://api.openai.com/v1"));
        assert!(!is_azure_host("https://openai.azure.com.evil.example"));
    }
}
