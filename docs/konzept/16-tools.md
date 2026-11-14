# Tool-Inventar und Tool-Richtlinien (AIGW-87)

Anwendungen schicken mit jeder Anfrage die Tools mit, die das Modell aufrufen darf – Claude Code
etwa alle Tools seiner MCP-Server als `mcp__<server>__<tool>`. Das Gateway führt diese Tools
nicht aus, sieht sie aber vollständig. Vorbild ist die LiteLLM-Ansicht „von Anwendungen
bereitgestellte Tools“ (dort blockierbar). ai-gw

- **inventarisiert** jedes Tool (Name, Art, vermutete Herkunft, Client, Organisation/Team/Key,
  erste/letzte Sichtung, Anfragen, Aufrufe durch das Modell, Hashes von Beschreibung und
  Schema),
- **erkennt Änderungen** an Beschreibung oder Schema bekannter Tools („Rug Pull“) und prüft
  Beschreibungen mit dem Prompt-Injection-Erkenner des `prompt-filter`,
- **setzt Richtlinien durch** (Ressource `ToolPolicy`): erlauben, entfernen, ablehnen,
  beobachten, „erst nach Freigabe“ – auch für Tool-Aufrufe in der Antwort.

## 1. Pipeline und Reihenfolge

```text
Auth ─► Body/IR ─► Modellfreigabe ─► [Tool-Richtlinien + Inventar] ─► Limits/Budgets
     ─► Pre-Plugins (Guardrails …) ─► Cache ─► Routing
     ─► [Tool-Aufruf-Filter] ─► Post-/Stream-Plugins ─► Encoder
```

- **Vor Limits und Plugin-Kette:** entfernte Tools zählen nicht zur Token-Schätzung, Guardrails
  und Webhook-Plugins sehen nur noch erlaubte Tools; eine Ablehnung (`block-reject`) belegt kein
  Kontingent.
- **Tool-Aufruf-Filter als innerste Antwortschicht** (vor Post- und Stream-Plugins, nach dem
  Cache): Antwort-Guardrails sehen eine bereits gefilterte Antwort; Cache-Treffer laufen wie
  Upstream-Antworten durch den Filter.
- Gilt für `/v1/chat/completions`, `/v1/messages` und `/v1/responses` (alle über die IR) und
  den Playground. Ohne Tools in der Anfrage entfällt der Schritt vollständig.

## 2. Inventar

### 2.1 Erkennung

| Feld | Herkunft |
|---|---|
| Name | `function.name` bzw. `name` des Anbieter-Tools; OpenAI-Built-ins ohne Namen: `type` (`code_interpreter`); Responses-MCP: `mcp:<server_label>` |
| Art | `function` (JSON-Schema), `server-tool` (versionierter Typ, z. B. Anthropic `web_search_20250305`), `builtin` (eingebaute ohne Version) |
| Herkunft | `mcp__<server>__<tool>` (Claude Code/Desktop) bzw. `<server>__<tool>` (aggregierte MCP-Gateways, auch `/mcp` dieses Gateways) → `<server>`; sonst `client` |
| Client | erster gesetzter Header aus `tools.client_headers` (Standard `x-aigw-client`, `x-title`), sonst `User-Agent`; auf 96 Bytes gekürzt |
| Identität | (Organisation, Team, API-Key, Name) – je Anwendung/Key ein Eintrag |
| Aufrufe | `ToolCall`-Teile der Antwort bzw. `ToolCallStart` im Stream |

**Gateway-MCP:** Einträge, deren Herkunft (oder ein `__<server>__`-Segment) einem
`McpServer` des Gateways entspricht, tragen in der Admin-API `gatewayMcp: true`.

### 2.2 Hashes und Inhalte

Je Tool zwei BLAKE3-Hashes (128 Bit, hex): Beschreibung und Schema. Das Schema wird als
JSON-Baum gehasht (Typ-Präfixe, Längen, Schlüssel sortiert) – ohne Serialisierung, deshalb
unabhängig von Leerzeichen und Schlüsselreihenfolge. Die **Inhalte** (Beschreibung, Schema)
speichert das Gateway nur, wenn der wirksame `payloadMode` des Keys Inhalte erlaubt:
`full` unverändert, `masked` mit der PII-Maskierung des Request-Logs; bei `off`/`metadata`
nur die Hashes. Geprüft (Injection) wird die Beschreibung in jedem Fall – im Speicher.

### 2.3 Hot Path und Schreiben

- Je Tool: ein schneller Fingerabdruck (foldhash, Schlüssel je Prozess zufällig), ein Lookup in einer geshardeten Map (`DashMap`) mit Zählern; nur bei
  **neuen oder geänderten** Hashes (Abgleich mit dem RAM-Stand bekannter Tools) wird die
  Definition für den Abgleich kopiert.
- Ein Hintergrund-Task schreibt alle `tools.flush_interval_ms` (Standard 5 s) aggregiert in
  einer Transaktion (`tool_inventory`, `tool_inventory_history`) – wie das Request-Log nie im
  Anfragepfad. Beim Start lädt das Gateway die bekannten Hashes (Warmstart), beim Beenden
  schreibt es den Puffer.
- `tools.max_entries` (Standard 100 000) begrenzt die Map; neue Einträge darüber werden
  verworfen und gezählt (`aigw_tool_inventory_events_total{event="dropped"}`).
- Mehrere Instanzen schreiben in dieselben Zeilen (Upsert, Zähler additiv).

Messung: siehe §7.

### 2.4 Änderungen und verdächtige Beschreibungen

- **Geändert:** Weicht ein Hash vom gespeicherten Stand ab, setzt der Abgleich `changed`,
  schreibt einen Verlaufseintrag (`changed`), einen Audit-Eintrag (`kind: tool`,
  `action: changed`, mit Organisation/Team für eingeschränkte Rollen) und sendet das Ereignis
  `tool.changed` an die Ereignis-Webhooks (`events.webhooks`). Mit `tools.on_change: block`
  landet das Tool zusätzlich in der Sperr-Richtlinie seines Keys (`tools-block-api-key-<key>`,
  `block-strip`) – wirksam mit dem nächsten Snapshot (im Prozess sofort, im Cluster per
  `NOTIFY`/Poll). Bestätigen (`…/acknowledge`) bzw. Freigeben setzt `changed` zurück.
- **Verdächtig:** Die eingebauten Prompt-Injection-Signale des `prompt-filter`
  (Standardschwelle 1,0) laufen über jede neue/geänderte Beschreibung. Treffer werden als
  `suspicious` (Signalnamen) gespeichert, auditiert (`action: suspicious`) und als
  `tool.suspicious` gemeldet. Sperren erfolgt nicht automatisch.

## 3. Richtlinien (`ToolPolicy`)

```yaml
apiVersion: aigw/v1alpha1
kind: ToolPolicy
metadata: {name: block-destructive}
spec:
  scope: {level: global}             # global | organization | team | api-key | model-group (+ target)
  tools: ["*delete*", "!delete_draft"] # Glob (* ?), !Ausschluss; leer = alle
  servers: [github]                  # Herkunft (MCP-Server bzw. client); leer = jede
  action: block-strip                # allow | block-strip | block-reject | observe | require-approval
  locked: true                       # nur block-*: gilt auch gegen spezifischere allow
  # unapproved: strip | reject       # nur require-approval (Standard strip)
  # enabled: true
  description: Zerstörerische Tools nie an Modelle geben
```

Ein Tool-Muster passt auf den vollen Namen **oder** den Namen ohne Server-Präfix
(`create_issue` passt auf `mcp__github__create_issue`).

**Entscheidung je Tool** (Kontext: Organisation, Team, Key, Modellgruppe der Anfrage):

1. Eine passende **gesperrte Sperre** (`locked`, beliebige Ebene) gewinnt immer.
2. Sonst die passende `allow`/`block-*`-Richtlinie der **spezifischsten Ebene**
   (`api-key` > `model-group` > `team` > `organization` > `global`); auf gleicher Ebene die
   strengere (`block-reject` > `block-strip` > `allow`).
3. Ohne solche Richtlinie: passt eine `require-approval`-Richtlinie, gilt deren `unapproved`
   (das Tool ist **nicht freigegeben**, bis eine `allow`-Richtlinie es erfasst).
4. Sonst erlaubt. `observe` wirkt nie, schreibt aber eine Warnung ins Log und zählt.

**Wirkungen:**

| Wirkung | Anfrage | Antwort |
|---|---|---|
| allow | unverändert | – |
| strip | Tool entfernt; erzwungenes `tool_choice` auf dieses Tool → `auto`; ohne verbleibende Tools entfallen `tool_choice` und `parallel_tool_calls` | Aufrufe dieses Tools unterbunden |
| reject | `403`, Code `tool_blocked`, Meldung nennt Tools und Richtlinien (Format des Eingangs) | – |

**Aufrufe nicht erlaubter Tools in der Antwort** (Halluzination, Tools aus dem Verlauf,
eingebaute Anbieter-Tools) werden wie eine Ausgabe-Ablehnung der Guardrails behandelt (F160,
„wie Azure“): JSON `200` mit leerem Inhalt und `finish_reason: content_filter` (OpenAI) bzw.
`stop_reason: refusal` (Anthropic), Header `x-aigw-guardrail: tool-policy`. Im Stream verwirft
der Filter ab dem ersten solchen Aufruf alle Inhaltsereignisse, beendet mit `content_filter`
und lässt Usage durch (bereits gesendeter Text bleibt).

**Freigabe-Status:** In der Admin-API berechnet je Eintrag aus allen Richtlinien (ohne
Modellgruppen-Ebene): `new` (keine Richtlinie), `approved` (`allow`), `blocked` (`block-*`),
`pending` (`require-approval`, nicht freigegeben).

**Freigeben/Sperren** aus dem Inventar schreibt Entscheidungs-Richtlinien je Ebene:
`tools-allow-<ebene>[-<ziel>]` bzw. `tools-block-<ebene>[-<ziel>]` (Label
`aigw.io/managed-by: tool-inventory`); das Tool wird aus der Gegenrichtlinie derselben Ebene
entfernt (leere Richtlinien werden gelöscht). Alles läuft über die normale Ressourcen-API:
Audit je Objekt, inkrementeller Snapshot, GitOps-Export.

**Rechte** wie Plugin-Bindungen (F183): lesen jede wirksame Rolle; schreiben `global`/
`model-group` nur globaler `admin`, `organization` O auch `org-admin` von O, `team` T auch
`team-admin` von T, `api-key` wie der Key. Das Inventar sieht eine eingeschränkte Rolle nur in
ihrem Bereich (Teams bzw. teamlose Keys ihrer Organisation).

## 4. Admin-API

| Methode | Pfad | Zweck |
|---|---|---|
| GET/POST | `/admin/api/v1/tool-policies` | Liste (`level`, `target`, `action`, `q`, `label`), Anlegen |
| GET/PUT/DELETE | `/admin/api/v1/tool-policies/{name}` | Lesen (ETag), Ersetzen/Löschen mit `If-Match` |
| GET | `/admin/api/v1/tools` | Inventar: `q`, `origin`, `client`, `kind`, `organization`, `team`, `apiKey`, `status` (`new`, `approved`, `blocked`, `pending`, `changed`, `suspicious`), Keyset-Pagination |
| GET | `/admin/api/v1/tools/{id}` | Detail: Beschreibung/Schema (falls gespeichert), Verlauf (50), passende Richtlinien |
| POST | `/admin/api/v1/tools/{id}/approve` | `{scope?}` – Freigabe (Standard `global`; Ebene ohne Ziel = Ziel des Eintrags) |
| POST | `/admin/api/v1/tools/{id}/block` | `{scope?, action?: block-strip\|block-reject}` |
| POST | `/admin/api/v1/tools/{id}/acknowledge` | Änderung bestätigen |
| DELETE | `/admin/api/v1/tools/{id}` | Eintrag samt Verlauf entfernen |

## 5. Konfiguration

```yaml
tools:
  inventory: true            # Inventar an (Richtlinien wirken immer)
  flush_interval_ms: 5000
  max_entries: 100000
  on_change: warn            # warn | block
  injection_check: true
  client_headers: [x-aigw-client, x-title]
```

ENV wie üblich, z. B. `AIGW_TOOLS__ON_CHANGE=block`.

## 6. Beobachtung

- `aigw_tool_policy_actions_total{action="strip|reject|observe|call_blocked"}`
- `aigw_tool_inventory_events_total{event="changed|suspicious|dropped|flush_error"}`
- Log: `info` je greifender Richtlinie (Tool, Richtlinie, Wirkung), `warn` für `observe`,
  Änderungen und verdächtige Beschreibungen.
- Audit: `kind: tool`, `action: changed|suspicious`; Richtlinien wie jede Ressource
  (`tool-policy`).
- Ereignis-Webhooks: `tool.changed`, `tool.suspicious` (Daten: `toolId`, `tool`, `origin`,
  IDs von Organisation/Team/Key, `signals`, `blockedBy`).

## 7. Messwerte

`cargo run --release -p aigw-proxy --example tools_bench` (Box cpx42, ein Kern), 30 Tools je
Anfrage (Claude-Code-ähnlich: Beschreibung ~600 Zeichen, Schema mit 8 Feldern), Rohdaten
[messwerte/87-tools-bench.txt](messwerte/87-tools-bench.txt):

| Fall | µs je Anfrage |
|---|---|
| ohne Tools | 0,02 |
| 30 Tools, ohne Inventar, ohne Richtlinien | 2,6 |
| 30 Tools, 6 Richtlinien | 7,9 |
| 30 Tools, Inventar (bekannte Definitionen) | 13,2 |
| 30 Tools, Inventar + 6 Richtlinien | 18,0 |

Zum Vergleich: allein das Dekodieren eines solchen ~40-KB-Bodys kostet ein Vielfaches. Das
Inventar erkennt bekannte Definitionen über einen schnellen, prozesszufällig geschlüsselten
Fingerabdruck (foldhash) – BLAKE3 läuft nur für neue oder geänderte Definitionen. Ein erster
Entwurf mit BLAKE3 je Tool und Anfrage lag bei ≈ 75 µs. Ohne Tools kostet der Schritt nichts
Messbares; die Ziele Q1/Q4 (`docs/betrieb/leistung.md`) bleiben unberührt.

## 8. Grenzen und offene Punkte

- Der Status in der Admin-Liste ignoriert Richtlinien auf Modellgruppen-Ebene (der Eintrag
  hat keine Modellgruppe); die Anfrage selbst berücksichtigt sie.
- Mehrere Clients mit demselben Key teilen einen Eintrag (letzter Client wird angezeigt).
- Automatisches Sperren wirkt ab dem nächsten Snapshot; Anfragen im selben Schreibintervall
  gehen noch durch.
- Tool-Ergebnisse (`tool_result`) und MCP-Aufrufe über `/mcp` werden hier nicht geprüft
  (Guardrails bzw. `McpGrant`).
