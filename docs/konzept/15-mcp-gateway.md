# 15 – MCP-Gateway (AIGW-41)

ai-gw ist **MCP-Server** für Clients (Agenten, IDEs) und **MCP-Client** zu registrierten
Upstream-MCP-Servern. Clients sehen über einen Endpunkt alle für ihren API-Key freigegebenen
Tools, Prompts und Ressourcen; ai-gw prüft die Rechte, setzt die Zugangsdaten zum Upstream,
wendet die Limits des Keys an und protokolliert jeden Tool-Aufruf.

Eigenentwicklung (Crate `aigw-mcp`): JSON-RPC 2.0, Streamable HTTP (Server und Client), älteres
HTTP+SSE und stdio (Client). Das offizielle Rust-SDK (`rmcp`) wird nicht verwendet: Das Gateway
braucht beide Seiten gleichzeitig mit eigener Session-, Rechte- und Pool-Logik, reicht
Nachrichten weitgehend unverändert durch (`serde_json::Value`, unbekannte Felder bleiben
erhalten) und nutzt den vorhandenen HTTP-Stack (hyper, rustls/aws-lc-rs) mit SSRF-Prüfung beim
Verbindungsaufbau. Ein SDK mit eigenen Typen für jede Protokollversion hätte hier mehr
Anpassung als Nutzen gebracht.

## 1. Überblick

```
Client ──POST/GET/DELETE /mcp[/{server}]──▶ aigw-proxy::mcp  (Auth, Session, Limits, Log)
                                                │
                                                ▼
                                       aigw_mcp::McpGateway  (Aggregation, Namensraum, Rechte)
                                                │
                                                ▼
                                       aigw_mcp::client::Pool (eine Verbindung je Server)
                                          │        │        │
                              Streamable HTTP   HTTP+SSE   stdio (nur nach Freischaltung)
```

| Crate/Datei | Inhalt |
|---|---|
| `aigw-mcp/src/jsonrpc.rs` | Nachrichten, IDs, Fehlercodes |
| `aigw-mcp/src/protocol.rs` | Protokollversionen, Header, Methoden |
| `aigw-mcp/src/client.rs` | Upstream-Transporte, Initialisierung, Abbruch, Listen-Cache, Pool |
| `aigw-mcp/src/httpc.rs` | HTTP-Client mit DNS-Filter (SSRF) |
| `aigw-mcp/src/grants.rs` | Auswertung der Freigaben |
| `aigw-mcp/src/session.rs` | Client-Sessions |
| `aigw-mcp/src/gateway.rs` | Aggregation, Namensraum, Weiterleitung |
| `aigw-proxy/src/mcp.rs` | HTTP-Endpunkte der Datenebene |
| `aigw-admin/src/mcp.rs` | Admin-API `mcp-servers`, `mcp-grants` |
| `aigw-core/src/mcp.rs` | Einstellungen, Prüfregeln (Admin + GitOps), Tool-Muster |

## 2. Endpunkte (Datenebene, Streamable HTTP)

| Endpunkt | Inhalt |
|---|---|
| `/mcp` | Alle freigegebenen Server. Tools und Prompts heißen `<server>__<name>`. |
| `/mcp/{server}` | Ein Server, Namen wie beim Upstream. Unbekannter Server `404`, nicht freigegeben `403`. |

Je Endpunkt:

- `POST` – genau eine JSON-RPC-Nachricht (Batches → `-32600`). Anfragen werden mit JSON
  beantwortet; `tools/call` mit **SSE**, wenn der Client `text/event-stream` akzeptiert
  (Fortschritt und Log-Meldungen des Upstreams, dann das Ergebnis, Keep-Alive-Kommentare alle
  15 s). Benachrichtigungen und Antworten → `202`.
- `GET` – SSE-Strom für Benachrichtigungen der Session (`notifications/*/list_changed`).
- `DELETE` – beendet die Session (`204`).

Anmeldung wie auf `/v1/*` (`Authorization: Bearer`, `api-key`, `x-api-key`); ohne gültigen Key
`401` mit `WWW-Authenticate: Bearer` und JSON-RPC-Fehler `-32001`.

**Protokollversionen:** `2025-11-25` (neueste), `2025-06-18`, `2025-03-26`, `2024-11-05`.
`initialize` antwortet mit der angefragten Version, falls unterstützt, sonst mit der neuesten.
Ein Header `MCP-Protocol-Version` mit nicht unterstützter Version → `400`; fehlt er, gilt
`2025-03-26` (Spezifikation).

**Sessions:** `initialize` legt eine Session an (`Mcp-Session-Id`, 256 Bit zufällig). Alle
weiteren Anfragen brauchen den Header: fehlt er → `400`; unbekannt, abgelaufen, an einen
anderen Key oder anderen Endpunkt gebunden → `404` (Client initialisiert neu). Sessions enden
nach `mcp.session_idle_timeout_secs` ohne Anfrage; höchstens `mcp.max_sessions` je Instanz.
Sessions liegen im Speicher der Instanz – bei mehreren Instanzen braucht der Load-Balancer
Sticky Sessions (z. B. nach `Mcp-Session-Id`), siehe offene Punkte.

**Methoden:** `initialize`, `ping`, `tools/list`, `tools/call`, `resources/list`,
`resources/templates/list`, `resources/read`, `prompts/list`, `prompts/get`,
`notifications/initialized`, `notifications/cancelled`. Andere Methoden (z. B.
`resources/subscribe`, `logging/setLevel`, `completion/complete`) → `-32601`; die Fähigkeiten in
`initialize` nennen nur Unterstütztes (`tools`, `resources`, `prompts`, jeweils `listChanged`).

**Listen:** Das Gateway liest Upstream-Listen vollständig (alle Seiten, höchstens 50) und
liefert sie ohne `nextCursor`. Listen werden je Upstream zwischengespeichert
(`mcp.list_cache_secs`), `…/list_changed` des Upstreams leert sofort.

### 2.1 Aggregation und Namensraum

- Tools und Prompts: `<server>__<name>`. Servernamen dürfen `__` nicht enthalten (Prüfung in
  Admin-API und GitOps), Tool-Namen schon (`gh__get__odd` → Server `gh`, Tool `get__odd`).
- Ressourcen behalten ihre URI (Clients deuten URIs wie `file://`). Das Gateway merkt sich je
  Session, welcher Server eine URI geliefert hat; `resources/read` geht dorthin. Unbekannte URI:
  gibt es nur einen freigegebenen Server mit Ressourcen, geht sie dorthin, sonst `-32002` mit
  Hinweis auf `resources/list` bzw. `/mcp/{server}`.
- Ein nicht erreichbarer Server fehlt im aggregierten Listen-Ergebnis (Warnung im Log); am
  Einzel-Endpunkt ist das ein Fehler (`-32005`).

## 3. Freigaben (`McpGrant`)

```yaml
apiVersion: aigw/v1alpha1
kind: McpGrant
metadata: {name: platform-github}
spec:
  server: github
  subject: {level: team, target: platform}     # team | api-key
  tools: ["get_*", "search_*", "!get_secret*"]  # leer = alle
```

- **Standard: nichts.** Ohne passende Freigabe sieht ein Key keinen Server.
- `tools`: Glob-Muster (`*`, `?`) auf den Tool-Namen beim Upstream; `!muster` schließt aus.
  Leere Liste bzw. nur Ausschlüsse = alle übrigen Tools.
- Freigaben derselben Ebene werden vereinigt.
- **Team als Obergrenze** (wie Modellfreigaben): Gehört der Key zu einem Team, braucht es eine
  Team-Freigabe für den Server; Key-Freigaben schränken innerhalb davon weiter ein. Keys ohne
  Team brauchen eine Key-Freigabe. So kann ein Team-Admin (schreibt Key-Freigaben in seinem
  Team) nicht mehr freigeben, als das Team hat.
- **Ausschluss gewinnt:** `!muster` aus allen anwendbaren Freigaben gilt immer.
- `tools/list` zeigt nur Erlaubtes; `tools/call` prüft erneut.
- Ressourcen und Prompts: Freigabe auf Server-Ebene (jede Freigabe des Servers).
- **Fehler** (präzise wie bei Modellen): unbekannter Server/Tool → `-32602` mit
  `data.status = 404` (`server_not_found`/`tool_not_found`), nicht freigegeben → `-32003` mit
  `data.status = 403` (`server_not_allowed`/`tool_not_allowed`). Reihenfolge: Server unbekannt →
  Server nicht freigegeben → Tool unbekannt → Tool nicht freigegeben.

Änderungen an Servern oder Freigaben wirken mit dem nächsten Snapshot (Hot Reload); offene
Sessions erhalten `notifications/{tools,resources,prompts}/list_changed`.

## 4. Upstream-Server (`McpServer`)

```yaml
apiVersion: aigw/v1alpha1
kind: McpServer
metadata: {name: github}
spec:
  url: https://mcp.example.org/mcp
  transport: streamable-http        # Standard; sse | stdio
  credential: github-token          # ProviderCredential (optional)
  settings:
    headers: {x-tenant: acme}        # nicht geheim
    credentialHeader: authorization  # Standard; sonst Wert unverändert, z. B. x-api-key
    credentialField: apiKey          # Standard
    timeoutMs: 60000                 # Standard: mcp.call_timeout_secs
```

**Zugangsdaten:** Verweis auf eine `ProviderCredential` (verschlüsselt wie bei Anbietern;
Anbieterart beliebig, empfohlen `openai-compatible`, da ohne Pflichtfelder). Das Feld
`credentialField` (Standard `apiKey`) geht als `Authorization: Bearer …` bzw. unverändert in
`credentialHeader`. Statische Header dürfen keine Geheimnisse und keine Protokoll-Header
enthalten (`authorization`, `cookie`, `mcp-session-id` … werden abgelehnt).

**OAuth (Erweiterungspunkt, nicht gebaut):** Die MCP-Spezifikation sieht OAuth 2.1 zwischen
Client und MCP-Server vor. Zwei Ausbaustufen sind vorgesehen: (a) Gateway als OAuth-Client mit
eigenem Token je Server (Client-Credentials/Refresh-Token als `ProviderCredential`, Erneuerung
im Pool – Einhängestelle `http_headers` in `client.rs`); (b) Weiterleitung eines Nutzer-Tokens
(Token-Austausch je Key/Nutzer; braucht Token-Speicher je Nutzer und eine Session je Nutzer statt
der geteilten Verbindung). Beides ist offen.

**Transporte (Client-Seite):**

- *Streamable HTTP:* jede Nachricht ein `POST` (Antwort JSON oder SSE); `Mcp-Session-Id` und
  `MCP-Protocol-Version` nach `initialize`; `404` auf die Session → Neuaufbau (einmalige
  Wiederholung, auch für `tools/call`, da der Upstream ihn nicht verarbeitet hat); `GET`-Strom
  für Benachrichtigungen (`405` → ohne). Beim Abbau `DELETE` der Session.
- *HTTP+SSE (`2024-11-05`):* `GET` öffnet den Strom, `endpoint` nennt die POST-Adresse – nur
  gleiche Herkunft (Schema, Host, Port) ist erlaubt (SSRF).
- *stdio:* nur Programme aus `mcp.allow_stdio_commands` (absolute Pfade; leer = stdio aus,
  Standard). Der Prozess startet mit leerer Umgebung (nur `PATH`, `settings.env`, optional das
  Token in `settings.credentialEnv`), Argumente aus `settings.args`, `stderr` ins Debug-Log,
  Abbruch beim Abbau der Verbindung (`kill_on_drop`).

> **Sicherheitshinweis stdio:** Ein stdio-Server läuft als Kindprozess mit den Rechten des
> Gateways (Dateisystem, Netz, Umgebung). Nur vertrauenswürdige, fest installierte Programme
> freischalten; in Containern ohne zusätzliche Rechte betreiben. Die Admin-API prüft nur das
> Format des Pfads – starten darf ausschließlich die Datenebene nach Freischaltung in der
> Startkonfiguration (kein Admin-Recht genügt, um beliebige Programme zu starten).

**Pool und Health:** Eine initialisierte Verbindung je Server, geteilt von allen Clients
(Fortschritts-Tokens werden dafür je Aufruf eindeutig umgeschrieben und zurückübersetzt).
Fehler (Verbindung, Session, `5xx`, `401/403`) markieren die Verbindung als tot; Neuaufbau beim
nächsten Zugriff mit exponentiellem Backoff (1 s … 60 s, währenddessen sofortiger Fehler).
Geänderte Spezifikation oder Zugangsdaten (Fingerabdruck) → neue Verbindung; gelöschte Server
werden beim nächsten Snapshot entfernt. `McpGateway::status()` liefert den Zustand je Server
(Auskunft über die Admin-API: offener Punkt).

**Zeitlimits:** Verbindungsaufbau `mcp.connect_timeout_secs`, jede Anfrage
`settings.timeoutMs` bzw. `mcp.call_timeout_secs` (Fehler `-32006`, `data.status = 504`).
Nachrichten über `mcp.max_message_bytes` werden abgelehnt.

**Abbruch:** `notifications/cancelled` des Clients bricht den laufenden `tools/call` ab (keine
Antwort, Spezifikation); ebenso ein Verbindungsabbruch während des Aufrufs (ohne
Wiederaufnahme ist das Ergebnis sonst nicht zustellbar). In beiden Fällen und bei Zeitüberschreitung
erhält der Upstream `notifications/cancelled`.

**Anfragen des Upstreams** (Sampling, Elicitation, Roots): nicht unterstützt (`-32601`, das
Gateway meldet diese Fähigkeiten nicht an); `ping` wird beantwortet.

## 5. SSRF-Schutz

1. Admin-API und GitOps prüfen `url` mit denselben Regeln wie Deployment-Endpunkte
   (`aigw_core::upstream`, `admin.allow_insecure_upstreams`/`allow_private_upstreams`).
2. Vor jedem Verbindungsaufbau prüft die Datenebene die URL erneut (auch für Server, die am
   Admin vorbei in die Datenbank kamen).
3. Der DNS-Resolver des MCP-Clients liefert nur erlaubte Adressen
   (`UpstreamPolicy::resolved_ip_allowed`): Ein öffentlicher Name, der auf eine private Adresse
   zeigt (auch per DNS-Rebinding), wird nicht verbunden – außer der **Name** ist freigeschaltet.
4. Keine Weiterleitungen (3xx), `endpoint` beim SSE-Transport nur gleiche Herkunft.

## 6. Limits, Log, Metriken

- **Limits:** Jeder `tools/call` reserviert eine Anfrage in den Limits von Key, Team und
  Organisation (gleicher `Limiter` wie `/v1/*`, keine Tokens, keine Modellgruppe). Abgelehnt →
  `-32004` mit `data.status = 429`, `data.retryAfter` und Header `Retry-After`. Listen,
  `ping` usw. zählen nicht. Budgets: harte Budgetgrenzen sperren auch Tool-Aufrufe; Kosten
  entstehen keine.
- **Request-Log:** je `tools/call` ein `RequestRecord` mit `ingress = mcp` (Spalte
  `endpoint = "mcp"`), `model_requested = "<server>__<tool>"`, `RequestRecord.mcp` (Server,
  Tool, Ergebnis), Status (`200`, `403`, `404`, `429`, `502`, `504`, `499` bei Abbruch), Dauer,
  Key/Team/Organisation. Argumente und Ergebnis nur gemäß Payload-Modus (`observe.payload`:
  `metadata` → nur Größen, `masked` → Maskierung des Writers, `full`), wie bei Chat-Anfragen.
- **Metriken:** `aigw_mcp_calls_total{server,tool,status}` mit `status` ∈ `ok`, `tool_error`,
  `not_found`, `denied`, `rate_limited`, `upstream_error`, `timeout`, `cancelled`;
  `aigw_mcp_call_duration_seconds{server,tool,status}` (Summe/Anzahl). Höchstens 2000 Serien,
  danach `tool="other"`. Zusätzlich erscheinen Tool-Aufrufe in `aigw_requests_total` mit
  `ingress="mcp"`.
- **Traces:** Wurzel-Span `mcp tools/call <server>__<tool>`; der Upstream erhält `traceparent`.
- **Plugins:** Die Plugin-Kette (AIGW-36) arbeitet auf der Chat-IR und greift bei MCP noch
  nicht; Tool-Argumente/-Ergebnisse durch Plugins (z. B. PII) ist ein offener Punkt.

## 7. Konfiguration (`mcp:`)

| Feld | Standard | Bedeutung |
|---|---|---|
| `enabled` | `true` | Endpunkte `/mcp` anbieten |
| `allow_stdio_commands` | `[]` | freigeschaltete stdio-Programme (leer = stdio aus) |
| `session_idle_timeout_secs` | `3600` | Ablauf einer Client-Session |
| `max_sessions` | `10000` | Sessions je Instanz |
| `call_timeout_secs` | `120` | Zeitlimit je Upstream-Anfrage |
| `connect_timeout_secs` | `10` | Verbindungsaufbau |
| `max_message_bytes` | `16777216` | größte Upstream-Nachricht |
| `list_cache_secs` | `300` | Cache der Upstream-Listen |

ENV wie üblich, z. B. `AIGW_MCP__ALLOW_STDIO_COMMANDS=/usr/local/bin/mcp-fs`.

## 8. Admin-API und GitOps

- `GET/POST /admin/api/v1/mcp-servers`, `GET/PUT/DELETE …/{name}` – Katalog (`Target::Catalog`:
  lesen jede wirksame Rolle, schreiben globaler `admin`). Filter `q` (Name/URL), `label`,
  `transport`, `credential`, `enabled`. Prüfung: `aigw_core::mcp::validate_server` (Name ohne
  `__`, SSRF, Einstellungen ohne unbekannte Schlüssel, stdio: absoluter Pfad), Zugangsdaten
  müssen existieren (`422`). Löschen entfernt die Freigaben des Servers mit (Fremdschlüssel);
  Zugangsdaten in Benutzung lassen sich nicht löschen (`409`).
- `GET/POST /admin/api/v1/mcp-grants`, `GET/PUT/DELETE …/{name}` – Geltungsbereich: Team-Freigabe
  = das Team (Org-Admin/Admin schreiben), Key-Freigabe = im Team des Keys (auch Team-Admin),
  Key ohne Team = global. Filter `server`, `team`, `apiKey`, `q`, `label`. Prüfung:
  `aigw_core::mcp::validate_grant` (Ebene, Muster), Server/Team/Key müssen existieren (`422`).
- ETag/If-Match, Cursor-Pagination, Problem Details, Audit-Log wie bei allen Ressourcen.
- GitOps: Kinds `McpServer`/`McpGrant` (Export, Diff, Apply) mit denselben Regeln
  (`check_rules` in `aigw-store/src/gitops/plan.rs`).

## 9. Tests

- `aigw-mcp`: JSON-RPC-Parser, Aushandlung, Freigaben (Standard nichts, Team als Obergrenze,
  Ausschlüsse), Glob-Muster, Sessions.
- `aigw-proxy/tests/mcp.rs`: Fake-Upstreams (Streamable HTTP mit JSON und SSE, GET-Strom;
  HTTP+SSE) und eigener Minimal-Client: Aggregation, Pagination, Namensraum, Einzel-Endpunkt,
  404/403 auf Server- und Tool-Ebene, Team-Obergrenze, Sessions (fehlend/fremd/Endpunkt,
  DELETE), Versionsaushandlung, abgelaufene Upstream-Session, Abbruch mit Fortschritt und
  Token-Übersetzung, `list_changed` (Upstream und Konfiguration), SSRF zur Laufzeit, stdio
  gesperrt, Limits, Request-Log.
- `aigw-admin/tests/mcp_api.rs` (SQLite, Postgres): CRUD, Prüfungen, Rollen (Team-Admin),
  GitOps-Regeln, Ende-zu-Ende über Snapshot mit entschlüsselten Zugangsdaten bis zur Datenebene.
- `scripts/runtime-smoke.sh`: SSRF-Ablehnung, `initialize`/`tools/list` am laufenden Binary.

## 10. Offene Punkte

- Interop mit offiziellen MCP-Clients (Inspector, SDKs) – lokal ohne Netz nicht testbar;
  geprüft ist das Protokoll mit eigenem Minimal-Client.
- stdio-Ende-zu-Ende-Test (braucht ein MCP-Programm im Testsystem).
- Sessions über mehrere Instanzen (Sticky Sessions oder geteilter Session-Speicher).
- Wiederaufnahme von SSE-Strömen (`Last-Event-ID`) – nicht umgesetzt.
- Plugin-Kette auf Tool-Argumente/-Ergebnisse, Health-Auskunft über die Admin-API,
  OAuth (Abschnitt 4), Freigaben auf Ressourcen-/Prompt-Ebene.
