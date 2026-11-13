# ai-gw – Admin-API: Mandanten, Nutzer, Keys, Modelle, Preise, Anmeldung (AIGW-28, AIGW-65, AIGW-32)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §3, §5, §6, §8, §13, §15 · [02-datenmodell.md](02-datenmodell.md)

## 1. Überblick

Die Admin-API läuft auf der Steuerebene (`server.control.listen`, Standard `:4001`) unter
`/admin/api/v1`. Sie ist vollständig in der OpenAPI-Spezifikation beschrieben
(`GET /admin/api/openapi.json`, eingecheckt als `ui/openapi.json`, daraus
`ui/src/lib/api/schema.d.ts`). Code: `crates/aigw-admin` (`resources.rs` mit den generischen
Abläufen, `deployments.rs`, `credentials.rs`, `prices.rs`, `auth.rs`,
`problem.rs`; Anmeldung und Rollen siehe §5).

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
| `/admin-api-keys`, `/admin-api-keys/{name}` | GET, POST · GET, PUT, DELETE | Admin-API-Keys (§5.5) |
| `/session`, `/auth/info` | GET | angemeldeter Principal bzw. Anmeldeverfahren (§5) |
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
| 401 | `unauthorized` | keine/ungültige/abgelaufene Anmeldung (`WWW-Authenticate: Bearer realm="aigw-admin"`) |
| 403 | `forbidden` | Rolle oder Geltungsbereich reicht nicht; CSRF-Prüfung fehlgeschlagen |
| 404 | `not-found` | Ressource unbekannt |
| 409 | `conflict` | Name vergeben, Ressource noch verwendet, Key gesperrt (Rotation) |
| 412 | `precondition-failed` | `If-Match` passt nicht |
| 415 | `unsupported-media-type` | kein `Content-Type: application/json` |
| 422 | `validation-failed` | ungültiger Name, unbekannte Felder, unbekannter Verweis oder Modell |
| 500 | `internal` | interner Fehler – ohne Details nach außen, Ursache im Log |

`instance` (Pfad) wird derzeit nur bei `401` gesetzt.

## 5. Authentifizierung, Sessions, Rollen (AIGW-32)

Code: `crates/aigw-admin` – `auth.rs` (Principal, Middleware, CSRF, Bootstrap), `oidc.rs`
(Relying Party), `session.rs` (Login, Session, Abmeldung), `rbac.rs` (Rollen), `admin_keys.rs`
(Admin-API-Keys); Speicher `aigw-store/src/admin_auth.rs`, Migration `0003_admin_auth.sql`;
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

### 5.2 OIDC-Login (Authorization Code + PKCE)

| Schritt | Pfad | Inhalt |
|---|---|---|
| 1 | `GET /admin/auth/login?returnTo=/admin/keys` | erzeugt `state`, `nonce`, PKCE-Verifier (je 256 Bit); speichert sie (Tabelle `admin_login_state`, Schlüssel SHA-256 von `state`, 10 min); setzt Cookie `__Secure-aigw-login=<state>` (`HttpOnly; Secure; SameSite=Lax; Path=/admin/auth`); `303` zum IdP mit `code_challenge_method=S256` |
| 2 | `GET /admin/auth/callback?code&state` | `state` muss zum Login-Cookie passen (Schutz vor Login-CSRF) und wird beim Lesen gelöscht (einmalig); Code-Tausch mit Verifier; ID-Token-Prüfung; Nutzerzuordnung; Session anlegen; `303` nach `returnTo` (nur Pfade unter `/admin/`, kein offener Redirect) |
| – | Fehler | `303` nach `/admin/?auth_error=<code>` mit `idp`, `state`, `token`, `no-access`, `internal`; Grund nur im Server-Log |

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
`sub` nicht leer. Gruppen aus `groups_claim` (Liste oder einzelner Wert; Punkte für verschachtelte
Claims, z. B. `realm_access.roles` bei Keycloak). Das Access-Token wird nicht verwendet.

**Client-Authentifizierung:** mit Secret `client_secret_basic`, sonst öffentlicher Client
(`client_id` im Formular, nur PKCE). Das Secret kommt aus `client_secret_file` oder der
Umgebungsvariable `client_secret_env` (Standard `AIGW_OIDC_CLIENT_SECRET`), nie aus der YAML.

**Nutzerzuordnung** (Ressource `User`, §2):
1. Nutzer mit gleichem `oidcIssuer` + `oidcSubject`;
2. sonst (wenn `link_by_email`) ein vorab angelegter Nutzer **ohne** `oidcSubject`, dessen
   E-Mail (ohne Groß-/Kleinschreibung) der vom IdP **bestätigten** E-Mail (`email_verified`)
   entspricht – genau ein Treffer; Issuer/Subjekt werden eingetragen (Audit-Akteur
   `oidc-login`);
3. sonst (wenn `provision_users` und mindestens eine Gruppenrolle) Neuanlage mit Name aus
   `preferred_username` bzw. dem lokalen Teil der E-Mail (bei Kollision mit Hash-Suffix), ohne
   direkte Rollen;
4. sonst bzw. wenn keine wirksame Rolle übrig bleibt: `auth_error=no-access`, keine Session.

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
  Rollen aus den gespeicherten Gruppen (aktuelle Konfiguration). Eine Rollenänderung in der DB
  wirkt damit sofort; Gruppenänderungen beim IdP erst beim nächsten Login.
- **CSRF** (zusätzlich zu `SameSite=Strict`), für `POST/PUT/PATCH/DELETE` mit Session-Cookie:
  1. Header `X-CSRF-Token` muss passen (Synchronizer-Token). Das Token ist
     SHA-256(`"aigw-csrf\0" + Session-ID`) – an die Session gebunden, ohne eigene Spalte und
     aus dem gespeicherten Hash nicht ableitbar. Die UI liest es aus
     `GET /admin/api/v1/auth/info` bzw. `GET /admin/api/v1/session` (`csrfToken`).
  2. `Sec-Fetch-Site: cross-site|same-site` → `403`.
  3. Ein `Origin`-Header muss dem Ursprung von `oidc.redirect_url` entsprechen (ohne OIDC: dem
     `Host`).
  Bearer-Anfragen (Admin-Key, Bootstrap) brauchen kein CSRF-Token.
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
| `Global` (GitOps) | `config:export` = Lesen global (`admin`, `viewer`, `auditor`), `config:apply` = Schreiben global (nur `admin`) |
| `Catalog` | Modellgruppen, Deployments, Anbieter-Zugangsdaten (nur Metadaten, Werte sind nie lesbar), Preise (lesen: jede wirksame Rolle; schreiben: globaler `admin`) |
| `Organization(o)` | die Organisation selbst (lesen, ändern, löschen) |
| `Team(o, t)` | das Team selbst |
| `InTeam(o, t)` | API-Keys des Teams (anlegen, ändern, sperren, rotieren, löschen) |

| Rolle | Lesen | Schreiben |
|---|---|---|
| `admin` (global) | alles | alles |
| `admin` (Org/Team) | im Bereich | im Bereich |
| `org-admin` (Org O) | O, Teams und Keys in O, Katalog | Teams und Keys in O, nicht O selbst |
| `team-admin` (Team T) | T, Keys in T, Katalog; O nur, wenn die Zuweisung `organization` nennt | Keys in T |
| `viewer` | im Bereich (global: alles), Katalog | – |
| `auditor` | wie `viewer` (Audit-Log-Endpunkt folgt) | – |

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
- Rollen: mindestens eine, wirksam, Verweise müssen existieren (`422`). `expiresAt` optional,
  beim Anlegen/Ändern in der Zukunft; abgelaufene Keys ergeben `401`.
- `PUT` ersetzt `description`, `roles`, `expiresAt`; `DELETE` widerruft sofort (keine
  Snapshot-Verzögerung, da je Anfrage aus der DB gelesen).
- `lastUsedAt` (höchstens minütlich aktualisiert). Audit: `create`/`update`/`delete` mit Art
  `admin-api-key` (ohne Hash).

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
    oidc:
      issuer: "https://idp.example.org/realms/acme"     # leer = OIDC aus
      client_id: aigw
      client_secret_file: /run/secrets/aigw-oidc-secret  # oder Env AIGW_OIDC_CLIENT_SECRET
      redirect_url: "https://gateway.example.org/admin/auth/callback"
      scopes: [openid, profile, email]
      groups_claim: groups
      group_roles:
        - {group: aigw-admins, role: admin}
        - {group: acme-platform-leads, role: team-admin, organization: acme, team: platform}
      provision_users: true
      link_by_email: true
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
- AIGW-32 (Vorschläge, noch nicht entschieden): Höchstlaufzeit bzw. Pflichtablauf für
  Admin-API-Keys; Verwaltung von Admin-Keys auch durch `org-admin` (nur im eigenen Bereich);
  `team-admin`-Grenzen für Key-Limits/-Budgets („im Rahmen der Team-Grenzen“) inhaltlich prüfen;
  Audit-Log-Endpunkt für `auditor`; Abmelden aller Sessions eines Nutzers bei Rollenentzug;
  IdP-Gruppen bei jedem Request statt nur beim Login (Refresh-Token/Introspection).
