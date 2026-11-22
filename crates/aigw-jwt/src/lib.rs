// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! JWS-/JWT-Prüfung von ai-gw (AIGW-115).
//!
//! - [`jws`]: gemeinsame Grundbausteine (JWS Compact, JWK/JWKS, Signaturprüfung über
//!   aws-lc-rs) – genutzt vom OIDC-Login der Admin-UI (`aigw-admin::oidc`) und von
//! - [`workload`]: Access-Tokens von Workload-/Agent-Identitäten (z. B. Microsoft Entra
//!   Agent ID) für Datenebene und MCP-Gateway.
//!
//! Konzept: `docs/konzept/18-workload-identitaeten.md`.

pub mod jws;
#[cfg(feature = "testkit")]
pub mod testkit;
pub mod workload;

pub use jws::{Jwk, Jwks, JwsError, JwsHeader, looks_like_jwt};
pub use workload::{TokenError, WorkloadVerifier, check_claims, now_unix};
