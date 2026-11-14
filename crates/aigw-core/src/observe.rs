// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Beobachtung: Kennzahlen einer abgeschlossenen Anfrage ([`RequestRecord`]) und der Hook
//! [`RequestObserver`], über den der Proxy sie am Ende jeder Anfrage abgibt (AIGW-43).
//!
//! Der Proxy ruft [`RequestObserver::record`] genau einmal je Anfrage auf – nach dem letzten
//! Byte an den Client bzw. nach dem Abbruch. Implementierungen dürfen dabei **nie** blockieren
//! oder auf I/O warten (Ziel < 1 µs); Verarbeitung, Maskierung und Schreiben passieren
//! entkoppelt (siehe Crate `aigw-observe`, `docs/konzept/07-beobachtung.md`).

use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::ir::{ErrorClass, ErrorKind, Usage};
use crate::resource::{Amount, Id};
use crate::snapshot::KeyCtx;
use crate::types::ProviderKind;
use crate::wire::WireFormat;

/// Header, deren Werte nie geloggt werden (auch nicht im Modus `full`).
///
/// Vergleich ohne Beachtung der Groß-/Kleinschreibung.
pub const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "api-key",
    "x-api-key",
    "cookie",
    "set-cookie",
];

/// Namensbestandteile, die auf Zugangsdaten hindeuten (AIGW-58: z. B. `x-goog-api-key`,
/// `x-amz-security-token`, `x-csrf-token`, `x-aigw-signature`). Vergleich klein geschrieben.
const SENSITIVE_HEADER_PARTS: &[&str] = &[
    "auth",
    "token",
    "secret",
    "password",
    "passwd",
    "api-key",
    "apikey",
    "api_key",
    "signature",
    "credential",
    "session",
    "cookie",
    "subscription-key",
];

/// `true`, wenn der Header nie geloggt werden darf: feste Liste [`SENSITIVE_HEADERS`] plus
/// Namen, die einen Bestandteil aus `SENSITIVE_HEADER_PARTS` enthalten.
pub fn is_sensitive_header(name: &str) -> bool {
    if SENSITIVE_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
    {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    SENSITIVE_HEADER_PARTS.iter().any(|p| lower.contains(p))
}

/// Umfang des Payload-Loggings.
///
/// Die Stufen sind geordnet (`off` < `metadata` < `masked` < `full`, S-09): `payloadModeMax`
/// begrenzt mit [`Ord::min`].
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum PayloadMode {
    /// Kein Payload (Standard); nur die Kennzahlen.
    #[default]
    Off,
    /// Nur Metadaten des Payloads: Größen, Header-Namen, Content-Type – kein Inhalt.
    Metadata,
    /// Inhalt, maskiert (E-Mail, Kreditkarte, IBAN …; später PII-Engine AIGW-37).
    Masked,
    /// Inhalt unmaskiert (nur Header aus [`SENSITIVE_HEADERS`] werden entfernt).
    Full,
}

impl PayloadMode {
    /// `true`, wenn der Proxy Request/Response überhaupt mitgeben soll.
    pub fn captures(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Bezeichner wie in der Konfiguration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Metadata => "metadata",
            Self::Masked => "masked",
            Self::Full => "full",
        }
    }
}

/// Ergebnis des Antwort-Caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheOutcome {
    /// Exakter Treffer.
    HitExact,
    /// Semantischer Treffer.
    HitSemantic,
    /// Kein Treffer.
    Miss,
    /// Cache für diese Anfrage umgangen (Client-Steuerung `Cache-Control: no-cache`, nicht
    /// cachebare Anfrage, z. B. `temperature > 0`).
    Bypass,
    /// Kein Lookup, sondern der Embedding-Aufruf des semantischen Caches selbst – eigener
    /// Eintrag, dem anfragenden Key berechnet (F169).
    #[serde(rename = "cache-embedding")]
    Embedding,
}

impl CacheOutcome {
    /// `true` bei einem Treffer.
    pub fn is_hit(self) -> bool {
        matches!(self, Self::HitExact | Self::HitSemantic)
    }

    /// Bezeichner (Spalte `request_log.cache`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HitExact => "hit-exact",
            Self::HitSemantic => "hit-semantic",
            Self::Miss => "miss",
            Self::Bypass => "bypass",
            Self::Embedding => "cache-embedding",
        }
    }
}

/// Roh-Payload einer Anfrage, wie der Proxy ihn ohnehin vorliegen hat.
///
/// `Bytes` sind referenzgezählt: Das Mitgeben kostet im Hot Path keine Kopie. Maskierung,
/// Kürzung und das Entfernen sensibler Header passieren erst im Writer.
#[derive(Debug, Clone, Default)]
pub struct RawPayload {
    /// Request-Header (Name, Wert). Sensible Header dürfen enthalten sein; sie werden nie
    /// gespeichert.
    pub request_headers: Vec<(String, String)>,
    /// Request-Body (wie vom Client gesendet).
    pub request_body: Bytes,
    /// Antwort-Body an den Client (bei Streams die zusammengesetzten Events bzw. der Text).
    pub response_body: Bytes,
    /// Wirksamer Modus dieser Anfrage (F30: je Key/Team/Organisation, sonst global).
    pub mode: PayloadMode,
}

/// Ein Versuch gegen ein Deployment (Retries und Fallbacks, AIGW-25).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// Modellgruppe des Deployments (bei Fallbacks die Ziel-Gruppe).
    pub model_group: Option<String>,
    /// Name des Deployments.
    pub deployment: String,
    /// ID des Deployments (`None` bei programmatisch gebauten Routen).
    pub deployment_id: Option<Id>,
    /// Fehlerklasse; `None` = erfolgreich (bis zum ersten Ereignis bzw. zur Antwort).
    pub error: Option<ErrorClass>,
    /// HTTP-Status des Anbieters bei Fehlern (falls vorhanden).
    pub upstream_status: Option<u16>,
    /// Dauer bis zur Antwort, zum ersten Stream-Ereignis bzw. zum Fehler.
    pub duration: Duration,
}

/// Erkennungen eines Guardrail-Plugins (AIGW-37/38) – nur Zähler, **nie** Inhalte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardrailHit {
    /// Plugin (`pii`, `prompt-filter`, `content-safety`).
    pub plugin: &'static str,
    /// Kategorie (z. B. `email`, `injection`, `hate`).
    pub category: String,
    /// Ausgeführte Aktion (`mask`, `redact`, `reject`, `pseudonymize`, `log-only`, `flag`).
    pub action: &'static str,
    /// Anzahl der Erkennungen.
    pub count: u32,
}

/// Sammelstelle für Guardrail-Erkennungen einer Anfrage; Plugins (auch im Stream) und
/// Recorder teilen sich einen Klon. Wird nur angelegt, wenn die Anfrage Plugins hat.
#[derive(Debug, Clone, Default)]
pub struct GuardrailHits(Arc<Mutex<Vec<GuardrailHit>>>);

impl GuardrailHits {
    /// Zählt `count` Erkennungen (gleiche Plugin/Kategorie/Aktion werden zusammengefasst).
    pub fn add(&self, plugin: &'static str, category: &str, action: &'static str, count: u32) {
        if count == 0 {
            return;
        }
        let mut v = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match v
            .iter_mut()
            .find(|h| h.plugin == plugin && h.action == action && h.category == category)
        {
            Some(h) => h.count = h.count.saturating_add(count),
            None => v.push(GuardrailHit {
                plugin,
                category: category.to_owned(),
                action,
                count,
            }),
        }
    }

    /// Bisherige Erkennungen (Kopie).
    pub fn snapshot(&self) -> Vec<GuardrailHit> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Entnimmt alle Erkennungen.
    pub fn take(&self) -> Vec<GuardrailHit> {
        std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// Modellaufruf eines Plugins im Auftrag einer Anfrage (z. B. Prüfmodell von
/// `content-safety`, AIGW-89/S-13). Wird dem Key/Team der Anfrage zugerechnet.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginCall {
    /// Angefragte Modellgruppe.
    pub model_group: String,
    /// Anbieter des Deployments, das geantwortet hat (Preisfindung).
    pub provider: Option<ProviderKind>,
    /// Modell beim Anbieter (Preisfindung).
    pub provider_model: Option<String>,
    /// Verbrauch laut Antwort.
    pub usage: Usage,
}

#[derive(Debug, Default)]
struct PluginCallsInner {
    calls: Vec<PluginCall>,
    /// Bisherige Prüfungen je Plugin-Instanz (Obergrenze je Anfrage).
    checks: Vec<(usize, u32)>,
}

/// Sammelstelle für Plugin-Modellaufrufe einer Anfrage und Zähler für die Obergrenze der
/// Prüfungen je Anfrage (AIGW-89/S-13). Plugins, Gateway-Host und Recorder teilen sich einen
/// Klon; der Recorder rechnet die Aufrufe am Ende dem Key/Team zu (Kosten, Budget,
/// Token-Limits).
#[derive(Debug, Clone, Default)]
pub struct PluginCalls(Arc<Mutex<PluginCallsInner>>);

impl PluginCalls {
    fn lock(&self) -> std::sync::MutexGuard<'_, PluginCallsInner> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Bucht einen Modellaufruf.
    pub fn record(&self, call: PluginCall) {
        self.lock().calls.push(call);
    }

    /// Reserviert `n` Prüfungen für die Plugin-Instanz `owner` (z. B. Adresse ihrer
    /// Konfiguration); `false`, wenn damit mehr als `max` Prüfungen in dieser Anfrage liefen
    /// (dann wird nichts gebucht).
    pub fn try_reserve(&self, owner: usize, n: u32, max: u32) -> bool {
        let mut g = self.lock();
        let i = match g.checks.iter().position(|(o, _)| *o == owner) {
            Some(i) => i,
            None => {
                g.checks.push((owner, 0));
                g.checks.len() - 1
            }
        };
        let used = &mut g.checks[i].1;
        match used.checked_add(n) {
            Some(total) if total <= max => {
                *used = total;
                true
            }
            _ => false,
        }
    }

    /// Bisher reservierte Prüfungen der Instanz `owner`.
    pub fn checks(&self, owner: usize) -> u32 {
        self.lock()
            .checks
            .iter()
            .find(|(o, _)| *o == owner)
            .map_or(0, |(_, n)| *n)
    }

    /// Entnimmt alle gebuchten Aufrufe.
    pub fn take(&self) -> Vec<PluginCall> {
        std::mem::take(&mut self.lock().calls)
    }
}

/// Kennzahlen einer abgeschlossenen Anfrage (ein Eintrag im Request-Log).
///
/// Feldnamen abgestimmt mit dem Proxy-Hook (AIGW-14/15). Optionale Felder sind `None`, wenn
/// die Anfrage vorher endete (z. B. Auth-Fehler ohne Key, unbekanntes Modell ohne Deployment).
#[derive(Debug, Clone)]
pub struct RequestRecord {
    /// Request-ID (`x-aigw-request-id`, UUIDv7).
    pub request_id: String,
    /// Zeitpunkt der Annahme der Anfrage.
    pub started_at: SystemTime,
    /// Eingangsformat (Endpunkt des Clients).
    pub ingress: WireFormat,
    /// Vom Client angefordertes Modell (Alias, wie gesendet).
    pub model_alias: Option<String>,
    /// Aufgelöste Modellgruppe (Name).
    pub model_group: Option<String>,
    /// Gewähltes Deployment (nach Fallbacks das zuletzt verwendete).
    pub deployment_id: Option<Id>,
    /// Anbieter des Deployments.
    pub provider: Option<ProviderKind>,
    /// Modellname beim Anbieter.
    pub provider_model: Option<String>,
    /// HTTP-Status an den Client.
    pub status: u16,
    /// Fehlerklasse, falls die Anfrage fehlschlug.
    pub error_kind: Option<ErrorKind>,
    /// Feine Fehlerklasse (F47, z. B. `timeout`, `rate_limit`, `content_filter`); bei
    /// Upstream-Fehlern die des letzten Versuchs.
    pub error_class: Option<ErrorClass>,
    /// Verbrauch (vom Anbieter gemeldet oder geschätzt).
    pub usage: Usage,
    /// `true`, wenn `usage` geschätzt wurde (z. B. Client-Abbruch im Stream).
    pub usage_estimated: bool,
    /// Kosten (Platzhalter bis zur Kostenberechnung; `None` = unbekannt).
    pub cost: Option<Amount>,
    /// Zeit bis zum ersten Token (nur bei Erfolg).
    pub ttft: Option<Duration>,
    /// Gesamtdauer bis zum letzten Byte bzw. Abbruch.
    pub duration: Duration,
    /// Streaming-Anfrage.
    pub stream: bool,
    /// Client hat die Verbindung vorzeitig getrennt.
    pub client_aborted: bool,
    /// Ergebnis des Antwort-Caches (`None` = Cache nicht aktiv).
    pub cache: Option<CacheOutcome>,
    /// API-Key.
    pub api_key_id: Option<Id>,
    /// Team des Keys.
    pub team_id: Option<Id>,
    /// Organisation des Teams.
    pub organization_id: Option<Id>,
    /// Client-Adresse.
    pub client_ip: Option<IpAddr>,
    /// Vom Client mitgegebene Request-ID (`x-request-id` bzw. `x-client-request-id`), zur
    /// Korrelation im Log-Explorer (AIGW-46). Unverändert vom Client; der Writer kürzt und
    /// filtert sie.
    pub client_request_id: Option<String>,
    /// Roh-Payload; nur gesetzt, wenn [`RequestObserver::payload_mode`] es verlangt.
    pub payload: Option<Box<RawPayload>>,
    /// Trace-ID (32 Hex-Zeichen) des Wurzel-Spans, falls Tracing aktiv ist (F100): Verknüpfung
    /// Request-Log ↔ Trace.
    pub trace_id: Option<String>,
    /// Auslösende Anfrage (F191), z. B. die Chat-Anfrage, deren semantischer Cache-Lookup diesen
    /// Embedding-Aufruf ausgelöst hat.
    pub parent_request_id: Option<String>,
    /// Alle Versuche in Reihenfolge (leer, wenn kein Deployment angefragt wurde). Der letzte
    /// Versuch ist der, dessen Ergebnis der Client erhielt.
    pub attempts: Vec<Attempt>,
    /// Erkennungen der Guardrail-Plugins (Zähler je Plugin/Kategorie/Aktion, ohne Inhalte).
    pub guardrails: Vec<GuardrailHit>,
    /// MCP-Tool-Aufruf (nur bei `ingress` = [`WireFormat::Mcp`], AIGW-41).
    pub mcp: Option<Box<McpCall>>,
    /// Verbrauch der Modellaufrufe von Plugins (z. B. Prüfmodell von `content-safety`,
    /// AIGW-89/S-13): **nicht** in `usage` enthalten, aber auf die Token-Limits des Keys
    /// gebucht; deren Kosten sind in `cost` enthalten (siehe `plugin_cost`).
    pub plugin_usage: Usage,
    /// Anteil der Plugin-Modellaufrufe an `cost` (nur mit Preis).
    pub plugin_cost: Option<Amount>,
}

/// Ergebnis eines MCP-Tool-Aufrufs (Label `status` von `aigw_mcp_calls_total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOutcome {
    /// Erfolgreich (`isError` nicht gesetzt).
    Ok,
    /// Tool meldet einen Fehler (`isError: true`).
    ToolError,
    /// Tool oder Server unbekannt.
    NotFound,
    /// Nicht freigegeben.
    Denied,
    /// Limit oder Budget überschritten.
    RateLimited,
    /// Upstream nicht erreichbar oder Protokollfehler (auch JSON-RPC-Fehler des Upstreams).
    UpstreamError,
    /// Zeitlimit überschritten.
    Timeout,
    /// Vom Client abgebrochen (`notifications/cancelled` oder Verbindungsabbruch).
    Cancelled,
}

impl McpOutcome {
    /// Bezeichner (Label-Wert).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ToolError => "tool_error",
            Self::NotFound => "not_found",
            Self::Denied => "denied",
            Self::RateLimited => "rate_limited",
            Self::UpstreamError => "upstream_error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Kennzahlen eines MCP-Tool-Aufrufs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCall {
    /// MCP-Server (Name der Ressource).
    pub server: String,
    /// Tool-Name beim Upstream (ohne Namensraum).
    pub tool: String,
    /// Ergebnis.
    pub outcome: McpOutcome,
}

impl RequestRecord {
    /// Minimaler Eintrag (alles Optionale leer), z. B. für Tests oder frühe Fehler.
    pub fn new(request_id: impl Into<String>, ingress: WireFormat, status: u16) -> Self {
        Self {
            request_id: request_id.into(),
            started_at: SystemTime::now(),
            ingress,
            model_alias: None,
            model_group: None,
            deployment_id: None,
            provider: None,
            provider_model: None,
            status,
            error_kind: None,
            error_class: None,
            usage: Usage::default(),
            usage_estimated: false,
            cost: None,
            ttft: None,
            duration: Duration::ZERO,
            stream: false,
            client_aborted: false,
            cache: None,
            api_key_id: None,
            team_id: None,
            organization_id: None,
            client_ip: None,
            client_request_id: None,
            payload: None,
            trace_id: None,
            parent_request_id: None,
            attempts: Vec::new(),
            guardrails: Vec::new(),
            mcp: None,
            plugin_usage: Usage::default(),
            plugin_cost: None,
        }
    }

    /// `true`, wenn die Anfrage als Fehler zählt (Status ≥ 400 oder Fehlerklasse gesetzt).
    pub fn is_error(&self) -> bool {
        self.status >= 400 || self.error_kind.is_some()
    }
}

/// Hook am Ende jeder Anfrage.
///
/// `record` darf nicht blockieren und nicht warten (der Proxy wartet nie auf Logs); ist der
/// Puffer voll, wird verworfen und gezählt.
pub trait RequestObserver: Send + Sync + fmt::Debug + 'static {
    /// Eine Anfrage beginnt (für `aigw_inflight_requests`). Zu jedem Aufruf folgt genau ein
    /// [`record`](Self::record). Muss so billig sein wie ein atomares Inkrement.
    fn request_started(&self) {}

    /// Übergibt die Kennzahlen einer abgeschlossenen Anfrage.
    fn record(&self, record: RequestRecord);

    /// Ob (und wie) der Proxy den Payload für eine Anfrage dieses Keys mitgeben soll (F30:
    /// `payloadMode` an Key, Team bzw. Organisation, sonst der globale Standard; `None` = ohne
    /// Key). Bei [`PayloadMode::Off`] lässt der Proxy [`RequestRecord::payload`] leer und spart
    /// das Mitschneiden der Antwort. Aufgerufen nach der Schlüsselprüfung, einmal je Anfrage.
    fn payload_mode(&self, key: Option<&KeyCtx>) -> PayloadMode {
        let _ = key;
        PayloadMode::Off
    }
}

/// Wirksamer Payload-Modus: das spezifischste `payloadMode` (Key → Team → Organisation), sonst
/// `default` (F30), unter Beachtung von `payloadModeLocked` und `payloadModeMax` (S-09, siehe
/// [`KeyCtx::payload_mode`]). Ohne Key gilt `default`.
pub fn effective_payload_mode(key: Option<&KeyCtx>, default: PayloadMode) -> PayloadMode {
    key.map_or(default, |k| k.payload_mode(default))
}

/// `target` der `tracing`-Spans und -Events, die als OpenTelemetry-Traces exportiert werden
/// (AIGW-45). Proxy und Anbieter-Adapter instrumentieren nur über `tracing`; welche Spans
/// aufgezeichnet und exportiert werden, entscheidet die Crate `aigw-observe`.
///
/// Konventionen für Felder: `otel.name` (Span-Name), `otel.kind` (`server`/`client`/…),
/// `otel.status_code` (`ok`/`error`), `otel.status_message`, am Wurzel-Span
/// `otel.traceparent` (eingehender W3C-Header); alle anderen Felder werden Attribute
/// (GenAI-Semantic-Conventions, z. B. `gen_ai.request.model`).
pub const TRACE_TARGET: &str = "aigw::otel";

/// Quelle des `traceparent` für ausgehende Anfragen (gesetzt von `aigw-observe`).
static TRACEPARENT_SOURCE: OnceLock<fn() -> Option<String>> = OnceLock::new();

/// Quelle der Trace-ID des aktuellen Spans (gesetzt von `aigw-observe`, F100).
static TRACE_ID_SOURCE: OnceLock<fn() -> Option<String>> = OnceLock::new();

/// Registriert die Quelle für [`current_trace_id`] (einmal je Prozess; `false`, wenn bereits
/// gesetzt).
pub fn set_trace_id_source(source: fn() -> Option<String>) -> bool {
    TRACE_ID_SOURCE.set(source).is_ok()
}

/// Trace-ID (32 Hex-Zeichen) des aktuellen Spans – auch ohne Sampling und unabhängig von der
/// Weitergabe an Upstreams; `None`, wenn Tracing aus ist oder kein Span aktiv ist.
pub fn current_trace_id() -> Option<String> {
    TRACE_ID_SOURCE.get().and_then(|f| f())
}

/// Registriert die Quelle für [`current_traceparent`] (einmal je Prozess; `false`, wenn
/// bereits gesetzt).
pub fn set_traceparent_source(source: fn() -> Option<String>) -> bool {
    TRACEPARENT_SOURCE.set(source).is_ok()
}

/// W3C-`traceparent` des aktuellen Spans für eine ausgehende Anfrage (`None`, wenn Tracing
/// aus ist, kein Span aktiv ist oder die Weitergabe abgeschaltet ist).
pub fn current_traceparent() -> Option<String> {
    TRACEPARENT_SOURCE.get().and_then(|f| f())
}

/// Wert von `error.type` in Traces/Logs bzw. Label `class` (`snake_case` wie die
/// Serialisierung von [`ErrorKind`]).
pub fn error_type(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::InvalidRequest => "invalid_request",
        ErrorKind::Authentication => "authentication",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::NotFound => "not_found",
        ErrorKind::MethodNotAllowed => "method_not_allowed",
        ErrorKind::RequestTooLarge => "request_too_large",
        ErrorKind::RateLimit => "rate_limit",
        ErrorKind::Internal => "internal",
        ErrorKind::NotImplemented => "not_implemented",
        ErrorKind::BadGateway => "bad_gateway",
        ErrorKind::Overloaded => "overloaded",
        ErrorKind::Timeout => "timeout",
        #[allow(unreachable_patterns)]
        _ => "other",
    }
}

/// Wert von `gen_ai.system` (OpenTelemetry GenAI-Semantic-Conventions) für einen Anbieter;
/// Anbieter ohne festgelegten Wert behalten ihren Bezeichner.
pub fn gen_ai_system(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::OpenAi => "openai",
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::AzureOpenAi => "az.ai.openai",
        ProviderKind::Bedrock => "aws.bedrock",
        ProviderKind::Vertex => "gcp.vertex_ai",
        ProviderKind::Mistral => "mistral_ai",
        other => other.as_str(),
    }
}

/// Wert von `gen_ai.provider.name` (OpenTelemetry GenAI-Semantic-Conventions ab v1.37, löst
/// `gen_ai.system` ab; beide werden gesetzt, F101). Anbieter ohne festgelegten Wert behalten
/// ihren Bezeichner.
pub fn gen_ai_provider_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::AzureOpenAi => "azure.ai.openai",
        other => gen_ai_system(other),
    }
}

/// Beobachter, der alles verwirft (Standard ohne Request-Log).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopObserver;

impl RequestObserver for NoopObserver {
    fn record(&self, _record: RequestRecord) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_headers_case_insensitive() {
        assert!(is_sensitive_header("Authorization"));
        assert!(is_sensitive_header("X-API-KEY"));
        assert!(is_sensitive_header("api-key"));
        assert!(!is_sensitive_header("content-type"));
        // AIGW-58: Zugangsdaten anderer SDKs
        for h in [
            "x-goog-api-key",
            "X-Amz-Security-Token",
            "x-csrf-token",
            "x-aigw-signature",
            "x-collector-token",
            "Ocp-Apim-Subscription-Key",
        ] {
            assert!(is_sensitive_header(h), "{h}");
        }
        assert!(!is_sensitive_header("x-request-id"));
        assert!(!is_sensitive_header("user-agent"));
    }

    #[test]
    fn payload_mode_serde() {
        let m: PayloadMode = serde_json::from_str("\"masked\"").unwrap();
        assert_eq!(m, PayloadMode::Masked);
        assert!(m.captures());
        assert!(!PayloadMode::Off.captures());
        assert_eq!(PayloadMode::Metadata.as_str(), "metadata");
    }

    #[test]
    fn error_detection() {
        let mut r = RequestRecord::new("x", WireFormat::OpenAiChat, 200);
        assert!(!r.is_error());
        r.error_kind = Some(ErrorKind::Timeout);
        assert!(r.is_error());
        r = RequestRecord::new("x", WireFormat::OpenAiChat, 429);
        assert!(r.is_error());
    }
}
