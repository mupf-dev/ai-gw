# Anmeldung über OIDC (Admin-UI und Admin-API)

Die Admin-UI und die Admin-API (Session-Cookie) melden Nutzer per OpenID Connect
(Authorization Code + PKCE) an. Maschinen nutzen Admin-API-Keys (`sk-aigw-admin-…`).
Konzept und alle Prüfungen: [08-admin-api.md](../konzept/08-admin-api.md) §5.

## 1. Grundkonfiguration

```yaml
admin:
  auth:
    bootstrap: "auto"                 # Bootstrap-Token erlischt nach dem ersten OIDC-Admin
    session: {max_age_secs: 28800, idle_timeout_secs: 1800}
    oidc:
      issuer: "https://idp.example.org/realms/acme"
      client_id: aigw
      # Secret nie in der YAML: Datei oder AIGW_OIDC_CLIENT_SECRET (ohne beides: öffentlicher Client)
      client_secret_file: /run/secrets/aigw-oidc-secret
      redirect_url: "https://aigw-admin.example.org/admin/auth/callback"
      scopes: [openid, profile, email]
      groups_claim: groups
      group_roles:
        - {group: aigw-admins, role: admin}
        - {group: aigw-viewer, role: viewer}
        - {group: acme-platform-leads, role: team-admin, organization: acme, team: platform}
      provision_users: true
```

- **Redirect-URI** beim IdP: `https://<steuerebene>/admin/auth/callback`. Die UI muss über
  HTTPS erreichbar sein (Cookies sind `Secure`).
- **Abmelden beim IdP** (`end_session: true`): als Post-Logout-URL
  `post_logout_redirect_url` (z. B. `https://<steuerebene>/admin/`) beim IdP erlauben.
- **Rollen:** `admin`, `org-admin`, `team-admin`, `viewer`, `auditor`, `gitops`
  (admin-gleichwertig!). `organization`/`team` grenzen eine Rolle ein. Gruppenrollen gelten bis
  zum Ende der Session.
- **Nutzer:** mit `provision_users: true` beim ersten Login angelegt, wenn eine Gruppenrolle
  passt. Alternativ vorab anlegen (Issuer + Kennung) oder per Einladungslink
  (`POST /admin/api/v1/users/{name}/invitation`).
- **Ausgehende Verbindungen zum IdP** laufen über denselben Client wie alle Upstreams: Die
  Wurzelzertifikate kommen aus **`upstream.roots`** (`webpki` eingebettet, `system` oder
  `both`) bzw. `upstream.ca_file`; für einen IdP mit interner CA zusätzlich
  `admin.auth.oidc.ca_file`. Der Issuer gilt als vom Betreiber konfiguriert und
  vertrauenswürdig (keine SSRF-Freigabe nötig, auch im privaten Netz). Discovery und JWKS
  werden erst beim ersten Login geladen und `cache_secs` lang gecacht.
- Nach dem ersten erfolgreichen Admin-Login prüfen: `GET /admin/api/v1/auth/info` meldet
  `bootstrap: false`.

## 2. Microsoft Entra ID

1. App-Registrierung anlegen, Plattform **Web**, Redirect-URI wie oben; ein Client-Secret
   erzeugen (oder öffentlichen Client mit PKCE verwenden).
2. Token-Konfiguration: **Gruppenanspruch** hinzufügen (Sicherheitsgruppen, Format
   „Gruppen-ID“). In `group_roles` stehen dann die **Objekt-IDs** der Gruppen. Bei mehr als
   200 Gruppen pro Nutzer liefert Entra keinen `groups`-Claim (Überschreitung) – dann
   App-Rollen verwenden (Claim `roles`, `groups_claim: roles`).
3. Konfiguration:

```yaml
admin:
  auth:
    oidc:
      issuer: "https://login.microsoftonline.com/<tenant-id>/v2.0"
      client_id: "<application-id>"
      client_secret_env: AIGW_OIDC_CLIENT_SECRET
      redirect_url: "https://aigw-admin.example.org/admin/auth/callback"
      profile: entra                  # Kennung = oid, tid muss tenant_id entsprechen
      tenant_id: "<tenant-id>"
      groups_claim: groups            # bzw. roles bei App-Rollen
      group_roles:
        - {group: "6f1c…-…", role: admin}
```

Mit `profile: entra` ist die Kennung `oid` (über alle Anwendungen des Tenants stabil; `sub` ist
bei Entra je Anwendung verschieden) und Tokens fremder Tenants werden abgelehnt.

## 3. Keycloak

1. Realm → Clients → **Create client** (OpenID Connect, Client-ID `aigw`), „Client
   authentication“ an (vertraulich) oder aus (öffentlich, PKCE), Standard-Flow an.
   Valid Redirect URIs: `https://aigw-admin.example.org/admin/auth/callback`,
   Valid post logout redirect URIs: `https://aigw-admin.example.org/admin/`.
2. Gruppen in das ID-Token bringen: Client scopes → `aigw-dedicated` → Mapper „Group
   Membership“ (Token Claim Name `groups`, „Full group path“ aus) – oder Realm-Rollen
   verwenden (`groups_claim: realm_access.roles`).
3. Konfiguration:

```yaml
admin:
  auth:
    oidc:
      issuer: "https://keycloak.example.org/realms/acme"
      client_id: aigw
      client_secret_file: /run/secrets/aigw-oidc-secret
      redirect_url: "https://aigw-admin.example.org/admin/auth/callback"
      groups_claim: groups            # bzw. realm_access.roles
      group_roles:
        - {group: aigw-admins, role: admin}
```

## 4. Fehlersuche

| Symptom | Ursache |
|---|---|
| Start bricht ab „admin.auth: …“ | Secret-Datei nicht lesbar, `tenant_id` fehlt bei `profile: entra`, ungültige Werte |
| Login-Seite meldet Fehler, Log „Discovery …“ | Issuer nicht erreichbar oder `issuer` passt nicht exakt zum Discovery-Dokument (Schrägstrich am Ende, `/v2.0` bei Entra) |
| Rücksprung mit `auth_error=token` | Signatur/`aud`/`iss`/Uhrzeit (`clock_skew_secs`) oder falscher Tenant |
| Login ok, aber keine Rechte | Gruppen-Claim fehlt im ID-Token oder Gruppenname/-ID passt nicht zu `group_roles` |
| `auth_error=invite` | Einladung abgelaufen, schon verwendet oder Identität gehört schon einem anderen Nutzer |
| TLS-Fehler zum IdP | interne CA: `upstream.roots: system`/`both`, `upstream.ca_file` oder `oidc.ca_file` |
