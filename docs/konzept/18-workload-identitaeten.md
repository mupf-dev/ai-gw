# Workload- und Agent-Identitäten (AIGW-115)

Neben virtuellen API-Keys nimmt ai-gw auf der Datenebene (`:4000`, OpenAI- und
Anthropic-Eingang, Embeddings, Bilder, Audio, `GET /v1/models`) und im MCP-Gateway (`/mcp`)
**signierte Access-Tokens (JWT)** als `Authorization: Bearer …` an. Hauptfall ist
**Microsoft Entra Agent ID**: KI-Agenten mit eigener Entra-Identität melden sich ohne
verteilten Gateway-Key an; dasselbe Verfahren trägt allgemein für Workload-Identitäten
(Service Principals, Managed Identities, andere OAuth-2.0-Aussteller).

```text
Agent (Entra Agent ID) ──Client Credentials / OBO / Agent-User──► Entra ID: Token aud=api://ai-gw
        │
        └─ Authorization: Bearer eyJ… ─► ai-gw :4000 / /mcp
                                         1. Auth-Middleware: JWKS ggf. nachladen (async, gedrosselt)
                                         2. Prüfung im RAM: alg ∈ {RS256, ES256}, Signatur, iss, aud,
                                            exp/nbf/iat, tid, ggf. Rolle/Scope/Agent-Pflicht
                                         3. (Aussteller, azp|appid) → aktiver API-Key (workloadIdentities)
                                         4. ab hier wie mit Key: Modellfreigabe, Limits, Budgets,
                                            Plugins/Guardrails, Tool-Richtlinien, MCP-Freigaben
                                         Upstream: nur die Zugangsdaten des Deployments – nie das Token
```

## 1. Recherche: was Microsoft dokumentiert (Stand 21.11.2026)

Quellen (learn.microsoft.com, abgerufen 21.11.2026; Seitenstand jeweils in Klammern):

- [Token claims reference for agent IDs](https://learn.microsoft.com/en-us/entra/agent-id/agent-token-claims) (ms.date 2026-11-06)
- [Tokens in Microsoft agent identity platform](https://learn.microsoft.com/en-us/entra/agent-id/agent-tokens) (2026-11-06)
- [Secure a web API … / Validate agent tokens in a downstream API](https://learn.microsoft.com/en-us/entra/agent-id/how-to-validate-agent-tokens-downstream-api) (2026-11-05)
- [Choose an authentication protocol for an agent](https://learn.microsoft.com/en-us/entra/agent-id/agent-oauth-protocols) (2026-11-03)

**Flüsse** (Sicht des Agenten; das Gateway ist immer die *Ressource*, die ein fertiges Access-
Token prüft):

| Fluss laut Doku | `idtyp` | Subjekt (`sub`/`oid`) | Akteur (`azp`/`appid`) | Berechtigung |
|---|---|---|---|---|
| Agent handelt autonom (Client Credentials; bei Multi-Instance-Agenten erst Blueprint-Token-Austausch, dann Client Assertion) | `app` | Service Principal der Agent-Identität | App-ID der Agent-Identität | `roles` |
| Agent handelt für einen angemeldeten Menschen (On-Behalf-Of, interaktiver Agent) | `user` | der Mensch | App-ID der Agent-Identität | `scp` |
| Agent handelt über sein eigenes Nutzerkonto (Agent User, Impersonation) | `user` | Nutzerkonto des Agenten | App-ID der Agent-Identität | `scp` |
| Lokaler Agent mit Auth-Code + PKCE (Single-Instance) | `user` | der Mensch | geteilte Client-ID | `scp` |

**Dokumentierte Claims** (Auszug, nur was die Doku nennt):

| Claim | Bedeutung laut Doku | Verwendung in ai-gw |
|---|---|---|
| `iss`, `aud`, `exp`, `nbf`, `iat`, Signatur | Standard; „Resource servers must validate the signature, issuer, audience, and lifetime before using any claims.“ | immer geprüft |
| `tid` | Tenant der Agent-Identität | muss `tenant_id` entsprechen (Profil `entra`) |
| `azp` (v2) / `appid` (v1) | „Authorized party / actor. The application ID of the agent identity.“ | **Kennung für die Zuordnung** (Standard) |
| `oid` | Objekt-ID des Subjekts (App-only: SP der Agent-Identität; OBO: Nutzer; Agent User: Nutzerkonto des Agenten) | Attribution `user` bei `idtyp: user` |
| `idtyp` | `app` oder `user` | Art des Tokens |
| `xms_act_fct` | Facetten des Akteurs, mehrwertig, Leerzeichen-getrennt; `11` = AgentIdentity | `agent: true`; optional Pflicht (`require_agent`) |
| `xms_sub_fct` | Facetten des Subjekts; `11` = AgentIdentity, `13` = AgentIDUser | `13` ⇒ Art `agent-user` |
| `xms_par_app_azp` | Blueprint (Elternanwendung) des Akteurs; „Log the parent application ID for auditing … not recommended … for authorization decisions“ | nur Protokoll |
| `xms_idrel`, `xms_tnt_fct` | Beziehung Subjekt↔Ressourcen-Tenant, Tenant-Facetten | nicht ausgewertet |
| `scp` / `roles` | delegierte Scopes bzw. App-Rollen | optional `required_scopes` / `required_roles` |

Wichtige Aussagen der Doku, die das Design bestimmen:

- „Don't authorize a request based only on the presence of an Agent ID claim.“ → ai-gw ordnet
  nur **explizit eingetragene** Identitäten einem Key zu (Allowlist), zusätzlich optional
  Rollen-/Scope-Pflicht.
- 401 bei fehlendem/ungültigem Token, 403 bei gültigem Token ohne Berechtigung → so umgesetzt.
- „Don't include tokens or sensitive claim values in logs.“ → Token nie im Log; Fehlerdetails
  nur `debug`.
- Beispiel-Token in der Doku ist ein **v1**-Token (`ver: 1.0`, `iss: https://sts.windows.net/<tid>/`,
  `appid`); die Validierungsseite beschreibt v2 (`azp`, `login.microsoftonline.com/<tid>/v2.0`).
  Welche Version kommt, entscheidet die App-Registrierung des Gateways
  (`requestedAccessTokenVersion`) → beide Aussteller standardmäßig erlaubt, Kennung `azp`
  mit Rückfall `appid`, Zielgruppe als Liste (App-ID-URI **und** Client-ID).

**Unklar / konfigurierbar gemacht:** Wer statt der Agent-App-ID z. B. den Service Principal
(`oid`) zuordnen will, setzt `identity_claims: [oid]`. Facetten werden als Leerzeichen-Liste
gelesen, zusätzlich Arrays toleriert (Doku legt Datentyp nur über das Beispiel fest). `idtyp`
fehlt bei klassischen App-Tokens ohne optionalen Claim → dann gilt „`scp` vorhanden ⇒
delegiert, sonst App-only“.

## 2. Konfiguration (Startkonfiguration `auth.jwt`)

Aussteller sind Betreiber-Konfiguration (nicht per Admin-API änderbar). Details und alle
Felder: [konfiguration.md](../betrieb/konfiguration.md#workload--und-agent-identitäten-jwt).

```yaml
auth:
  jwt:
    issuers:
      - name: entra-prod            # Verweis aus workloadIdentities[].issuer
        profile: entra              # generic | entra
        tenant_id: "<tenant-guid>"  # Pflicht bei entra; Aussteller v1+v2 und JWKS daraus
        audiences: ["api://ai-gw", "<client-id-des-gateways>"]
        required_roles: [Gateway.Invoke]   # optional (App-only), sonst jede Identität des Tenants
        # required_scopes: [access_as_user]  # optional (OBO/Agent User)
        # require_agent: true                # nur Agent-Identitäten (xms_act_fct ∋ 11)
    clock_skew_secs: 60
    jwks_cache_secs: 3600
```

## 3. Zuordnung zu Ressourcen

Entscheidung: **Erweiterung des virtuellen Keys** statt neuer Ressourcenart. Ein Key bekommt
`spec.workloadIdentities: [{issuer, subject, description?}]`; ein Token mit passender
(Aussteller, Kennung) wirkt dann **genau wie dieser Key**. Dadurch gelten Modellfreigaben,
Limits, Budgets, Team/Organisation, Plugin-/Guardrail-Bindungen der Ebene Key,
Tool-Richtlinien, MCP-Freigaben, Payload-Richtlinie und Request-Log-Zuordnung ohne jede
Sonderlogik; Admin-RBAC (wer Keys eines Teams pflegen darf) ebenso.

- Eindeutigkeit: jede Identität höchstens an **einem** nicht widerrufenen Key. Admin-API
  `409`, GitOps prüft innerhalb der Datei beim Lesen und gegen die Datenbank nach allen
  Schritten vor dem Commit (Umhängen zwischen Keys in einem Apply klappt
  reihenfolgeunabhängig). Der Snapshot lässt doppelt zugeordnete Identitäten (z. B. an der
  API vorbei in die DB geschrieben) weg – keine Anmeldung statt Zufallstreffer.
- GUIDs werden ohne Rücksicht auf Groß-/Kleinschreibung verglichen, andere Kennungen exakt.
- Rotation (`/rotate`): Identitäten ziehen zum neuen Key um; der alte Key bleibt nur mit
  Klartext bis zum Ablauf nutzbar.
- Gesperrte (`suspended`) und abgelaufene Keys sperren auch die Token-Anmeldung.
- Unbekannte Identität ⇒ `403 identity_not_allowed`, **kein Auto-Provisioning**.
- Ein Key kann mehrere Identitäten tragen (z. B. alle Instanzen eines Agenten-Blueprints
  einzeln); mehrere Agenten an einem Key teilen sich dessen Limits/Budgets und MCP-Sessions.
- Ohne `auth.jwt`-Eintrag für den Aussteller-Namen wirkt eine Identität nicht (die UI bietet
  nur konfigurierte Aussteller an; die Admin-API prüft nur das Format, damit GitOps-Dateien
  auch ohne laufende Konfiguration validierbar bleiben).

Speicherung: Spalte `api_key.workload_identities` (JSON, Migration 0012, beide Dialekte); im
Snapshot ein abgeleiteter Index (Aussteller, Kennung) → Key-Hash, aufgebaut bei vollem und
inkrementellem Neubau. Audit wie jede Key-Änderung.

## 4. Prüfung (Crate `aigw-jwt`)

- **Format-Unterscheidung:** `Bearer` mit `eyJ…`, genau zwei Punkten und nur
  Base64url-Zeichen ⇒ Token; sonst API-Key (virtuelle Keys `sk-aigw-…` enthalten keine Punkte).
  `api-key`/`x-api-key`-Header bleiben reine Key-Header.
- **Größe:** `max_token_bytes` (16 KiB) vor jeder Auswertung.
- **Verfahren:** Allowlist je Aussteller, höchstens `RS256`, `ES256`; `none`, `HS*` und alles
  andere werden nie angenommen; `crit` im Header ⇒ Ablehnung. Schlüssel aus `jku`/`x5u` werden
  nie geladen – nur das konfigurierte JWKS.
- **Signatur:** aws-lc-rs (keine eigene Kryptografie); Kandidaten nach `kid`, `use`, `alg`.
- **Claims:** `iss` exakt aus der Liste, `aud` (String oder Liste) ∈ `audiences`, `exp`
  Pflicht, `nbf`/`iat` falls vorhanden, Uhrenabweichung `clock_skew_secs`; Entra: `tid`.
- **JWKS:** Abruf über den gemeinsamen Outbound-Baustein (`aigw-outbound`, F63): nur
  `https://` (`http://` nur Loopback), keine Weiterleitungen, Zeitlimit `http_timeout_secs`,
  Größenlimit `max_jwks_bytes`, TLS-Wurzeln aus `upstream.roots`/`ca_file`. Wie der OIDC-IdP
  der Admin-UI stammen die Ziele aus der Startkonfiguration und laufen ohne SSRF-Adressfilter.
  Cache `jwks_cache_secs`; unbekannte `kid` ⇒ Nachladen höchstens alle
  `jwks_refresh_min_secs` (Schlüsselwechsel); ohne erreichbaren Aussteller bleiben alte
  Schlüssel in Gebrauch; Abrufe je Aussteller einzeln (Single-Flight).
- **Ablauf je Anfrage:** Die Auth-Middleware (vor dem Body, AIGW-58) ruft async
  `prepare` (ggf. JWKS laden), danach läuft die Prüfung synchron im RAM; geprüfte Tokens liegen
  bis `exp` in einem begrenzten Cache (Schlüssel SHA-256 des Tokens). Der Handler prüft erneut
  (Cache-Treffer) – keine zweite Signaturprüfung.
- Generische Aussteller: JWKS fest (`jwks_uri`) oder per Discovery
  (`<issuers[0]>/.well-known/openid-configuration`, `issuer` muss passen).
- Der OIDC-Login der Admin-UI (`aigw-admin::oidc`) nutzt dieselben JWS-Bausteine
  (`aigw_jwt::jws`).

## 5. Attribution und Beobachtung

- `RequestRecord.workload` / `request_log.workload` (JSON, Migration 0012): Aussteller,
  Kennung, Art (`app`, `delegated`, `agent-user`, `unknown`), `agent`, `user` (nur bei
  delegierten Tokens: Entra `oid`), `blueprint` (`xms_par_app_azp`), `tenant`. Nie das Token.
  Im Log-Explorer sehen die Nutzerkennung nur `admin`/`auditor` (wie die Client-Adresse, F147).
- Trace: Attribute `aigw.workload.issuer|subject|kind|agent` am Span `auth` (keine
  Nutzerkennung); OTLP-Logs `aigw.workload.issuer|subject|kind`.
- **Keine Metrik-Labels** je Agent: die Zahl der Identitäten ist unbegrenzt (Kardinalität);
  Auswertungen über Request-Log/Analytics je Key.
- Payload-Logging entfernt `Authorization` (und alle sensiblen Header) in jedem Modus.

## 6. Sicherheit und Grenzen

- **Token nie an Upstreams:** Anbieter-Adapter und MCP-Client bauen ausgehende Anfragen aus
  den Zugangsdaten des Deployments/MCP-Servers; eingehende Header werden nicht durchgereicht
  (Test `agent_token_on_mcp_gateway`).
- **Widerruf:** JWTs sind bis `exp` gültig (Entra-Access-Tokens laut Plattform-Doku zufällig
  60–90 Minuten; das Agent-Token-Beispiel der Doku läuft rund 8 Stunden, `exp − iat` = 29 100 s). ai-gw fragt keinen Widerruf beim Aussteller ab. Sofort wirksam sind: Identität am Key
  entfernen, Key sperren/widerrufen (Hot Reload) – der Token-Cache hilft nichts, weil die
  Zuordnung bei jeder Anfrage gegen den aktuellen Snapshot läuft. Deaktivierte Agent-
  Identitäten in Entra wirken erst, wenn ihr letztes Token abläuft.
- **Tenant-weite Tokens:** Ohne `required_roles`/`required_scopes` kann jede Anwendung des
  Tenants ein Token für die Gateway-API holen; zugeordnet werden aber nur eingetragene
  Identitäten. Empfehlung: App-Rolle (`Gateway.Invoke`) definieren, „Zuweisung erforderlich“
  aktivieren und `required_roles` setzen.
- Fehlerantworten ohne Detail (`401 invalid_api_key` bzw. `403 identity_not_allowed`);
  Gründe im `debug`-Log ohne Token.
- Sicherheitsreview: S-39 in [sicherheitsreview.md](../betrieb/sicherheitsreview.md).

## 7. Beispiel Azure (nur Doku – nichts automatisch anlegen)

1. **App-Registrierung für das Gateway** (Ressource), z. B. „ai-gw“:
   - *Expose an API*: Application ID URI `api://ai-gw` (bzw. `api://<client-id>`).
   - *App roles*: `Gateway.Invoke` (Allowed member types: Applications) für autonome Agenten;
     optional Scope `access_as_user` für OBO/Agent-User.
   - Optional *Manifest*: `requestedAccessTokenVersion: 2` (dann `azp`, Aussteller v2).
   - Enterprise Application: „Assignment required?“ = Ja.
2. **Agent-Identität berechtigen:** der Agent-Identität (bzw. dem Blueprint mit vererbbaren
   Berechtigungen) die App-Rolle `Gateway.Invoke` zuweisen und Admin-Consent erteilen.
3. **ai-gw konfigurieren:** `auth.jwt.issuers` wie in §2 mit `tenant_id`,
   `audiences: ["api://ai-gw", "<client-id>"]`, `required_roles: [Gateway.Invoke]`.
4. **Key anlegen** (UI: Key → „Workload-/Agent-Identitäten“) bzw. GitOps:

   ```yaml
   apiVersion: aigw/v1alpha1
   kind: ApiKey
   metadata: {name: support-agent}
   spec:
     organization: acme
     team: support
     allowedModels: [gpt-4o]
     limits: [{metric: tokens, window: day, limit: 2000000}]
     workloadIdentities:
       - issuer: entra-prod
         subject: "<App-ID der Agent-Identität>"
         description: Support-Agent (Blueprint Kundenservice)
   ```

5. **Agent:** Token für `api://ai-gw/.default` holen (MSAL/Microsoft.Identity.Web bzw.
   Entra-ID-Auth-SDK-Sidecar, Client Credentials bzw. OBO) und als
   `Authorization: Bearer <token>` an `https://gateway/v1/chat/completions` oder `/mcp` senden.
