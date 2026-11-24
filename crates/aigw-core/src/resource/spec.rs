// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

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
    /// Obergrenze für `payloadMode` auf dieser und allen untergeordneten Ebenen (S-09, F203;
    /// Stufen `off` < `metadata` < `masked` < `full`). Wird beim Schreiben geprüft und zur
    /// Laufzeit erzwungen (wirksamer Modus = Minimum aus Modus und allen Obergrenzen).
    /// Setzen dürfen sie nur `admin` (Organisation) bzw. `admin`/`org-admin` (Team).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_mode_max: Option<PayloadMode>,
    /// `true`: Untergeordnete Ebenen dürfen `payloadMode` nicht setzen; es gilt der Modus
    /// dieser Ebene (bzw. der darüber, sonst `observe.payload`), begrenzt durch die
    /// Obergrenzen (S-09).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub payload_mode_locked: bool,
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
    /// Obergrenze für `payloadMode` auf dieser und allen untergeordneten Ebenen (S-09, F203;
    /// Stufen `off` < `metadata` < `masked` < `full`). Wird beim Schreiben geprüft und zur
    /// Laufzeit erzwungen (wirksamer Modus = Minimum aus Modus und allen Obergrenzen).
    /// Setzen dürfen sie nur `admin` (Organisation) bzw. `admin`/`org-admin` (Team).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_mode_max: Option<PayloadMode>,
    /// `true`: Untergeordnete Ebenen dürfen `payloadMode` nicht setzen; es gilt der Modus
    /// dieser Ebene (bzw. der darüber, sonst `observe.payload`), begrenzt durch die
    /// Obergrenzen (S-09).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub payload_mode_locked: bool,
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
    /// Workload-/Agent-Identitäten, die sich statt mit dem Klartext-Key mit einem
    /// Access-Token (JWT) eines Ausstellers aus `auth.jwt.issuers` anmelden (AIGW-115). Jede
    /// Identität darf nur einem Key zugeordnet sein.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workload_identities: Vec<crate::workload::WorkloadIdentity>,
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
    /// Anmeldung über Microsoft Entra ID (AIGW-84; nur `azure-openai` und `anthropic`).
    /// Nicht geheim – Geheimnisse (`clientSecret`, `clientCertificate`) stehen in `data`.
    /// Deployments mit `credential: <name>` übernehmen sie (siehe [`EntraAuth::apply_to`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entra: Option<EntraAuth>,
    /// Felder, z. B. `apiKey`, `accessKeyId`/`secretAccessKey`, `serviceAccountJson`
    /// (mit `entra` und Managed/Workload Identity auch leer).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, SecretRef>,
}

/// Art der Entra-ID-Anmeldung (wie `settings.entraCredential`, AIGW-82/84).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum EntraCredentialKind {
    /// Managed Identity des Gateways (IMDS bzw. `IDENTITY_ENDPOINT`); optional `clientId`
    /// einer benutzerzugewiesenen Identität.
    ManagedIdentity,
    /// Workload Identity (AKS, föderiertes Token aus `AZURE_FEDERATED_TOKEN_FILE`).
    WorkloadIdentity,
    /// Service Principal mit Client-Secret (`data.clientSecret`).
    ClientSecret,
    /// Service Principal mit Zertifikat (`data.clientCertificate`, PEM).
    ClientCertificate,
}

impl EntraCredentialKind {
    /// Name wie in `settings.entraCredential`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManagedIdentity => "managedIdentity",
            Self::WorkloadIdentity => "workloadIdentity",
            Self::ClientSecret => "clientSecret",
            Self::ClientCertificate => "clientCertificate",
        }
    }

    /// Identität des Gateways selbst (ohne gespeichertes Geheimnis)?
    pub fn is_ambient(self) -> bool {
        matches!(self, Self::ManagedIdentity | Self::WorkloadIdentity)
    }
}

/// Entra-ID-Anmeldung einer [`ProviderCredentialSpec`] (AIGW-84). Enthält keine Geheimnisse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EntraAuth {
    /// Art der Anmeldung.
    pub credential: EntraCredentialKind,
    /// Tenant-ID (GUID oder Domäne; Pflicht für `clientSecret`/`clientCertificate`, falls
    /// nicht in `data.tenantId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Client-ID (Service Principal bzw. benutzerzugewiesene Managed Identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Authority (nur `https://host`, Sovereign Clouds; Standard
    /// `https://login.microsoftonline.com`; nicht bei `workloadIdentity`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authority: Option<String>,
    /// Scope (Standard `https://cognitiveservices.azure.com/.default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Zertifikatskette als `x5c` im Header der Client-Assertion mitsenden (AIGW-94; nur
    /// `clientCertificate`; nötig für Subject-Name/Issuer-Anmeldung, „SNI“).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_x5c: Option<bool>,
}

impl EntraAuth {
    /// Zuordnung zu den Deployment-Einstellungen (`settings.<schlüssel>`).
    pub fn settings(&self) -> Vec<(&'static str, &str)> {
        let mut out = vec![
            ("auth", "entraId"),
            ("entraCredential", self.credential.as_str()),
        ];
        let opt = [
            ("entraTenantId", &self.tenant_id),
            ("entraClientId", &self.client_id),
            ("entraAuthority", &self.authority),
            ("entraScope", &self.scope),
        ];
        out.extend(
            opt.into_iter()
                .filter_map(|(k, v)| v.as_deref().map(|v| (k, v))),
        );
        out
    }

    /// Übernimmt die Anmeldung in Deployment-Einstellungen: fehlende Schlüssel (`auth`,
    /// `entraCredential`, `entraTenantId`, `entraClientId`, `entraAuthority`, `entraScope`,
    /// `entraSendX5c`) werden gesetzt, eigene Einstellungen des Deployments haben Vorrang. Setzt das
    /// Deployment `auth` auf etwas anderes als `entraId`, bleibt alles unverändert.
    /// Gibt zurück, ob sich etwas geändert hat.
    pub fn apply_to(&self, settings: &mut JsonObject) -> bool {
        if settings
            .get("auth")
            .is_some_and(|v| v.as_str() != Some("entraId"))
        {
            return false;
        }
        let mut changed = false;
        for (k, v) in self.settings() {
            if !settings.contains_key(k) {
                settings.insert(k.to_owned(), serde_json::Value::String(v.to_owned()));
                changed = true;
            }
        }
        if let Some(b) = self.send_x5c
            && !settings.contains_key("entraSendX5c")
        {
            settings.insert("entraSendX5c".to_owned(), serde_json::Value::Bool(b));
            changed = true;
        }
        changed
    }
}

/// Zugangsdaten eines Anbieters (gelesene Sicht, **ohne** Werte).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialInfo {
    /// Anbieter.
    pub provider: ProviderKind,
    /// Entra-ID-Anmeldung (AIGW-84, nicht geheim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entra: Option<EntraAuth>,
    /// Namen der gespeicherten Felder.
    pub fields: Vec<String>,
    /// ID des Master-Keys, mit dem verschlüsselt wurde (Rotation).
    pub key_id: String,
}

/// Allgemeines Geheimnis (F149), z. B. HMAC-Schlüssel eines Webhooks, API-Key eines
/// Guardrail-Dienstes oder Token eines MCP-Servers. Werte werden mit dem Master-Key
/// verschlüsselt gespeichert; in YAML nur als `valueFrom`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecretSpec {
    /// Beschreibung (Zweck, Besitzer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Felder (mindestens eines), z. B. `value` (Standardfeld der Verweise) oder `apiKey`.
    pub data: BTreeMap<String, SecretRef>,
}

impl SecretSpec {
    /// Standardfeld, wenn ein Verweis kein Feld nennt.
    pub const DEFAULT_FIELD: &'static str = "value";
}

/// Geheimnis (gelesene Sicht, **ohne** Werte).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretInfo {
    /// Beschreibung.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    /// Kosten gleichmäßig verteilen (AIGW-126): das Deployment mit dem geringsten
    /// Budgetverbrauch in Prozent (ohne Budget: geringste Kosten), Auswahl per
    /// „power of two choices“ gegen Herdenverhalten.
    BudgetBalanced,
}

impl RoutingStrategy {
    /// Bezeichner in der Datenbank.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::LeastLatency => "least-latency",
            Self::LeastBusy => "least-busy",
            Self::Cost => "cost",
            Self::BudgetBalanced => "budget-balanced",
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

/// Art von Anfragen, die eine Modellgruppe annimmt (F123, AIGW-21).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum ModelCapability {
    /// Chat Completions, Messages, Responses.
    Chat,
    /// Embeddings.
    Embeddings,
    /// Bilderzeugung.
    Images,
    /// Transkription und Übersetzung.
    AudioTranscription,
    /// Sprachausgabe.
    AudioSpeech,
}

impl ModelCapability {
    /// Bezeichner.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Embeddings => "embeddings",
            Self::Images => "images",
            Self::AudioTranscription => "audio-transcription",
            Self::AudioSpeech => "audio-speech",
        }
    }
}

/// Modellgruppe: öffentlicher Modellname mit Deployments dahinter.
///
/// Abwärtskompatibilität (F123): `settings.capabilities` (Form vor dem typisierten Feld) wird
/// beim Lesen in `capabilities` übernommen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "RawModelGroupSpec"
)]
pub struct ModelGroupSpec {
    /// Weitere Namen, unter denen Clients die Gruppe ansprechen.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Arten von Anfragen, die die Gruppe annimmt (F123); leer = nur `chat`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<ModelCapability>,
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

/// Eingabeform von [`ModelGroupSpec`] (vor der Übernahme von `settings.capabilities`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawModelGroupSpec {
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    capabilities: Vec<ModelCapability>,
    #[serde(default)]
    routing: RoutingStrategy,
    #[serde(default)]
    fallbacks: Vec<Fallback>,
    #[serde(default)]
    limits: Vec<LimitSpec>,
    #[serde(default)]
    settings: JsonObject,
}

impl TryFrom<RawModelGroupSpec> for ModelGroupSpec {
    type Error = String;

    fn try_from(r: RawModelGroupSpec) -> Result<Self, String> {
        let mut spec = Self {
            aliases: r.aliases,
            capabilities: r.capabilities,
            routing: r.routing,
            fallbacks: r.fallbacks,
            limits: r.limits,
            settings: r.settings,
        };
        spec.lift_legacy_capabilities()?;
        Ok(spec)
    }
}

impl ModelGroupSpec {
    /// Übernimmt `settings.capabilities` (Form vor F123) in `capabilities` und entfernt den
    /// Schlüssel. Fehler bei ungültigem Wert oder Widerspruch zum Feld; der Schlüssel bleibt
    /// dann in `settings`.
    pub fn lift_legacy_capabilities(&mut self) -> Result<(), String> {
        let Some(v) = self.settings.get("capabilities") else {
            return Ok(());
        };
        let mut list: Vec<ModelCapability> = serde_json::from_value(v.clone()).map_err(|e| {
            format!("settings.capabilities: ungültig ({e}); bitte das Feld capabilities verwenden")
        })?;
        if list.is_empty() {
            return Err("settings.capabilities darf nicht leer sein".into());
        }
        list.sort();
        list.dedup();
        let mut cur = self.capabilities.clone();
        cur.sort();
        cur.dedup();
        if !cur.is_empty() && cur != list {
            return Err(
                "capabilities steht abweichend in settings.capabilities und capabilities – nur \
                 capabilities verwenden"
                    .into(),
            );
        }
        self.capabilities = list;
        self.settings.remove("capabilities");
        Ok(())
    }

    /// Wirksame Fähigkeiten (leer = nur `chat`).
    pub fn effective_capabilities(&self) -> Vec<ModelCapability> {
        if self.capabilities.is_empty() {
            vec![ModelCapability::Chat]
        } else {
            self.capabilities.clone()
        }
    }
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
    /// Budget dieses Deployments (AIGW-126), z. B. das Guthaben des Anbieter-Kontos. Ist es
    /// erschöpft, überspringt das Routing das Deployment (kein Fehler, solange ein anderes
    /// verfügbar ist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<DeploymentBudget>,
}

/// Budget eines Deployments (AIGW-126). Beträge in `billing.currency`; die Kosten kommen aus
/// der Preisliste (tatsächliche Usage nach der Antwort).
///
/// Wirkung im Routing: Ein Deployment, dessen Verbrauch `switchAtPercent` des Betrags
/// erreicht hat, wird bei der Auswahl übersprungen; `strict` reserviert zusätzlich die
/// geschätzten Kosten je Versuch und überspringt, wenn sie nicht mehr in den Rest passen.
/// Sind alle Deployments der Gruppe erschöpft, antwortet der Proxy mit `429`
/// (`deployment_budget_exhausted`). Nachladen: `amount` erhöhen (Hot Reload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploymentBudget {
    /// Betrag je Zeitraum in der Abrechnungswährung (`billing.currency`).
    pub amount: Amount,
    /// Zeitraum (`total` = Guthaben ohne Zurücksetzen, `month` = Kalendermonat in
    /// `billing.timezone`).
    pub period: BudgetPeriod,
    /// Warnschwellen in Prozent (Ereignis/Webhook `budget.threshold`), z. B. `[80, 95]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warn_at_percent: Vec<u8>,
    /// Umschalten ab diesem Verbrauch in Prozent (1–100, Standard 100): Reserve, damit der
    /// letzte Request das Guthaben nicht überzieht.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_at_percent: Option<u8>,
    /// `strict` (Standard): geschätzte Kosten je Versuch reservieren; `soft`: nur den
    /// bisherigen Verbrauch prüfen.
    #[serde(default, skip_serializing_if = "is_default")]
    pub enforcement: BudgetEnforcement,
}

impl DeploymentBudget {
    /// Prozent, ab dem umgeschaltet wird (1–100).
    pub fn switch_percent(&self) -> u8 {
        self.switch_at_percent.unwrap_or(100).clamp(1, 100)
    }

    /// Als [`Budget`] (Betrag, Warnschwellen; harte Grenze = Überspringen im Routing).
    pub fn as_budget(&self) -> Budget {
        Budget {
            amount: self.amount,
            period: self.period,
            warn_at_percent: self.warn_at_percent.clone(),
            hard_limit: true,
            enforcement: self.enforcement,
        }
    }

    /// Budget für die Auswahl im Routing: Betrag × `switchAtPercent` / 100.
    pub fn switch_budget(&self) -> Budget {
        let mut b = self.as_budget();
        let p = i128::from(self.switch_percent());
        let n = i128::from(self.amount.nanos()) * p / 100;
        b.amount = Amount::from_nanos(i64::try_from(n).unwrap_or(i64::MAX));
        b.warn_at_percent.clear();
        b
    }
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
    /// Preis je erzeugtem Bild (`/v1/images/generations`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_image: Option<Amount>,
    /// Preis je Minute Audio-Eingabe (Transkription/Übersetzung, z. B. Whisper); anteilig
    /// je Millisekunde abgerechnet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_audio_minute: Option<Amount>,
    /// Preis je 1 Mio. Zeichen Text-Eingabe einer Sprachausgabe (`/v1/audio/speech`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_million_characters: Option<Amount>,
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

/// Verhältnis einer Plugin-Bindung zu Bindungen desselben Plugins auf äußeren Ebenen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PluginInherit {
    /// Zusätzlich ausführen (Standard).
    #[default]
    Append,
    /// Geerbte Bindungen desselben Plugins ersetzen.
    Override,
    /// Geerbte Bindungen desselben Plugins abschalten (die Bindung selbst läuft nicht).
    Disable,
}

impl PluginInherit {
    /// Bezeichner.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Override => "override",
            Self::Disable => "disable",
        }
    }
}

/// Verhalten bei Fehlern eines Plugins.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum PluginFailMode {
    /// Fehler protokollieren, Plugin überspringen (Anfrage läuft weiter).
    Open,
    /// Anfrage ablehnen (`500 plugin_error`) bzw. Stream mit Fehler beenden.
    Closed,
}

impl PluginFailMode {
    /// Bezeichner.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

/// Wirkung einer Guardrail-Ablehnung **der Ausgabe** (F160). Eingabe-Ablehnungen sind immer
/// Fehler (`400`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OutputRejectMode {
    /// Wie Azure OpenAI: `200`, Inhalt entfernt, `finish_reason: content_filter` (OpenAI) bzw.
    /// `stop_reason: refusal` (Anthropic); im Stream sauberer Abschluss statt Fehlerereignis.
    #[default]
    Finish,
    /// Fehlerantwort (`400`) bzw. Fehlerereignis im Stream.
    Error,
}

impl OutputRejectMode {
    /// Bezeichner.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Finish => "finish",
            Self::Error => "error",
        }
    }
}

/// Plugin-Bindung: ein Plugin mit Konfiguration auf einer Ebene.
///
/// Die Kettensteuerung (`inherit`, `locked`, `failMode`, `timeoutMs`, `outputRejectMode`)
/// steht in eigenen Feldern (F117). Abwärtskompatibilität: Stehen diese Schlüssel (wie vor
/// F117) in `config`, werden sie beim Lesen in die Felder übernommen; steht derselbe Schlüssel
/// in beiden mit verschiedenen Werten, ist die Eingabe ungültig.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "RawPluginBindingSpec"
)]
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
    /// Verhältnis zu Bindungen desselben Plugins auf äußeren Ebenen (Standard `append`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit: Option<PluginInherit>,
    /// Innere Ebenen dürfen diese Bindung weder ersetzen noch abschalten.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    /// Fehlermodus (Standard je Plugin-Art, F148).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_mode: Option<PluginFailMode>,
    /// Zeitlimit asynchroner Haken in Millisekunden (1–60000; Standard je Plugin-Art).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Wirkung einer Ausgabe-Ablehnung (Standard `finish`, F160).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_reject_mode: Option<OutputRejectMode>,
    /// Plugin-Konfiguration (Schema je Plugin).
    #[serde(default, skip_serializing_if = "JsonObject::is_empty")]
    #[schema(value_type = Object)]
    pub config: JsonObject,
}

/// Schlüssel der Kettensteuerung, die vor F117 in `config` standen.
pub const PLUGIN_CONTROL_KEYS: [&str; 5] = [
    "inherit",
    "locked",
    "failMode",
    "timeoutMs",
    "outputRejectMode",
];

/// Eingabeform von [`PluginBindingSpec`] (vor der Übernahme alter Steuerschlüssel).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawPluginBindingSpec {
    scope: Scope,
    plugin: String,
    #[serde(default)]
    order: i32,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    inherit: Option<PluginInherit>,
    #[serde(default)]
    locked: bool,
    #[serde(default)]
    fail_mode: Option<PluginFailMode>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    output_reject_mode: Option<OutputRejectMode>,
    #[serde(default)]
    config: JsonObject,
}

impl TryFrom<RawPluginBindingSpec> for PluginBindingSpec {
    type Error = String;

    fn try_from(r: RawPluginBindingSpec) -> Result<Self, String> {
        let mut spec = Self {
            scope: r.scope,
            plugin: r.plugin,
            order: r.order,
            enabled: r.enabled,
            inherit: r.inherit,
            locked: r.locked,
            fail_mode: r.fail_mode,
            timeout_ms: r.timeout_ms,
            output_reject_mode: r.output_reject_mode,
            config: r.config,
        };
        spec.lift_legacy_control()?;
        Ok(spec)
    }
}

impl PluginBindingSpec {
    /// Übernimmt Steuerschlüssel aus `config` (Form vor F117) in die eigenen Felder und
    /// entfernt sie aus `config`. Fehler bei ungültigem Wert oder Widerspruch zu einem
    /// gesetzten Feld; der betroffene Schlüssel bleibt dann in `config` (die Plugin-Kette
    /// behandelt die Bindung als ungültig).
    pub fn lift_legacy_control(&mut self) -> Result<(), String> {
        fn take<T: serde::de::DeserializeOwned + PartialEq>(
            config: &mut JsonObject,
            key: &str,
            field: &mut Option<T>,
        ) -> Result<(), String> {
            let Some(v) = config.get(key) else {
                return Ok(());
            };
            let parsed: T = serde_json::from_value(v.clone())
                .map_err(|e| format!("config.{key}: ungültiger Wert ({e})"))?;
            if field.as_ref().is_some_and(|cur| *cur != parsed) {
                return Err(format!(
                    "{key} steht abweichend in `config.{key}` und `{key}` – nur `{key}` verwenden"
                ));
            }
            *field = Some(parsed);
            config.remove(key);
            Ok(())
        }
        let mut locked = self.locked.then_some(true);
        let results = [
            take(&mut self.config, "inherit", &mut self.inherit),
            take(&mut self.config, "failMode", &mut self.fail_mode),
            take(&mut self.config, "timeoutMs", &mut self.timeout_ms),
            take(
                &mut self.config,
                "outputRejectMode",
                &mut self.output_reject_mode,
            ),
            // `locked: false` in `config` ohne gesetztes Feld ist kein Widerspruch.
            if self.config.get("locked") == Some(&serde_json::Value::Bool(false)) {
                self.config.remove("locked");
                Ok(())
            } else {
                take(&mut self.config, "locked", &mut locked)
            },
        ];
        self.locked = locked.unwrap_or(false);
        let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Gesetzte Steuerfelder als JSON-Objekt (camelCase-Schlüssel wie früher in `config`).
    pub fn control_entries(&self) -> JsonObject {
        let mut m = JsonObject::new();
        let s = |v: &str| serde_json::Value::String(v.to_owned());
        if let Some(i) = self.inherit {
            m.insert("inherit".into(), s(i.as_str()));
        }
        if self.locked {
            m.insert("locked".into(), serde_json::Value::Bool(true));
        }
        if let Some(f) = self.fail_mode {
            m.insert("failMode".into(), s(f.as_str()));
        }
        if let Some(t) = self.timeout_ms {
            m.insert("timeoutMs".into(), serde_json::Value::from(t));
        }
        if let Some(o) = self.output_reject_mode {
            m.insert("outputRejectMode".into(), s(o.as_str()));
        }
        m
    }

    /// `config` zusammen mit den Steuerfeldern (Felder gewinnen) – die Form, die die
    /// Plugin-Kette auswertet.
    pub fn merged_config(&self) -> JsonObject {
        let mut m = self.config.clone();
        m.extend(self.control_entries());
        m
    }
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
    /// Zugangsdaten (Name einer `ProviderCredential`; älterer Weg, bevorzugt `secret`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// Geheimnis (Name eines `Secret`, F149); höchstens eines von `credential`/`secret`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding(v: serde_json::Value) -> Result<PluginBindingSpec, serde_json::Error> {
        serde_json::from_value(v)
    }

    #[test]
    fn plugin_control_fields_and_legacy_keys() {
        // Neue Form (F117).
        let b = binding(json!({"scope": {"level": "global"}, "plugin": "pii",
            "failMode": "open", "timeoutMs": 150, "locked": true,
            "outputRejectMode": "error", "inherit": "override", "config": {"mode": "mask"}}))
        .unwrap();
        assert_eq!(b.fail_mode, Some(PluginFailMode::Open));
        assert_eq!(b.timeout_ms, Some(150));
        assert!(b.locked);
        assert_eq!(b.inherit, Some(PluginInherit::Override));
        assert_eq!(b.output_reject_mode, Some(OutputRejectMode::Error));
        // Alte Form: Schlüssel in config werden übernommen und entfernt.
        let legacy = binding(json!({"scope": {"level": "global"}, "plugin": "pii",
            "config": {"mode": "mask", "failMode": "open", "timeoutMs": 150, "locked": true,
                "outputRejectMode": "error", "inherit": "override"}}))
        .unwrap();
        assert_eq!(legacy, b);
        // Rundreise: Steuerfelder auf oberster Ebene.
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["failMode"], "open");
        assert!(v["config"].get("failMode").is_none());
        // Widerspruch und ungültige Werte.
        assert!(
            binding(json!({"scope": {"level": "global"}, "plugin": "pii",
                "failMode": "closed", "config": {"failMode": "open"}}))
            .is_err()
        );
        assert!(
            binding(json!({"scope": {"level": "global"}, "plugin": "pii",
                "config": {"failMode": "sometimes"}}))
            .is_err()
        );
        // Gleicher Wert doppelt und `locked: false` sind kein Widerspruch.
        let same = binding(json!({"scope": {"level": "global"}, "plugin": "pii",
            "failMode": "open", "config": {"failMode": "open", "locked": false}}))
        .unwrap();
        assert!(same.config.is_empty());
        assert!(!same.locked);
        assert_eq!(
            serde_json::Value::Object(b.merged_config()),
            json!({"mode": "mask", "failMode": "open", "timeoutMs": 150, "locked": true,
                "outputRejectMode": "error", "inherit": "override"})
        );
    }

    #[test]
    fn model_group_capabilities_typed_and_legacy() {
        let g = |v: serde_json::Value| serde_json::from_value::<ModelGroupSpec>(v);
        let new = g(json!({"capabilities": ["embeddings", "images"]})).unwrap();
        assert_eq!(
            new.capabilities,
            [ModelCapability::Embeddings, ModelCapability::Images]
        );
        // Alte Form in settings → Feld (sortiert), Schlüssel entfernt.
        let old = g(
            json!({"settings": {"capabilities": ["images", "embeddings"],
            "retry": {"maxAttempts": 2}}}),
        )
        .unwrap();
        assert_eq!(old.capabilities, new.capabilities);
        assert!(!old.settings.contains_key("capabilities"));
        assert!(old.settings.contains_key("retry"));
        // Ungültig, leer, Widerspruch.
        assert!(g(json!({"capabilities": ["video"]})).is_err());
        assert!(g(json!({"settings": {"capabilities": ["video"]}})).is_err());
        assert!(g(json!({"settings": {"capabilities": []}})).is_err());
        assert!(
            g(json!({"capabilities": ["chat"], "settings": {"capabilities": ["images"]}})).is_err()
        );
        // Standard: nur Chat.
        assert_eq!(
            ModelGroupSpec::default().effective_capabilities(),
            [ModelCapability::Chat]
        );
        assert_eq!(
            serde_json::to_value(&new).unwrap()["capabilities"],
            json!(["embeddings", "images"])
        );
    }

    #[test]
    fn lenient_lift_keeps_invalid_key() {
        let mut b = PluginBindingSpec {
            scope: Scope::global(),
            plugin: "pii".into(),
            order: 0,
            enabled: true,
            inherit: None,
            locked: false,
            fail_mode: None,
            timeout_ms: None,
            output_reject_mode: None,
            config: json!({"failMode": "closed", "timeoutMs": "x"})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        };
        assert!(b.lift_legacy_control().is_err());
        assert_eq!(b.fail_mode, Some(PluginFailMode::Closed));
        assert!(b.config.contains_key("timeoutMs"));
    }
}
