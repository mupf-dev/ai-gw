// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Preise (AIGW-65), versioniert über `validFrom`.
//!
//! Eine Preisversion gilt für (Anbieter, Modell) ab `validFrom` bis zur nächsten Version.
//! **Bereits gültige Versionen sind unveränderlich** (nur Labels lassen sich ändern) und
//! lassen sich nicht löschen – eine Preisänderung ist immer eine neue Version. Künftige
//! Versionen dürfen bis zu ihrem Beginn geändert und gelöscht werden. Ohne `metadata.name`
//! vergibt der Server `<anbieter|any>-<modell>-<JJJJMMTT[hhmmss]>`.

// `#[utoipa::path(params(…))]` erzeugt für IntoParams-Typen qualifizierte Pfade.
#![allow(unused_qualifications)]

use std::collections::HashMap;

use aigw_core::resource::{Id, Object, ObjectMeta, PriceSpec};
use aigw_core::types::ProviderKind;
use aigw_core::validate;
use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::{IntoParams, ToSchema};

use crate::AdminState;
use crate::auth::{Principal, Target};
use crate::problem::Problem;
use crate::resources::{
    AdminResource, ApiResult, ListArgs, St, create_impl, delete_impl, get_impl, json_with,
    list_impl, plain_resource, put_impl,
};

/// Filter für Preise.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query, rename_all = "camelCase")]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PriceQuery {
    /// Seitengröße (1–500, Standard 50).
    pub limit: Option<u32>,
    /// Fortsetzung (`nextCursor` der vorigen Seite).
    pub cursor: Option<String>,
    /// Teilstring im Namen oder Modell.
    pub q: Option<String>,
    /// Label-Selektor `k=v,k2`.
    pub label: Option<String>,
    /// Nur dieses Modell (exakt, wie `Deployment.providerModel`).
    pub model: Option<String>,
    /// Nur dieser Anbieter (`any` = Preise ohne Anbieter).
    pub provider: Option<String>,
    /// Nur die zum Zeitpunkt gültige Version je (Anbieter, Modell) (RFC 3339).
    #[param(value_type = Option<String>, format = DateTime)]
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub effective_at: Option<OffsetDateTime>,
}

plain_resource!(
    spec = PriceSpec,
    res = Price,
    input = PriceInput,
    list = PriceList,
    query = PriceQuery,
    tag = "prices",
    path = "/admin/api/v1/prices",
    item_path = "/admin/api/v1/prices/{name}",
    what = "Preisversion",
    handlers = [
        list_prices,
        create_price,
        get_price,
        put_price,
        delete_price
    ],
    input_meta = [serde(default)]
);

impl AdminResource for PriceSpec {
    type Res = Price;
    type Query = PriceQuery;
    const COLLECTION: &'static str = "prices";

    fn target(_name: &str, _spec: &Self) -> Target {
        Target::Catalog
    }

    fn list_args(q: &Self::Query) -> ListArgs<'_> {
        ListArgs {
            limit: q.limit,
            cursor: q.cursor.as_deref(),
            q: None, // auch Modell, siehe `filter`
            label: q.label.as_deref(),
        }
    }

    fn filter(o: &Object<Self>, q: &Self::Query) -> bool {
        let provider = o.spec.provider.map_or("any", ProviderKind::as_str);
        q.q.as_deref()
            .is_none_or(|s| o.metadata.name.contains(s) || o.spec.model.contains(s))
            && q.model.as_ref().is_none_or(|m| *m == o.spec.model)
            && q.provider.as_deref().is_none_or(|p| p == provider)
    }

    fn post_filter(
        items: Vec<Object<Self>>,
        q: &Self::Query,
    ) -> Result<Vec<Object<Self>>, Problem> {
        if q.provider
            .as_deref()
            .is_some_and(|p| p != "any" && ProviderKind::parse(p).is_none())
        {
            return Err(Problem::bad_request(
                "provider: unbekannter Anbieter (oder `any`)",
            ));
        }
        let Some(at) = q.effective_at else {
            return Ok(items);
        };
        // Je (Anbieter, Modell) die jüngste Version mit validFrom ≤ at.
        let mut best: HashMap<(Option<ProviderKind>, String), Object<Self>> = HashMap::new();
        for o in items.into_iter().filter(|o| o.spec.valid_from <= at) {
            let key = (o.spec.provider, o.spec.model.clone());
            if best
                .get(&key)
                .is_none_or(|b| b.spec.valid_from < o.spec.valid_from)
            {
                best.insert(key, o);
            }
        }
        let mut out: Vec<_> = best.into_values().collect();
        out.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));
        Ok(out)
    }

    fn default_name(spec: &Self) -> Option<String> {
        Some(price_name(spec))
    }

    async fn check(
        _st: &AdminState,
        _name: &str,
        spec: &Self,
        current: Option<&Object<Self>>,
    ) -> Result<(), Problem> {
        validate::price(spec).map_err(Problem::invalid)?;
        if let Some(cur) = current {
            validate::price_change(&cur.spec, spec, OffsetDateTime::now_utc())
                .map_err(Problem::conflict)?;
        }
        Ok(())
    }

    async fn check_delete(_st: &AdminState, current: &Object<Self>) -> Result<(), Problem> {
        validate::price_delete(&current.spec, OffsetDateTime::now_utc()).map_err(Problem::conflict)
    }
}

/// Standardname `<anbieter|any>-<modell>-<JJJJMMTT>` (mit `hhmmss`, wenn nicht Mitternacht).
fn price_name(p: &PriceSpec) -> String {
    let t = p.valid_from.to_offset(time::UtcOffset::UTC);
    let mut stamp = format!("{:04}{:02}{:02}", t.year(), u8::from(t.month()), t.day());
    if (t.hour(), t.minute(), t.second()) != (0, 0, 0) {
        stamp.push_str(&format!(
            "{:02}{:02}{:02}",
            t.hour(),
            t.minute(),
            t.second()
        ));
    }
    let provider = p.provider.map_or("any", ProviderKind::as_str);
    let model: String = p
        .model
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let model = model.trim_matches(['-', '.', '_']);
    // Name max. 128 Zeichen (ASCII, daher Kürzen nach Bytes sicher).
    let keep = 128 - provider.len() - stamp.len() - 2;
    let model = model.get(..model.len().min(keep)).unwrap_or(model);
    format!("{provider}-{model}-{stamp}")
}

/// Routen der Preise (ohne Authentifizierungsschicht).
pub(crate) fn router() -> axum::Router<AdminState> {
    use axum::routing::get;
    axum::Router::new()
        .route("/admin/api/v1/prices", get(list_prices).post(create_price))
        .route(
            "/admin/api/v1/prices/{name}",
            get(get_price).put(put_price).delete(delete_price),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigw_core::resource::TokenPrices;

    fn price(provider: Option<ProviderKind>, model: &str, ts: i64) -> PriceSpec {
        PriceSpec {
            provider,
            model: model.into(),
            valid_from: OffsetDateTime::from_unix_timestamp(ts)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            per_million_tokens: TokenPrices::default(),
            per_request: None,
            per_image: None,
        }
    }

    #[test]
    fn default_names() {
        assert_eq!(
            price_name(&price(
                Some(ProviderKind::OpenAi),
                "gpt-4o-2024-08-06",
                1_722_902_400
            )),
            "openai-gpt-4o-2024-08-06-20240806"
        );
        assert_eq!(
            price_name(&price(None, "anthropic/Claude 3:5@x", 1_722_902_401)),
            "any-anthropic-claude-3-5-x-20240806000001"
        );
        let long = "m".repeat(300);
        assert_eq!(price_name(&price(None, &long, 0)).len(), 128);
    }
}
