// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin `webhook` (AIGW-61, Architektur §9): ruft einen externen HTTP-Dienst in der
//! Anfrage- und/oder Antwort-Phase auf.
//!
//! ```yaml
//! plugin: webhook
//! config:
//!   url: https://policy.example.org/aigw      # SSRF-Regeln wie Deployments
//!   phases: [request]                          # request | response (Standard: request)
//!   secret: {secret: hooks}                    # HMAC-Schlüssel: secret(+field, F149) | credential | env | file
//!   includeHeaders: [x-client-app]             # Client-Header im Payload (nie Auth/Cookies)
//!   maxPayloadBytes: 1048576                   # Größe des Aufrufs (Standard 1 MiB)
//!   maxResponseBytes: 1048576                  # Größe der Antwort (Standard 1 MiB)
//!   timeoutMs: 200                             # Kettensteuerung (Standard 200 ms)
//!   failMode: closed                           # Kettensteuerung (Standard open, F148)
//! ```
//!
//! **Aufruf:** `POST` mit JSON `{version, phase, requestId, ingress, modelGroup, stream,
//! key, team, organization, headers, request | response}` (IR, ohne Geheimnisse: keine
//! Gateway-Keys, keine Upstream-Header aus `hints`), Header `x-aigw-request-id`,
//! `x-aigw-delivery` (`<request-id>:<phase>`) und – mit Geheimnis – `x-aigw-signature:
//! t=<ts>,v1=<hex>` (HMAC-SHA256 über `"<ts>." + body`, siehe [`aigw_webhook::sign`]).
//!
//! **Antwort** (2xx, JSON; `204`/leer = `continue`):
//!
//! - `{"action": "continue"}`
//! - `{"action": "modify", "request": {…IR…}}` bzw. `"response"` – ersetzt Anfrage/Antwort.
//!   `model` und `stream` müssen unverändert bleiben (sonst Plugin-Fehler); `hints`,
//!   `source` und `stream_options` übernimmt das Gateway aus dem Original.
//! - `{"action": "reject", "status": 403, "message": "…", "code": "…"}` – Status 400–499.
//! - `{"action": "respond", "response": {…IR…}}` – Anfrage-Phase: feste Antwort ohne
//!   Upstream; Antwort-Phase: wie `modify`.
//!
//! Alles andere (Zeitüberschreitung, Status ≠ 2xx, ungültiges JSON, verbotene Änderung,
//! Größenlimit) ist ein Plugin-Fehler → `failMode` der Bindung. Die Antwort-Phase läuft nur
//! auf der ganzen Antwort: gestreamte Anfragen werden dafür gepuffert
//! ([`Plugin::needs_buffered_output`]); im Chunk-Pfad läuft der Webhook nie.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aigw_core::config::WebhookSecret;
use aigw_core::ir::{ApiError, ChatRequest, ChatResponse, ErrorKind};
use aigw_webhook::{
    ClientOptions, CredentialSource, DELIVERY_HEADER, REQUEST_ID_HEADER, SecretResolver,
    WebhookClient,
};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::binding::FailMode;
use crate::plugin::{
    CallInfo, Flow, Hook, Meta, MetaExt as _, Phases, Plugin, PluginError, ReqCtx, RespCtx,
};
use crate::registry::PluginFactory;

/// Standard-Zeitlimit eines Webhook-Aufrufs.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(200);
/// Standard-Größenlimit für Aufruf und Antwort.
pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
/// Höchstwert für die Größenlimits.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Version des Payload-Formats.
pub const PAYLOAD_VERSION: u32 = 1;

/// Client-Header, die nie in den Payload dürfen (Gateway-Keys, Sitzungen).
pub(crate) const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "api-key",
    "x-api-key",
    "x-goog-api-key",
    "cookie",
    "set-cookie",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PhaseName {
    Request,
    Response,
}

fn default_phases() -> Vec<PhaseName> {
    vec![PhaseName::Request]
}

fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    url: String,
    #[serde(default = "default_phases")]
    phases: Vec<PhaseName>,
    #[serde(default)]
    secret: Option<WebhookSecret>,
    #[serde(default)]
    include_headers: Vec<String>,
    #[serde(default = "default_max_bytes")]
    max_payload_bytes: usize,
    #[serde(default = "default_max_bytes")]
    max_response_bytes: usize,
}

/// Antwort des Dienstes.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum Reply {
    Continue,
    Modify {
        #[serde(default)]
        request: Option<Box<ChatRequest>>,
        #[serde(default)]
        response: Option<Box<ChatResponse>>,
    },
    Reject {
        #[serde(default)]
        status: Option<u16>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        code: Option<String>,
    },
    Respond {
        response: Box<ChatResponse>,
    },
}

/// Gemeinsame Umgebung aller Webhook-Bindungen: HTTP-Client (ein Pool) und Quelle für
/// Credential-Referenzen.
struct Env {
    client: OnceLock<Result<WebhookClient, String>>,
    credentials: Option<Arc<dyn CredentialSource>>,
    /// SSRF-Regeln für einen erst bei Bedarf gebauten Client.
    policy: aigw_core::upstream::UpstreamPolicy,
}

impl Env {
    fn client(&self) -> Result<WebhookClient, String> {
        self.client
            .get_or_init(|| {
                WebhookClient::new(self.policy.clone(), ClientOptions::default())
                    .map_err(|e| format!("Webhook-Client: {e}"))
            })
            .clone()
    }
}

/// Fabrik des Plugins `webhook`.
///
/// [`Factory::default`] (in [`crate::Registry::with_builtin`]) nutzt strenge SSRF-Regeln und
/// kennt keine Credential-Referenzen (sie scheitern dann beim Aufruf); der Server registriert
/// stattdessen [`Factory::new`] mit `admin.allow_*_upstreams` und dem Snapshot.
#[derive(Clone)]
pub struct Factory {
    env: Arc<Env>,
}

impl std::fmt::Debug for Factory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("webhook::Factory").finish_non_exhaustive()
    }
}

impl Default for Factory {
    fn default() -> Self {
        Self::with_policy(aigw_core::upstream::UpstreamPolicy::strict())
    }
}

impl Factory {
    /// Fabrik mit eigenen SSRF-Regeln, ohne Credential-Quelle (Prüfen von Bindungen in
    /// Admin-API/GitOps, [`crate::Registry::for_validation`]); der Client entsteht erst beim
    /// ersten Bau einer Bindung.
    pub fn with_policy(policy: aigw_core::upstream::UpstreamPolicy) -> Self {
        Self {
            env: Arc::new(Env {
                client: OnceLock::new(),
                credentials: None,
                policy,
            }),
        }
    }

    /// Fabrik mit eigenem Client (SSRF-Regeln, Pool) und Credential-Quelle.
    pub fn new(client: WebhookClient, credentials: Option<Arc<dyn CredentialSource>>) -> Self {
        let cell = OnceLock::new();
        let _ = cell.set(Ok(client));
        Self {
            env: Arc::new(Env {
                client: cell,
                credentials,
                policy: aigw_core::upstream::UpstreamPolicy::strict(),
            }),
        }
    }
}

impl PluginFactory for Factory {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        let c: Config = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        let client = self.env.client()?;
        let uri = client.check_url(&c.url).map_err(|e| format!("url: {e}"))?;
        let secret = SecretResolver::from_config(c.secret.as_ref(), self.env.credentials.clone())?;
        if c.phases.is_empty() {
            return Err("phases: mindestens eine Phase (request, response) angeben".into());
        }
        for (what, v) in [
            ("maxPayloadBytes", c.max_payload_bytes),
            ("maxResponseBytes", c.max_response_bytes),
        ] {
            if v == 0 || v > MAX_BYTES {
                return Err(format!("{what}: 1–{MAX_BYTES} Byte"));
            }
        }
        let mut include = Vec::with_capacity(c.include_headers.len());
        for h in &c.include_headers {
            let name = HeaderName::from_bytes(h.trim().as_bytes())
                .map_err(|_| format!("includeHeaders: ungültiger Header {h:?}"))?;
            if SECRET_HEADERS.contains(&name.as_str()) {
                return Err(format!(
                    "includeHeaders: {name} darf nicht an Webhooks gehen (Zugangsdaten)"
                ));
            }
            include.push(name);
        }
        Ok(Arc::new(Webhook {
            client,
            uri,
            secret,
            phases: Phases {
                request: c.phases.contains(&PhaseName::Request),
                response: c.phases.contains(&PhaseName::Response),
                chunk: false,
            },
            include,
            max_payload: c.max_payload_bytes,
            max_response: c.max_response_bytes,
        }))
    }

    fn default_timeout(&self) -> Duration {
        DEFAULT_TIMEOUT
    }

    /// F148: ein nicht erreichbarer Webhook (meist Zusatzlogik, Audit) soll den Verkehr nicht
    /// stoppen – Standard `open`; Policy-Webhooks setzen `failMode: closed`.
    fn default_fail_mode(&self) -> FailMode {
        FailMode::Open
    }
}

/// Plugin `webhook`.
#[derive(Debug)]
pub struct Webhook {
    client: WebhookClient,
    uri: Uri,
    secret: SecretResolver,
    phases: Phases,
    include: Vec<HeaderName>,
    max_payload: usize,
    max_response: usize,
}

fn err(msg: impl Into<String>) -> PluginError {
    PluginError::new(msg)
}

fn object_ref<S>(o: &aigw_core::resource::Object<S>) -> Value {
    json!({"id": o.id, "name": o.metadata.name})
}

impl Webhook {
    fn context(
        &self,
        phase: &str,
        info: &CallInfo,
        headers: Option<&HeaderMap>,
        meta: &Meta,
    ) -> Map<String, Value> {
        let key = info.key.as_deref();
        let mut m = Map::new();
        m.insert("version".into(), json!(PAYLOAD_VERSION));
        m.insert("phase".into(), json!(phase));
        m.insert("requestId".into(), json!(info.request_id));
        m.insert("ingress".into(), json!(info.ingress));
        m.insert("modelGroup".into(), json!(info.model_group));
        m.insert("stream".into(), json!(info.stream));
        m.insert(
            "key".into(),
            key.map_or(
                Value::Null,
                |k| json!({"id": k.key.id, "name": k.key.metadata.name}),
            ),
        );
        m.insert(
            "team".into(),
            key.and_then(|k| k.team.as_deref())
                .map_or(Value::Null, object_ref),
        );
        m.insert(
            "organization".into(),
            key.and_then(|k| k.organization.as_deref())
                .map_or(Value::Null, object_ref),
        );
        // Meta früherer Plugins – nur die externe Sicht: interne Schlüssel (`aigw.*`, z. B. die
        // Pseudonym-Zuordnung des `pii`-Plugins mit Originalwerten) gehen nie hinaus.
        let meta = meta.external_view();
        if !meta.is_empty() {
            m.insert("meta".into(), Value::Object(meta));
        }
        if let Some(h) = headers
            && !self.include.is_empty()
        {
            let mut out = Map::new();
            for name in &self.include {
                if let Some(v) = h.get(name).and_then(|v| v.to_str().ok()) {
                    out.insert(name.as_str().to_owned(), json!(v));
                }
            }
            m.insert("headers".into(), Value::Object(out));
        }
        m
    }

    async fn call(
        &self,
        info: &CallInfo,
        phase: &str,
        payload: Map<String, Value>,
    ) -> Result<Option<Reply>, PluginError> {
        let body = serde_json::to_vec(&Value::Object(payload))
            .map_err(|e| err(format!("Payload: {e}")))?;
        if body.len() > self.max_payload {
            return Err(err(format!(
                "Payload größer als maxPayloadBytes ({} > {})",
                body.len(),
                self.max_payload
            )));
        }
        let secret = self.secret.get().map_err(err)?;
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&info.request_id) {
            headers.insert(REQUEST_ID_HEADER, v.clone());
        }
        if let Ok(v) = HeaderValue::from_str(&format!("{}:{phase}", info.request_id)) {
            headers.insert(DELIVERY_HEADER, v);
        }
        // Das Zeitlimit setzt die Kette (timeoutMs); hier nur eine Obergrenze.
        let res = self
            .client
            .post_json(
                &self.uri,
                headers,
                Bytes::from(body),
                secret.as_ref(),
                crate::binding::MAX_TIMEOUT,
                self.max_response,
            )
            .await
            .map_err(|e| err(e.to_string()))?;
        if !res.status.is_success() {
            return Err(err(format!(
                "Webhook antwortete mit Status {}",
                res.status.as_u16()
            )));
        }
        if res.status == StatusCode::NO_CONTENT || res.body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice::<Reply>(&res.body)
            .map(Some)
            .map_err(|e| err(format!("Antwort des Webhooks ungültig: {e}")))
    }
}

fn reject(
    status: Option<u16>,
    message: Option<String>,
    code: Option<String>,
) -> Result<Flow, PluginError> {
    let status = status.unwrap_or(400);
    if !(400..=499).contains(&status) {
        return Err(err(format!(
            "reject: Status {status} nicht erlaubt (400–499)"
        )));
    }
    let mut message = message
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "Anfrage durch Richtlinie abgelehnt".to_owned());
    if message.len() > 1024 {
        let mut cut = 1024;
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        message.truncate(cut);
    }
    let code = code
        .filter(|c| {
            !c.is_empty()
                && c.len() <= 64
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        })
        .unwrap_or_else(|| "webhook_rejected".to_owned());
    Ok(Flow::Reject(
        // F152: Status unverändert (z. B. 422), Klasse nur für `type` im Fehlerobjekt.
        ApiError::new(ErrorKind::from_status(status), message)
            .with_code(code)
            .with_status(status),
    ))
}

/// Prüft eine geänderte Anfrage und übernimmt, was der Webhook nicht ändern darf.
fn accept_request(orig: &ChatRequest, mut new: ChatRequest) -> Result<ChatRequest, PluginError> {
    if new.model != orig.model {
        return Err(err(
            "modify: `model` darf nicht geändert werden (Modell-Umleitung ist gesperrt)",
        ));
    }
    if new.is_stream() != orig.is_stream() {
        return Err(err("modify: `stream` darf nicht geändert werden"));
    }
    if new.messages.is_empty() {
        return Err(err("modify: `messages` darf nicht leer sein"));
    }
    new.stream = orig.stream;
    new.stream_options.clone_from(&orig.stream_options);
    new.hints.clone_from(&orig.hints);
    new.source = orig.source;
    Ok(new)
}

/// Prüft eine Antwort des Webhooks (ersetzte oder feste Antwort).
fn accept_response(
    info: &CallInfo,
    model: &str,
    mut r: ChatResponse,
) -> Result<ChatResponse, PluginError> {
    if r.choices.is_empty() {
        return Err(err("Antwort ohne `choices`"));
    }
    if r.id.is_empty() {
        r.id = format!("aigw-webhook-{}", info.request_id);
    }
    if r.model.is_empty() {
        model.clone_into(&mut r.model);
    }
    Ok(r)
}

impl Plugin for Webhook {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn phases(&self) -> Phases {
        self.phases
    }

    fn needs_buffered_output(&self) -> bool {
        self.phases.response
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::future(async move {
            let mut payload =
                self.context("request", ctx.info, Some(ctx.client_headers), &ctx.meta);
            let mut req =
                serde_json::to_value(&ctx.request).map_err(|e| err(format!("Payload: {e}")))?;
            // Upstream-Header früherer Plugins können Zugangsdaten tragen.
            if let Some(h) = req.get_mut("hints").and_then(Value::as_object_mut) {
                h.remove("upstream_headers");
            }
            payload.insert("request".into(), req);
            match self.call(ctx.info, "request", payload).await? {
                None | Some(Reply::Continue) => Ok(Flow::Continue),
                Some(Reply::Modify {
                    request: Some(r), ..
                }) => {
                    ctx.request = accept_request(&ctx.request, *r)?;
                    Ok(Flow::Continue)
                }
                Some(Reply::Modify { request: None, .. }) => {
                    Err(err("modify ohne `request` in der Anfrage-Phase"))
                }
                Some(Reply::Reject {
                    status,
                    message,
                    code,
                }) => reject(status, message, code),
                Some(Reply::Respond { response }) => {
                    let model = ctx.request.model.clone();
                    Ok(Flow::Respond(Box::new(accept_response(
                        ctx.info, &model, *response,
                    )?)))
                }
            }
        })
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        Hook::future(async move {
            let mut payload = self.context("response", ctx.info, None, ctx.meta);
            payload.insert(
                "response".into(),
                serde_json::to_value(&ctx.response).map_err(|e| err(format!("Payload: {e}")))?,
            );
            let replace = |r: ChatResponse| -> Result<Flow, PluginError> {
                let model = ctx.response.model.clone();
                let mut r = accept_response(ctx.info, &model, r)?;
                r.source = ctx.response.source;
                Ok(Flow::Respond(Box::new(r)))
            };
            match self.call(ctx.info, "response", payload).await? {
                None | Some(Reply::Continue) => Ok(Flow::Continue),
                Some(
                    Reply::Modify {
                        response: Some(r), ..
                    }
                    | Reply::Respond { response: r },
                ) => replace(*r),
                Some(Reply::Modify { response: None, .. }) => {
                    Err(err("modify ohne `response` in der Antwort-Phase"))
                }
                Some(Reply::Reject {
                    status,
                    message,
                    code,
                }) => reject(status, message, code),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn build(cfg: Value) -> Result<Arc<dyn Plugin>, String> {
        Factory::default().build(&cfg)
    }

    #[tokio::test]
    async fn config_validation() {
        assert!(build(json!({"url": "https://hooks.example.org/x"})).is_ok());
        // SSRF: privat, http, Zugangsdaten in der URL.
        for url in [
            "https://127.0.0.1/x",
            "https://169.254.169.254/latest",
            "http://hooks.example.org/x",
            "https://user:pw@hooks.example.org/x",
            "https://localhost/x",
        ] {
            assert!(build(json!({"url": url})).is_err(), "{url}");
        }
        assert!(build(json!({"url": "https://h.example.org", "unknown": 1})).is_err());
        assert!(build(json!({"url": "https://h.example.org", "phases": []})).is_err());
        assert!(
            build(json!({"url": "https://h.example.org", "includeHeaders": ["Authorization"]}))
                .is_err()
        );
        assert!(build(json!({"url": "https://h.example.org", "maxPayloadBytes": 0})).is_err());
        // Kein Klartext-Geheimnis.
        assert!(build(json!({"url": "https://h.example.org", "secret": {"value": "x"}})).is_err());
        assert!(
            build(json!({"url": "https://h.example.org", "secret": {"credential": "c"}})).is_ok()
        );
        assert!(build(json!({"url": "https://h.example.org", "secret": {"env": "AIGW_TEST_UNSET_WEBHOOK_SECRET"}})).is_err());
        let p = build(json!({"url": "https://h.example.org", "phases": ["request", "response"]}))
            .unwrap();
        assert!(p.needs_buffered_output());
        assert!(!p.phases().chunk);
        assert_eq!(
            Factory::default().default_timeout(),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn modify_guards() {
        let orig = ChatRequest {
            model: "m".into(),
            stream: Some(true),
            messages: vec![aigw_core::ir::Message::text(
                aigw_core::ir::Role::User,
                "hi",
            )],
            ..ChatRequest::default()
        };
        let mut other = orig.clone();
        other.model = "x".into();
        assert!(accept_request(&orig, other).is_err());
        let mut other = orig.clone();
        other.stream = Some(false);
        assert!(accept_request(&orig, other).is_err());
        let mut other = orig.clone();
        other.hints.upstream_headers = vec![("x-evil".into(), "1".into())];
        assert!(
            accept_request(&orig, other)
                .unwrap()
                .hints
                .upstream_headers
                .is_empty()
        );
        assert!(reject(Some(500), None, None).is_err());
        let Ok(Flow::Reject(e)) = reject(Some(403), Some("nein".into()), None) else {
            panic!()
        };
        assert_eq!(e.status(), 403);
        assert_eq!(e.code.as_deref(), Some("webhook_rejected"));
        // F152: exakter Status (422 bleibt 422, nicht 400).
        let Ok(Flow::Reject(e)) = reject(Some(422), None, Some("policy".into())) else {
            panic!()
        };
        assert_eq!(e.status(), 422);
        assert_eq!(e.kind, ErrorKind::InvalidRequest);
    }
}
