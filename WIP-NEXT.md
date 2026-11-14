# AIGW-87 Tool-Inventar – WIP-Stand (nicht kompiliert geprüft!)

Erledigt (Code geschrieben, noch NICHT auf der Box gebaut):
- aigw-core: `tools.rs` (ToolPolicySpec, Glob, origin_of, ToolPolicyIndex::decide, Inventar-Typen,
  Sink-Trait, ToolChangeAction), ResourceKind::ToolPolicy, Manifest, Snapshot.tool_policies/tool_index
  (parts.rs inkl. apply_patch), Config `tools:` (ToolsConfig).
- aigw-store: Migration 0007_tools (beide Dialekte), Resource-Impl (kinds.rs), table/delete-Kaskade,
  list.rs (Level/Target/Action-Filter), Snapshot-Laden/Patch/Cache, GitOps (apply/state/document/plan),
  `tools.rs` (StoreToolSink, Flush mit Verlauf/Änderung/Audit/Auto-Block, list_tools, set_tool_decision).
- aigw-plugins: `prompt_filter::injection_signals`.
- aigw-proxy: `tools.rs` (ToolInventory, begin(), ToolGuard: Strip/Reject, Antwort-/Stream-Filter),
  ProxyState.tools, Einbau in handlers.rs (chat) und endpoints.rs (responses), Recorder::payload_mode,
  Cargo.toml+Cargo.lock (aigw-pii, blake3).
- aigw (Binary): main.rs tool_inventory()/Task/Shutdown-Flush, Webhook-Hook, Metriken.
- aigw-admin: `tools.rs` geschrieben (ToolPolicy-CRUD per plain_resource!, /tools Liste/Detail/approve/block/acknowledge/delete).

Nächste Schritte:
1. aigw-admin: `pub mod tools;` in lib.rs, `.merge(tools::router())`, Pfade+Tag in api.rs; ToolPolicy in
   resources.rs `is_catalog`. In admin/tools.rs `Directory::matching` aufräumen (Dummy-Index entfernen).
2. Bauen auf der Box (`cargo fmt --all`, clippy, tests) und Kompilierfehler beheben; fmt-Ergebnis per --fetch holen.
3. Tests: Store (Flush/Änderung/Decision), Admin-API, Proxy-Integration (OpenAI+Anthropic, Stream), e2e (eigener AIGW_E2E_PORT).
4. UI: Seite „Tools“ (nav.ts, Router, Tabelle/Filter/Drawer, Badges), OpenAPI + schema.d.ts neu (scripts/gen-api.sh).
5. Doku: docs/konzept/16-tools.md (neu), 08, 04-ux, 13 (Reihenfolge: Auth→Modellfreigabe→Tool-Richtlinien→Limits→Plugins/Guardrails→…→Tool-Aufruf-Filter→Post-Plugins), CHANGELOG.
6. Benchmark mit/ohne Inventar (Mikrobenchmark), runtime-smoke, voller check.sh.
