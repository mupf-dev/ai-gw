// SPDX-FileCopyrightText: 2026 ai-gw contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Plugin-Instanz einer Bindung: wandelt Kontexte in WIT-Werte, führt die Komponente aus und
//! wendet das Ergebnis an.

use std::sync::Arc;
use std::time::Instant;

use aigw_core::ir::{ApiError, ChatRequest, ChatResponse, ErrorKind, StreamEvent};
use aigw_core::wire::WireFormat;
use aigw_plugins::{
    CallInfo, ChunkCtx, ChunkFlow, Flow, Hook, INTERNAL_META_PREFIX, Meta, MetaExt as _, Phases,
    Plugin, PluginError, ReqCtx, RespCtx, ResponseHeaders,
};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use wasmtime::component::Val;

use crate::engine::{CallError, CallLimits, Compiled, Export, WasmRuntime};

/// Höchstzahl Header je Liste in der Ausgabe.
const MAX_HEADERS: usize = 32;
/// Code von Ablehnungen ohne eigenen Code.
const DEFAULT_CODE: &str = "wasm_rejected";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Request,
    Response,
    Chunk,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Chunk => "chunk",
        }
    }
}

/// Ablehnung aus dem Modul.
#[derive(Debug, Clone)]
struct Rejection {
    status: u16,
    code: Option<String>,
    message: String,
}

#[derive(Debug, Clone)]
enum Action {
    Pass,
    Modify(String),
    Block(Rejection),
    Reject(Rejection),
    Respond(String),
    Drop,
}

impl Action {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Modify(_) => "modify",
            Self::Block(_) => "block",
            Self::Reject(_) => "reject",
            Self::Respond(_) => "respond",
            Self::Drop => "drop",
        }
    }
}

#[derive(Debug, Clone)]
struct Output {
    action: Action,
    meta: Option<String>,
    upstream_headers: Vec<(String, String)>,
    response_headers: Vec<(String, String)>,
}

/// Instanz einer Bindung.
pub(crate) struct WasmPlugin {
    pub(crate) rt: Arc<WasmRuntime>,
    pub(crate) module: Arc<str>,
    pub(crate) compiled: Arc<Compiled>,
    pub(crate) config_json: Arc<str>,
    pub(crate) include: Vec<HeaderName>,
    pub(crate) phases: Phases,
    pub(crate) limits: CallLimits,
}

impl std::fmt::Debug for WasmPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmPlugin")
            .field("module", &self.module)
            .field("sha256", &self.compiled.sha256)
            .field("phases", &self.phases)
            .finish_non_exhaustive()
    }
}

fn s(v: impl Into<String>) -> Val {
    Val::String(v.into())
}

fn opt_s(v: Option<&str>) -> Val {
    Val::Option(v.map(|x| Box::new(s(x))))
}

fn header_list(h: Vec<(String, String)>) -> Val {
    Val::List(
        h.into_iter()
            .map(|(n, v)| Val::Record(vec![("name".into(), s(n)), ("value".into(), s(v))]))
            .collect(),
    )
}

fn ingress(w: WireFormat) -> String {
    match w {
        WireFormat::OpenAiChat => "openai".into(),
        WireFormat::AnthropicMessages => "anthropic".into(),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
    }
}

fn call_info(info: &CallInfo) -> Val {
    let key = info.key.as_deref();
    Val::Record(vec![
        ("request-id".into(), s(info.request_id.as_str())),
        ("ingress".into(), s(ingress(info.ingress))),
        ("model-group".into(), opt_s(info.model_group.as_deref())),
        ("stream".into(), Val::Bool(info.stream)),
        (
            "key".into(),
            opt_s(key.map(|k| k.key.metadata.name.as_str())),
        ),
        (
            "team".into(),
            opt_s(
                key.and_then(|k| k.team.as_deref())
                    .map(|t| t.metadata.name.as_str()),
            ),
        ),
        (
            "organization".into(),
            opt_s(
                key.and_then(|k| k.organization.as_deref())
                    .map(|o| o.metadata.name.as_str()),
            ),
        ),
    ])
}

/// Beschreibung eines Moduls → Phasen.
pub(crate) fn parse_phases(v: &Val) -> Result<Phases, String> {
    match v {
        Val::Result(Ok(Some(b))) => match &**b {
            Val::Flags(f) => Ok(Phases {
                request: f.iter().any(|x| x == "request"),
                response: f.iter().any(|x| x == "response"),
                chunk: f.iter().any(|x| x == "chunk"),
            }),
            other => Err(format!("describe: unerwarteter Typ {other:?}")),
        },
        Val::Result(Err(Some(b))) => match &**b {
            Val::String(m) => Err(format!("Konfiguration vom Modul abgelehnt: {m}")),
            _ => Err("Konfiguration vom Modul abgelehnt".into()),
        },
        other => Err(format!("describe: unerwarteter Typ {other:?}")),
    }
}

fn field<'v>(fields: &'v [(String, Val)], name: &str) -> Result<&'v Val, CallError> {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v)
        .ok_or_else(|| CallError::Output(format!("Feld {name} fehlt")))
}

fn as_string(v: &Val) -> Result<String, CallError> {
    match v {
        Val::String(x) => Ok(x.clone()),
        other => Err(CallError::Output(format!(
            "Text erwartet, erhalten {other:?}"
        ))),
    }
}

fn headers_from(v: &Val) -> Result<Vec<(String, String)>, CallError> {
    let Val::List(items) = v else {
        return Err(CallError::Output("Header-Liste erwartet".into()));
    };
    if items.len() > MAX_HEADERS {
        return Err(CallError::Output(format!(
            "höchstens {MAX_HEADERS} Header je Liste"
        )));
    }
    items
        .iter()
        .map(|h| match h {
            Val::Record(f) => Ok((
                as_string(field(f, "name")?)?,
                as_string(field(f, "value")?)?,
            )),
            _ => Err(CallError::Output("Header erwartet".into())),
        })
        .collect()
}

fn rejection(v: Option<&Val>) -> Result<Rejection, CallError> {
    let Some(Val::Record(f)) = v else {
        return Err(CallError::Output("rejection erwartet".into()));
    };
    let status = match field(f, "status")? {
        Val::U16(x) => *x,
        _ => 400,
    };
    let code = match field(f, "code")? {
        Val::Option(Some(c)) => Some(as_string(c)?),
        _ => None,
    };
    Ok(Rejection {
        status,
        code,
        message: as_string(field(f, "message")?)?,
    })
}

fn parse_output(v: Val) -> Result<Output, CallError> {
    let fields = match v {
        Val::Result(Ok(Some(b))) => match *b {
            Val::Record(f) => f,
            other => return Err(CallError::Output(format!("output erwartet: {other:?}"))),
        },
        Val::Result(Err(Some(b))) => {
            return Err(CallError::Guest(match *b {
                Val::String(m) => truncate(m, 1024),
                _ => "unbekannter Fehler".into(),
            }));
        }
        other => return Err(CallError::Output(format!("result erwartet: {other:?}"))),
    };
    let action = match field(&fields, "action")? {
        Val::Variant(name, payload) => {
            let p = payload.as_deref();
            match name.as_str() {
                "pass" => Action::Pass,
                "drop" => Action::Drop,
                "modify" => Action::Modify(as_string(
                    p.ok_or_else(|| CallError::Output("modify ohne Inhalt".into()))?,
                )?),
                "respond" => Action::Respond(as_string(
                    p.ok_or_else(|| CallError::Output("respond ohne Inhalt".into()))?,
                )?),
                "block" => Action::Block(rejection(p)?),
                "reject" => Action::Reject(rejection(p)?),
                other => return Err(CallError::Output(format!("unbekannte Aktion {other}"))),
            }
        }
        other => return Err(CallError::Output(format!("action erwartet: {other:?}"))),
    };
    let meta = match field(&fields, "meta")? {
        Val::Option(Some(m)) => Some(as_string(m)?),
        _ => None,
    };
    Ok(Output {
        action,
        meta,
        upstream_headers: headers_from(field(&fields, "upstream-headers")?)?,
        response_headers: headers_from(field(&fields, "response-headers")?)?,
    })
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

fn api_error(r: Rejection) -> ApiError {
    let status = if (400..=499).contains(&r.status) {
        r.status
    } else {
        400
    };
    let mut message = truncate(r.message, 1024);
    if message.trim().is_empty() {
        "Anfrage durch WASM-Plugin abgelehnt".clone_into(&mut message);
    }
    let code = r
        .code
        .filter(|c| {
            !c.is_empty()
                && c.len() <= 64
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
        })
        .unwrap_or_else(|| DEFAULT_CODE.to_owned());
    ApiError::new(ErrorKind::from_status(status), message)
        .with_code(code)
        .with_status(status)
}

/// Übernimmt neues Meta (externe Schlüssel); interne Schlüssel (`aigw.*`) bleiben erhalten
/// und dürfen vom Modul nicht gesetzt werden.
fn apply_meta(meta: &mut Meta, new: Option<String>) -> Result<(), CallError> {
    let Some(new) = new else {
        return Ok(());
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&new) else {
        return Err(CallError::Output("meta: JSON-Objekt erwartet".into()));
    };
    meta.retain(|k, _| k.starts_with(INTERNAL_META_PREFIX));
    for (k, v) in obj {
        if !k.starts_with(INTERNAL_META_PREFIX) {
            meta.insert(k, v);
        }
    }
    Ok(())
}

fn apply_headers(
    target: &mut HeaderMap,
    list: Vec<(String, String)>,
    allowed: impl Fn(&HeaderName) -> bool,
) -> Result<(), CallError> {
    for (n, v) in list {
        let name = HeaderName::from_bytes(n.as_bytes())
            .map_err(|_| CallError::Output(format!("ungültiger Header-Name {n:?}")))?;
        let value = HeaderValue::from_str(&v)
            .map_err(|_| CallError::Output(format!("ungültiger Wert für Header {name}")))?;
        if !allowed(&name) {
            return Err(CallError::Output(format!("Header {name} ist gesperrt")));
        }
        target.insert(name, value);
    }
    Ok(())
}

fn apply_response_headers(
    target: &mut ResponseHeaders,
    list: Vec<(String, String)>,
) -> Result<(), CallError> {
    // Geschützte Header filtert `ResponseHeaders::apply` am Ende selbst.
    apply_headers(&mut target.set, list, |_| true)
}

/// Prüft eine geänderte Anfrage und übernimmt, was ein Plugin nicht ändern darf.
fn accept_request(orig: &ChatRequest, mut new: ChatRequest) -> Result<ChatRequest, CallError> {
    if new.model != orig.model {
        return Err(CallError::Output(
            "modify: `model` darf nicht geändert werden".into(),
        ));
    }
    if new.is_stream() != orig.is_stream() {
        return Err(CallError::Output(
            "modify: `stream` darf nicht geändert werden".into(),
        ));
    }
    if new.messages.is_empty() {
        return Err(CallError::Output(
            "modify: `messages` darf nicht leer sein".into(),
        ));
    }
    new.stream = orig.stream;
    new.stream_options.clone_from(&orig.stream_options);
    new.hints.clone_from(&orig.hints);
    new.source = orig.source;
    Ok(new)
}

fn accept_response(
    info: &CallInfo,
    model: &str,
    mut r: ChatResponse,
) -> Result<ChatResponse, CallError> {
    if r.choices.is_empty() {
        return Err(CallError::Output("Antwort ohne `choices`".into()));
    }
    if r.id.is_empty() {
        r.id = format!("aigw-wasm-{}", info.request_id);
    }
    if r.model.is_empty() {
        model.clone_into(&mut r.model);
    }
    Ok(r)
}

/// Ein vorbereiteter Aufruf (eigenständig, für `spawn_blocking`).
struct Job {
    rt: Arc<WasmRuntime>,
    compiled: Arc<Compiled>,
    module: Arc<str>,
    phase: Phase,
    input: Val,
    limits: CallLimits,
}

impl Job {
    fn run(self) -> Result<Output, CallError> {
        let started = Instant::now();
        let res = self
            .rt
            .call(
                &self.compiled,
                Export::Handle,
                &self.module,
                self.input,
                self.limits,
            )
            .and_then(|(v, fuel)| parse_output(v).map(|o| (o, fuel)));
        let m = self.rt.metrics();
        match res {
            Ok((o, fuel)) => {
                m.record_call(
                    &self.module,
                    self.phase.as_str(),
                    o.action.as_str(),
                    started.elapsed(),
                    fuel,
                );
                Ok(o)
            }
            Err(e) => {
                m.record_call(
                    &self.module,
                    self.phase.as_str(),
                    "error",
                    started.elapsed(),
                    0,
                );
                m.record_error(&self.module, e.kind());
                Err(e)
            }
        }
    }
}

impl WasmPlugin {
    fn job(
        &self,
        phase: Phase,
        info: &CallInfo,
        payload: String,
        meta: &Meta,
        headers: Vec<(String, String)>,
    ) -> Job {
        let meta = serde_json::to_string(&Value::Object(meta.external_view()))
            .unwrap_or_else(|_| "{}".to_owned());
        let input = Val::Record(vec![
            ("phase".into(), Val::Enum(phase.as_str().into())),
            ("info".into(), call_info(info)),
            ("config".into(), s(&*self.config_json)),
            ("payload".into(), s(payload)),
            ("meta".into(), s(meta)),
            ("headers".into(), header_list(headers)),
        ]);
        Job {
            rt: Arc::clone(&self.rt),
            compiled: Arc::clone(&self.compiled),
            module: Arc::clone(&self.module),
            phase,
            input,
            limits: self.limits,
        }
    }

    fn err(&self, e: &CallError) -> PluginError {
        PluginError::new(e.message(&self.module))
    }

    fn client_headers(&self, h: &HeaderMap) -> Vec<(String, String)> {
        self.include
            .iter()
            .filter_map(|name| {
                h.get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(|v| (name.as_str().to_owned(), v.to_owned()))
            })
            .collect()
    }
}

async fn run_async(job: Job) -> Result<Output, CallError> {
    tokio::task::spawn_blocking(move || job.run())
        .await
        .unwrap_or_else(|e| Err(CallError::Trap(format!("Ausführung abgebrochen: {e}"))))
}

fn request_json(req: &ChatRequest) -> Result<String, PluginError> {
    let mut v = serde_json::to_value(req).map_err(|e| PluginError::new(format!("Payload: {e}")))?;
    // Upstream-Header früherer Plugins können Zugangsdaten tragen.
    if let Some(h) = v.get_mut("hints").and_then(Value::as_object_mut) {
        h.remove("upstream_headers");
    }
    serde_json::to_string(&v).map_err(|e| PluginError::new(format!("Payload: {e}")))
}

impl Plugin for WasmPlugin {
    fn name(&self) -> &'static str {
        aigw_plugins::builtin::wasm::NAME
    }

    fn phases(&self) -> Phases {
        self.phases
    }

    fn needs_buffered_output(&self) -> bool {
        // Ohne Stream-Haken braucht die Antwort-Phase die ganze Antwort.
        self.phases.response && !self.phases.chunk
    }

    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let payload = match request_json(&ctx.request) {
            Ok(p) => p,
            Err(e) => return Hook::Ready(Err(e)),
        };
        let headers = self.client_headers(ctx.client_headers);
        let job = self.job(Phase::Request, ctx.info, payload, &ctx.meta, headers);
        Hook::future(async move {
            let out = run_async(job).await.map_err(|e| self.err(&e))?;
            self.apply_request(ctx, out).map_err(|e| {
                self.rt.metrics().record_error(&self.module, e.kind());
                self.err(&e)
            })
        })
    }

    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let payload = match serde_json::to_string(&ctx.response) {
            Ok(p) => p,
            Err(e) => return Hook::Ready(Err(PluginError::new(format!("Payload: {e}")))),
        };
        let job = self.job(Phase::Response, ctx.info, payload, ctx.meta, Vec::new());
        Hook::future(async move {
            let out = run_async(job).await.map_err(|e| self.err(&e))?;
            self.apply_response(ctx, out).map_err(|e| {
                self.rt.metrics().record_error(&self.module, e.kind());
                self.err(&e)
            })
        })
    }

    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let payload =
            serde_json::to_string(&*ev).map_err(|e| PluginError::new(format!("Payload: {e}")))?;
        let job = self.job(Phase::Chunk, ctx.info, payload, ctx.meta, Vec::new());
        let out = crate::run_blocking(move || job.run()).map_err(|e| self.err(&e))?;
        self.apply_chunk(ctx, ev, out).map_err(|e| {
            self.rt.metrics().record_error(&self.module, e.kind());
            self.err(&e)
        })
    }
}

impl WasmPlugin {
    fn apply_request(&self, ctx: &mut ReqCtx<'_>, out: Output) -> Result<Flow, CallError> {
        apply_meta(&mut ctx.meta, out.meta)?;
        apply_headers(&mut ctx.upstream_headers, out.upstream_headers, |n| {
            !aigw_plugins::headers::is_forbidden_upstream(n)
        })?;
        apply_response_headers(&mut ctx.response_headers, out.response_headers)?;
        Ok(match out.action {
            Action::Pass => Flow::Continue,
            Action::Modify(json) => {
                let new: ChatRequest = serde_json::from_str(&json).map_err(|e| {
                    CallError::Output(format!("modify: keine gültige Anfrage: {e}"))
                })?;
                ctx.request = accept_request(&ctx.request, new)?;
                Flow::Continue
            }
            Action::Respond(json) => {
                let r: ChatResponse = serde_json::from_str(&json).map_err(|e| {
                    CallError::Output(format!("respond: keine gültige Antwort: {e}"))
                })?;
                let model = ctx.request.model.clone();
                Flow::Respond(Box::new(accept_response(ctx.info, &model, r)?))
            }
            Action::Block(r) => Flow::Block(api_error(r)),
            Action::Reject(r) => Flow::Reject(api_error(r)),
            Action::Drop => {
                return Err(CallError::Output(
                    "drop gibt es nur in der Stream-Phase".into(),
                ));
            }
        })
    }

    fn apply_response(&self, ctx: &mut RespCtx<'_>, out: Output) -> Result<Flow, CallError> {
        apply_meta(ctx.meta, out.meta)?;
        if !out.upstream_headers.is_empty() {
            return Err(CallError::Output(
                "upstream-headers gibt es nur in der Anfrage-Phase".into(),
            ));
        }
        apply_response_headers(ctx.response_headers, out.response_headers)?;
        Ok(match out.action {
            Action::Pass => Flow::Continue,
            Action::Modify(json) | Action::Respond(json) => {
                let r: ChatResponse = serde_json::from_str(&json).map_err(|e| {
                    CallError::Output(format!("modify: keine gültige Antwort: {e}"))
                })?;
                let model = ctx.response.model.clone();
                ctx.response = accept_response(ctx.info, &model, r)?;
                Flow::Continue
            }
            Action::Block(r) => Flow::Block(api_error(r)),
            Action::Reject(r) => Flow::Reject(api_error(r)),
            Action::Drop => {
                return Err(CallError::Output(
                    "drop gibt es nur in der Stream-Phase".into(),
                ));
            }
        })
    }

    fn apply_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
        out: Output,
    ) -> Result<ChunkFlow, CallError> {
        apply_meta(ctx.meta, out.meta)?;
        if !out.upstream_headers.is_empty() || !out.response_headers.is_empty() {
            return Err(CallError::Output(
                "Header können im Stream nicht mehr gesetzt werden".into(),
            ));
        }
        Ok(match out.action {
            Action::Pass => ChunkFlow::Continue,
            Action::Drop => ChunkFlow::Drop,
            Action::Modify(json) => {
                *ev = serde_json::from_str(&json).map_err(|e| {
                    CallError::Output(format!("modify: kein gültiges Stream-Ereignis: {e}"))
                })?;
                ChunkFlow::Continue
            }
            Action::Block(r) => ChunkFlow::Block(api_error(r)),
            Action::Reject(r) => ChunkFlow::Reject(api_error(r)),
            Action::Respond(_) => {
                return Err(CallError::Output(
                    "respond gibt es nur in der Anfrage-Phase".into(),
                ));
            }
        })
    }
}

/// Beschreibt ein Modul (Konfiguration prüfen, Phasen) – blockierend.
pub(crate) fn describe(
    rt: &Arc<WasmRuntime>,
    compiled: &Compiled,
    module: &Arc<str>,
    config_json: &str,
    limits: CallLimits,
) -> Result<Phases, String> {
    let rt2 = Arc::clone(rt);
    let arg = s(config_json);
    let res = crate::run_blocking(|| rt2.call(compiled, Export::Describe, module, arg, limits));
    match res {
        Ok((v, _)) => parse_phases(&v),
        Err(e) => {
            rt.metrics().record_error(module, e.kind());
            Err(e.message(module))
        }
    }
}
