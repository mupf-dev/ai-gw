// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Modus „nur YAML“ (AIGW-33, `gitops.read_only_admin`).
//!
//! Schreibende Anfragen (alles außer `GET`/`HEAD`/`OPTIONS`) an die Admin-API werden mit
//! `403 read-only` abgelehnt – bevor Anmeldung und Berechtigung geprüft werden. Ausgenommen ist
//! `POST /admin/api/v1/config:apply`: das ist der GitOps-Weg selbst (z. B. aus einer
//! CI-Pipeline) – sowie der Testlauf `POST /admin/api/v1/plugin-bindings:test`, der nichts
//! schreibt, und der Playground `POST /admin/api/v1/playground/chat` (AIGW-54, ändert keine
//! Konfiguration). Die CLI (`aigw config apply`) schreibt direkt in die Datenbank und ist nicht
//! betroffen.

use axum::Router;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};

use crate::gitops::APPLY_PATH;
use crate::playground::CHAT_PATH;
use crate::plugins::TEST_PATH;
use crate::problem::Problem;

/// Präfix der Admin-API.
const API_PREFIX: &str = "/admin/api/";

/// `403 read-only` mit Hinweis.
pub fn problem() -> Problem {
    Problem::new(
        StatusCode::FORBIDDEN,
        "read-only",
        "Admin-API schreibgeschützt",
        "Modus „nur YAML“ (gitops.read_only_admin): Änderungen nur über GitOps – \
         `aigw config apply` bzw. POST /admin/api/v1/config:apply",
    )
}

fn is_write(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Middleware: lehnt schreibende Admin-Anfragen ab (außer GitOps-Apply und Plugin-Testlauf).
pub async fn reject_writes(req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if is_write(req.method())
        && path.starts_with(API_PREFIX)
        && path != APPLY_PATH
        && path != TEST_PATH
        && path != CHAT_PATH
    {
        return problem().at(path.to_owned()).into_response();
    }
    next.run(req).await
}

/// Legt die Middleware um `router`, falls `enabled`.
pub fn apply(router: Router, enabled: bool) -> Router {
    if enabled {
        router.layer(axum::middleware::from_fn(reject_writes))
    } else {
        router
    }
}
