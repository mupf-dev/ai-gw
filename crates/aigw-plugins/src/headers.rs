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
];

/// Darf ein Plugin den Header **nicht** an den Upstream senden?
pub fn is_forbidden_upstream(name: &HeaderName) -> bool {
    let n = name.as_str();
    FORBIDDEN_UPSTREAM.contains(&n) || FORBIDDEN_UPSTREAM_PREFIX.iter().any(|p| n.starts_with(p))
}

/// Ist der Antwort-Header vor Plugins geschützt?
pub fn is_protected_response(name: &HeaderName) -> bool {
    PROTECTED_RESPONSE.contains(&name.as_str())
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
    }
}
