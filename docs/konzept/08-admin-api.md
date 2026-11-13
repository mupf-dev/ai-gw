# ai-gw – Admin-API: Mandanten, Nutzer, Keys, Modelle und Preise (AIGW-28, AIGW-65)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §3, §5, §6, §8, §13, §15 · [02-datenmodell.md](02-datenmodell.md)

## 1. Überblick

Die Admin-API läuft auf der Steuerebene (`server.control.listen`, Standard `:4001`) unter
`/admin/api/v1`. Sie ist vollständig in der OpenAPI-Spezifikation beschrieben
(`GET /admin/api/openapi.json`, eingecheckt als `ui/openapi.json`, daraus
`ui/src/lib/api/schema.d.ts`). Code: `crates/aigw-admin` (`resources.rs` mit den generischen
Abläufen, `deployments.rs`, `credentials.rs`, `prices.rs`, `auth.rs`,
`problem.rs`).

Jede Änderung läuft als eine Datenbank-Transaktion mit `config_version + 1` und Audit-Eintrag
(Akteur = angemeldeter Principal). Der Reloader baut danach den Snapshot neu (§14), d. h. ein
neuer, gesperrter oder geänderter Key wirkt auf der Datenebene **ohne Neustart** – in einer
Instanz nach ca. 100 ms (Debounce), bei Postgres auf allen Instanzen per `NOTIFY`.

## 2. Ressourcen

| Pfad | Methoden | Inhalt |
|---|---|---|
| `/organizations`, `/organizations/{name}` | GET, POST · GET, PUT, DELETE | Organisationen |
| `/teams`, `/teams/{name}` | GET, POST · GET, PUT, DELETE | Teams (`organization`, `allowedModels`, Limits, Budget) |
| `/users`, `/users/{name}` | GET, POST · GET, PUT, DELETE | Nutzer (OIDC-Issuer/-Subjekt, E-Mail, Rollen) |
| `/api-keys`, `/api-keys/{name}` | GET, POST · GET, PUT, DELETE | virtuelle Keys der Datenebene |
| `/api-keys/{name}/revoke` | POST | sperren (`state: revoked`) |
| `/api-keys/{name}/reactivate` | POST | entsperren (`state: active`) |
| `/api-keys/{name}/rotate` | POST | neuer Key, alter läuft ab |
| `/model-groups`, `/model-groups/{name}` | GET, POST · GET, PUT, DELETE | Modellgruppen (Aliasse, Routing-Strategie, Fallbacks, Limits) |
| `/deployments`, `/deployments/{name}` | GET, POST · GET, PUT, DELETE | Deployments (Anbieter, Endpunkt, Anbieter-Modell, Zugangsdaten, Gewicht, Priorität, Kapazität, aktiv) |
| `/provider-credentials`, `/provider-credentials/{name}` | GET, POST · GET, PUT, PATCH, DELETE | Anbieter-Zugangsdaten (Werte nur schreibbar) |
| `/prices`, `/prices/{name}` | GET, POST · GET, PUT, DELETE | Preisversionen je Anbieter-Modell |
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
- **Sperren/Entsperren**: `POST …/revoke` bzw. `…/reactivate`; idempotent (gleicher Zustand
  erzeugt keine neue Konfigurationsversion). Gesperrte Keys fehlen im Key-Index des Snapshots.
- **Rotation**: `POST …/rotate` mit optionalem Körper
  `{"name": "ci-2", "gracePeriodSeconds": 3600}`. Legt in **einer** Transaktion einen neuen Key
  mit derselben Spezifikation und denselben Labels an und setzt beim alten `expiresAt` auf
  jetzt + Übergangszeit (Standard 24 h, `0` = sofort, höchstens 365 Tage; ein früherer Ablauf
  bleibt). Standardname: alter Name mit Zeitstempel `-JJJJMMTThhmmss` (ein vorhandener
  Zeitstempel wird ersetzt). Antwort `201` mit `{"newKey": {key, secret}, "previousKey": {…}}`.
  Gesperrte Keys lassen sich nicht rotieren (`409`). Audit: `create` (neu) und `rotate` (alt).
- **Team ist optional** (Stand AIGW-9). Keys ohne Team haben keine Organisation; ihre Freigaben
  ergeben sich nur aus `spec.allowedModels`.

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
| Zugangsdaten | `credential.provider` = `provider` (`422`); Zugangsdaten mit Deployments lassen sich nicht löschen (`409`) und nicht auf einen anderen Anbieter umstellen (`409`) |
| Endpunkt | Pflicht bei `azure-openai`, `vllm`, `openai-compatible`; `ollama` ohne Endpunkt nutzt `http://localhost:11434/v1` und unterliegt ebenfalls den SSRF-Regeln (Freischaltung nötig); siehe §2.6 |
| Azure OpenAI | `providerModel` = Azure-Deployment-Name (Buchstaben, Ziffern, `-_.`); `settings.apiVersion` Pflicht bei `settings.apiStyle` `deployment` (Standard), optional bei `v1` |
| AWS Bedrock | `settings.region` Pflicht |
| Gewicht/Kapazität | `weight` 1–1 000 000 (abschalten über `enabled: false`), `capacity.rpm/tpm` > 0, falls angegeben |
| `providerModel` | 1–256 Zeichen ohne Steuerzeichen |

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
```

ENV: `AIGW_ADMIN__ALLOW_INSECURE_UPSTREAMS=localhost,ollama`. Ein lokaler Ollama per `http`
braucht beide Listen. Ungültige Einträge brechen den Start ab.

**Grenze**: Hostnamen werden beim Schreiben nicht aufgelöst – ein öffentlicher Name, der auf
eine private Adresse zeigt (oder per DNS-Rebinding später zeigt), fällt hier nicht auf.
`UpstreamPolicy::ip_allowed(ip)` ist für den HTTP-Client der Anbieter-Adapter gedacht, der
nach der Namensauflösung prüfen sollte (siehe offene Punkte).

## 3. Konventionen

| Thema | Regel |
|---|---|
| Adressierung | über den Namen (je Art global eindeutig); IDs sind nur Information |
| Paginierung | `?limit=` (1–500, Standard 50) und `?cursor=` (Wert aus `nextCursor` der vorigen Seite); Sortierung nach Name; `nextCursor` fehlt auf der letzten Seite |
| Filter (alle) | `?q=` Teilstring im Namen (Keys: auch Präfix, Modellgruppen: auch Aliasse), `?label=k=v,k2` (alle müssen passen, ohne `=` genügt das Vorhandensein) |
| Filter (je Art) | Teams `organization`; Nutzer `email` (ohne Groß-/Kleinschreibung); Keys `team`, `user`, `organization`, `state`; Deployments `modelGroup`, `provider`, `credential`, `enabled`; Zugangsdaten `provider`; Preise `model`, `provider`, `effectiveAt` |
| Teiländerung | `PATCH` (JSON Merge Patch) nur bei Zugangsdaten (Werte bleiben erhalten); sonst `PUT` mit vollständiger `spec` |
| Unbekannte Parameter | `400` (Tippfehler fallen auf) |
| Optimistische Sperre | Einzelobjekte tragen einen starken `ETag` (SHA-256 über den Inhalt; bei Keys ohne `lastUsedAt`). `PUT`, `DELETE`, `revoke`, `reactivate`, `rotate` werten `If-Match` aus (`*` oder Liste erlaubt); passt er nicht: `412 precondition-failed`. Ohne `If-Match` wird ohne Prüfung geschrieben. |
| Anlegen | `201` mit `Location` und `ETag` |
| Löschen | `204`; Verweise anderer Ressourcen verhindern das Löschen (`409`), Plugin-Bindungen und MCP-Freigaben des Objekts werden mitgelöscht |
| Zeitpunkte | RFC 3339 (UTC) |

Die Prüfung von `If-Match` und das Schreiben laufen innerhalb einer Instanz unter einer
Sperre. Zwischen mehreren Instanzen (Postgres-HA) bleibt ein kleines Zeitfenster; siehe
offene Punkte.

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
| 401 | `unauthorized` | kein/ungültiges Admin-Token (`WWW-Authenticate: Bearer realm="aigw-admin"`) |
| 403 | `forbidden` | Rolle reicht nicht |
| 404 | `not-found` | Ressource unbekannt |
| 409 | `conflict` | Name vergeben, Ressource noch verwendet, Key gesperrt (Rotation) |
| 412 | `precondition-failed` | `If-Match` passt nicht |
| 415 | `unsupported-media-type` | kein `Content-Type: application/json` |
| 422 | `validation-failed` | ungültiger Name, unbekannte Felder, unbekannter Verweis oder Modell |
| 500 | `internal` | interner Fehler – ohne Details nach außen, Ursache im Log |

`instance` (Pfad) wird derzeit nur bei `401` gesetzt.

## 5. Authentifizierung und Übergang zu OIDC

Die Prüfung ist eine austauschbare Schicht (`aigw_admin::auth`):

- `AdminAuthenticator` (Trait, asynchron) liefert aus den Request-Headern einen `Principal`
  (`subject` für das Audit-Log, `roles`, Verfahren). Die Middleware `require_principal` liegt als
  `route_layer` vor allen Ressourcen-Routen und legt den Principal in die Request-Extensions.
- Handler fordern eine `Permission` an (`Read`/`Write`, `Principal::require`). Vorläufige
  Zuordnung: `admin` darf alles, `viewer`/`auditor` lesen; `org-admin`/`team-admin` brauchen den
  Org-/Team-Bezug und sind bis AIGW-32 ohne Rechte.
- **Bootstrap-Admin** (§15): `Authorization: Bearer <token>`. Quelle `AIGW_BOOTSTRAP_TOKEN`
  (mindestens 32 Zeichen); fehlt die Variable, erzeugt der Server beim Start ein zufälliges
  Token `aigw-admin-<64 hex>` und gibt es **einmal** im Log (WARN) aus. Ein so erzeugtes Token
  gilt nur für diesen Prozess. Gespeichert wird nur SHA-256, verglichen in konstanter Zeit.
  Audit-Akteur: `bootstrap-admin`.
- Ohne Anmeldung erreichbar: `GET /admin/api/v1/version`, `GET /admin/api/openapi.json`, die UI.

**AIGW-32** ergänzt weitere `AdminAuthenticator`-Implementierungen, ohne Handler zu ändern:
OIDC-Session (Cookie `HttpOnly`, `SameSite=Strict`, CSRF-Token; Principal = Nutzer mit Rollen
aus `UserSpec.roles` bzw. IdP-Gruppen) und Admin-API-Keys mit Rollen und Ablauf. Mehrere
Verfahren lassen sich hintereinander schalten (erst Session, dann Bearer). `Permission` wird
dann um den Ressourcenbezug erweitert (Org/Team des Ziels), damit `team-admin` nur im eigenen Team
schreibt. Ob das Bootstrap-Token nach dem ersten OIDC-Login deaktiviert wird, ist offen (§7).

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
   `403 model_not_allowed`. Unbekanntes Modell → `404 model_not_found`, aber **nur** für Keys ohne
   jede Einschränkung; eingeschränkte Keys erhalten auch hier `403`, damit sie nicht erkunden
   können, welche Modelle es gibt.
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
- Getrennte Zustände „gesperrt (umkehrbar)“ und „widerrufen (endgültig)“.
- `If-Match` verpflichtend machen (`428 Precondition Required`) und die Prüfung in die
  Store-Transaktion verlegen (instanzübergreifend dicht).
- Paginierung in SQL statt im Speicher, sobald Mengen es erfordern.
- Persistentes Bootstrap-Token (Hash in der DB) statt Erzeugung je Prozessstart.
- **SSRF zur Verbindungszeit** (AIGW-65): Die Endpunktprüfung löst keine Namen auf. Der
  HTTP-Client der Anbieter-Adapter sollte nach der Auflösung `UpstreamPolicy::ip_allowed`
  anwenden (schützt auch vor DNS-Rebinding).
- Verweise ohne Fremdschlüssel auf Modellgruppen (`allowedModels` von Teams/Keys,
  `LimitSpec.modelGroup`): Löschen oder Alias-Umbenennen lässt sie derzeit stehen (sie greifen
  dann ins Leere bzw. bei Neuanlage desselben Namens wieder).
- Deployments ohne Zugangsdaten-Anbieter-Gleichheit (z. B. OpenAI-Key für einen
  `openai-compatible`-Endpunkt) sind derzeit nicht erlaubt.
- Preisversionen mit `validFrom` in der Vergangenheit dürfen angelegt werden (Import
  historischer Preise); bereits berechnete Kosten im Request-Log ändern sich dadurch nicht.
