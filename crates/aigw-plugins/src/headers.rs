// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Header-Regeln für Plugins: welche Upstream-Header gesperrt und welche Antwort-Header
//! geschützt sind.

use http::HeaderName;

/// Header, die Plugins nie an den Upstream setzen oder vom Client durchreichen dürfen:
/// Zugangsdaten (Gateway-Key des Clients bzw. Anbieter-Key des Gateways), Signaturen,
/// Hop-by-Hop- und Rahmen-Header.
const FORBIDDEN_UPSTREAM: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "api-key",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
    // AIGW-58: Herkunftsangaben nicht fälschbar
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "host",
    "content-length",
    "content-type",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "expect",
    "anthropic-version",
    // W3C-Trace-Kontext setzt der Upstream-Client selbst (AIGW-45).
    "traceparent",
    "tracestate",
];

/// Präfixe gesperrter Upstream-Header (AWS-Signatur, Proxy-Header).
const FORBIDDEN_UPSTREAM_PREFIX: &[&str] = &["x-amz-", "proxy-", "x-aigw-"];

/// Antwort-Header, die Plugins weder setzen noch entfernen dürfen.
const PROTECTED_RESPONSE: &[&str] = &[
    "content-type",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "x-aigw-request-id",
    // AIGW-58: Sicherheits- und Limit-Header des Gateways nicht fälschbar
    "set-cookie",
    "strict-transport-security",
    "content-security-policy",
    "x-frame-options",
    "x-content-type-options",
    "retry-after",
];

/// Präfixe geschützter Antwort-Header (CORS, Limit-Angaben des Gateways; AIGW-58).
const PROTECTED_RESPONSE_PREFIX: &[&str] = &["access-control-", "x-ratelimit-", "ratelimit"];

/// Darf ein Plugin den Header **nicht** an den Upstream senden?
pub fn is_forbidden_upstream(name: &HeaderName) -> bool {
    let n = name.as_str();
    FORBIDDEN_UPSTREAM.contains(&n) || FORBIDDEN_UPSTREAM_PREFIX.iter().any(|p| n.starts_with(p))
}

/// Ist der Antwort-Header vor Plugins geschützt?
pub fn is_protected_response(name: &HeaderName) -> bool {
    let n = name.as_str();
    PROTECTED_RESPONSE.contains(&n) || PROTECTED_RESPONSE_PREFIX.iter().any(|p| n.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbidden_and_protected() {
        for h in [
            "authorization",
            "x-api-key",
            "x-amz-date",
            "host",
            "x-aigw-foo",
            "traceparent",
            "tracestate",
        ] {
            assert!(is_forbidden_upstream(&HeaderName::from_static(h)), "{h}");
        }
        assert!(!is_forbidden_upstream(&HeaderName::from_static(
            "x-request-source"
        )));
        assert!(is_protected_response(&HeaderName::from_static(
            "x-aigw-request-id"
        )));
        assert!(!is_protected_response(&HeaderName::from_static(
            "x-aigw-deployment"
        )));
        // AIGW-58
        for h in [
            "set-cookie",
            "access-control-allow-origin",
            "x-ratelimit-remaining-requests",
            "retry-after",
            "strict-transport-security",
        ] {
            assert!(is_protected_response(&HeaderName::from_static(h)), "{h}");
        }
        for h in ["forwarded", "x-forwarded-for", "x-real-ip"] {
            assert!(is_forbidden_upstream(&HeaderName::from_static(h)), "{h}");
        }
    }
}
