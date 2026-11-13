# ai-gw – Admin-API: Mandanten, Nutzer, Keys, Modelle, Preise, Anmeldung, Audit-Log (AIGW-28, AIGW-65, AIGW-32, AIGW-66, AIGW-74)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §3, §5, §6, §8, §13, §15 · [02-datenmodell.md](02-datenmodell.md)

## 1. Überblick

Die Admin-API läuft auf der Steuerebene (`server.control.listen`, Standard `:4001`) unter
`/admin/api/v1`. Sie ist vollständig in der OpenAPI-Spezifikation beschrieben
(`GET /admin/api/openapi.json`, eingecheckt als `ui/openapi.json`, daraus
`ui/src/lib/api/schema.d.ts`). Code: `crates/aigw-admin` (`resources.rs` mit den generischen
Abläufen, `deployments.rs`, `credentials.rs`, `prices.rs`, `auth.rs`,
`problem.rs`; Anmeldung und Rollen siehe §5, Audit-Log `audit.rs` §5.10).

Jede Änderung läuft als eine Datenbank-Transaktion mit `config_version + 1` und Audit-Eintrag
(Akteur = angemeldeter Principal). Der Reloader baut danach den Snapshot neu (§14), d. h. ein
neuer, gesperrter oder geänderter Key wirkt auf der Datenebene **ohne Neustart** – in einer
Instanz nach ca. 100 ms (Debounce), bei Postgres auf allen Instanzen per `NOTIFY`.

## 2. Ressourcen

| Pfad | Methoden | Inhalt |
|---|---|---|
| `/organizations`, `/organizations/{name}` | GET, POST · GET, PUT, DELETE | Organisationen |
| `/teams`, `/teams/{name}` | GET, POST · GET, PUT, DELETE | Teams (`organization`, `allowedModels`, Limits, Budget) |
| `/users`, `/users/{name}` | GET, POST · GET, PUT, DELETE | Nutzer (OIDC-Issuer + Kennung, E-Mail, Rollen; §5.2) |
| `/users/{name}/invitation` | GET, POST, DELETE | Einladungslink für einen vorab angelegten Nutzer (F90, §5.2) |
| `/api-keys`, `/api-keys/{name}` | GET, POST · GET, PUT, DELETE | virtuelle Keys der Datenebene |
| `/api-keys/{name}/suspend` | POST | vorübergehend sperren (`state: suspended`, F37) |
| `/api-keys/{name}/resume` | POST | Sperre aufheben (`state: active`) |
| `/api-keys/{name}/revoke` | POST | **endgültig** widerrufen (`state: revoked`) |
| `/api-keys/{name}/rotate` | POST | neuer Key, alter läuft ab |
| `/model-groups`, `/model-groups/{name}` | GET, POST · GET, PUT, DELETE | Modellgruppen (Aliasse, Routing-Strategie, Fallbacks, Limits) |
| `/deployments`, `/deployments/{name}` | GET, POST · GET, PUT, DELETE | Deployments (Anbieter, Endpunkt, Anbieter-Modell, Zugangsdaten, Gewicht, Priorität, Kapazität, aktiv) |
| `/provider-credentials`, `/provider-credentials/{name}` | GET, POST · GET, PUT, PATCH, DELETE | Anbieter-Zugangsdaten (Werte nur schreibbar) |
| `/prices`, `/prices/{name}` | GET, POST · GET, PUT, DELETE | Preisversionen je Anbieter-Modell |
| `/plugin-bindings`, `/plugin-bindings/{name}` | GET, POST · GET, PUT, DELETE | Plugin-Bindungen; Prüfung mit der Plugin-Registry (422), gelesen mit wirksamem `failMode` (`effective`), siehe [12-plugins.md](12-plugins.md) §3–4 |
| `/admin-api-keys`, `/admin-api-keys/{name}` | GET, POST · GET, PUT, DELETE | Admin-API-Keys (§5.5) |
| `/session`, `/auth/info` | GET | angemeldeter Principal bzw. Anmeldeverfahren (§5) |
| `/audit-log` | GET | Audit-Log lesen, filtern, als JSON Lines exportieren (§5.10) |
| `/audit-log/verify` | GET | Hash-Kette des Audit-Logs prüfen (F116; nur globaler `admin`/`auditor`), auch `aigw audit verify` |
| `/secrets`, `/secrets/{name}` | GET, POST · GET, PUT, PATCH, DELETE | Allgemeine Geheimnisse (F149): Werte nur schreibbar, nie in Antworten/Audit; Verweise aus Webhook (`secret: {secret: name}`), `content-safety` (`secret: name`) und MCP-Server (`spec.secret`) |
| `/mcp-servers:status` | GET | Zustand der Upstream-MCP-Server (`up`/`down`/`idle`/`disabled`, letzter Fehler; AIGW-41-Folge) |
| `/usage/budgets` | GET | Budgetstände ([11-limits-und-budgets.md](11-limits-und-budgets.md)) |
| `/analytics/timeseries`, `/analytics/summary`, `/analytics/top` | GET | Dashboards: Zeitreihen, KPIs mit Vorperiode, Ranglisten (§5.11, [07-beobachtung.md](07-beobachtung.md) §13) |
| `/request-logs`, `/request-logs/{id}`, `/request-logs/{id}/payload` | GET | Log-Explorer: Anfragen, Upstream-Versuche, Payload (§5.11) |
| `/usage/limit-warnings` | GET | Hinweise zu ungenau wirkenden Limits (§5.12, F29) |
| `/version` | GET | Version (ohne Anmeldung) |

Körper wie in GitOps-YAML ([02-datenmodell.md](02-datenmodell.md) §3), als JSON:

```json
POST /admin/api/v1/teams
{"metadata": {"name": "platform", "labels": {"env": "prod"}},
 "spec": {"organization": "acme", "allowedModels": ["gpt-4o"]}}
```

Gelesen kommt zusätzlich `id` (UUIDv7) und `metadata.createdAt/updatedAt`. Verweise (Team →
Organisation, Key → Team/Nutzer) stehen als **Name** im Körper; unbekannte Verweise ergeben `422`.
`allowedModels` wird gegen die vorhandenen Modellgruppen (Name oder Alias) geprüft (`422` bei
Unbekanntem). `PUT` ersetzt Labels und `spec` vollständig; Umbenennen ist nicht möglich
(`metadata.name` muss zum Pfad passen oder leer sein).

### 2.1 API-Keys

- **Erzeugen** (`POST /api-keys`) antwortet `201` mit `{"key": {…}, "secret": "sk-aigw-…"}`. Der
  Klartext steht **nur** in dieser Antwort (`Cache-Control: no-store`), gespeichert werden
  Präfix und SHA-256.
- **Liste/Detail** zeigen nur `status.prefix` (12 Zeichen), `status.state`, `status.lastUsedAt`.
- **Zustände (F37):** `active` → `suspended` (`POST …/suspend`, umkehrbar mit `…/resume`) bzw.
  → `revoked` (`POST …/revoke`, **endgültig**: kein `resume`/`suspend`, kein `PUT`, keine
  Rotation – `409`; nur noch Löschen). Gleicher Zustand ist idempotent (keine neue
  Konfigurationsversion). Gesperrte und widerrufene Keys fehlen im Key-Index des Snapshots
  (wirkt nach dem Hot Reload, i. d. R. < 1 s). Audit: `suspend`, `resume`, `revoke`. Die
  frühere Route `…/reactivate` entfällt. GitOps ändert den Zustand nicht (09-gitops.md §4.3).
- **Rotation**: `POST …/rotate` mit optionalem Körper
  `{"name": "ci-2", "gracePeriodSeconds": 3600}`. Legt in **einer** Transaktion einen neuen Key
  mit derselben Spezifikation und denselben Labels an und setzt beim alten `expiresAt` auf
  jetzt + Übergangszeit (Standard 24 h, `0` = sofort, höchstens 365 Tage; ein früherer Ablauf
  bleibt). Standardname: alter Name mit Zeitstempel `-JJJJMMTThhmmss` (ein vorhandener
  Zeitstempel wird ersetzt). Antwort `201` mit `{"newKey": {key, secret}, "previousKey": {…}}`.
  Gesperrte und widerrufene Keys lassen sich nicht rotieren (`409`). Audit: `create` (neu) und
  `rotate` (alt).
- **Organisation ist Pflicht, Team optional** (F3, AIGW-73): `spec.organization` muss existieren
  (`422`), mit Team muss es die Organisation des Teams sein (`422`). Keys ohne Team erben Limits,
  Budgets und Plugins der Organisation und werden bei den Grenzen (AIGW-67) direkt gegen die
  Organisation geprüft (auch `?cascade=clamp` beim Senken der Organisationsgrenzen). Zieht ein
  Team in eine andere Organisation um, ziehen seine Keys mit (Fremdschlüssel `ON UPDATE
  CASCADE`). Rollen (F193): Keys ohne Team liegen **direkt in ihrer Organisation**
  (`Target::InOrganization`) – `org-admin` der Organisation (und dort eingeschränkte
  `admin`/`viewer`/`auditor`) verwalten bzw. lesen sie, `team-admin` nicht; Listen filtern das in
  SQL. Die Sichtbarkeit in Dashboards/Log-Explorer (F146) folgt mit AIGW-76.

### 2.2 Modellgruppen (AIGW-65)

```json
POST /admin/api/v1/model-groups
{"metadata": {"name": "gpt-4o"},
 "spec": {"aliases": ["gpt-4o-latest", "openai/gpt-4o"], "routing": "least-latency",
          "fallbacks": [{"modelGroup": "claude-sonnet", "on": ["rate-limit", "unavailable"]}]}}
```

- **Aliasse** teilen sich mit allen Gruppennamen **einen** Namensraum (Tabelle `model_alias`):
  ein Alias, der bereits Name oder Alias einer Gruppe ist (oder umgekehrt ein neuer Name, der
  schon Alias ist), ergibt `409 conflict` – ohne neue Konfigurationsversion. Aliasse dürfen
  bis 256 Zeichen lang sein und `/`, `:`, `@` enthalten (Modellnamen der Clients), aber keine
  Leer- oder Steuerzeichen; doppelte Aliasse und der eigene Name als Alias → `422`.
- **Routing**: `weighted` (Standard), `least-latency`, `least-busy`, `cost` (Architektur §6).
- **`settings`** (`retry`, `circuitBreaker`, `healthCheck`) werden mit dem Parser der
  Datenebene geprüft (`GroupPolicy::from_settings`); unbekannte Felder oder ungültige Werte →
  `422` (statt still durch Standardwerte ersetzt zu werden). GitOps prüft ebenso.
- **Fallbacks**: Ziel ist der **kanonische Name** einer vorhandenen Gruppe (ein Alias ergibt
  `422` mit Hinweis), nicht die Gruppe selbst, ohne Doppelungen und **ohne Zyklen** über
  Gruppen hinweg (`a → b → a` → `422`). Eine Gruppe, die Fallback-Ziel ist, lässt sich nicht
  löschen (`409`); ebenso eine Gruppe mit Deployments (Fremdschlüssel).
- **Verweise (F66):** Löschen einer Gruppe bzw. Entfernen eines Alias (`PUT` ohne ihn) ergibt
  `409` mit `violations[]`, solange `allowedModels` von Teams/Keys oder `limits[].modelGroup` von
  Organisationen/Teams/Keys den Namen bzw. Alias nennen. Mit `?cascade=remove` werden diese
  Verweise in **derselben Transaktion** entfernt (Audit `cascade-remove` je Objekt). Ausnahme:
  Würde dadurch eine Freigabeliste leer (leer = **alle** Modelle), bleibt es bei `409` – die
  Liste muss zuerst angepasst werden (keine stille Rechteausweitung). Andere `cascade`-Werte und
  `force` → `400`. GitOps hat kein `cascade`: Verweise auf entfernte Gruppen sind dort
  Validierungsfehler.
- Suche `?q=` trifft Namen und Aliasse.

### 2.3 Deployments (AIGW-65)

```json
POST /admin/api/v1/deployments
{"metadata": {"name": "gpt-4o-azure-ptu"},
 "spec": {"modelGroup": "gpt-4o", "provider": "azure-openai",
          "endpoint": "https://acme.openai.azure.com", "providerModel": "gpt4o-ptu",
          "credential": "azure-prod", "weight": 1, "priority": 0,
          "capacity": {"rpm": 600, "tpm": 400000}, "enabled": true,
          "settings": {"apiVersion": "2024-10-21"}}}
```

| Prüfung | Regel |
|---|---|
| Verweise | `modelGroup` und `credential` müssen existieren (`422`) |
| Zugangsdaten | Anbieter der Zugangsdaten passt zum Deployment (`422`): gleicher Anbieter oder laut **Kompatibilitätsliste** (F65, `aigw_core::validate::CREDENTIAL_COMPAT`): `openai` auch für `openai-compatible`, `vllm`, `ollama`. Zugangsdaten mit Deployments lassen sich nicht löschen (`409`) und nur auf einen Anbieter umstellen, zu dem alle ihre Deployments passen (`409`). GitOps prüft dieselbe Liste |
| Endpunkt | Pflicht bei `azure-openai`, `vllm`, `openai-compatible`; `ollama` ohne Endpunkt nutzt `http://localhost:11434/v1` und unterliegt ebenfalls den SSRF-Regeln (Freischaltung nötig); siehe §2.6 |
| Azure OpenAI | `providerModel` = Azure-Deployment-Name (Buchstaben, Ziffern, `-_.`); `settings.apiVersion` Pflicht bei `settings.apiStyle` `deployment` (Standard), optional bei `v1` |
| AWS Bedrock | `settings.region` Pflicht |
| Gewicht/Kapazität | `weight` 1–1 000 000 (abschalten über `enabled: false`), `capacity.rpm/tpm` > 0, falls angegeben |
| `providerModel` | 1–256 Zeichen ohne Steuerzeichen |
| `settings.caFile` | nur mit `upstream.ca_dir` (F177): relativ = darin, absolut = muss darin liegen, ohne `..`; die Admin-API löst Symlinks auf und lehnt Ziele außerhalb sowie unlesbare Dateien ab (`422`). GitOps per CLI prüft nur textuell (kann auf einem anderen Rechner laufen); beim Aufbau der Route prüft der Gateway erneut (Symlinks, Lesbarkeit) |

Filter: `?modelGroup=`, `?provider=`, `?credential=`, `?enabled=true|false`, `?q=` (Name oder
`providerModel`). Deaktivierte Deployments bleiben in der Datenbank, fehlen aber im Snapshot.

### 2.4 Anbieter-Zugangsdaten (AIGW-65)

```json
POST /admin/api/v1/provider-credentials
{"metadata": {"name": "openai-prod"},
 "spec": {"provider": "openai", "data": {"apiKey": {"value": "sk-…"}}}}
→ 201
{"id": "…", "metadata": {"name": "openai-prod", "createdAt": "…", "updatedAt": "…"},
 "spec": {"provider": "openai", "fields": ["apiKey"]},
 "status": {"hasSecret": true, "keyId": "3f1c…"}}
```

- Werte werden sofort mit dem Master-Key verschlüsselt (AES-256-GCM, AAD = Ressourcen-ID).
  **Keine Antwort, kein Fehlertext und kein Audit-Eintrag enthält einen Wert**; JSON-Fehler,
  in denen serde einen Wert zitieren würde, werden gekürzt. Antworten tragen
  `Cache-Control: no-store`.
- `valueFrom` (Umgebungsvariable/Datei) ist über die API **nicht** erlaubt (`422`): Der Server
  würde seine eigene Umgebung bzw. sein Dateisystem lesen (z. B. `AIGW_MASTER_KEY`) und den
  Wert an einen frei wählbaren Endpunkt senden. GitOps löst `valueFrom` auf dem Rechner auf,
  der `aigw config apply` ausführt, und sendet den Wert.
- Pflichtfelder je Anbieter (eine Alternative vollständig): `openai`, `anthropic`, `mistral`:
  `apiKey`; `azure-openai`: `apiKey` oder `tenantId` + `clientId` + `clientSecret`; `bedrock`:
  `accessKeyId` + `secretAccessKey` (optional `sessionToken`) oder `apiKey`; `vertex`:
  `serviceAccountJson`, `clientEmail` + `privateKey`, `accessToken` oder `apiKey`; `ollama`,
  `vllm`, `openai-compatible`, `mock`: beliebig. Feldnamen: Buchstabe, dann
  Buchstaben/Ziffern/`_` (max. 64), höchstens 32 Felder, Werte 1 Byte – 64 KiB.
- `PUT` ersetzt Anbieter, Labels und **alle** Werte.
- `PATCH` (JSON Merge Patch, `application/merge-patch+json` oder `application/json`):
  ```json
  PATCH /admin/api/v1/provider-credentials/openai-prod
  {"metadata": {"labels": {"env": "prod"}},
   "spec": {"data": {"organization": {"value": "org-…"}, "project": null}}}
  ```
  Angegebene Felder werden gesetzt, `null` entfernt ein Feld, **nicht genannte Felder behalten
  ihren gespeicherten Wert** (der Server entschlüsselt, führt zusammen und verschlüsselt neu –
  dabei auch mit dem aktuellen Master-Key). `metadata.labels` ersetzt die Labels nur, wenn
  angegeben; `spec.provider` wechselt den Anbieter. Die Pflichtfelder werden gegen das
  Ergebnis geprüft.
- `ETag`/`If-Match` wie überall; der ETag ändert sich auch, wenn nur ein Wert ersetzt wurde
  (`updatedAt`).

### 2.5 Preise (AIGW-65)

```json
POST /admin/api/v1/prices
{"spec": {"provider": "openai", "model": "gpt-4o-2024-08-06",
          "validFrom": "2024-08-06T00:00:00Z",
          "perMillionTokens": {"input": "2.50", "cachedInput": "1.25", "output": "10"}}}
→ 201, metadata.name = "openai-gpt-4o-2024-08-06-20240806"
```

- **Versioniert**: Eine Version gilt für (Anbieter, Modell) ab `validFrom` bis zur nächsten.
  (Anbieter, Modell, `validFrom`) ist eindeutig (`409`). Ohne `provider` gilt der Preis für alle
  Anbieter des Modells (Filter `?provider=any`).
- **Keine Änderung vergangener Versionen**: Ist `validFrom` erreicht, ergeben `PUT` mit
  geänderter `spec` und `DELETE` `409` („neue Version anlegen“); nur Labels lassen sich ändern.
  Künftige Versionen dürfen bis zu ihrem Beginn geändert und gelöscht werden, aber nicht in die
  Vergangenheit verlegt werden.
- Token-Arten in `perMillionTokens`: `input`, `output`, `cachedInput`, `cacheWrite`,
  `reasoning`, `audioInput`, `audioOutput`, `imageInput`; dazu `perRequest`, `perImage`.
  Beträge als Dezimaltext (in JSON auch als Zahl), nicht negativ.
- Ohne `metadata.name` vergibt der Server `<anbieter|any>-<modell>-<JJJJMMTT[hhmmss]>`
  (Modellname klein, andere Zeichen als `a-z0-9-._` → `-`).
- Filter: `?model=` (exakt), `?provider=` (`any` = ohne Anbieter), `?effectiveAt=<RFC 3339>`
  (nur die zu diesem Zeitpunkt gültige Version je (Anbieter, Modell)).

**GitOps** (`aigw config apply`, `POST /config:apply`) wendet dieselben Regeln an – sie liegen
gemeinsam in `aigw_core::validate` bzw. `aigw_core::upstream`: Aliasse, Fallback-Ziele und
-Zyklen (im Endzustand), Deployments (SSRF, Pflichtfelder, Zugangsdaten desselben Anbieters),
Feldnamen/Pflichtfelder der Zugangsdaten, Preisbeträge und Unveränderlichkeit gültiger
Preisversionen. Die CLI nutzt die Ausnahmen aus `admin.*` derselben Startkonfiguration.
Abweichung: `--prune` darf gültige Preisversionen löschen (Manifest = Sollzustand).

### 2.6 Endpunktprüfung (SSRF-Schutz)

Jeder `endpoint` eines Deployments wird beim Schreiben geprüft (`aigw_core::upstream`):

- Schema `https`; `http` nur für Hosts aus `admin.allow_insecure_upstreams`.
- Keine Zugangsdaten in der URL (`user:pass@`), kein Fragment, keine Leer-/Steuerzeichen.
- **Private, lokale und interne Ziele sind verboten**, außer sie stehen in
  `admin.allow_private_upstreams`: Loopback, RFC 1918, CGNAT `100.64/10`, Link-Local
  (inkl. Cloud-Metadaten `169.254.169.254`), `198.18/15`, reservierte Bereiche, IPv6 `::1`,
  ULA `fc00::/7`, Link-/Site-Local, IPv4 in IPv6 (`::ffff:…`, NAT64 `64:ff9b::/96`); dazu
  Namen `localhost`, `*.localhost`, `*.local`, `*.internal`, `*.lan`, `*.home.arpa`,
  `*.localdomain` und einteilige Namen (Docker-/Kubernetes-Dienstnamen wie `ollama`).
- Immer verboten: unspezifizierte, Broadcast- und Multicast-Adressen sowie numerische
  Altschreibweisen von IPv4 (`2130706433`, `0x7f.1`, `127.1`), die manche Resolver als IP deuten.

```yaml
admin:
  # Einträge: Hostname, Muster *.endung, IP-Adresse oder CIDR-Bereich
  allow_insecure_upstreams: [localhost, ollama, "*.lan"]
  allow_private_upstreams: [localhost, ollama, "*.lan", 192.168.0.0/16, "fd00::/8"]
  allow_mock: true   # false: Deployments mit provider: mock werden abgelehnt (F85)
```

`admin.allow_mock` (Standard `true`, F85): Mit `false` lehnen Admin-API und GitOps Deployments
mit `provider: mock` ab (`422`), und das Routing lässt bereits gespeicherte aus (Warnung im
Log; die Gruppe antwortet ohne weitere Deployments mit `503`).

ENV: `AIGW_ADMIN__ALLOW_INSECURE_UPSTREAMS=localhost,ollama`. Ein lokaler Ollama per `http`
braucht beide Listen. Ungültige Einträge brechen den Start ab.

**Grenze**: Hostnamen werden beim Schreiben nicht aufgelöst – ein öffentlicher Name, der auf
eine private Adresse zeigt (oder per DNS-Rebinding später zeigt), fällt hier nicht auf. Das
übernimmt der gemeinsame Outbound-Baustein (`aigw-outbound`, AIGW-72): Alle ausgehenden
Clients (Anbieter, Webhooks, MCP, Guardrail-Dienste) prüfen bei jedem Verbindungsaufbau die
aufgelösten Adressen mit `UpstreamPolicy::resolved_ip_allowed` (siehe
[06-streaming-und-anbieter.md](06-streaming-und-anbieter.md) §4).

## 3. Konventionen

| Thema | Regel |
|---|---|
| Adressierung | über den Namen (je Art global eindeutig); IDs sind nur Information |
| Paginierung | `?limit=` (1–500, Standard 50) und `?cursor=` (Wert aus `nextCursor` der vorigen Seite = Name des letzten Eintrags); Sortierung nach Name; `nextCursor` fehlt auf der letzten Seite. Seit AIGW-73 (F42) echte SQL-Pagination: Keyset (`name > cursor`), Filter, Suche, Labels und der Geltungsbereich der Rolle stehen im `WHERE`, die Datenbank liefert höchstens `limit + 1` Zeilen (`Store::list_page`) |
| Filter (alle) | `?q=` Teilstring im Namen (Keys: auch Präfix, Modellgruppen: auch Aliasse), `?label=k=v,k2` (alle müssen passen, ohne `=` genügt das Vorhandensein) |
| Filter (je Art) | Teams `organization`; Nutzer `email` (ohne Groß-/Kleinschreibung); Keys `team`, `user`, `organization` (Organisation des Keys, F3), `state`; Deployments `modelGroup`, `provider`, `credential`, `enabled`; Zugangsdaten `provider`; Preise `model`, `provider`, `effectiveAt`; MCP-Server `transport`, `credential`, `enabled`; MCP-Freigaben `server`, `team`, `apiKey` |
| Teiländerung | `PATCH` (JSON Merge Patch) nur bei Zugangsdaten (Werte bleiben erhalten); sonst `PUT` mit vollständiger `spec` |
| Unbekannte Parameter | `400` (Tippfehler fallen auf) |
| Optimistische Sperre (F38) | Einzelobjekte tragen einen starken `ETag` (SHA-256 über den Inhalt; bei Keys ohne `lastUsedAt`; auch Admin-API-Keys). **`If-Match` ist Pflicht** für `PUT`, `PATCH`, `DELETE` und Aktionen (`suspend`, `resume`, `revoke`, `rotate`): fehlt er → `428 precondition-required`; passt keiner der angegebenen ETags → `412 precondition-failed`; `*` schreibt ohne Versionsvergleich. Anlegen (`POST` auf die Liste), `config:apply` (eigene Versionsprüfung) und Einladungen brauchen ihn nicht. |
| Anlegen | `201` mit `Location` und `ETag` |
| Löschen | `204`; Verweise anderer Ressourcen verhindern das Löschen (`409`), Plugin-Bindungen und MCP-Freigaben des Objekts werden mitgelöscht |
| Zeitpunkte | RFC 3339 (UTC) |

**Cluster-sicher:** Die Admin-API vergleicht den ETag mit dem gelesenen Objekt und übergibt
dessen `updatedAt` an den Store (`aigw_store::Expect`). Der Store vergleicht ihn **im** `UPDATE`
bzw. `DELETE` (`… WHERE id = $1 AND updated_at = $2`); trifft die Anweisung keine Zeile, wurde das
Objekt zwischenzeitlich geändert – auch von einer anderen Instanz – und die Antwort ist `412`,
ohne neue Konfigurationsversion. `updated_at` steigt je Objekt streng monoton (mindestens +1 ms je
Änderung), damit zwei Änderungen in derselben Millisekunde nicht denselben Stand tragen. Bei
`If-Match: *` entfällt der Vergleich.

## 4. Fehlerformat (RFC 9457)

Alle Fehler der Admin-API sind Problem Details mit `Content-Type: application/problem+json`:

```json
{"type": "urn:aigw:problem:precondition-failed", "title": "Ressource wurde zwischenzeitlich geändert",
 "status": 412, "detail": "If-Match passt nicht zum aktuellen ETag – neu laden und erneut versuchen",
 "code": "precondition-failed"}
```

| Status | `code` | Wann |
|---|---|---|
| 400 | `bad-request` | JSON nicht lesbar, unbekannte/ungültige Query-Parameter |
| 401 | `unauthorized` | keine/ungültige/abgelaufene Anmeldung (`WWW-Authenticate: Bearer realm="aigw-admin"`) |
| 403 | `forbidden` | Rolle oder Geltungsbereich reicht nicht; CSRF-Prüfung fehlgeschlagen |
| 404 | `not-found` | Ressource unbekannt |
| 409 | `conflict` | Name vergeben, Ressource noch verwendet, Key gesperrt (Rotation) |
| 412 | `precondition-failed` | `If-Match` passt nicht (auch: zwischen Lesen und Schreiben geändert) |
| 428 | `precondition-required` | `If-Match` fehlt bei `PUT`/`PATCH`/`DELETE`/Aktionen (F38) |
| 415 | `unsupported-media-type` | kein `Content-Type: application/json` |
| 422 | `validation-failed` | ungültiger Name, unbekannte Felder, unbekannter Verweis oder Modell |
| 500 | `internal` | interner Fehler – ohne Details nach außen, Ursache im Log |

`instance` (Pfad) wird derzeit nur bei `401` gesetzt.

## 5. Authentifizierung, Sessions, Rollen (AIGW-32)

Code: `crates/aigw-admin` – `auth.rs` (Principal, Middleware, CSRF, Bootstrap), `oidc.rs`
(Relying Party), `session.rs` (Login, Session, Abmeldung, Einladungen), `rbac.rs` (Rollen),
`admin_keys.rs` (Admin-API-Keys); Speicher `aigw-store/src/admin_auth.rs`, Migrationen
`0001_init.sql` und `0003_admin_auth.sql` (Einladungen, je Dialekt);
Konfiguration `admin.auth` (`aigw_core::config::AdminAuthConfig`).

### 5.1 Verfahren

Die Middleware `require_principal` liegt als `route_layer` vor allen geschützten Routen; sie
fragt eine `AuthChain` und legt den `Principal` (`subject` für das Audit-Log, wirksame Rollen,
Verfahren) in die Request-Extensions. Handler ändern sich dadurch nicht.

| Reihenfolge | Erkennung | Verfahren | Audit-Akteur |
|---|---|---|---|
| 1 | `Authorization: Bearer sk-aigw-admin-…` | Admin-API-Key (§5.5) | `admin-key:<name>` |
| 2 | `Authorization: Bearer <anderes>` | Bootstrap-Token (§5.6) | `bootstrap-admin` |
| 3 | kein `Authorization`, Cookie `__Secure-aigw-session` | Session nach OIDC-Login (§5.2/5.3) | `user:<name>` |

Mit `Authorization`-Header wird nie auf das Cookie zurückgefallen (eindeutige Zuordnung;
Skripte senden keine Cookies, Browser keinen Bearer). Ein Key der Datenebene (`sk-aigw-…`) ist
an der Admin-API ungültig und umgekehrt.

Ohne Anmeldung erreichbar: `GET /admin/api/v1/version`, `GET /admin/api/v1/auth/info`
(Anmeldeverfahren und – falls angemeldet – der Principal; immer `200`, damit die UI beim Start
keinen `401` in der Konsole erzeugt), `GET /admin/api/openapi.json`, die UI und
`/admin/auth/{login,callback,logout}`.

`auth/info` meldet zusätzlich (F56) `readOnly` (Modus „nur YAML“, `gitops.read_only_admin`: die UI
blendet Schreibaktionen aus) und `bootstrap` (das Bootstrap-Token gilt derzeit: Modus `on` bzw.
`auto` ohne Abschaltvermerk – die UI zeigt einen Warnhinweis).

```json
{"oidc": true, "loginUrl": "/admin/auth/login", "readOnly": false, "bootstrap": true,
 "session": {"subject": "user:anna", "method": "session", "roles": [{"role": "admin"}], "csrfToken": "…"}}
```

### 5.2 OIDC-Login (Authorization Code + PKCE)

| Schritt | Pfad | Inhalt |
|---|---|---|
| 1 | `GET /admin/auth/login?returnTo=/admin/keys` | erzeugt `state`, `nonce`, PKCE-Verifier (je 256 Bit); speichert sie (Tabelle `admin_login_state`, Schlüssel SHA-256 von `state`, 10 min); setzt Cookie `__Secure-aigw-login=<state>` (`HttpOnly; Secure; SameSite=Lax; Path=/admin/auth`); `303` zum IdP mit `code_challenge_method=S256` |
| 2 | `GET /admin/auth/callback?code&state` | `state` muss zum Login-Cookie passen (Schutz vor Login-CSRF) und wird beim Lesen gelöscht (einmalig); Code-Tausch mit Verifier; ID-Token-Prüfung; Nutzerzuordnung; Session anlegen; `303` nach `returnTo` (nur Pfade unter `/admin/`, kein offener Redirect) |
| – | Fehler | `303` nach `/admin/?auth_error=<code>` mit `idp`, `state`, `token`, `invite`, `no-access`, `internal`; Grund nur im Server-Log |

**Discovery und Schlüssel:** `<issuer>/.well-known/openid-configuration` wird beim ersten Login
geladen (der Server startet auch bei nicht erreichbarem IdP) und `cache_secs` gehalten; ebenso
das JWKS. Unbekannte `kid` bzw. fehlgeschlagene Prüfung lösen ein Nachladen des JWKS aus
(höchstens alle 30 s, Schlüsselwechsel beim IdP). `issuer` im Discovery-Dokument muss zur
Konfiguration passen; alle Endpunkte brauchen `https://` (`http://` nur für Loopback – lokale
Entwicklung und Tests). Bietet der IdP `code_challenge_methods_supported` ohne `S256` an, wird
abgelehnt.

**ID-Token-Prüfung:** JWS Compact; `alg` ∈ {`RS256`, `ES256`, `EdDSA` (Ed25519)}, `none`, HMAC
und `crit` werden abgelehnt; Schlüsselwahl über `kid`, `use=sig`, passendes `kty`/`crv`/`alg`.
Danach `iss` (exakt wie im Discovery-Dokument), `aud` enthält die Client-ID, `azp` (falls
vorhanden) = Client-ID, `exp`/`iat`/`nbf` mit `clock_skew_secs`, `nonce` (konstante Zeit),
`sub` nicht leer, dazu die Kennung aus `identity_claim` (siehe unten). Gruppen aus `groups_claim` (Liste oder einzelner Wert; Punkte für verschachtelte
Claims, z. B. `realm_access.roles` bei Keycloak). Das Access-Token wird nicht verwendet.

**Client-Authentifizierung:** mit Secret `client_secret_basic`, sonst öffentlicher Client
(`client_id` im Formular, nur PKCE). Das Secret kommt aus `client_secret_file` oder der
Umgebungsvariable `client_secret_env` (Standard `AIGW_OIDC_CLIENT_SECRET`), nie aus der YAML.

**Identität (F90):** Ein Nutzer ist über **(Issuer, Kennung)** zugeordnet – `oidcIssuer` und
`oidcSubject` am Nutzer. Die Kennung ist der Wert des Claims `admin.auth.oidc.identity_claim`
(Standard `sub`). Profil `entra` (Microsoft Entra ID): Kennung `oid` (über alle Anwendungen des
Tenants stabil; `sub` ist dort je Anwendung paarweise), und `tid` muss `tenant_id` entsprechen
(sonst `auth_error=token`); `tenant_id` ist dann Pflicht. Nutzer lassen sich **vorab** mit
Issuer + Kennung anlegen (Admin-API, GitOps); beide Felder nur gemeinsam (`422`).

**Nutzerzuordnung** (Ressource `User`, §2):
1. Login über einen **Einladungslink** (`/admin/auth/login?invite=<token>`): Die Einladung wird
   beim Rücksprung einmalig eingelöst und verknüpft die Identität mit dem eingeladenen Nutzer
   (der noch keine Identität haben darf; Audit-Akteur `oidc-login`). Gehört die Identität schon
   einem anderen Nutzer bzw. ist die Einladung abgelaufen oder verwendet → `auth_error=invite`
   (eine ungültige Einladung fällt schon beim Start des Logins auf);
2. Nutzer mit gleichem `oidcIssuer` + `oidcSubject`;
3. nur mit `link_by_email: true` (**Standard aus**, F90 – E-Mail-Adressen sind beim IdP oft
   änderbar): ein vorab angelegter Nutzer **ohne** Identität, dessen E-Mail (ohne
   Groß-/Kleinschreibung) der vom IdP **bestätigten** E-Mail (`email_verified`) entspricht –
   genau ein Treffer; Issuer/Kennung werden eingetragen (Audit-Akteur `oidc-login`);
4. sonst (wenn `provision_users` und mindestens eine Gruppenrolle) Neuanlage mit Name aus
   `preferred_username` bzw. dem lokalen Teil der E-Mail (bei Kollision mit Hash-Suffix), ohne
   direkte Rollen;
5. sonst bzw. wenn keine wirksame Rolle übrig bleibt: `auth_error=no-access`, keine Session.

**Einladungen** (`/admin/api/v1/users/{name}/invitation`, nur globaler `admin`; lesen: globale
Leserollen):

```json
POST /admin/api/v1/users/anna/invitation
→ 201 {"invitation": {"user": "anna", "createdBy": "user:root", "createdAt": "…",
                      "expiresAt": "…", "expired": false},
       "url": "https://gateway.example.org/admin/auth/login?invite=Q2x…"}
```

Token 256 Bit, gespeichert nur SHA-256 (Tabelle `admin_invitation`, Migration
`0003_admin_auth.sql`), gültig `oidc.invite_ttl_secs` (Standard 7 Tage), höchstens eine je Nutzer
(eine neue ersetzt die alte), der Link steht nur in dieser Antwort (`Cache-Control: no-store`).
Ohne OIDC bzw. für Nutzer mit Identität → `409`. `GET` zeigt den Stand ohne Link, `DELETE`
widerruft. Audit: Art `admin-invitation`, `create`/`delete` (ohne Token).

**Bibliothekswahl:** eigene, schlanke Relying Party (rund 500 Zeilen ohne Tests) statt `openidconnect-rs`:
- Kryptografie wird **nicht** selbst gebaut: Signaturprüfung ausschließlich über aws-lc-rs
  (`RsaPublicKeyComponents` + `RSA_PKCS1_2048_8192_SHA256`, `ECDSA_P256_SHA256_FIXED`,
  `ED25519`) – derselbe Krypto-Provider wie rustls, Keyring und Key-Hashes (kein ring, kein
  RustCrypto-Zweitstack, wie ihn `openidconnect`/`oauth2` bzw. `jsonwebtoken` mitbringen).
- Eigener Code ist nur Parsen (Base64url, JSON) und die Claim-Regeln aus OIDC Core §3.1.3.7 –
  überschaubar, vollständig getestet und frei von Ermessensentscheidungen einer Bibliothek
  (z. B. Algorithmus-Auswahl, Zeittoleranzen).
- Neue Abhängigkeiten nur für HTTPS und Kodierung: `hyper-rustls` (aws-lc-rs, HTTP/1.1),
  `rustls-native-certs` (System-CAs, optional `ca_file` für interne CAs), `base64`, `url`.

### 5.3 Session und CSRF

- **Cookie:** `__Secure-aigw-session=<256 Bit zufällig, Base64url>; Path=/admin;
  Max-Age=<max_age_secs>; HttpOnly; Secure; SameSite=Strict`. Der Präfix `__Host-` ist wegen
  `Path=/admin` nicht möglich. Browser behandeln `http://localhost`/`127.0.0.1` als sicher, das
  `Secure`-Flag stört die lokale Entwicklung daher nicht.
- **Speicher:** Tabelle `admin_session` mit SHA-256 der Session-ID, Nutzer, IdP-Gruppen beim
  Login, `created_at`, `last_seen_at`, `expires_at`. Wer die Datenbank liest, kann keine Session
  übernehmen. Löschen des Nutzers löscht seine Sessions (`ON DELETE CASCADE`).
- **Ablauf:** absolut nach `session.max_age_secs` (Standard 8 h), Leerlauf nach
  `session.idle_timeout_secs` (Standard 30 min); `last_seen_at` wird höchstens einmal pro Minute
  geschrieben. Abgelaufene Sessions werden bei der Prüfung gelöscht, alte Sessions und
  Login-Zustände bei jedem Login aufgeräumt.
- **Rollen je Anfrage:** direkte Rollen des Nutzers (`UserSpec.roles`, aktuell aus der DB) ∪
  Rollen aus den gespeicherten Gruppen (aktuelle Konfiguration). Gruppenänderungen beim IdP
  wirken erst beim nächsten Login.
- **Sessions enden** (AIGW-66), wenn sich die direkten Rollen (`spec.roles`, Reihenfolge egal)
  oder die OIDC-Identität (`oidcIssuer`/`oidcSubject`) eines Nutzers ändern oder der Nutzer
  gelöscht wird – über die Admin-API und GitOps, in derselben Transaktion wie die Änderung.
  Der Nutzer muss sich neu anmelden; im Audit-Log steht `revoke` (Art `admin-session`, Grund
  `roles-changed` bzw. `user-deleted`, Anzahl der Sessions). Andere Änderungen (E-Mail,
  Anzeigename, Labels) lassen Sessions bestehen.
- **CSRF** (zusätzlich zu `SameSite=Strict`), für `POST/PUT/PATCH/DELETE` mit Session-Cookie:
  1. Header `X-CSRF-Token` muss passen (Synchronizer-Token). Das Token ist
     SHA-256(`"aigw-csrf\0" + Session-ID`) – an die Session gebunden, ohne eigene Spalte und
     aus dem gespeicherten Hash nicht ableitbar. Die UI liest es aus
     `GET /admin/api/v1/auth/info` bzw. `GET /admin/api/v1/session` (`csrfToken`).
  2. `Sec-Fetch-Site: cross-site|same-site` → `403`.
  3. Ein `Origin`-Header muss dem Ursprung von `oidc.redirect_url` entsprechen (ohne OIDC: dem
     `Host`).
  Bearer-Anfragen (Admin-Key, Bootstrap) brauchen kein CSRF-Token.
- **Audit-Pflicht (F113):** Mit `admin.auth.audit_required: true` (Standard) entstehen Session
  und Audit-Eintrag `login` in **einer** Transaktion; kann der Eintrag nicht geschrieben werden,
  scheitert die Anmeldung (`auth_error=internal`, keine Session). Ebenso die Abmeldung: ohne
  Eintrag `logout` bleibt die Session bestehen (`500`). `false`: Eintrag nach bestem Bemühen
  (Warnung im Log).
- **Abmelden:** `POST /admin/auth/logout` (mit CSRF-Token) löscht die Session und das Cookie
  und liefert `{"redirectUrl": …}`: mit `end_session: true` und `end_session_endpoint` beim IdP
  die RP-initiated-Logout-URL (`client_id`, `post_logout_redirect_uri`), sonst `/admin/`.
  `id_token_hint` wird nicht verwendet (das ID-Token wird nicht gespeichert).

### 5.4 Rollen und Geltungsbereiche (§15)

Eine Rollenzuweisung (`RoleBinding`: `role`, optional `organization`, `team`) gilt global oder
eingeschränkt. Jede Operation fordert `Permission::Read`/`Write` auf einem `Target` an
(`Principal::require_on`); `Principal::require(perm)` bedeutet „global“.

| Ziel (`Target`) | Beispiele |
|---|---|
| `Global` | Nutzer, Admin-API-Keys, Organisation anlegen, API-Keys ohne Team |
| `GitOps` | `config:export` = Lesen (globale `admin`, `viewer`, `auditor` und `gitops`), `config:apply` inkl. Trockenlauf = Schreiben (globaler `admin` und `gitops`, F55) |
| `Catalog` | Modellgruppen, Deployments, Anbieter-Zugangsdaten (nur Metadaten, Werte sind nie lesbar), Preise (lesen: jede wirksame Rolle; schreiben: globaler `admin`) |
| `Organization(o)` | die Organisation selbst (lesen, ändern, löschen) |
| `Team(o, t)` | das Team selbst |
| `InTeam(o, t)` | API-Keys des Teams (anlegen, ändern, sperren, rotieren, löschen), Plugin-Bindungen des Teams |
| `InOrganization(o)` | Objekte direkt in der Organisation: API-Keys ohne Team (F193), Plugin-Bindungen der Organisation (F183) |

| Rolle | Lesen | Schreiben |
|---|---|---|
| `admin` (global) | alles | alles |
| `admin` (Org/Team) | im Bereich | im Bereich |
| `org-admin` (Org O) | O, Teams und Keys in O (auch ohne Team), Katalog | Teams, Keys und Plugin-Bindungen in O, nicht O selbst |
| `team-admin` (Team T) | T, Keys in T, Katalog; O nur, wenn die Zuweisung `organization` nennt | Keys und Plugin-Bindungen in T |
| `viewer` | im Bereich (global: alles), Katalog | – |
| `auditor` | wie `viewer`; dazu das Audit-Log (§5.10) und gespeicherte Payloads (§5.11) | – |
| `gitops` (nur global, F55) | nur `config:export` | nur `config:apply` (auch Trockenlauf) – für Admin-API-Keys der CI; sonst keine Rechte, Listen bleiben leer |

- **Plugin-Bindungen (F183):** Schreiben je Ebene – `global`/`model-group`: globaler `admin`;
  `organization` O: `InOrganization(O)`; `team` T: `InTeam(O, T)`; `api-key`: wie der Key (Team
  bzw. Organisation). Lesen weiterhin jede wirksame Rolle. Eingeschränkte Rollen dürfen eine
  Bindung, die auf einer äußeren Ebene (global, Organisation, Team des Ziels) für dasselbe Plugin
  `locked: true` ist, nicht per `inherit: override|disable` aushebeln (`403` mit Name der
  Sperre); zur Laufzeit wäre sie ohnehin wirkungslos. Ein globaler `admin` darf das (die Sperre
  gehört ihm).

- **Grenzen (AIGW-67):** Limits, Budgets und Modellfreigaben eines Keys dürfen die seines
  Teams und seiner Organisation nicht überschreiten, die eines Teams nicht die der
  Organisation – für alle Rollen hart (`422` mit `violations[]`). Senken eines Team-/Org-Werts
  unter bestehende Keys/Teams → `409` mit Liste, `?cascade=clamp` kürzt sie mit (Audit
  `clamp`); `?force=true` nur für den globalen `admin` (Audit `force`). Details:
  [11-limits-und-budgets.md](11-limits-und-budgets.md) §6a.
- `org-admin` ohne `organization` und `team-admin` ohne `team` sind wirkungslos.
- Listen sind gefiltert (eingeschränkte Rollen sehen nur ihren Bereich, z. B. `GET /teams`
  nur das eigene Team); Einzelobjekte außerhalb des Bereichs ergeben `403`.
- Beim Ändern muss der **alte und der neue** Zustand im Bereich liegen (kein Verschieben eines
  Teams in eine fremde Organisation bzw. eines Keys in ein fremdes Team).
- **Zuordnung:** direkt am Nutzer (`spec.roles`, über `/users`) und/oder per IdP-Gruppe
  (`admin.auth.oidc.group_roles`, je Eintrag `group`, `role`, optional `organization`/`team`).

### 5.5 Admin-API-Keys

`/admin/api/v1/admin-api-keys` (`GET`, `POST`) und `/admin-api-keys/{name}` (`GET`, `PUT`,
`DELETE`). Lesen: globale Leserolle; anlegen, ändern, löschen: nur globaler `admin` (keine
Rechteausweitung).

```json
POST /admin/api/v1/admin-api-keys
{"name": "gitops-ci", "description": "GitOps-Pipeline",
 "roles": [{"role": "team-admin", "organization": "acme", "team": "platform"}],
 "expiresAt": "2027-06-30T00:00:00Z"}
→ 201 {"key": {"name": "gitops-ci", "prefix": "Ab3dE…", "roles": […], "createdBy": "user:anna", …},
       "secret": "sk-aigw-admin-Ab3dE…-…"}
```

- Format wie die Keys der Datenebene mit eigenem Präfix: `sk-aigw-admin-<12 Base62>-<43 Base62>`
  (256 Bit). Gespeichert: Präfix und SHA-256 (Tabelle `admin_api_key`); Klartext nur in der
  Antwort (`Cache-Control: no-store`).
- Rollen: mindestens eine, wirksam, Verweise müssen existieren (`422`); `gitops` nur global.
  `expiresAt` optional, beim Anlegen/Ändern in der Zukunft; abgelaufene Keys ergeben `401`.
- **Höchstlaufzeit (F87):** `admin.auth.max_admin_key_lifetime` (z. B. `90d`; Einheiten `s`,
  `m`, `h`, `d`). Gesetzt: `expiresAt` höchstens jetzt + Höchstlaufzeit (`422`), fehlt die
  Angabe, setzt der Server jetzt + Höchstlaufzeit (auch beim `PUT`). Bestehende Keys bleiben
  unverändert.
- `PUT` ersetzt `description`, `roles`, `expiresAt`; `DELETE` widerruft sofort (keine
  Snapshot-Verzögerung, da je Anfrage aus der DB gelesen). Beide brauchen `If-Match` (ETag aus
  `GET`/Anlegen, F38).
- `lastUsedAt` (höchstens minütlich aktualisiert). Audit: `create`/`update`/`delete` mit Art
  `admin-api-key` (ohne Hash); die erste Nutzung eines Keys einmalig als `first-use` (Akteur
  `admin-key:<name>`, auch bei gleichzeitigen Anfragen genau ein Eintrag).

### 5.6 Bootstrap-Token

`Authorization: Bearer <token>`, Rolle `admin` global, Audit-Akteur `bootstrap-admin`. Quelle
`AIGW_BOOTSTRAP_TOKEN` (mindestens 32 Zeichen); fehlt die Variable, erzeugt der Server beim Start
ein Token `aigw-admin-<64 hex>` und gibt es **einmal** im Log (WARN) aus (nur für diesen
Prozess). Gespeichert wird nur SHA-256, verglichen in konstanter Zeit.

`admin.auth.bootstrap`:
- `auto` (Standard): aktiv, bis sich ein Nutzer mit **globaler** `admin`-Rolle per OIDC
  angemeldet hat oder ein Admin-API-Key mit globaler `admin`-Rolle angelegt bzw. darauf geändert
  wurde. Dann wird in `admin_auth_state` dauerhaft `bootstrap_disabled` (mit Grund) vermerkt –
  auch für alle anderen Instanzen (Prüfung je Bootstrap-Anfrage, danach im Speicher). Beim
  nächsten Start wird dann kein Token mehr erzeugt oder geloggt. Wieder einschalten nur mit `on`
  (Notfallzugang).
- `on`: immer aktiv. `off`: nie (auch kein Token in der Startausgabe).

### 5.7 Konfiguration (Auszug)

```yaml
admin:
  auth:
    bootstrap: "auto"            # auto | on | off
    session: {max_age_secs: 28800, idle_timeout_secs: 1800}
    audit_required: true         # F113: Login/Logout nur mit Audit-Eintrag (fail-closed)
    max_admin_key_lifetime: 90d  # F87: optional; ohne = unbegrenzt
    oidc:
      issuer: "https://idp.example.org/realms/acme"     # leer = OIDC aus
      profile: generic           # generic | entra (F90)
      identity_claim: sub        # Standard: sub (generic) bzw. oid (entra)
      # tenant_id: "…"           # Pflicht bei profile: entra (tid-Prüfung)
      invite_ttl_secs: 604800    # Einladungslinks (7 Tage)
      client_id: aigw
      client_secret_file: /run/secrets/aigw-oidc-secret  # oder Env AIGW_OIDC_CLIENT_SECRET
      redirect_url: "https://gateway.example.org/admin/auth/callback"
      scopes: [openid, profile, email]
      groups_claim: groups
      group_roles:
        - {group: aigw-admins, role: admin}
        - {group: acme-platform-leads, role: team-admin, organization: acme, team: platform}
      provision_users: true
      link_by_email: false       # F90: Standard aus
      end_session: true
```

Alle Felder außer `group_roles` sind per `AIGW_ADMIN__AUTH__…` überschreibbar (z. B.
`AIGW_ADMIN__AUTH__OIDC__ISSUER`). Ungültige OIDC-Konfiguration (fehlende `client_id`,
`redirect_url` nicht auf `/admin/auth/callback`, `http://` ohne Loopback, eingeschränkte Rolle
ohne Bereich in `group_roles`) verhindert den Start.

### 5.8 OpenAPI und UI

- Sicherheitsschemata: `bearer` (HTTP Bearer: Admin-API-Key oder Bootstrap-Token) und `session`
  (API-Key im Cookie `__Secure-aigw-session`, Beschreibung nennt `X-CSRF-Token`). Jede Operation
  mit `bearer` akzeptiert alternativ `session` (per Modifier ergänzt).
- Neue Pfade (Tag `auth`): `/admin/auth/login`, `/admin/auth/callback`, `/admin/auth/logout`,
  `/admin/api/v1/session`, `/admin/api/v1/auth/info`; Tag `admin-api-keys`.
- UI (`ui/src/lib/session.svelte.ts`): beim Start `auth/info`; ohne Session und mit OIDC
  Weiterleitung zum Login (`returnTo` = aktuelle Seite); `?auth_error=` zeigt einen Hinweis statt
  erneut weiterzuleiten. Das Nutzer-Menü zeigt Name, E-Mail und Rollen der Session und „Abmelden“
  (navigiert zur `redirectUrl`). Der API-Client setzt `X-CSRF-Token` bei schreibenden Anfragen.

### 5.9 Tests

`crates/aigw-admin/tests/oidc_login.rs` startet einen Mini-IdP (axum: Discovery, JWKS,
Authorize, Token mit PKCE-Prüfung und `client_secret_basic`; Testschlüssel RS256/ES256/EdDSA
aus aws-lc-rs) und prüft gegen SQLite und Postgres: vollständigen Login, Cookie-Attribute,
JIT-Anlage und E-Mail-Verknüpfung, Rollen und Bereiche (team-admin, org-admin, viewer, admin),
CSRF (fehlendes/falsches Token, fremder `Origin`, `Sec-Fetch-Site`), Leerlauf und absoluten
Ablauf, Nutzerlöschung, Abmeldung mit RP-initiated Logout, abgelehnte Tokens (`aud`, `exp`,
`nonce`, `iss`, Signatur), `state`-Fehler und -Wiederholung, offenen Redirect, Admin-API-Keys
(Rollen, Ablauf, Widerruf) und die Bootstrap-Modi.

`crates/aigw-admin/tests/oidc_login.rs` (`identity_*`) prüft F90/F113: vorab angelegte Nutzer
(Issuer + Kennung), E-Mail-Verknüpfung standardmäßig aus, Einladungen (einmalig, Ablauf,
Widerruf, fremde Identität, nur globale Admins, kein Token im Audit), Profil `entra` (`oid`,
`tid`), Login/Logout scheitern ohne Audit-Eintrag (bzw. nicht mit `audit_required: false`).
`crates/aigw-admin/tests/admin_auth_api.rs` (SQLite und Postgres) prüft F55, F56, F87, F38 bei
Admin-Keys und Zugangsdaten, F4, F52, F193, F183, F65, F177 und F66 (Alias);
`crates/aigw-store/tests/admin_auth.rs` den Versionsvergleich in der Datenbank (auch zwei
gleichzeitige Änderungen), Key-Zustände, Bereich teamloser Keys, Einladungen und Sessions mit
Audit in beiden Dialekten.

`crates/aigw-admin/tests/audit_log.rs` (SQLite und Postgres): Filter und Paginierung (auch
Seitengröße 3 vs. 500 und `order=asc`), Bereichsfilter je Rolle (org-admin, team-admin,
eingeschränkter auditor, viewer `403`), Vorher/Nachher (auch Verschieben eines Keys),
Redaktion (kein Zugangsdaten-Wert, kein Key-Klartext, kein Key-Hash in JSON und NDJSON),
NDJSON-Export (mit Filter, Höchstzahl, Bereich), Erstnutzung von Admin-Keys, Sessions enden
bei Rollenänderung/Löschen, Logout. `oidc_login.rs` prüft zusätzlich die Login-Ereignisse
(`login`, `login-failed` nur nach gültigem `state`, `logout`, Bootstrap-Abschaltung).

### 5.10 Audit-Log (AIGW-66)

**Arten (F115):** einheitlich kebab-case – `organization`, `team`, `user`, `api-key`,
`provider-credential`, `secret`, `model-group`, `deployment`, `price`, `plugin-binding`,
`mcp-server`, `mcp-grant`, dazu `admin-api-key`, `admin-session`, `bootstrap-token`,
`audit-log`. Migration 0005 schreibt ältere Einträge um; Filter akzeptieren weiter die
YAML-Schreibweise (`kind=ApiKey`).

**Manipulationsschutz (F116):** Jeder Eintrag trägt eine fortlaufende Nummer (`seq`), den Hash
seines Vorgängers und seinen eigenen Hash (SHA-256 über Vorgänger-Hash und eine kanonische
JSON-Form aller Felder); der Kettenkopf liegt in `audit_chain` und wird beim Schreiben gesperrt
(eindeutige Reihenfolge auch im Cluster). `GET /admin/api/v1/audit-log/verify` bzw.
`aigw audit verify -c <config> [--expect-head <hash>]` meldet veränderte, gelöschte,
eingeschobene Einträge und ein abgeschnittenes Ende. Grenze: Wer Schreibrechte auf die
Datenbank hat, kann die Kette komplett neu berechnen – dagegen den gemeldeten `headHash`
regelmäßig extern sichern (SIEM/WORM) und mit `--expect-head` vergleichen. Einträge aus der
Zeit vor der Kette zählen als „unverkettet“.

**Aufbewahrung (F116):** `audit.retention_days` (Standard `0` = unbegrenzt) löscht im Abstand
`audit.sweep_interval_secs` den Anfang der Kette; der letzte gelöschte Eintrag wird zum Anker,
die Kette bleibt prüfbar, die Löschung selbst steht als `audit-log`/`prune` im Log. Hinweis:
Der inkrementelle Snapshot-Neubau (AIGW-75) nutzt das Audit-Log als Journal – gelöschte
Versionen führen dort nur zum vollständigen Neubau.

`GET /admin/api/v1/audit-log` – Einträge **neueste zuerst** mit Vorher/Nachher-Zustand:

```json
{"items": [{"id": "0193…", "at": "2026-11-13T10:15:02.123Z", "actor": "user:anna",
            "action": "update", "kind": "ApiKey", "objectId": "0193…", "objectName": "ci",
            "configVersion": 42, "organization": "other", "team": "ops",
            "organizationBefore": "acme", "teamBefore": "data",
            "before": {"id": "…", "metadata": {…}, "spec": {"team": "data"}, "status": {…}},
            "after":  {"id": "…", "metadata": {…}, "spec": {"team": "ops"},  "status": {…}}}],
 "nextCursor": "1763028902123.0193…"}
```

| Parameter | Bedeutung |
|---|---|
| `limit`, `cursor` | Seitengröße 1–500 (Standard 50), Fortsetzung aus `nextCursor` (Keyset über Zeitpunkt + ID, stabil auch bei neuen Einträgen) |
| `order` | `desc` (Standard) oder `asc` |
| `from`, `to` | Zeitraum, RFC 3339; `from` einschließlich, `to` ausschließlich (`+` im Offset als `%2B` kodieren oder `Z` verwenden) |
| `actor` | exakte Akteure, kommagetrennt (`user:anna,admin-key:ci`) |
| `actorType` | `user`, `admin-key`, `bootstrap`, `gitops` (CLI `gitops`, API `gitops:<principal>`), `oidc-login`, `system` |
| `kind` | Arten, kommagetrennt: Ressourcen wie in YAML (`Organization`, `Team`, `User`, `ApiKey`, `ModelGroup`, `Deployment`, `ProviderCredential`, `Price` …) und `admin-api-key`, `admin-session`, `admin-invitation`, `bootstrap-token` |
| `action` | `create`, `update`, `delete`, `rotate`, `suspend`, `resume`, `revoke`, `clamp`, `force`, `cascade-remove`, `login`, `login-failed`, `logout`, `first-use`, `disable` |
| `object` | Name **oder** ID des Objekts (exakt) |

Unbekannte bzw. ungültige Parameter → `400`. Alle Filter wirken zusätzlich zum Bereich des
Lesers.

**Export (SIEM):** Mit `Accept: application/x-ndjson` kommen **alle** passenden Einträge (ohne
Seitengrenze; `limit` = Höchstzahl) als JSON Lines, je Zeile ein `AuditEntry`, gestreamt in
Blöcken zu 500 aus der Datenbank. Für inkrementelles Abholen: `order=asc&from=<letzter
Zeitpunkt>` und Duplikate über `id` verwerfen. Bricht die Datenbank während des Exports ab, wird
die Übertragung abgebrochen (kein vollständiger Abschluss) und der Fehler geloggt.

**Ereignisse** (ohne neue Konfigurationsversion; `configVersion` = damals aktuelle):

| Art / Aktion | Akteur | Inhalt (`after`) |
|---|---|---|
| `admin-session` / `login` | `user:<name>` | Verfahren, IdP-Gruppen, wirksame Rollen, Ablauf |
| `admin-session` / `login-failed` | `oidc-login` (Objekt: Nutzer, falls zugeordnet) | `code` (`token`, `no-access`, `state` bei abgelaufenem Login, `internal`), gekürzter Grund |
| `admin-session` / `logout` | `user:<name>` | – |
| `admin-session` / `revoke` | Akteur der Änderung | Grund, Anzahl der beendeten Sessions (§5.3) |
| `admin-api-key` / `first-use` | `admin-key:<name>` | – |
| `bootstrap-token` / `disable` | `system` | Grund (`oidc-admin:<name>`, `admin-api-key:<name>`) |

Fehlgeschlagene Logins werden **nur nach gültigem `state`** festgehalten (Login in diesem
Browser begonnen, Cookie passt): Aufrufe von `/admin/auth/callback` ohne gültigen `state`
(fremde Seiten, Wiederholungen, `error=` vom IdP ohne Login) könnte jeder ohne Anmeldung
erzeugen und damit das Audit-Log fluten; sie stehen nur im Server-Log. Fehlgeschlagene
Bearer-Anmeldungen (falscher Admin-Key) stehen aus demselben Grund nur im Debug-Log. Kann ein
Anmelde-Ereignis (`login`/`logout`) nicht geschrieben werden, scheitert die Anmeldung bzw.
Abmeldung (F113, `admin.auth.audit_required`, Standard); `login-failed` bleibt nach bestem
Bemühen.

**Berechtigung:**

| Rolle | Sicht |
|---|---|
| `admin`, `auditor` (global) | alle Einträge |
| `org-admin` (Org O), `admin`/`auditor` eingeschränkt auf O | Einträge, deren Objekt vorher oder nachher in O lag (O selbst, Teams und Keys in O) |
| `team-admin` (Team T), `admin`/`auditor` eingeschränkt auf T | Einträge zu T und Keys in T |
| `viewer` | kein Zugriff (`403`) |

Mehrere Zuweisungen ergeben die Vereinigung. Globale Objekte (Nutzer, Katalog, Zugangsdaten,
Admin-API-Keys, Sessions, Bootstrap) sehen nur globale Leser.

**Geltungsbereich der Einträge** (Migration `0004_audit_scope.sql`): Die ursprünglichen Einträge
trugen keinen Bereich. Jeder neue Eintrag speichert Organisation und Team des Objekts **zum
Zeitpunkt der Änderung** – nachher (`organization`, `team`; beim Löschen: vorher) und vorher
(`organizationBefore`, `teamBefore`). Wird ein Key in ein Team einer anderen Organisation
verschoben, sehen beide Bereiche den Eintrag. Bereich haben Organisationen (sich selbst), Teams
(Organisation + Team) und API-Keys mit Team (Organisation des Teams); alle anderen Arten sind
global. Spätere Umzüge ändern alte Einträge nicht. Bestandseinträge aus der Zeit vor der
Migration: nur Organisationen erhalten ihren Bereich, ältere Team-/Key-Einträge sind nur global
sichtbar.

**Geheimnisse:** Vorher/Nachher sind die Darstellungen der Admin-API: Zugangsdaten nur mit
Feldnamen (`spec.fields`), Keys nur mit Präfix, Admin-Keys ohne Hash, `SecretString` als
`<redacted>`. Session-IDs, CSRF-Tokens und ID-Tokens erscheinen nie. Antworten tragen
`Cache-Control: no-store`.

### 5.11 Dashboards und Log-Explorer (AIGW-46)

Code `crates/aigw-admin/src/analytics.rs`, Lesefunktionen `aigw_store::analytics`; Kennzahlen,
Aggregation und Messwerte in [07-beobachtung.md](07-beobachtung.md) §13.

**Geltungsbereich** (gilt für alle Routen):

| Principal | sieht |
|---|---|
| Leserecht `Global` (`admin`, `viewer`, `auditor` ohne Einschränkung) | alle Anfragen, auch ohne Team (Keys ohne Team, Fehler vor der Anmeldung) |
| eingeschränkte Rollen | Anfragen von Keys in Teams mit Leserecht `InTeam(o, t)` (Org-Rollen: alle Teams der Organisation) |
| an eine Organisation gebundene Rollen (`org-admin`, `viewer`/`auditor` mit `organization`, ohne `team`) | zusätzlich Anfragen von Keys **ohne Team** dieser Organisation (F146; seit F3 hat jeder Key eine Organisation) |

- Filter `team`/`key` außerhalb des Bereichs → `403`; `organization`, `modelGroup`,
  `deployment`, `provider` schneiden nur. Unbekannte Namen → `422`.
- Einzelanfrage außerhalb des Bereichs → `403`, unbekannt/abgelaufen → `404`.
- **Payload:** eigene Berechtigung `Permission::ReadPayload` – nur `admin` und `auditor` im
  jeweiligen Bereich (Ziel: das Team der Anfrage, ohne Team die Organisation, ganz ohne Key
  global). Andere Rollen erhalten die Metadaten mit `payloadStatus: "forbidden"`;
  `GET /request-logs/{id}/payload` antwortet `403`.
- **Client-Adresse** (`clientIp`) wie Payloads nur für `admin` und `auditor` (F147); andere
  Rollen sehen das Feld nicht.
- Teamlose Anfragen einer Organisation prüft die Admin-API über das RBAC-Ziel
  `InOrganization` (F193; deckt nur org-gebundene und globale Rollen ab).

**Parameter (Auszug):**

| Route | Parameter |
|---|---|
| `timeseries` | `metric` (Pflicht), `from`, `to`, `step` (`1m`/`5m`/`15m`/`1h`/`1d`; ohne Angabe höchstens 200 Punkte, 24 h → `15m`, F141), `groupBy` (`model_group`, `deployment`, `provider`, `team`, `key`, `org`, `error_class`), `limit` (Top-N 1–20, Standard 5) |
| `summary` | `from`, `to` |
| `top` | `dimension` (Pflicht, auch `error_class`), `metric` (Standard `requests`), `from`, `to`, `limit` (1–100, Standard 10) |
| alle drei | Filter `modelGroup`, `deployment`, `provider`, `team`, `key`, `organization` (kommagetrennt, Name oder ID), `errorClass` (F47, z. B. `timeout,rate_limit`) |
| `request-logs` | `from`, `to`, `status` (`429`, `4xx`, `error`, `success`), `errorCode`, `errorClass` (F47), `modelGroup`, `deployment`, `provider`, `team`, `key`, `organization`, `requestId`, `clientRequestId`, `parentRequestId` (F191: Folgeanfragen, z. B. Embeddings des semantischen Caches), `traceId` (F100), `stream`, `cache` (`hit`, `miss`, `none`, `hit-exact`, `hit-semantic`, `bypass`, `cache-embedding`), `limit` (1–500, Standard 50), `cursor` |

- **Tagesschritte** (`1d`, F143) beginnen um Mitternacht in `billing.timezone` (Sommerzeit:
  23/25-Stunden-Tage); die Antwort nennt die Zone in `timezone`. Andere Schritte sind an der
  Epoche ausgerichtet.
- **KPIs** (`summary`): Quoten (`errorRate`, `cacheHitRate`) tragen zusätzlich
  `deltaPoints` – die Änderung in Prozentpunkten (F144), `deltaPercent` bleibt relativ.
- Einträge des Log-Explorers tragen `errorClass`, `traceId`, `parentRequestId` und
  `guardrails` (Zähler je Plugin/Kategorie/Aktion, F161).
- `timeseries`, `summary` und `top` melden `rollups: false`, wenn `observe.rollups` aus ist
  (F33) – die Daten fehlen dann, die UI weist darauf hin.

Zeitpunkte RFC 3339; ohne `from`/`to` die letzten 24 h (Log-Explorer: ohne Zeitgrenze).
Kosten erscheinen als Zahl (`v`, gerundet, für Diagramme) **und** exakt als Dezimaltext
(`amount`). Der Cursor des Log-Explorers ist opak (Base64url von Zeitpunkt und Request-ID);
sortiert wird absteigend nach (`at`, `id`), die Paginierung läuft in SQL.

```json
GET /admin/api/v1/analytics/timeseries?metric=cost&groupBy=model_group&from=2026-11-06T00:00:00Z&to=2026-11-13T00:00:00Z
→ {"metric": "cost", "unit": "currency", "step": "1h", "stepSeconds": 3600, "timezone": "UTC",
   "groupBy": "model_group", "from": "…", "to": "…",
   "series": [{"key": "gpt-4o", "name": "gpt-4o", "other": false, "total": 812.4, "totalAmount": "812.412345678",
               "points": [{"t": "2026-11-06T00:00:00Z", "v": 3.2, "amount": "3.2"}, …]},
              …, {"key": "__other__", "other": true, …}]}
```

### 5.12 Hinweise zu Limits (F29)

`GET /admin/api/v1/usage/limit-warnings` → `{"counterBackend": "postgres-sync", "warnings":
[{"kind": "team", "name": "platform", "field": "limits[1]", "code":
"second-limit-postgres-sync", "message": "…"}]}` – Limits, die mit dem gewählten
Zähler-Backend nur ungenau wirken: Sekundenlimits mit `counters.backend: postgres-sync` (die
Instanzen gleichen nur im Abstand `counters.sync_interval_ms` ab). Nur Objekte mit Leserecht;
mit `local`/`redis` leer. Dieselbe Prüfung loggt das Gateway beim Start als Warnung. Grundlage
für den UI-Hinweis am Limit-Editor.

## 6. Datenebene: Schlüsselprüfung und Modellfreigaben

`aigw_proxy::SnapshotAuthenticator` (Architektur §3, Schritte 2–3):

1. Key aus `Authorization: Bearer`, `api-key` oder `x-api-key`; SHA-256; Nachschlagen im
   Key-Index des Snapshots (HashMap, O(1)). Der Snapshot wird **einmal pro Anfrage** geladen und
   im `AuthContext` mitgeführt.
2. Nur aktive Keys stehen im Index; der Ablauf (`expiresAt`) wird pro Anfrage geprüft.
3. Fehlender, unbekannter, gesperrter und abgelaufener Key ergeben **dieselbe** Antwort `401`
   (`code: invalid_api_key`) im Eingangsformat (OpenAI bzw. Anthropic `authentication_error`);
   der Grund steht nur im Debug-Log.
4. Ergebnis `AuthContext` mit `KeyCtx` (Key, Team, Organisation) – Accessoren `api_key_id()`,
   `team_id()`, `organization_id()`, `user()` für Limits und Request-Log.
5. Modellfreigabe (`AuthContext::authorize_model`): Modell (Name oder Alias) → Modellgruppe;
   erlaubt, wenn Key- **und** Team-Liste passen (leere Liste = alle). Nicht freigegeben →
   `403 model_not_allowed`. Unbekanntes Modell → `404 model_not_found` – für alle Keys, auch
   eingeschränkte (F39: präzise Fehler gehen vor dem Verbergen des Katalogs).
6. `GET /v1/models` listet nur die freigegebenen Modellgruppen (kanonische Namen, sortiert).

Modellgruppen, Deployments, Zugangsdaten und Preise sind über die Admin-API pflegbar (§2.2 ff.)
und stehen nach dem Hot Reload im Snapshot: `model_groups` (Name und Aliasse → Gruppe mit
aktiven Deployments nach Priorität und entschlüsselten Zugangsdaten), `prices`
(`Snapshot::price_at`). Die Datenebene routet daraus (AIGW-24); für Tests gibt es die
Anbieterart `mock` (`providerModel` `mock`, `mock-error`, `mock-stream-error`).

## 7. Offene Punkte

- Namensraum global vs. je Organisation und Pflicht-Team für Keys (offen aus AIGW-9). Die API
  adressiert über Namen und kapselt Verweise in `spec`; ein Wechsel zu Pfaden wie
  `/organizations/{org}/teams/{name}` ließe sich als zusätzliche Routen über dieselben generischen
  Abläufe (`AdminResource`) abbilden.
- Paginierung in SQL statt im Speicher, sobald Mengen es erfordern.
- Persistentes Bootstrap-Token (Hash in der DB) statt Erzeugung je Prozessstart.
- Preisversionen mit `validFrom` in der Vergangenheit dürfen angelegt werden (Import
  historischer Preise); bereits berechnete Kosten im Request-Log ändern sich dadurch nicht.
- AIGW-74 (Vorschläge, offen): Einladungen auch über die UI anlegen (AIGW-77); Einladungen
  vorab je Rolle/Ablauf per GitOps (derzeit nur API); Höchstlaufzeit auch rückwirkend auf
  bestehende Admin-Keys anwenden; ETag-Abgleich für Zugangsdaten zusätzlich über `keyId`.
- AIGW-66 (Vorschläge, noch nicht entschieden): Aufbewahrungsfrist bzw. Archivierung des
  Audit-Logs; Manipulationsschutz (Hash-Kette über die Einträge); Löschen eines Admin-API-Keys
  zusätzlich als `revoke` statt `delete` benennen; einheitliche Schreibweise der Arten
  (`ApiKey` vs. `admin-api-key`); Push-Export (Syslog/Webhook) statt Abholen per NDJSON;
  Audit-Lesezugriff selbst protokollieren.
