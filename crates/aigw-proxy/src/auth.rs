// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Auslesen des API-Keys und Schlüsselprüfung (AIGW-28).
//!
//! Unterstützte Header (Architektur §3, Schritt 2): `Authorization: Bearer <key>` (OpenAI),
//! `api-key: <key>` (Azure OpenAI) und `x-api-key: <key>` (Anthropic). Sind mehrere gesetzt,
//! gilt diese Reihenfolge.
//!
//! [`SnapshotAuthenticator`] prüft gegen den Konfigurations-Snapshot; das Ergebnis
//! ([`AuthContext`]) trägt den [`KeyCtx`] und prüft Modellfreigaben
//! ([`AuthContext::authorize_model`], [`AuthContext::visible_models`]).

use std::fmt;
use std::sync::Arc;

use aigw_core::ir::{ApiError, ErrorKind, ModelInfo};
use aigw_core::resource::Id;
use aigw_core::snapshot::{KeyCtx, KeyHash, ModelGroupCtx, Snapshot, SnapshotHandle};
use aws_lc_rs::digest;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use time::OffsetDateTime;

/// Herkunft des Schlüssels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `Authorization: Bearer …`.
    Bearer,
    /// `api-key: …`.
    ApiKeyHeader,
    /// `x-api-key: …`.
    XApiKey,
}

/// Vom Client übergebener Schlüssel. `Debug` zeigt den Schlüssel nicht.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    /// Herkunft.
    pub source: KeySource,
    /// Schlüssel im Klartext (nur für die Hash-Berechnung, nie loggen).
    pub secret: String,
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKey")
            .field("source", &self.source)
            .field("secret", &"***")
            .finish()
    }
}

/// Liest den API-Key aus den Headern (`None`, wenn keiner gesetzt ist).
pub fn extract_api_key(headers: &HeaderMap) -> Option<ApiKey> {
    if let Some(v) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some((scheme, token)) = v.split_once(' ')
            && scheme.eq_ignore_ascii_case("bearer")
            && !token.trim().is_empty()
        {
            return Some(ApiKey {
                source: KeySource::Bearer,
                secret: token.trim().to_owned(),
            });
        }
    }
    for (name, source) in [
        ("api-key", KeySource::ApiKeyHeader),
        ("x-api-key", KeySource::XApiKey),
    ] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(ApiKey {
                    source,
                    secret: v.to_owned(),
                });
            }
        }
    }
    None
}

/// Ergebnis einer erfolgreichen Prüfung.
///
/// Mit [`SnapshotAuthenticator`] trägt der Kontext den geprüften Key ([`KeyCtx`] mit Key,
/// Team, Organisation – Grundlage für Limits und Logs) und den Snapshot, der für die
/// **gesamte** Anfrage gilt (einmal pro Anfrage geladen, §14). Ohne Snapshot (z. B.
/// [`AllowAll`]) entfallen Modellprüfung und Modellfilter.
#[derive(Debug, Clone, Default)]
pub struct AuthContext {
    /// Kennung des Schlüssels für Logs (nie der Schlüssel selbst).
    pub key_id: Option<String>,
    /// Geprüfter Key samt Team und Organisation.
    pub key: Option<Arc<KeyCtx>>,
    /// Snapshot dieser Anfrage.
    pub snapshot: Option<Arc<Snapshot>>,
}

impl AuthContext {
    /// ID des Keys.
    pub fn api_key_id(&self) -> Option<Id> {
        self.key.as_ref().map(|k| k.key.id)
    }

    /// ID des Teams (falls der Key einem Team gehört).
    pub fn team_id(&self) -> Option<Id> {
        self.key.as_ref()?.team.as_ref().map(|t| t.id)
    }

    /// ID der Organisation (über das Team).
    pub fn organization_id(&self) -> Option<Id> {
        self.key.as_ref()?.organization.as_ref().map(|o| o.id)
    }

    /// Besitzender Nutzer (Name; der Snapshot führt Nutzer noch nicht nach ID).
    pub fn user(&self) -> Option<&str> {
        self.key.as_ref()?.key.spec.user.as_deref()
    }

    /// Prüft, ob der Key das angefragte Modell (Name oder Alias) nutzen darf, und liefert die
    /// Modellgruppe. Ohne Snapshot findet keine Prüfung statt (`Ok(None)`).
    ///
    /// Damit eingeschränkte Keys nicht erkunden können, welche Modelle es gibt, erhalten sie
    /// für unbekannte und nicht freigegebene Modelle dieselbe Antwort (`403`). Nur Keys ohne
    /// jede Einschränkung (Key und Team) bekommen für unbekannte Modelle `404`.
    pub fn authorize_model(&self, model: &str) -> Result<Option<Arc<ModelGroupCtx>>, ApiError> {
        let (Some(snap), Some(key)) = (&self.snapshot, &self.key) else {
            return Ok(None);
        };
        match snap.model_group(model) {
            Some(group) if key.allows_model(group) => Ok(Some(Arc::clone(group))),
            None if is_unrestricted(key) => Err(ApiError::new(
                ErrorKind::NotFound,
                format!("Das Modell {model:?} existiert nicht"),
            )
            .with_param("model")
            .with_code("model_not_found")),
            _ => Err(ApiError::new(
                ErrorKind::PermissionDenied,
                format!("Das Modell {model:?} ist für diesen API-Key nicht freigegeben"),
            )
            .with_param("model")
            .with_code("model_not_allowed")),
        }
    }

    /// Für `GET /v1/models`: die für den Key freigegebenen Modellgruppen (kanonische Namen,
    /// sortiert). `None` ohne Snapshot – dann gilt die Liste des Anbieters.
    pub fn visible_models(&self) -> Option<Vec<ModelInfo>> {
        let (snap, key) = (self.snapshot.as_ref()?, self.key.as_ref()?);
        let mut out: Vec<ModelInfo> = snap
            .model_groups
            .iter()
            // Jede Gruppe steht unter Name und Aliassen im Index – nur den Namen zeigen.
            .filter(|(name, g)| **name == g.group.metadata.name && key.allows_model(g))
            .map(|(name, g)| ModelInfo {
                id: name.clone(),
                display_name: None,
                created: g
                    .group
                    .metadata
                    .created_at
                    .map_or(0, OffsetDateTime::unix_timestamp),
                owned_by: "ai-gw".to_owned(),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Some(out)
    }
}

fn is_unrestricted(key: &KeyCtx) -> bool {
    key.key.spec.allowed_models.is_empty()
        && key
            .team
            .as_ref()
            .is_none_or(|t| t.spec.allowed_models.is_empty())
}

/// Schlüsselprüfung. Synchron: die Prüfung läuft im RAM (Hash-Lookup, Q7).
pub trait Authenticator: Send + Sync + 'static {
    /// Prüft den (optionalen) Schlüssel; Fehler sind üblicherweise
    /// [`ErrorKind::Authentication`].
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError>;
}

/// Platzhalter: lässt jede Anfrage zu (mit oder ohne Schlüssel).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Authenticator for AllowAll {
    fn authenticate(&self, _key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        Ok(AuthContext::default())
    }
}

/// Schlüsselprüfung gegen den Konfigurations-Snapshot (Architektur §3, Schritt 2).
///
/// SHA-256 über den übergebenen Key, Nachschlagen im Key-Index des Snapshots (O(1); der
/// Index enthält nur aktive Keys), dann Prüfung des Ablaufs. Alle Fehlschläge (kein Key,
/// unbekannt, gesperrt, abgelaufen) ergeben **dieselbe** Antwort, damit sich der Zustand
/// fremder Keys nicht ermitteln lässt; der Grund steht nur im Debug-Log.
#[derive(Debug, Clone)]
pub struct SnapshotAuthenticator {
    snapshot: SnapshotHandle,
}

impl SnapshotAuthenticator {
    /// Prüft gegen den jeweils aktuellen Snapshot von `snapshot`.
    pub fn new(snapshot: SnapshotHandle) -> Self {
        Self { snapshot }
    }

    /// Wie [`Authenticator::authenticate`], aber mit vorgegebener Uhrzeit.
    pub fn authenticate_at(
        &self,
        key: Option<&ApiKey>,
        now: OffsetDateTime,
    ) -> Result<AuthContext, ApiError> {
        let Some(key) = key else {
            return Err(unauthorized("kein API-Key übergeben"));
        };
        let hash = hash_key(&key.secret);
        let snap = self.snapshot.load();
        let Some(ctx) = snap.key(&hash).cloned() else {
            return Err(unauthorized("unbekannter oder gesperrter Key"));
        };
        if !ctx.is_valid_at(now) {
            return Err(unauthorized("Key abgelaufen"));
        }
        Ok(AuthContext {
            key_id: Some(ctx.key.id.to_string()),
            key: Some(ctx),
            snapshot: Some(snap),
        })
    }
}

impl Authenticator for SnapshotAuthenticator {
    fn authenticate(&self, key: Option<&ApiKey>) -> Result<AuthContext, ApiError> {
        self.authenticate_at(key, OffsetDateTime::now_utc())
    }
}

/// SHA-256 eines Keys (identisch zu `aigw_store::hash_api_key`).
pub fn hash_key(secret: &str) -> KeyHash {
    let d = digest::digest(&digest::SHA256, secret.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    KeyHash(out)
}

fn unauthorized(reason: &'static str) -> ApiError {
    tracing::debug!(reason, "API-Key abgewiesen");
    ApiError::new(
        ErrorKind::Authentication,
        "Ungültiger oder fehlender API-Key",
    )
    .with_code("invalid_api_key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn extracts_keys_in_priority_order() {
        let k = extract_api_key(&headers(&[("authorization", "Bearer sk-1")])).unwrap();
        assert_eq!((k.source, k.secret.as_str()), (KeySource::Bearer, "sk-1"));
        let k = extract_api_key(&headers(&[("authorization", "bearer  sk-2 ")])).unwrap();
        assert_eq!(k.secret, "sk-2");
        let k = extract_api_key(&headers(&[("api-key", "az"), ("x-api-key", "an")])).unwrap();
        assert_eq!(k.source, KeySource::ApiKeyHeader);
        let k = extract_api_key(&headers(&[("x-api-key", "an")])).unwrap();
        assert_eq!((k.source, k.secret.as_str()), (KeySource::XApiKey, "an"));
        let k = extract_api_key(&headers(&[
            ("authorization", "Basic abc"),
            ("x-api-key", "an"),
        ]))
        .unwrap();
        assert_eq!(k.source, KeySource::XApiKey);
        assert!(extract_api_key(&headers(&[("authorization", "Bearer ")])).is_none());
        assert!(extract_api_key(&HeaderMap::new()).is_none());
    }

    #[test]
    fn debug_hides_secret() {
        let k = ApiKey {
            source: KeySource::Bearer,
            secret: "geheim".into(),
        };
        assert!(!format!("{k:?}").contains("geheim"));
    }

    mod snapshot {
        use super::super::*;
        use aigw_core::resource::{
            self as r, ApiKeySpec, ApiKeyStatus, ModelGroupSpec, Object, ObjectMeta, TeamSpec,
            new_id,
        };
        use aigw_core::snapshot::ModelGroupCtx;
        use time::Duration;

        fn group(name: &str, aliases: &[&str]) -> Arc<ModelGroupCtx> {
            Arc::new(ModelGroupCtx {
                group: Object {
                    id: new_id(),
                    metadata: ObjectMeta::named(name),
                    spec: ModelGroupSpec {
                        aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
                        ..ModelGroupSpec::default()
                    },
                },
                deployments: Vec::new(),
            })
        }

        fn key_ctx(
            allowed: &[&str],
            team_allowed: Option<&[&str]>,
            expires_at: Option<OffsetDateTime>,
        ) -> KeyCtx {
            let list = |l: &[&str]| l.iter().map(|m| (*m).to_owned()).collect::<Vec<_>>();
            let team = team_allowed.map(|t| {
                Arc::new(Object {
                    id: new_id(),
                    metadata: ObjectMeta::named("team"),
                    spec: TeamSpec {
                        organization: "org".into(),
                        allowed_models: list(t),
                        ..TeamSpec::default()
                    },
                })
            });
            KeyCtx {
                key: r::ApiKey {
                    id: new_id(),
                    metadata: ObjectMeta::named("k"),
                    spec: ApiKeySpec {
                        team: team.as_ref().map(|_| "team".to_owned()),
                        allowed_models: list(allowed),
                        expires_at,
                        ..ApiKeySpec::default()
                    },
                    status: ApiKeyStatus {
                        prefix: "abc".into(),
                        state: r::ApiKeyState::Active,
                        last_used_at: None,
                    },
                },
                team,
                organization: None,
                expires_at,
            }
        }

        fn auth_with(secret: &str, ctx: KeyCtx) -> SnapshotAuthenticator {
            let mut snap = Snapshot {
                version: 1,
                ..Snapshot::default()
            };
            snap.keys.insert(hash_key(secret), Arc::new(ctx));
            for g in [group("gpt-4o", &["gpt-4o-latest"]), group("claude", &[])] {
                snap.model_groups
                    .insert(g.group.metadata.name.clone(), Arc::clone(&g));
                for a in &g.group.spec.aliases {
                    snap.model_groups.insert(a.clone(), Arc::clone(&g));
                }
            }
            SnapshotAuthenticator::new(SnapshotHandle::new(snap))
        }

        fn bearer(s: &str) -> ApiKey {
            ApiKey {
                source: KeySource::Bearer,
                secret: s.into(),
            }
        }

        #[test]
        fn rejects_missing_unknown_and_expired_keys_uniformly() {
            let now = OffsetDateTime::now_utc();
            let a = auth_with("sk-good", key_ctx(&[], None, Some(now)));
            let e1 = a.authenticate_at(None, now).unwrap_err();
            let e2 = a.authenticate_at(Some(&bearer("sk-bad")), now).unwrap_err();
            let e3 = a
                .authenticate_at(Some(&bearer("sk-good")), now)
                .unwrap_err();
            for e in [&e1, &e2, &e3] {
                assert_eq!(e.kind, ErrorKind::Authentication);
            }
            assert_eq!(e1, e2);
            assert_eq!(e2, e3, "abgelaufen und unbekannt sind nicht unterscheidbar");
            let ok = a
                .authenticate_at(Some(&bearer("sk-good")), now - Duration::seconds(1))
                .unwrap();
            assert!(ok.api_key_id().is_some());
            assert_eq!(ok.team_id(), None);
        }

        #[test]
        fn model_permissions_key_and_team() {
            let now = OffsetDateTime::now_utc();
            // Unbeschränkt: alles erlaubt, unbekannt = 404.
            let ctx = auth_with("k", key_ctx(&[], None, None))
                .authenticate_at(Some(&bearer("k")), now)
                .unwrap();
            assert!(ctx.authorize_model("gpt-4o-latest").unwrap().is_some());
            assert_eq!(
                ctx.authorize_model("nope").unwrap_err().kind,
                ErrorKind::NotFound
            );
            let names: Vec<_> = ctx
                .visible_models()
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect();
            assert_eq!(names, ["claude", "gpt-4o"]);

            // Key erlaubt gpt-4o über den Alias, Team erlaubt beide.
            let ctx = auth_with("k", key_ctx(&["gpt-4o-latest"], Some(&[]), None))
                .authenticate_at(Some(&bearer("k")), now)
                .unwrap();
            assert!(ctx.team_id().is_some());
            assert!(ctx.authorize_model("gpt-4o").unwrap().is_some());
            assert_eq!(
                ctx.authorize_model("claude").unwrap_err().kind,
                ErrorKind::PermissionDenied
            );
            // Eingeschränkt: unbekannt sieht aus wie verboten.
            assert_eq!(
                ctx.authorize_model("nope").unwrap_err(),
                ctx.authorize_model("nope").unwrap_err()
            );
            assert_eq!(
                ctx.authorize_model("nope").unwrap_err().kind,
                ErrorKind::PermissionDenied
            );
            assert_eq!(ctx.visible_models().unwrap().len(), 1);

            // Team schränkt ein, Key nicht.
            let ctx = auth_with("k", key_ctx(&[], Some(&["claude"]), None))
                .authenticate_at(Some(&bearer("k")), now)
                .unwrap();
            assert!(ctx.authorize_model("claude").unwrap().is_some());
            assert!(ctx.authorize_model("gpt-4o").is_err());
            let names: Vec<_> = ctx
                .visible_models()
                .unwrap()
                .into_iter()
                .map(|m| m.id)
                .collect();
            assert_eq!(names, ["claude"]);
        }

        #[test]
        fn allow_all_skips_model_checks() {
            let ctx = AllowAll.authenticate(None).unwrap();
            assert!(ctx.authorize_model("anything").unwrap().is_none());
            assert!(ctx.visible_models().is_none());
        }
    }
}
