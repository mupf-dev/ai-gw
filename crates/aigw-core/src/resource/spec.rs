// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Spezifikationen der Ressourcen (`spec`-Teil).
//!
//! Dieselben Typen dienen als Eingabe der Admin-API, als `spec` in GitOps-YAML und als
//! gelesene Ressource. Verweise auf andere Ressourcen erfolgen **über den Namen**
//! (`metadata.name`), nie über die ID – so bleibt YAML zwischen Installationen portabel.
//! Feldnamen sind camelCase (Kubernetes-Stil).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;

use super::money::Amount;
use super::secret::SecretRef;
use crate::observe::PayloadMode;
use crate::types::ProviderKind;

/// Frei wählbare Metadaten (Schlüssel/Wert), z. B. Kostenstelle.
pub type Metadata = BTreeMap<String, String>;

/// Beliebige JSON-Einstellungen (Plugin-Konfiguration, anbieterspezifische Felder).
pub type JsonObject = serde_json::Map<String, serde_json::Value>;

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

// ---------------------------------------------------------------------------------------------
// Limits und Budgets
// ---------------------------------------------------------------------------------------------

/// Gemessene Größe eines Limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LimitMetric {
    /// Anzahl Anfragen.
    Requests,
    /// Tokens gesamt (Eingabe + Ausgabe).
    Tokens,
    /// Eingabe-Tokens.
    InputTokens,
    /// Ausgabe-Tokens.
    OutputTokens,
    /// Gleichzeitig offene Anfragen (kein Zeitfenster).
    Concurrency,
}

/// Zeitfenster eines Limits (F25): gleitend (`second` … `30d`) oder Kalenderzeitraum
/// (`calendar-day|week|month` in der Zeitzone `billing.timezone`, wie Budgets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LimitWindow {
    /// Sekunde (gleitend).
    Second,
    /// Minute (RPM/TPM, gleitend).
    Minute,
    /// Stunde (gleitend).
    Hour,
    /// 24 Stunden (gleitend).
    Day,
    /// 30 × 24 Stunden (gleitend).
    #[serde(rename = "30d")]
    Days30,
    /// Kalendertag (ab Mitternacht in `billing.timezone`, dann zurückgesetzt).
    CalendarDay,
    /// Kalenderwoche (ab Montag 00:00 in `billing.timezone`).
    CalendarWeek,
    /// Kalendermonat (ab dem 1. um 00:00 in `billing.timezone`).
    CalendarMonth,
}

impl LimitWindow {
    /// Alle Fenster.
    pub const ALL: [Self; 8] = [
        Self::Second,
        Self::Minute,
        Self::Hour,
        Self::Day,
        Self::Days30,
        Self::CalendarDay,
        Self::CalendarWeek,
        Self::CalendarMonth,
    ];

    /// Bezeichner wie in der Konfiguration (`minute`, `30d`, `calendar-month` …).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Days30 => "30d",
            Self::CalendarDay => "calendar-day",
            Self::CalendarWeek => "calendar-week",
            Self::CalendarMonth => "calendar-month",
        }
    }

    /// `true` für Kalenderzeiträume (fest, nicht gleitend).
    pub const fn is_calendar(self) -> bool {
        matches!(
            self,
            Self::CalendarDay | Self::CalendarWeek | Self::CalendarMonth
        )
    }
}

/// Ein Limit, z. B. „100 000 Tokens je Minute für Modellgruppe gpt-4o“.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitSpec {
    /// Gemessene Größe.
    pub metric: LimitMetric,
    /// Höchstwert im Fenster.
    pub limit: u64,
    /// Fenster (bei `concurrency` ignoriert).
    #[serde(default = "default_window")]
    pub window: LimitWindow,
    /// Nur für diese Modellgruppe (Name); ohne Angabe für alle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_group: Option<String>,
}

fn default_window() -> LimitWindow {
    LimitWindow::Minute
}

/// Zeitraum eines Budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetPeriod {
    /// Kalendertag (UTC).
    Day,
    /// Kalenderwoche (Montag, UTC).
    Week,
    /// Kalendermonat (UTC).
    Month,
    /// Gesamtlaufzeit (kein Zurücksetzen).
    Total,
}

/// Budget mit Warnschwellen und harter Grenze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Budget {
    /// Betrag je Zeitraum in der Abrechnungswährung.
    pub amount: Amount,
    /// Zeitraum.
    pub period: BudgetPeriod,
    /// Warnschwellen in Prozent (Ereignis/Webhook), z. B. `[50, 80, 100]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warn_at_percent: Vec<u8>,
    /// Bei Erreichen ablehnen (`budget_exceeded`); `false` = nur warnen.
    #[serde(default = "default_true")]
    pub hard_limit: bool,
    /// Durchsetzung der harten Grenze (F106): `strict` (Standard) reserviert die geschätzten
    /// Kosten vor dem Upstream-Aufruf, `soft` lehnt erst ab, wenn der Verbrauch den Betrag
    /// erreicht hat.
    #[serde(default, skip_serializing_if = "is_default")]
    pub enforcement: BudgetEnforcement,
}

/// Durchsetzung der harten Grenze eines Budgets (F106, AIGW-71).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BudgetEnforcement {
    /// Geschätzte Kosten (Prompt-Tokens + `max_tokens` × Preis) werden vor dem Aufruf
    /// reserviert und nach der Usage abgeglichen; abgelehnt wird, wenn Verbrauch, laufende
    /// Reservierungen und Schätzung den Betrag überschreiten würden.
    #[default]
    Strict,
    /// Abgelehnt wird erst, wenn der bekannte Verbrauch den Betrag erreicht hat; gleichzeitig
    /// laufende Anfragen können das Budget um ihre Kosten überschreiten.
    Soft,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------------------------
// Mandanten
// ---------------------------------------------------------------------------------------------

/// Organisation (oberste Mandantenebene).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OrganizationSpec {
    /// Anzeigename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Limits für die gesamte Organisation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<LimitSpec>,
    /// Budget der Organisation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Payload-Logging (F30): `off` | `metadata` | `masked` | `full`. Vererbung Key → Team →
    /// Organisation → `observe.payload`; das spezifischste gesetzte Feld gilt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_mode: Option<PayloadMode>,
    /// Reasoning-Text an OpenAI-Clients ausgeben (`reasoning_content`, F10/F185). Vererbung
    /// Key → Team → Organisation; `false` blendet aus, auch wenn die Modellgruppe es erlaubt
    /// (`settings.exposeReasoning` muss zusätzlich an sein).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_reasoning: Option<bool>,
    /// Freie Metadaten.
    #[serde(default, skip_serializing_if = "Metadata::is_empty")]
    pub metadata: Metadata,
}

/// Team innerhalb einer Organisation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TeamSpec {
    /// Organisation (Name).
    pub organization: String,
    /// Anzeigename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Erlaubte Modellgruppen (Name oder Alias); leer = alle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    /// Limits des Teams.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<LimitSpec>,
    /// Budget des Teams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Payload-Logging (F30): `off` | `metadata` | `masked` | `full`. Vererbung Key → Team →
    /// Organisation → `observe.payload`; das spezifischste gesetzte Feld gilt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_mode: Option<PayloadMode>,
    /// Reasoning-Text an OpenAI-Clients ausgeben (`reasoning_content`, F10/F185). Vererbung
    /// Key → Team → Organisation; `false` blendet aus, auch wenn die Modellgruppe es erlaubt
    /// (`settings.exposeReasoning` muss zusätzlich an sein).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_reasoning: Option<bool>,
    /// Freie Metadaten.
    #[serde(default, skip_serializing_if = "Metadata::is_empty")]
    pub metadata: Metadata,
}

/// Rolle eines Nutzers (§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Alles.
    Admin,
    /// Administration einer Organisation.
    OrgAdmin,
    /// Keys, Budgets und Limits im eigenen Team.
    TeamAdmin,
    /// Lesen, Dashboards.
    Viewer,
    /// Audit-Log lesen.
    Auditor,
    /// Nur GitOps: `config:export` und `config:apply` (auch Trockenlauf), z. B. für einen
    /// Admin-API-Key der CI-Pipeline (F55). Nur global wirksam.
    Gitops,
}

/// Rollenzuweisung, optional eingeschränkt auf Organisation oder Team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoleBinding {
    /// Rolle.
    pub role: Role,
    /// Organisation (Name), falls eingeschränkt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Team (Name), falls eingeschränkt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
}

/// Nutzer der Admin-UI (Login per OIDC).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserSpec {
    /// OIDC-Issuer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_issuer: Option<String>,
    /// Unveränderliche OIDC-Kennung beim Issuer: Wert des Claims
    /// `admin.auth.oidc.identity_claim` (Standard `sub`; Profil `entra`: `oid`, F90). Kann vorab
    /// gesetzt werden (Nutzer anlegen, bevor er sich das erste Mal anmeldet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc_subject: Option<String>,
    /// E-Mail-Adresse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Anzeigename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Rollen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<RoleBinding>,
}

// ---------------------------------------------------------------------------------------------
// API-Keys
// ---------------------------------------------------------------------------------------------

/// Virtueller API-Key (Spezifikation; Präfix/Hash/Zustand siehe [`ApiKeyStatus`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApiKeySpec {
    /// Organisation (Name, Pflicht; F3). Mit Team muss es die Organisation des Teams sein.
    /// Keys ohne Team erben Limits, Budgets und Plugins der Organisation.
    pub organization: String,
    /// Team (Name, optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Besitzender Nutzer (Name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Ablaufzeitpunkt (RFC 3339).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub expires_at: Option<OffsetDateTime>,
    /// Erlaubte Modellgruppen (Name oder Alias); leer = alle, die das Team erlaubt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_models: Vec<String>,
    /// Limits des Keys.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<LimitSpec>,
    /// Budget des Keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Payload-Logging (F30): `off` | `metadata` | `masked` | `full`. Vererbung Key → Team →
    /// Organisation → `observe.payload`; das spezifischste gesetzte Feld gilt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_mode: Option<PayloadMode>,
    /// Reasoning-Text an OpenAI-Clients ausgeben (`reasoning_content`, F10/F185). Vererbung
    /// Key → Team → Organisation; `false` blendet aus, auch wenn die Modellgruppe es erlaubt
    /// (`settings.exposeReasoning` muss zusätzlich an sein).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_reasoning: Option<bool>,
    /// Freie Metadaten (erscheinen im Request-Log).
    #[serde(default, skip_serializing_if = "Metadata::is_empty")]
    pub metadata: Metadata,
}

/// Zustand eines API-Keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKeyState {
    /// Aktiv.
    #[default]
    Active,
    /// Vorübergehend gesperrt (umkehrbar mit `resume`, F37).
    Suspended,
    /// Widerrufen (endgültig, F37).
    Revoked,
}

impl ApiKeyState {
    /// Bezeichner in der Datenbank.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Revoked => "revoked",
        }
    }

    /// Gegenstück zu [`ApiKeyState::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// Vom Server verwaltete Felder eines API-Keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyStatus {
    /// Sichtbares Präfix (12 Zeichen), zur Wiedererkennung in UI und Logs.
    pub prefix: String,
    /// Zustand.
    pub state: ApiKeyState,
    /// Zuletzt verwendet (gepflegt vom Log-Writer, unscharf).
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub last_used_at: Option<OffsetDateTime>,
}

// ---------------------------------------------------------------------------------------------
// Anbieter, Modelle, Routing
// ---------------------------------------------------------------------------------------------

/// Zugangsdaten eines Anbieters (Eingabe). Werte werden verschlüsselt gespeichert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCredentialSpec {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Felder, z. B. `apiKey`, `accessKeyId`/`secretAccessKey`, `serviceAccountJson`.
    pub data: BTreeMap<String, SecretRef>,
}

/// Zugangsdaten eines Anbieters (gelesene Sicht, **ohne** Werte).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialInfo {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Namen der gespeicherten Felder.
    pub fields: Vec<String>,
    /// ID des Master-Keys, mit dem verschlüsselt wurde (Rotation).
    pub key_id: String,
}

/// Routing-Strategie einer Modellgruppe (§6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingStrategy {
    /// Gewichtet zufällig (Standard).
    #[default]
    Weighted,
    /// Geringste Latenz (EWMA).
    LeastLatency,
    /// Wenigste offene Anfragen.
    LeastBusy,
    /// Günstigstes Deployment zuerst.
    Cost,
}

impl RoutingStrategy {
    /// Bezeichner in der Datenbank.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::LeastLatency => "least-latency",
            Self::LeastBusy => "least-busy",
            Self::Cost => "cost",
        }
    }
}

/// Fehlerklasse, bei der eine Fallback-Kette greift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum FallbackTrigger {
    /// Jeder retrybare Fehler.
    Any,
    /// Rate-Limit/Quota (429).
    RateLimit,
    /// Kontext zu lang.
    ContextLength,
    /// Content-Filter des Anbieters.
    ContentFilter,
    /// Server- oder Verbindungsfehler.
    Unavailable,
}

/// Fallback auf eine andere Modellgruppe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fallback {
    /// Ziel-Modellgruppe (Name).
    pub model_group: String,
    /// Auslöser; leer = `any`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on: Vec<FallbackTrigger>,
}

/// Modellgruppe: öffentlicher Modellname mit Deployments dahinter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelGroupSpec {
    /// Weitere Namen, unter denen Clients die Gruppe ansprechen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Routing-Strategie.
    #[serde(default, skip_serializing_if = "is_default")]
    pub routing: RoutingStrategy,
    /// Fallback-Kette in Reihenfolge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Fallback>,
    /// Globale Limits der Modellgruppe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<LimitSpec>,
    /// Weitere Einstellungen (Timeouts, Retries …), Schema folgt mit dem Routing (AIGW-…).
    #[serde(default, skip_serializing_if = "JsonObject::is_empty")]
    #[schema(value_type = Object)]
    pub settings: JsonObject,
}

/// Kapazität eines Deployments (für das Routing).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capacity {
    /// Anfragen je Minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,
    /// Tokens je Minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u64>,
}

/// Deployment: konkretes Anbieter-Modell hinter einer Modellgruppe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentSpec {
    /// Modellgruppe (Name).
    pub model_group: String,
    /// Anbieter.
    pub provider: ProviderKind,
    /// Basis-URL; ohne Angabe der Standard des Anbieters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Modellname beim Anbieter (bzw. Azure-Deployment-Name).
    pub provider_model: String,
    /// Zugangsdaten (Name einer `ProviderCredential`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Gewicht für `weighted` (Standard 1).
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Prioritätsgruppe; 0 wird vor 1 genutzt (z. B. PTU vor PAYG).
    #[serde(default)]
    pub priority: u32,
    /// Kapazitätsgrenzen.
    #[serde(default, skip_serializing_if = "is_default")]
    pub capacity: Capacity,
    /// Deaktivierte Deployments werden nicht geroutet.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Anbieterspezifische Einstellungen (z. B. `apiVersion`, `region`).
    #[serde(default, skip_serializing_if = "JsonObject::is_empty")]
    #[schema(value_type = Object)]
    pub settings: JsonObject,
}

fn default_weight() -> u32 {
    1
}

// ---------------------------------------------------------------------------------------------
// Preise
// ---------------------------------------------------------------------------------------------

/// Preise je 1 Mio. Tokens bzw. je Einheit; fehlende Werte gelten als 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenPrices {
    /// Eingabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Amount>,
    /// Ausgabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Amount>,
    /// Gecachte Eingabe-Tokens (Lesen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<Amount>,
    /// Schreiben in den Prompt-Cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<Amount>,
    /// Reasoning-Tokens (falls abweichend von `output`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Amount>,
    /// Audio-Eingabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<Amount>,
    /// Audio-Ausgabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_output: Option<Amount>,
    /// Bild-Eingabe-Tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_input: Option<Amount>,
}

/// Preis eines Anbieter-Modells ab einem Zeitpunkt (versioniert).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PriceSpec {
    /// Anbieter; ohne Angabe für alle Anbieter dieses Modells.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    /// Modellname beim Anbieter (wie `Deployment.providerModel`).
    pub model: String,
    /// Gültig ab (RFC 3339); es gilt der jüngste Preis mit `validFrom` ≤ Anfragezeitpunkt.
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub valid_from: OffsetDateTime,
    /// Preise je 1 Mio. Tokens.
    #[serde(default, skip_serializing_if = "is_default")]
    pub per_million_tokens: TokenPrices,
    /// Fester Preis je Anfrage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_request: Option<Amount>,
    /// Preis je erzeugtem Bild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_image: Option<Amount>,
}

// ---------------------------------------------------------------------------------------------
// Plugins und MCP
// ---------------------------------------------------------------------------------------------

/// Ebene, auf der eine Plugin-Bindung oder Freigabe gilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ScopeLevel {
    /// Global.
    Global,
    /// Organisation.
    Organization,
    /// Team.
    Team,
    /// API-Key.
    ApiKey,
    /// Modellgruppe.
    ModelGroup,
}

impl ScopeLevel {
    /// Bezeichner in der Datenbank.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Organization => "organization",
            Self::Team => "team",
            Self::ApiKey => "api-key",
            Self::ModelGroup => "model-group",
        }
    }

    /// Gegenstück zu [`ScopeLevel::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "global" => Self::Global,
            "organization" => Self::Organization,
            "team" => Self::Team,
            "api-key" => Self::ApiKey,
            "model-group" => Self::ModelGroup,
            _ => return None,
        })
    }
}

/// Ziel einer Bindung: Ebene + Name des Ziels (bei `global` ohne Ziel).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scope {
    /// Ebene.
    pub level: ScopeLevel,
    /// Name des Ziels (Organisation, Team, API-Key oder Modellgruppe).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl Scope {
    /// Globale Ebene.
    pub fn global() -> Self {
        Self {
            level: ScopeLevel::Global,
            target: None,
        }
    }
}

/// Plugin-Bindung: ein Plugin mit Konfiguration auf einer Ebene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginBindingSpec {
    /// Geltungsbereich.
    pub scope: Scope,
    /// Plugin-Name (z. B. `pii`, `webhook`).
    pub plugin: String,
    /// Reihenfolge innerhalb der Ebene (aufsteigend).
    #[serde(default)]
    pub order: i32,
    /// Aktiv?
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Plugin-Konfiguration (Schema je Plugin).
    #[serde(default, skip_serializing_if = "JsonObject::is_empty")]
    #[schema(value_type = Object)]
    pub config: JsonObject,
}

/// Transport zu einem Upstream-MCP-Server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    /// Streamable HTTP (Standard).
    #[default]
    StreamableHttp,
    /// Server-Sent Events (älteres Protokoll).
    Sse,
    /// Lokaler Prozess über stdio.
    Stdio,
}

/// Upstream-MCP-Server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerSpec {
    /// URL (HTTP-Transporte) bzw. Befehl (stdio).
    pub url: String,
    /// Transport.
    #[serde(default, skip_serializing_if = "is_default")]
    pub transport: McpTransport,
    /// Zugangsdaten (Name einer `ProviderCredential`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Aktiv?
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Weitere Einstellungen (Header, Timeouts …).
    #[serde(default, skip_serializing_if = "JsonObject::is_empty")]
    #[schema(value_type = Object)]
    pub settings: JsonObject,
}

/// Freigabe von MCP-Tools für einen API-Key oder ein Team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpGrantSpec {
    /// MCP-Server (Name).
    pub server: String,
    /// Empfänger der Freigabe (`level` = `team` oder `api-key`).
    pub subject: Scope,
    /// Freigegebene Tools; leer = alle Tools des Servers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
}
