// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Einheitliches Fehlerformat der Admin-API: Problem Details nach RFC 9457
//! (`application/problem+json`).
//!
//! `type` ist eine URN `urn:aigw:problem:<code>`, `code` derselbe Bezeichner als eigenes Feld
//! (bequemer in Clients), `instance` der angefragte Pfad. Interne Fehler geben keine Details
//! preis; die Ursache steht im Log.

use aigw_core::validate::bounds::Violation;
use aigw_store::StoreError;
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// Medientyp der Fehlerantworten.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Problem Details (RFC 9457).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Problem {
    /// Problemtyp (`urn:aigw:problem:<code>`).
    #[serde(rename = "type")]
    pub type_: String,
    /// Kurzbeschreibung des Problemtyps.
    pub title: &'static str,
    /// HTTP-Status.
    pub status: u16,
    /// Beschreibung dieses Falls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Betroffener Pfad.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Maschinenlesbarer Code (`not-found`, `conflict`, `precondition-failed` …).
    pub code: &'static str,
    /// Einzelne Grenzverstöße (AIGW-67: `422` beim Objekt selbst, `409` bei betroffenen
    /// bestehenden Keys/Teams); fehlt sonst. Geboxt, damit `Result<_, Problem>` klein bleibt.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<Violation>>)]
    pub violations: Option<Box<Vec<Violation>>>,
}

impl Problem {
    /// Neues Problem.
    pub fn new(
        status: StatusCode,
        code: &'static str,
        title: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            type_: format!("urn:aigw:problem:{code}"),
            title,
            status: status.as_u16(),
            detail: Some(detail.into()),
            instance: None,
            code,
            violations: None,
        }
    }

    /// `400 bad-request`.
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "bad-request",
            "Ungültige Anfrage",
            detail,
        )
    }

    /// `401 unauthorized`.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Nicht angemeldet",
            "Gültiges Admin-Token erforderlich (Authorization: Bearer …)",
        )
    }

    /// `403 forbidden`.
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Keine Berechtigung",
            detail,
        )
    }

    /// `404 not-found`.
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not-found", "Nicht gefunden", detail)
    }

    /// `409 conflict`.
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", "Konflikt", detail)
    }

    /// `412 precondition-failed` (ETag passt nicht).
    pub fn precondition_failed() -> Self {
        Self::new(
            StatusCode::PRECONDITION_FAILED,
            "precondition-failed",
            "Ressource wurde zwischenzeitlich geändert",
            "If-Match passt nicht zum aktuellen ETag – neu laden und erneut versuchen",
        )
    }

    /// `428 precondition-required` (F38): schreibende Anfrage ohne `If-Match`.
    pub fn precondition_required() -> Self {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            "precondition-required",
            "If-Match fehlt",
            "Schreibende Anfragen brauchen If-Match mit dem ETag aus dem letzten Lesen \
             (oder `*`, um ohne Prüfung zu überschreiben)",
        )
    }

    /// `422 validation-failed`.
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation-failed",
            "Ungültige Eingabe",
            detail,
        )
    }

    /// `500 internal` (ohne Details).
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Interner Fehler",
            "Interner Fehler, Details im Server-Log",
        )
    }

    /// Setzt die einzelnen Grenzverstöße.
    #[must_use]
    pub fn with_violations(mut self, violations: Vec<Violation>) -> Self {
        self.violations = (!violations.is_empty()).then(|| Box::new(violations));
        self
    }

    /// Setzt `instance`.
    #[must_use]
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.instance = Some(path.into());
        self
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let unauthorized = status == StatusCode::UNAUTHORIZED;
        let mut res = (status, Json(self)).into_response();
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        if unauthorized {
            h.insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"aigw-admin\""),
            );
        }
        res
    }
}

impl From<StoreError> for Problem {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound { kind, name } => {
                Self::not_found(format!("{kind} {name:?} nicht gefunden"))
            }
            StoreError::InvalidReference { kind, name } => {
                Self::invalid(format!("Verweis auf unbekannte Ressource {kind} {name:?}"))
            }
            StoreError::Conflict(msg) => Self::conflict(msg),
            StoreError::Invalid(msg) => Self::invalid(msg),
            StoreError::PreconditionFailed { .. } => Self::precondition_failed(),
            other => {
                tracing::error!(error = %other, "Admin-API: Fehler der Persistenzschicht");
                Self::internal()
            }
        }
    }
}

impl From<JsonRejection> for Problem {
    fn from(r: JsonRejection) -> Self {
        match r.status() {
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Self::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported-media-type",
                "Falscher Medientyp",
                "Content-Type: application/json erwartet",
            ),
            StatusCode::PAYLOAD_TOO_LARGE => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload-too-large",
                "Anfrage zu groß",
                r.body_text(),
            ),
            StatusCode::UNPROCESSABLE_ENTITY => Self::invalid(r.body_text()),
            _ => Self::bad_request(r.body_text()),
        }
    }
}

impl From<QueryRejection> for Problem {
    fn from(r: QueryRejection) -> Self {
        Self::bad_request(r.body_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::resource::ResourceKind;

    #[test]
    fn maps_store_errors() {
        let p: Problem = StoreError::NotFound {
            kind: ResourceKind::Team,
            name: "x".into(),
        }
        .into();
        assert_eq!((p.status, p.code), (404, "not-found"));
        assert_eq!(p.type_, "urn:aigw:problem:not-found");
        let p: Problem = StoreError::Conflict("dup".into()).into();
        assert_eq!(p.status, 409);
        let p: Problem = StoreError::Corrupt("geheim".into()).into();
        assert_eq!(p.status, 500);
        assert!(!p.detail.unwrap_or_default().contains("geheim"));
    }

    #[test]
    fn response_has_problem_content_type() {
        let res = Problem::unauthorized().into_response();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(res.headers()[header::CONTENT_TYPE], PROBLEM_JSON);
        assert!(res.headers().contains_key(header::WWW_AUTHENTICATE));
    }
}
