# ai-gw – Admin-API: Mandanten, Nutzer, virtuelle Keys (AIGW-28)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §3, §13, §15 · [02-datenmodell.md](02-datenmodell.md)

## 1. Überblick

Die Admin-API läuft auf der Steuerebene (`server.control.listen`, Standard `:4001`) unter
`/admin/api/v1`. Sie ist vollständig in der OpenAPI-Spezifikation beschrieben
(`GET /admin/api/openapi.json`, eingecheckt als `ui/openapi.json`, daraus
`ui/src/lib/api/schema.d.ts`). Code: `crates/aigw-admin` (`resources.rs`, `auth.rs`,
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
| `/model-groups`, `/model-groups/{name}` | GET | Modellgruppen lesen (Namen/Aliasse für Freigaben) |
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

## 3. Konventionen

| Thema | Regel |
|---|---|
| Adressierung | über den Namen (je Art global eindeutig); IDs sind nur Information |
| Paginierung | `?limit=` (1–500, Standard 50) und `?cursor=` (Wert aus `nextCursor` der vorigen Seite); Sortierung nach Name; `nextCursor` fehlt auf der letzten Seite |
| Filter (alle) | `?q=` Teilstring im Namen (Keys: auch Präfix, Modellgruppen: auch Aliasse), `?label=k=v,k2` (alle müssen passen, ohne `=` genügt das Vorhandensein) |
| Filter (je Art) | Teams `organization`; Nutzer `email` (ohne Groß-/Kleinschreibung); Keys `team`, `user`, `organization`, `state` |
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

Solange keine Modellgruppen angelegt sind, beantwortet die Datenebene Chat-Anfragen daher mit
`404`; Modellgruppen und Deployments werden mit dem Routing (AIGW-15 ff.) über die Admin-API
pflegbar.

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
