// SPDX-FileCopyrightText: 2026 mupf-dev
// SPDX-License-Identifier: FSL-1.1-ALv2

//! Tests der Plugin-Kette: Reihenfolge, Ebenen-Vererbung, Fehlermodi, Zeitlimits,
//! Stream-Haken, ungültige Bindungen und Hot Reload.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use aigw_core::ir::{
    ApiError, ChatRequest, ChatResponse, Choice, Content, ErrorKind, Message, Role, StopReason,
    StreamEvent,
};
use aigw_core::resource::{
    ApiKey, ApiKeySpec, ApiKeyState, ApiKeyStatus, JsonObject, Object, ObjectMeta,
    OrganizationSpec, PluginBinding, PluginBindingSpec, Scope, ScopeLevel, TeamSpec,
};
use aigw_core::snapshot::{KeyCtx, KeyHash, Snapshot, SnapshotHandle};
use aigw_core::wire::WireFormat;
use aigw_plugins::{
    CallInfo, Chain, ChunkCtx, ChunkFlow, ChunkMode, ChunkRunner, Entry, FailMode, Flow,
    GUARDRAIL_HEADER, Hook, IndexOptions, Meta, OutputRejectMode, Phases, Plugin, PluginChains,
    PluginError, PluginFactory, PluginIndex, PluginStream, Registry, ReqCtx, RespCtx,
    ResponseHeaders, TextWindow,
};
use futures_util::StreamExt as _;
use http::HeaderMap;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Test-Plugins
// ---------------------------------------------------------------------------------------------

/// Hängt seinen Namen an `meta.trace` an (Reihenfolge prüfen).
struct Tag(String);

impl Plugin for Tag {
    fn name(&self) -> &'static str {
        "tag"
    }
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let trace = ctx
            .meta
            .entry("trace")
            .or_insert_with(|| Value::Array(vec![]));
        if let Value::Array(a) = trace {
            a.push(Value::String(self.0.clone()));
        }
        Hook::ok(Flow::Continue)
    }
}

/// Fabrik für `tag` (Konfiguration: `{label: "…"}`) und `fail` (Fehler zur Laufzeit).
struct TestFactory(&'static str);

impl PluginFactory for TestFactory {
    fn name(&self) -> &'static str {
        self.0
    }
    fn build(&self, config: &Value) -> Result<Arc<dyn Plugin>, String> {
        match self.0 {
            "tag" => {
                let label = config
                    .get("label")
                    .and_then(Value::as_str)
                    .ok_or("label fehlt")?;
                Ok(Arc::new(Tag(label.to_owned())))
            }
            _ => Ok(Arc::new(Failing)),
        }
    }
}

struct Failing;

impl Plugin for Failing {
    fn name(&self) -> &'static str {
        "fail"
    }
    fn phases(&self) -> Phases {
        Phases::ALL
    }
    fn on_request<'a>(&'a self, _ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::Ready(Err(PluginError::new("kaputt")))
    }
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        _ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        Err(PluginError::new("kaputt"))
    }
}

/// Asynchron und zu langsam.
struct Slow;

impl Plugin for Slow {
    fn name(&self) -> &'static str {
        "slow"
    }
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::future(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            ctx.request.max_tokens = Some(1);
            Ok(Flow::Continue)
        })
    }
}

/// Asynchron, schnell, setzt max_tokens.
struct Quick;

impl Plugin for Quick {
    fn name(&self) -> &'static str {
        "quick"
    }
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        Hook::future(async move {
            tokio::task::yield_now().await;
            ctx.request.max_tokens = Some(42);
            Ok(Flow::Continue)
        })
    }
}

/// Versucht eine Modell-Umleitung.
struct Redirect;

impl Plugin for Redirect {
    fn name(&self) -> &'static str {
        "redirect"
    }
    fn phases(&self) -> Phases {
        Phases::REQUEST
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        ctx.request.model = "teuer".into();
        Hook::ok(Flow::Continue)
    }
}

/// Ersetzt „geheim“ durch „******“ – auch über Chunk-Grenzen hinweg: hält die letzten
/// `len("geheim") - 1` Bytes zurück (Gleitfenster), gibt den Rest am Ende aus.
struct Redact;

const WORD: &str = "geheim";

#[derive(Default)]
struct RedactState {
    pending: String,
    index: u32,
}

impl Plugin for Redact {
    fn name(&self) -> &'static str {
        "redact"
    }
    fn phases(&self) -> Phases {
        Phases {
            request: false,
            response: false,
            chunk: true,
        }
    }
    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        let StreamEvent::TextDelta { index, text } = ev else {
            // Vor Nicht-Text-Ereignissen den Rest ausgeben.
            let st = ctx.state::<RedactState>();
            if !st.pending.is_empty() {
                let text = std::mem::take(&mut st.pending).replace(WORD, "******");
                let index = st.index;
                ctx.emit(StreamEvent::TextDelta { index, text });
            }
            return Ok(ChunkFlow::Continue);
        };
        let st = ctx.state::<RedactState>();
        st.index = *index;
        st.pending.push_str(text);
        st.pending = st.pending.replace(WORD, "******");
        let keep = WORD.len() - 1;
        if st.pending.len() <= keep {
            return Ok(ChunkFlow::Drop);
        }
        let mut cut = st.pending.len() - keep;
        while !st.pending.is_char_boundary(cut) {
            cut -= 1;
        }
        let rest = st.pending.split_off(cut);
        *text = std::mem::replace(&mut st.pending, rest);
        Ok(ChunkFlow::Continue)
    }
    fn on_stream_end(&self, ctx: &mut ChunkCtx<'_>) -> Result<ChunkFlow, PluginError> {
        let st = ctx.state::<RedactState>();
        if !st.pending.is_empty() {
            let text = std::mem::take(&mut st.pending);
            let index = st.index;
            ctx.emit(StreamEvent::TextDelta { index, text });
        }
        Ok(ChunkFlow::Continue)
    }
}

/// Beendet den Stream, sobald im Gleitfenster „STOP“ steht.
struct Guard;

impl Plugin for Guard {
    fn name(&self) -> &'static str {
        "guard"
    }
    fn phases(&self) -> Phases {
        Phases {
            request: false,
            response: false,
            chunk: true,
        }
    }
    fn on_chunk(
        &self,
        ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        if let StreamEvent::TextDelta { text, .. } = ev {
            let w = ctx.state::<TextWindow>();
            if w.capacity() == 0 {
                w.set_capacity(16);
            }
            w.push(text);
            if w.as_str().contains("STOP") {
                return Ok(ChunkFlow::Reject(
                    ApiError::new(ErrorKind::InvalidRequest, "Ausgabe blockiert")
                        .with_code("output_blocked"),
                ));
            }
        }
        Ok(ChunkFlow::Continue)
    }
}

// ---------------------------------------------------------------------------------------------
// Hilfen
// ---------------------------------------------------------------------------------------------

fn registry() -> Registry {
    let mut r = Registry::with_builtin();
    r.register(Arc::new(TestFactory("tag")));
    r.register(Arc::new(TestFactory("fail")));
    r
}

fn obj(v: Value) -> JsonObject {
    match v {
        Value::Object(m) => m,
        _ => JsonObject::new(),
    }
}

fn binding(
    name: &str,
    level: ScopeLevel,
    target: Option<&str>,
    plugin: &str,
    order: i32,
    config: Value,
) -> Arc<PluginBinding> {
    Arc::new(Object {
        id: uuid::Uuid::now_v7(),
        metadata: ObjectMeta::named(name),
        spec: PluginBindingSpec {
            inherit: None,
            locked: false,
            fail_mode: None,
            timeout_ms: None,
            output_reject_mode: None,
            scope: Scope {
                level,
                target: target.map(str::to_owned),
            },
            plugin: plugin.into(),
            order,
            enabled: true,
            config: obj(config),
        },
    })
}

fn tag(
    name: &str,
    level: ScopeLevel,
    target: Option<&str>,
    order: i32,
    extra: Value,
) -> Arc<PluginBinding> {
    let mut c = obj(json!({"label": name}));
    c.extend(obj(extra));
    binding(name, level, target, "tag", order, Value::Object(c))
}

fn key(name: &str, team: Option<&str>) -> Arc<KeyCtx> {
    let org = Arc::new(Object {
        id: uuid::Uuid::now_v7(),
        metadata: ObjectMeta::named("acme"),
        spec: OrganizationSpec::default(),
    });
    let team = team.map(|t| {
        Arc::new(Object {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named(t),
            spec: TeamSpec {
                organization: "acme".into(),
                ..TeamSpec::default()
            },
        })
    });
    Arc::new(KeyCtx {
        key: ApiKey {
            id: uuid::Uuid::now_v7(),
            metadata: ObjectMeta::named(name),
            spec: ApiKeySpec {
                team: team.as_ref().map(|t| t.metadata.name.clone()),
                ..ApiKeySpec::default()
            },
            status: ApiKeyStatus {
                prefix: "sk-test".into(),
                state: ApiKeyState::Active,
                last_used_at: None,
            },
        },
        organization: team.as_ref().map(|_| org),
        team,
        expires_at: None,
    })
}

fn snapshot(version: i64, keys: &[&Arc<KeyCtx>], bindings: Vec<Arc<PluginBinding>>) -> Snapshot {
    let mut s = Snapshot {
        version,
        plugin_bindings: bindings,
        ..Snapshot::default()
    };
    for (i, k) in keys.iter().enumerate() {
        let mut h = [0u8; 32];
        h[0] = u8::try_from(i).unwrap();
        s.keys.insert(KeyHash(h), Arc::clone(k));
    }
    s
}

async fn trace(chain: &Chain) -> (Flow, Vec<String>) {
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let headers = HeaderMap::new();
    let mut ctx = ReqCtx {
        info: &info,
        request: ChatRequest {
            model: "m".into(),
            ..ChatRequest::default()
        },
        client_headers: &headers,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    let flow = chain.run_request(&mut ctx).await;
    let t = ctx
        .meta
        .get("trace")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (flow, t)
}

// ---------------------------------------------------------------------------------------------
// Reihenfolge und Vererbung
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn order_and_inheritance_across_levels() {
    let k1 = key("k1", Some("platform"));
    let k2 = key("k2", Some("research"));
    let k3 = key("k3", None);
    let snap = snapshot(
        7,
        &[&k1, &k2, &k3],
        vec![
            tag("g-audit", ScopeLevel::Global, None, 100, json!({})),
            tag("g-early", ScopeLevel::Global, None, -10, json!({})),
            tag(
                "o-acme",
                ScopeLevel::Organization,
                Some("acme"),
                50,
                json!({}),
            ),
            // Team „platform“ ersetzt die geerbte tag-Bindung… (gleiches Plugin `tag`)
            tag(
                "t-platform",
                ScopeLevel::Team,
                Some("platform"),
                20,
                json!({"inherit": "override"}),
            ),
            tag(
                "t-research",
                ScopeLevel::Team,
                Some("research"),
                60,
                json!({}),
            ),
            tag("k-k2", ScopeLevel::ApiKey, Some("k2"), 0, json!({})),
            // Modellgruppe „gpt“ schaltet `tag` ab.
            binding(
                "mg-off",
                ScopeLevel::ModelGroup,
                Some("gpt"),
                "tag",
                0,
                json!({"inherit": "disable"}),
            ),
            tag(
                "mg-claude",
                ScopeLevel::ModelGroup,
                Some("claude"),
                55,
                json!({}),
            ),
        ],
    );
    let idx = PluginIndex::build(&snap, &registry(), IndexOptions::default());
    assert_eq!(idx.version(), 7);

    let names = |k: Option<&KeyCtx>, g: Option<&str>| -> Vec<String> {
        idx.chain(k, g).bindings().map(str::to_owned).collect()
    };
    // Ohne Key: nur global, nach order.
    assert_eq!(names(None, None), ["g-early", "g-audit"]);
    // k1 (Team platform): override entfernt global + org, übrig bleibt die Team-Bindung.
    assert_eq!(names(Some(&k1), None), ["t-platform"]);
    // k2 (Team research): alles angehängt, sortiert nach order über Ebenen hinweg.
    assert_eq!(
        names(Some(&k2), None),
        ["g-early", "k-k2", "o-acme", "t-research", "g-audit"]
    );
    // k2 + Modellgruppe claude: zusätzlich mg-claude an Position order 55.
    assert_eq!(
        names(Some(&k2), Some("claude")),
        [
            "g-early",
            "k-k2",
            "o-acme",
            "mg-claude",
            "t-research",
            "g-audit"
        ]
    );
    // Modellgruppe gpt: tag ganz abgeschaltet.
    assert!(idx.chain(Some(&k2), Some("gpt")).is_empty());
    // Key ohne Team/Org-Bindungen: nur global; unbekannte Modellgruppe: wie ohne.
    assert_eq!(names(Some(&k3), Some("unbekannt")), ["g-early", "g-audit"]);

    // Die Kette läuft in dieser Reihenfolge.
    let (flow, t) = trace(&idx.chain(Some(&k2), Some("claude"))).await;
    assert_eq!(flow, Flow::Continue);
    assert_eq!(
        t,
        [
            "g-early",
            "k-k2",
            "o-acme",
            "mg-claude",
            "t-research",
            "g-audit"
        ]
    );
}

#[tokio::test]
async fn locked_bindings_cannot_be_overridden_or_disabled() {
    let k = key("k", Some("platform"));
    let snap = snapshot(
        1,
        &[&k],
        vec![
            tag(
                "org-locked",
                ScopeLevel::Organization,
                Some("acme"),
                0,
                json!({"locked": true}),
            ),
            tag(
                "team-override",
                ScopeLevel::Team,
                Some("platform"),
                1,
                json!({"inherit": "override"}),
            ),
            binding(
                "key-off",
                ScopeLevel::ApiKey,
                Some("k"),
                "tag",
                0,
                json!({"inherit": "disable"}),
            ),
        ],
    );
    let idx = PluginIndex::build(&snap, &registry(), IndexOptions::default());
    let names: Vec<_> = idx
        .chain(Some(&k), None)
        .bindings()
        .map(str::to_owned)
        .collect();
    assert_eq!(names, ["org-locked"]);
}

#[test]
fn chains_are_shared_between_keys_with_same_prefix() {
    let a = key("a", Some("platform"));
    let b = key("b", Some("platform"));
    let snap = snapshot(
        1,
        &[&a, &b],
        vec![tag("t", ScopeLevel::Team, Some("platform"), 0, json!({}))],
    );
    let idx = PluginIndex::build(&snap, &registry(), IndexOptions::default());
    assert!(Arc::ptr_eq(
        &idx.chain(Some(&a), None),
        &idx.chain(Some(&b), None)
    ));
}

// ---------------------------------------------------------------------------------------------
// Ungültige Bindungen
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn invalid_bindings_are_skipped_or_block_the_chain() {
    let snap = snapshot(
        1,
        &[],
        vec![
            tag("ok", ScopeLevel::Global, None, 0, json!({})),
            // Fehlendes label → übersprungen (Standard).
            binding("broken-open", ScopeLevel::Global, None, "tag", 1, json!({})),
            binding(
                "unknown",
                ScopeLevel::Global,
                None,
                "gibt-es-nicht",
                2,
                json!({}),
            ),
            binding(
                "bad-header",
                ScopeLevel::Global,
                None,
                "header",
                3,
                json!({"request": {"set": {"authorization": "x"}}}),
            ),
        ],
    );
    let idx = PluginIndex::build(&snap, &registry(), IndexOptions::default());
    assert_eq!(idx.chain(None, None).bindings().collect::<Vec<_>>(), ["ok"]);

    // Mit failMode: closed sperrt die ungültige Bindung die Kette.
    let mut bindings = snap.plugin_bindings.clone();
    bindings.push(binding(
        "broken-closed",
        ScopeLevel::Global,
        None,
        "header",
        4,
        json!({"failMode": "closed", "request": {"bogus": 1}}),
    ));
    let snap = snapshot(2, &[], bindings);
    let idx = PluginIndex::build(&snap, &registry(), IndexOptions::default());
    let (flow, _) = trace(&idx.chain(None, None)).await;
    match flow {
        Flow::Reject(e) => assert_eq!(e.code.as_deref(), Some("plugin_config_invalid")),
        other => panic!("erwartet Reject, war {other:?}"),
    }

    // Globale Option: alle ungültigen Bindungen sperren.
    let idx = PluginIndex::build(
        &snapshot(
            3,
            &[],
            vec![binding("x", ScopeLevel::Global, None, "nope", 0, json!({}))],
        ),
        &registry(),
        IndexOptions {
            invalid_binding: FailMode::Closed,
        },
    );
    assert!(matches!(
        trace(&idx.chain(None, None)).await.0,
        Flow::Reject(_)
    ));
}

#[test]
fn registry_validates_configs() {
    let r = registry();
    assert!(
        r.validate("header", &obj(json!({"response": {"set": {"x-a": "b"}}})))
            .is_ok()
    );
    assert!(
        r.validate(
            "header",
            &obj(json!({"response": {"set": {"content-type": "b"}}}))
        )
        .is_err()
    );
    assert!(
        r.validate(
            "header",
            &obj(json!({"request": {"forward": ["x-api-key"]}}))
        )
        .is_err()
    );
    assert!(
        r.validate("header", &obj(json!({"inherit": "never"})))
            .is_err()
    );
    assert!(r.validate("nope", &obj(json!({}))).is_err());
}

// ---------------------------------------------------------------------------------------------
// Fehlermodi und Zeitlimits
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn fail_open_and_fail_closed() {
    let open = Chain::new(vec![
        Arc::new(Entry::new("f", Arc::new(Failing)).with_fail_mode(FailMode::Open)),
        Arc::new(Entry::new("t", Arc::new(Tag("t".into())))),
    ]);
    let (flow, t) = trace(&open).await;
    assert_eq!(flow, Flow::Continue);
    assert_eq!(t, ["t"]);

    let closed = Chain::new(vec![
        Arc::new(Entry::new("f", Arc::new(Failing)).with_fail_mode(FailMode::Closed)),
        Arc::new(Entry::new("t", Arc::new(Tag("t".into())))),
    ]);
    match trace(&closed).await.0 {
        Flow::Reject(e) => {
            assert_eq!(e.kind, ErrorKind::Internal);
            assert_eq!(e.code.as_deref(), Some("plugin_error"));
        }
        other => panic!("erwartet Reject, war {other:?}"),
    }
}

#[tokio::test]
async fn async_hooks_respect_timeouts() {
    let run = |mode: FailMode| async move {
        let chain = Chain::new(vec![
            Arc::new(
                Entry::new("slow", Arc::new(Slow))
                    .with_timeout(Duration::from_millis(50))
                    .with_fail_mode(mode),
            ),
            Arc::new(Entry::new("quick", Arc::new(Quick))),
        ]);
        let info = CallInfo::new("rid", WireFormat::OpenAiChat);
        let headers = HeaderMap::new();
        let mut ctx = ReqCtx {
            info: &info,
            request: ChatRequest::default(),
            client_headers: &headers,
            upstream_headers: HeaderMap::new(),
            response_headers: ResponseHeaders::default(),
            meta: Meta::new(),
        };
        let flow = chain.run_request(&mut ctx).await;
        (flow, ctx.request.max_tokens)
    };
    // open: Zeitlimit → übersprungen, das nächste (asynchrone) Plugin läuft.
    assert_eq!(run(FailMode::Open).await, (Flow::Continue, Some(42)));
    // closed: Ablehnung.
    let (flow, _) = run(FailMode::Closed).await;
    assert!(matches!(flow, Flow::Reject(_)));
}

#[tokio::test]
async fn model_redirect_is_blocked() {
    let chain = Chain::new(vec![Arc::new(
        Entry::new("r", Arc::new(Redirect)).with_fail_mode(FailMode::Open),
    )]);
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let headers = HeaderMap::new();
    let mut ctx = ReqCtx {
        info: &info,
        request: ChatRequest {
            model: "billig".into(),
            ..ChatRequest::default()
        },
        client_headers: &headers,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    assert_eq!(chain.run_request(&mut ctx).await, Flow::Continue);
    assert_eq!(ctx.request.model, "billig");
}

// ---------------------------------------------------------------------------------------------
// Stream-Haken
// ---------------------------------------------------------------------------------------------

fn text(s: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index: 0,
        text: s.into(),
    }
}

async fn run_stream(chain: Chain, events: Vec<StreamEvent>) -> Vec<StreamEvent> {
    let runner = ChunkRunner::new(
        Arc::new(chain),
        Arc::new(CallInfo::new("rid", WireFormat::OpenAiChat)),
        Meta::new(),
        ChunkMode::Live,
    );
    PluginStream::new(futures_util::stream::iter(events), runner)
        .collect()
        .await
}

fn joined(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn chunk_plugin_rewrites_across_chunk_boundaries() {
    let chain = Chain::new(vec![Arc::new(Entry::new("redact", Arc::new(Redact)))]);
    let start = StreamEvent::Start {
        id: "x".into(),
        model: "m".into(),
        created: None,
    };
    let out = run_stream(
        chain,
        vec![
            start.clone(),
            text("Das ist ge"),
            text("he"),
            text("im und bleibt geheim!"),
            StreamEvent::Stop {
                reason: StopReason::EndTurn,
                stop_sequence: None,
            },
        ],
    )
    .await;
    assert_eq!(out.first(), Some(&start));
    assert_eq!(joined(&out), "Das ist ****** und bleibt ******!");
    assert!(matches!(out.last(), Some(StreamEvent::Stop { .. })));

    // Ohne Stop-Ereignis: Rest kommt aus on_stream_end.
    let chain = Chain::new(vec![Arc::new(Entry::new("redact", Arc::new(Redact)))]);
    let out = run_stream(chain, vec![text("ge"), text("heim")]).await;
    assert_eq!(joined(&out), "******");
}

#[tokio::test]
async fn chunk_reject_ends_stream_and_later_plugins_still_see_events() {
    // Stream-Haken laufen von innen nach außen (Zwiebel): guard (order 1) vor redact
    // (order 0). Das Fehlerereignis beendet den Stream; was bis dahin zurückgehalten war,
    // kommt nicht mehr ungefiltert heraus.
    let chain = Chain::new(vec![
        Arc::new(Entry::new("redact", Arc::new(Redact))),
        Arc::new(Entry::new("guard", Arc::new(Guard)).with_order(1)),
    ]);
    let out = run_stream(
        chain,
        vec![text("geheim ST"), text("OP danach"), text("nie gesehen")],
    )
    .await;
    assert!(matches!(
        out.last(),
        Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("output_blocked")
    ));
    let all = joined(&out);
    assert!(!all.contains("geheim"), "{all}");
    assert!(!all.contains("nie gesehen"), "{all}");
}

#[tokio::test]
async fn chunk_fail_modes() {
    let open = Chain::new(vec![Arc::new(
        Entry::new("f", Arc::new(Failing)).with_fail_mode(FailMode::Open),
    )]);
    assert_eq!(
        joined(&run_stream(open, vec![text("a"), text("b")]).await),
        "ab"
    );
    let closed = Chain::new(vec![Arc::new(Entry::new("f", Arc::new(Failing)))]);
    let out = run_stream(closed, vec![text("a"), text("b")]).await;
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0], StreamEvent::Error(e) if e.code.as_deref() == Some("plugin_error")));
}

#[test]
fn buffered_mode_picks_chunk_only_plugins() {
    struct Both;
    impl Plugin for Both {
        fn name(&self) -> &'static str {
            "both"
        }
        fn phases(&self) -> Phases {
            Phases::OUTPUT
        }
        fn needs_buffered_output(&self) -> bool {
            true
        }
    }
    let chain = Chain::new(vec![
        Arc::new(Entry::new("both", Arc::new(Both))),
        Arc::new(Entry::new("redact", Arc::new(Redact))),
    ]);
    assert!(chain.needs_buffered_output());
    assert!(chain.has_response());
    assert!(chain.has_chunk(ChunkMode::Live));
    assert!(chain.has_chunk(ChunkMode::Buffered));
    assert!(!chain.has_request());
    let only_both = Chain::new(vec![Arc::new(Entry::new("both", Arc::new(Both)))]);
    assert!(!only_both.has_chunk(ChunkMode::Buffered));
}

// ---------------------------------------------------------------------------------------------
// Hot Reload
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn hot_reload_changes_chains() {
    let k = key("k", Some("platform"));
    let handle = SnapshotHandle::new(snapshot(
        1,
        &[&k],
        vec![tag("a", ScopeLevel::Global, None, 0, json!({}))],
    ));
    let chains = Arc::new(PluginChains::new(registry(), IndexOptions::default()));
    let task = chains.spawn(handle.clone());
    assert_eq!(
        chains
            .chain_for(None, Some(&k), None)
            .bindings()
            .collect::<Vec<_>>(),
        ["a"]
    );
    let before = chains.index();

    // Neue Version über den Handle → Task baut neu.
    handle.store(snapshot(
        2,
        &[&k],
        vec![
            tag("a", ScopeLevel::Global, None, 0, json!({})),
            tag("b", ScopeLevel::Team, Some("platform"), 1, json!({})),
        ],
    ));
    for _ in 0..200 {
        if chains.index().version() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(chains.index().version(), 2);
    assert_eq!(
        chains
            .chain_for(None, Some(&k), None)
            .bindings()
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    // Unveränderte Bindung „a“ wurde wiederverwendet (gleiche Instanz).
    let a_old = Arc::clone(&before.chain(None, None).entries()[0]);
    let a_new = Arc::clone(&chains.index().chain(None, None).entries()[0]);
    assert_eq!(a_old.binding(), a_new.binding());
    assert!(std::ptr::addr_eq(
        Arc::as_ptr(a_old.plugin()),
        Arc::as_ptr(a_new.plugin())
    ));
    drop(task);

    // Anfrage mit neuerem Snapshot als der Index: sofortiger Neuaufbau (ohne Task).
    let snap3 = snapshot(3, &[&k], vec![]);
    assert!(chains.chain_for(Some(&snap3), Some(&k), None).is_empty());
    assert_eq!(chains.index().version(), 3);
}

// ---------------------------------------------------------------------------------------------
// Zwiebelprinzip (F167b), Guardrail-Ablehnung der Ausgabe (F160), Fehlermodus-Metrik (F148)
// ---------------------------------------------------------------------------------------------

/// Protokolliert in allen Phasen seinen Namen (Anfrage: `meta.trace`, Antwort: Text der
/// Antwort, Stream: an jedes Textstück angehängt).
struct Onion(&'static str);

impl Plugin for Onion {
    fn name(&self) -> &'static str {
        self.0
    }
    fn phases(&self) -> Phases {
        Phases::ALL
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let trace = ctx
            .meta
            .entry("trace")
            .or_insert_with(|| Value::Array(vec![]));
        if let Value::Array(a) = trace {
            a.push(Value::String(self.0.into()));
        }
        Hook::ok(Flow::Continue)
    }
    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let c = &mut ctx.response.choices[0].message.content;
        *c = Content::Text(format!("{}{}", c.text(), self.0));
        Hook::ok(Flow::Continue)
    }
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        if let StreamEvent::TextDelta { text, .. } = ev {
            text.push_str(self.0);
        }
        Ok(ChunkFlow::Continue)
    }
}

fn answer(text: &str) -> ChatResponse {
    ChatResponse {
        id: "r".into(),
        model: "m".into(),
        created: None,
        choices: vec![Choice {
            index: 0,
            message: Message::text(Role::Assistant, text),
            stop_reason: Some(StopReason::EndTurn),
            stop_sequence: None,
            extra: Default::default(),
        }],
        usage: Some(aigw_core::ir::Usage {
            input_tokens: 3,
            output_tokens: 5,
            ..Default::default()
        }),
        extra: Default::default(),
        source: None,
    }
}

async fn respond(chain: &Chain, resp: ChatResponse) -> (Flow, ChatResponse, ResponseHeaders) {
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let mut headers = ResponseHeaders::default();
    let mut meta = Meta::new();
    let mut ctx = RespCtx {
        info: &info,
        response: resp,
        response_headers: &mut headers,
        meta: &mut meta,
    };
    let flow = chain.run_response(&mut ctx).await;
    let resp = ctx.response;
    (flow, resp, headers)
}

#[tokio::test]
async fn response_and_stream_hooks_run_in_reverse_order() {
    let chain = || {
        Chain::new(vec![
            Arc::new(Entry::new("a", Arc::new(Onion("a")))),
            Arc::new(Entry::new("b", Arc::new(Onion("b"))).with_order(1)),
            Arc::new(Entry::new("c", Arc::new(Onion("c"))).with_order(2)),
        ])
    };
    let (_, t) = trace(&chain()).await;
    assert_eq!(t, ["a", "b", "c"]);
    let (flow, resp, _) = respond(&chain(), answer(">")).await;
    assert_eq!(flow, Flow::Continue);
    assert_eq!(resp.choices[0].message.content.text(), ">cba");
    let out = run_stream(chain(), vec![text(">")]).await;
    assert_eq!(joined(&out), ">cba");
}

/// Blockiert jede Ein- oder Ausgabe mit „STOP“ (Guardrail, F160).
struct Blocker;

fn blocked() -> ApiError {
    ApiError::invalid_request("Antwort blockiert").with_code("content_filtered")
}

impl Plugin for Blocker {
    fn name(&self) -> &'static str {
        "blocker"
    }
    fn phases(&self) -> Phases {
        Phases::ALL
    }
    fn on_request<'a>(&'a self, ctx: &'a mut ReqCtx<'_>) -> Hook<'a, Flow> {
        let hit = ctx
            .request
            .messages
            .iter()
            .any(|m| m.content.text().contains("STOP"));
        Hook::ok(if hit {
            Flow::Block(blocked())
        } else {
            Flow::Continue
        })
    }
    fn on_response<'a>(&'a self, ctx: &'a mut RespCtx<'_>) -> Hook<'a, Flow> {
        let hit = ctx.response.choices[0]
            .message
            .content
            .text()
            .contains("STOP");
        Hook::ok(if hit {
            Flow::Block(blocked())
        } else {
            Flow::Continue
        })
    }
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        Ok(match ev {
            StreamEvent::TextDelta { text, .. } if text.contains("STOP") => {
                ChunkFlow::Block(blocked())
            }
            _ => ChunkFlow::Continue,
        })
    }
}

/// Hält Stop-Ereignisse zurück (äußeres Plugin, das sie nie mehr ausgibt).
struct SwallowStop;

impl Plugin for SwallowStop {
    fn name(&self) -> &'static str {
        "swallow"
    }
    fn phases(&self) -> Phases {
        Phases {
            request: false,
            response: false,
            chunk: true,
        }
    }
    fn on_chunk(
        &self,
        _ctx: &mut ChunkCtx<'_>,
        ev: &mut StreamEvent,
    ) -> Result<ChunkFlow, PluginError> {
        Ok(if matches!(ev, StreamEvent::Stop { .. }) {
            ChunkFlow::Drop
        } else {
            ChunkFlow::Continue
        })
    }
}

#[tokio::test]
async fn output_block_finishes_with_content_filter_by_default() {
    // Äußeres Plugin „outer“ (order 0) läuft nach dem Blocker nicht mehr.
    let chain = |mode| {
        Chain::new(vec![
            Arc::new(Entry::new("outer", Arc::new(Onion("outer")))),
            Arc::new(
                Entry::new("guard", Arc::new(Blocker))
                    .with_order(1)
                    .with_output_reject(mode),
            ),
        ])
    };
    let (flow, resp, headers) =
        respond(&chain(OutputRejectMode::Finish), answer("geheim STOP")).await;
    assert_eq!(flow, Flow::Continue);
    let c = &resp.choices[0];
    assert_eq!(c.stop_reason, Some(StopReason::ContentFilter));
    assert_eq!(c.message.content.text(), "");
    assert_eq!(resp.usage.as_ref().map(|u| u.output_tokens), Some(5));
    assert_eq!(
        headers
            .set
            .get(GUARDRAIL_HEADER)
            .map(|v| v.to_str().unwrap()),
        Some("blocker")
    );

    let (flow, _, headers) = respond(&chain(OutputRejectMode::Error), answer("STOP")).await;
    let Flow::Reject(e) = flow else {
        panic!("{flow:?}")
    };
    assert_eq!(e.code.as_deref(), Some("content_filtered"));
    assert!(headers.set.contains_key(GUARDRAIL_HEADER));

    // Eingabe-Ablehnung bleibt ein Fehler – auch mit `finish`.
    let info = CallInfo::new("rid", WireFormat::OpenAiChat);
    let h = HeaderMap::new();
    let mut ctx = ReqCtx {
        info: &info,
        request: ChatRequest {
            model: "m".into(),
            messages: vec![Message::text(Role::User, "bitte STOP")],
            ..ChatRequest::default()
        },
        client_headers: &h,
        upstream_headers: HeaderMap::new(),
        response_headers: ResponseHeaders::default(),
        meta: Meta::new(),
    };
    let flow = chain(OutputRejectMode::Finish).run_request(&mut ctx).await;
    assert!(
        matches!(flow, Flow::Reject(ref e) if e.status() == 400),
        "{flow:?}"
    );
    assert!(ctx.response_headers.set.contains_key(GUARDRAIL_HEADER));
}

#[tokio::test]
async fn stream_block_ends_cleanly_even_if_outer_plugin_holds_events() {
    let stop = StreamEvent::Stop {
        reason: StopReason::ContentFilter,
        stop_sequence: None,
    };
    let chain = |mode| {
        Chain::new(vec![
            Arc::new(Entry::new("swallow", Arc::new(SwallowStop))),
            Arc::new(
                Entry::new("guard", Arc::new(Blocker))
                    .with_order(1)
                    .with_output_reject(mode),
            ),
        ])
    };
    let events = || vec![text("harmlos "), text("dann STOP"), text("nie gesehen")];
    let out = run_stream(chain(OutputRejectMode::Finish), events()).await;
    assert_eq!(out.last(), Some(&stop), "{out:?}");
    assert_eq!(joined(&out), "harmlos ");
    let out = run_stream(chain(OutputRejectMode::Error), events()).await;
    assert!(
        matches!(out.last(), Some(StreamEvent::Error(e)) if e.code.as_deref() == Some("content_filtered")),
        "{out:?}"
    );
    assert_eq!(joined(&out), "harmlos ");
}

#[tokio::test]
async fn failmode_metric_counts_open_and_closed() {
    let reg = registry();
    let snap = snapshot(
        1,
        &[],
        vec![
            binding(
                "f-open",
                ScopeLevel::Global,
                None,
                "fail",
                0,
                json!({"failMode": "open"}),
            ),
            binding("f-closed", ScopeLevel::Global, None, "fail", 1, json!({})),
        ],
    );
    let idx = PluginIndex::build(&snap, &reg, IndexOptions::default());
    let (flow, _) = trace(&idx.chain(None, None)).await;
    assert!(matches!(flow, Flow::Reject(_)));
    let m = reg.metrics();
    assert_eq!(m.failmode_count("fail", FailMode::Open), 1);
    assert_eq!(m.failmode_count("fail", FailMode::Closed), 1);
    let mut out = String::new();
    m.render_prometheus(&mut out);
    assert!(
        out.contains("aigw_plugin_failmode_triggered_total{plugin=\"fail\",mode=\"open\"} 1"),
        "{out}"
    );
}

#[test]
fn default_fail_modes_and_effective_control() {
    // F148: content-safety closed, webhook open, sonst closed; je Bindung überschreibbar.
    let r = Registry::with_builtin();
    let eff = |plugin: &str, cfg: Value| r.effective(plugin, &obj(cfg)).unwrap();
    let cs = eff("content-safety", json!({}));
    assert_eq!(cs.fail_mode, FailMode::Closed);
    assert!(cs.fail_mode_default);
    assert_eq!(cs.timeout, Duration::from_secs(3));
    let wh = eff("webhook", json!({}));
    assert_eq!(wh.fail_mode, FailMode::Open);
    assert_eq!(wh.timeout, Duration::from_millis(200));
    for p in [
        "pii",
        "prompt-filter",
        "prompt-leak",
        "header",
        "request-transform",
        "cache",
    ] {
        assert_eq!(eff(p, json!({})).fail_mode, FailMode::Closed, "{p}");
    }
    let wh = eff(
        "webhook",
        json!({"failMode": "closed", "outputRejectMode": "error"}),
    );
    assert_eq!(wh.fail_mode, FailMode::Closed);
    assert!(!wh.fail_mode_default);
    assert_eq!(wh.output_reject, OutputRejectMode::Error);
    assert!(r.effective("nope", &obj(json!({}))).is_err());
    assert!(
        r.effective("pii", &obj(json!({"outputRejectMode": "x"})))
            .is_err()
    );
}

#[test]
fn validation_registry_uses_upstream_policy() {
    use aigw_core::upstream::UpstreamPolicy;
    let cfg = obj(json!({"url": "http://hooks.internal:8080/x"}));
    let strict = Registry::for_validation(&UpstreamPolicy::strict());
    let err = strict.validate("webhook", &cfg).unwrap_err();
    assert!(err.contains("url"), "{err}");
    let cs = obj(
        json!({"provider": "azure", "endpoint": "http://cs.internal:8080",
                        "credential": "c"}),
    );
    assert!(strict.validate("content-safety", &cs).is_err());
    let admin = aigw_core::config::AdminConfig {
        allow_insecure_upstreams: vec!["hooks.internal".into(), "cs.internal".into()],
        allow_private_upstreams: vec!["hooks.internal".into(), "cs.internal".into()],
        ..Default::default()
    };
    let lax = Registry::for_validation(&UpstreamPolicy::from_config(&admin).unwrap());
    lax.validate("webhook", &cfg).unwrap();
    lax.validate("content-safety", &cs).unwrap();
}

/// S-22: Beim Aufbau der Ketten erreichen nur Bindungen der Ebenen `global`/`model-group`
/// private Ziele aus `admin.allow_private_upstreams`; Team-/Org-/Key-Bindungen mit demselben
/// Ziel sind ungültig (übersprungen) – auch wenn sie z. B. per Datenbank am Admin-API vorbei
/// geschrieben wurden.
#[test]
fn private_targets_only_for_global_bindings_at_runtime() {
    use aigw_core::upstream::UpstreamPolicy;
    use aigw_plugins::NoHost;
    let admin = aigw_core::config::AdminConfig {
        allow_insecure_upstreams: vec!["hooks.internal".into(), "cs.internal".into()],
        allow_private_upstreams: vec!["hooks.internal".into(), "cs.internal".into()],
        ..Default::default()
    };
    let lax = UpstreamPolicy::from_config(&admin).unwrap();
    let mut reg = Registry::with_builtin_host(Arc::new(NoHost::with_policy(lax.clone())));
    reg.register(Arc::new(
        aigw_plugins::builtin::webhook::Factory::with_policy(lax),
    ));
    let hook = json!({"url": "http://hooks.internal:8080/x"});
    let cs = json!({"provider": "azure", "endpoint": "http://cs.internal:8080",
                    "credential": "c"});
    let k = key("k", Some("platform"));
    let snap = snapshot(
        1,
        &[&k],
        vec![
            binding(
                "g-hook",
                ScopeLevel::Global,
                None,
                "webhook",
                0,
                hook.clone(),
            ),
            binding(
                "g-cs",
                ScopeLevel::Global,
                None,
                "content-safety",
                1,
                cs.clone(),
            ),
            binding(
                "t-hook",
                ScopeLevel::Team,
                Some("platform"),
                "webhook",
                2,
                hook,
            ),
            binding(
                "t-cs",
                ScopeLevel::Team,
                Some("platform"),
                "content-safety",
                3,
                cs,
            ),
            binding(
                "t-pub",
                ScopeLevel::Team,
                Some("platform"),
                "webhook",
                4,
                json!({"url": "https://hooks.example.org/x"}),
            ),
        ],
    );
    let idx = PluginIndex::build(&snap, &reg, IndexOptions::default());
    assert_eq!(
        idx.chain(Some(&k), None).bindings().collect::<Vec<_>>(),
        ["g-hook", "g-cs", "t-pub"]
    );
}
