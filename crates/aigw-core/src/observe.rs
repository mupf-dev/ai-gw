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
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::ir::{ErrorClass, ErrorKind, Usage};
use crate::resource::{Amount, Id};
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

/// `true`, wenn der Header nie geloggt werden darf.
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(name))
}

/// Umfang des Payload-Loggings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

impl CacheOutcome {
    /// `true` bei einem Treffer.
    pub fn is_hit(self) -> bool {
        !matches!(self, Self::Miss)
    }

    /// Bezeichner (Spalte `request_log.cache`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HitExact => "hit-exact",
            Self::HitSemantic => "hit-semantic",
            Self::Miss => "miss",
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
    /// Roh-Payload; nur gesetzt, wenn [`RequestObserver::payload_mode`] es verlangt.
    pub payload: Option<Box<RawPayload>>,
    /// Alle Versuche in Reihenfolge (leer, wenn kein Deployment angefragt wurde). Der letzte
    /// Versuch ist der, dessen Ergebnis der Client erhielt.
    pub attempts: Vec<Attempt>,
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
            payload: None,
            attempts: Vec::new(),
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

    /// Ob (und wie) der Proxy den Payload mitgeben soll. Bei [`PayloadMode::Off`] lässt der
    /// Proxy [`RequestRecord::payload`] leer und spart das Mitschneiden der Antwort.
    ///
    /// Später je Key/Team (Parameter folgen mit der Policy-Auflösung).
    fn payload_mode(&self) -> PayloadMode {
        PayloadMode::Off
    }
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
