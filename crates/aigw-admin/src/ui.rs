// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Eingebettete Admin-UI (statische Svelte-SPA unter `/admin/`).
//!
//! Die Dateien werden zur Compile-Zeit eingebettet (`rust-embed`); das Verzeichnis wählt
//! `build.rs` (gebaute UI aus `ui/dist` oder eine Platzhalterseite). Unbekannte Pfade ohne
//! Dateiendung liefern `index.html` (SPA-Routing), Pfade unter `/admin/api/` nie.

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use rust_embed::RustEmbed;

/// Art der eingebetteten UI: `ui` (gebaut) oder `fallback` (Platzhalterseite).
pub const KIND: &str = env!("AIGW_UI_KIND");

#[derive(RustEmbed)]
#[folder = "$AIGW_UI_DIR"]
struct Assets;

/// Content-Security-Policy der UI: nur eigene Quellen, keine Inline-Skripte.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; font-src 'self'; object-src 'none'; base-uri 'self'; \
                   frame-ancestors 'none'; form-action 'self'";

pub(crate) fn router() -> Router {
    Router::new()
        .route("/admin", get(|| async { Redirect::permanent("/admin/") }))
        .route("/admin/", get(|| async { serve("index.html") }))
        .route(
            "/admin/{*path}",
            get(|axum::extract::Path(path): axum::extract::Path<String>| async move {
                serve(&path)
            }),
        )
}

fn serve(path: &str) -> Response {
    if path.starts_with("api/") || path == "api" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (path, file) = match Assets::get(path) {
        Some(f) => (path, f),
        // SPA-Fallback nur für "Seiten" (ohne Dateiendung), fehlende Assets bleiben 404.
        None if !path.rsplit('/').next().unwrap_or("").contains('.') => {
            match Assets::get("index.html") {
                Some(f) => ("index.html", f),
                None => return StatusCode::NOT_FOUND.into_response(),
            }
        }
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let cache = if path.starts_with("assets/") {
        // Vite versieht Dateien unter assets/ mit Inhalts-Hash.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mime = file.metadata.mimetype();
    let content_type = if mime.starts_with("text/") || mime.ends_with("javascript") {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_owned()
    };
    let mut res = file.data.into_owned().into_response();
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_status(uri: &str) -> (StatusCode, Option<HeaderValue>) {
        let res = router()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        (
            res.status(),
            res.headers().get(header::CONTENT_TYPE).cloned(),
        )
    }

    #[tokio::test]
    async fn serves_index_and_spa_fallback() {
        let (s, ct) = get_status("/admin/").await;
        assert_eq!(s, StatusCode::OK);
        assert!(ct.unwrap().to_str().unwrap().starts_with("text/html"));
        assert_eq!(get_status("/admin/keys/123").await.0, StatusCode::OK);
        assert_eq!(
            get_status("/admin/missing.js").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get_status("/admin/api/unknown").await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(get_status("/admin").await.0, StatusCode::PERMANENT_REDIRECT);
    }
}
