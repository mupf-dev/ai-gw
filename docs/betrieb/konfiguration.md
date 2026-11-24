# Konfiguration

ai-gw trennt zwei Arten von Konfiguration:

| | Startkonfiguration | Ressourcen |
|---|---|---|
| Inhalt | Listener, Datenbank, Zähler-Backend, Master-Key, Logging, Observability, Cache-Speicher, OIDC, SSRF-Freigaben | Organisationen, Teams, Nutzer, API-Keys, Zugangsdaten, Modellgruppen, Deployments, Preise, Plugin-Bindungen, MCP-Server |
| Format | eine YAML-Datei + Umgebungsvariablen | YAML-Manifeste `apiVersion: aigw/v1alpha1` (GitOps) bzw. Admin-API/-UI |
| Ablage | Datei, gelesen beim Start | Datenbank |
| Änderung wirkt | nach Neustart | sofort (Hot Reload über den Konfigurations-Snapshot) |

## 1. Startkonfiguration

Pfad über `aigw serve --config <datei>` oder `AIGW_CONFIG`. Alle Felder sind optional; das
vollständig kommentierte Beispiel mit allen Standardwerten ist
[config/aigw.example.yaml](../../config/aigw.example.yaml). Unbekannte Felder sind ein Fehler
(Tippfehler fallen beim Start auf).

### Umgebungsvariablen

Jeder Wert lässt sich überschreiben: `AIGW_<ABSCHNITT>__<FELD>` (doppelter Unterstrich als
Trenner, auch für tiefere Ebenen; Groß-/Kleinschreibung egal).

```sh
AIGW_STORE__URL=postgres://aigw:geheim@db:5432/aigw
AIGW_STORE__MAX_CONNECTIONS=20
AIGW_COUNTERS__BACKEND=redis
AIGW_OBSERVE__OTLP__ENDPOINT=http://otel-collector:4318
AIGW_ADMIN__ALLOW_PRIVATE_UPSTREAMS=ollama,10.20.0.0/16     # Listen kommagetrennt
AIGW_SECRETS__MASTER_KEY_FILE=/run/secrets/aigw-master-key
```

- Zahlen und Wahrheitswerte (`true/false/1/0/yes/no/on/off`) werden passend zum Feld
  umgewandelt, Listen kommagetrennt.
- Unbekannte Abschnitte/Felder in `AIGW_…__…`-Variablen sind ein Startfehler.
- Variablen **ohne** `__` sind keine Overrides: `AIGW_CONFIG`, `AIGW_LOG` (Log-Filter wie
  `RUST_LOG`), `AIGW_LOG_FORMAT` (`text|json`), `AIGW_MASTER_KEY`, `AIGW_BOOTSTRAP_TOKEN`,
  `AIGW_OIDC_CLIENT_SECRET`, `AIGW_SECRET_*` (GitOps-Verweise).

### Prüfen

```sh
aigw config check -c aigw.yaml          # YAML + Umgebung, unbekannte Felder, Typen, SSRF-Regeln
aigw config validate resources.yaml     # Manifeste offline
aigw config validate resources.yaml -c aigw.yaml   # zusätzlich Verweise/Geheimnisse gegen die DB
```

`config check` öffnet weder Datenbank noch Master-Key und eignet sich für CI und als
Vorabprüfung vor einem Rollout.

### Beispiele

Unter [config/examples/](../../config/examples/) liegt je Thema eine Startkonfiguration
(`aigw.yaml`) und, wo sinnvoll, ein Manifest (`resources.yaml`):

| Verzeichnis | Inhalt |
|---|---|
| `minimal/` | SQLite, ein OpenAI-Zugang, eine Modellgruppe |
| `providers/` | alle Anbieter: OpenAI, Azure OpenAI, Anthropic, AWS Bedrock, Gemini API, Vertex AI, Mistral, Ollama, vLLM; Fallbacks, Gewichte, Prioritäten, Preise |
| `teams-budgets/` | Organisation, Teams, API-Key mit Modellfreigaben, Limits, Budgets (strikt/weich), Budget-Webhook |
| `guardrails/` | PII (global, gesperrt), Prompt-Filter mit unsichtbaren Zeichen und Ausgabe-Regeln (Team), Prompt-Leck/Canary (Team), Azure Content Safety (Organisation) |
| `cache/` | exakter + semantischer Cache mit Redis-Ebene und Embedding-Modellgruppe |
| `mcp/` | MCP-Gateway mit Server und Werkzeugfreigaben je Team |
| `ha-postgres-redis/` | mehrere Instanzen: Postgres, Redis-Zähler, Snapshot-Cache, OTLP |
| `gitops.yaml` | gemischtes GitOps-Beispiel aus [09-gitops.md](../konzept/09-gitops.md) |

Alle Beispiele werden im Test `config_examples_are_valid` (`crates/aigw/src/cli_config.rs`,
Teil von `scripts/check.sh`) geprüft: Startkonfiguration wie `aigw config check`, Manifeste
per Apply in eine frische Datenbank mit den SSRF-Regeln der zugehörigen Startkonfiguration und
anschließendem Snapshot-Aufbau. Wer ein Feld umbenennt, merkt es dort.

### Wichtige Standardwerte und Hinweise

- **Strikte Budgets reservieren vorab:** Bei `budget.enforcement: strict` (Standard) und
  Token-Limits wird vor dem Upstream-Aufruf der mögliche Höchstbetrag reserviert. Fehlt
  `max_tokens` in der Anfrage, gelten **`limits.default_max_tokens: 4096`** Ausgabe-Tokens.
  Bei kleinen Budgets oder Limits führt das zu frühen Ablehnungen – Clients sollten
  `max_tokens` setzen, oder den Wert anpassen ([11-limits-und-budgets.md](../konzept/11-limits-und-budgets.md)).
- **Allokator:** Das Binary nutzt standardmäßig **mimalloc** (weniger CPU je Anfrage, keine
  Arena-Sperren unter Last, [leistung.md](leistung.md) §3.1). Ohne:
  `cargo build --release -p aigw --no-default-features` bzw. im Image
  `--build-arg CARGO_FEATURES=--no-default-features`.
- **Tokenizer-Tabellen (AIGW-95, AIGW-92):** Die Rangtabellen `o200k_base`/`cl100k_base`
  von OpenAI sind **nicht** enthalten (Lizenz ungeklärt). ai-gw liest sie aus
  `tokenizer.ranks_dir` (`cl100k_base.tiktoken`, `o200k_base.tiktoken`, SHA-256 geprüft; im
  Image `/data/tiktoken`). Bereitstellen mit `aigw tokenizer fetch` (Init-Container,
  `docker run --rm -v aigw-data:/data <image> tokenizer fetch`) oder per Opt-in
  `tokenizer.download_ranks: true` beim Start (über `aigw-outbound`, SSRF-Regeln wie
  Anbieter). Ohne Tabellen gilt die Heuristik (Warnung beim Start). Details:
  [11-limits-und-budgets.md](../konzept/11-limits-und-budgets.md) §8a.
- **Budget je Deployment (AIGW-126):** `Deployment.spec.budget` (`amount`, `period`
  `total|month|week|day`, `warnAtPercent`, `switchAtPercent` 1–100, Standard 100,
  `enforcement` `strict|soft`). Beträge in `billing.currency` wie die Preisliste – Anthropic
  rechnet in USD ab. Erschöpfte Deployments überspringt das Routing ohne Fehler; sind alle
  erschöpft, kommt `429 deployment_budget_exhausted`. Nachladen: Betrag erhöhen (Hot Reload).
  Strategie `routing: budget-balanced` verteilt die Kosten gleichmäßig. Rezept mit vier
  Anthropic-Accounts: [rezept-accounts-rotieren.md](rezept-accounts-rotieren.md).
- **Request-Body:** `server.data.max_body_bytes` (32 MiB), Audio `max_audio_bytes` (25 MiB).
- **Mock-Anbieter:** `admin.allow_mock: true` ist Standard (Demos, Smoke-Tests). Für
  Produktion `false`, ebenso `admin.ui.mock: false`.

### Preisvorschläge (Azure Retail Prices, Preiskatalog)

Die Preise-Seite zeigt je Deployment einen Preisvorschlag neben dem aktuellen Preis
(Details [11-limits-und-budgets.md](../konzept/11-limits-und-budgets.md) §4a). Abschnitt
`billing.price_suggestions`:

- Ausgehend nur **`prices.azure.com`** (HTTPS, öffentlich, ohne Anmeldung), und nur wenn jemand
  die Vorschläge abruft bzw. `check_interval_secs > 0` ist. In abgeschotteten Netzen
  `azure.enabled: false` setzen – dann gilt nur der mitgelieferte Katalog.
- Region je Azure-Deployment in `settings.pricing.region` (aus `*.openai.azure.com` nicht
  ablesbar) oder einmal global `azure.default_region`; Bereitstellungsart
  `settings.pricing.deploymentType` (`global`, `data-zone`, `regional`). Weicht der Deployment-
  Name vom Modell ab: `settings.pricing.model: gpt-5.4-mini`.
- Katalog per URL aktualisieren (Standard aus): `catalog.url` + `catalog.format: litellm` für
  die LiteLLM-Datei. Der Host der URL ist – wie `azure.api_url` – von den SSRF-Regeln
  ausgenommen, weil er aus der Startkonfiguration stammt.
- Übernahme nur ausdrücklich (UI „Übernehmen“ bzw. `POST …/price-suggestions:accept`); manuelle
  Preise werden nur nach Bestätigung ersetzt.

```yaml
billing:
  price_suggestions:
    azure:
      default_region: swedencentral
      default_deployment_type: data-zone
    check_interval_secs: 86400   # tägliche Warnung bei Preisänderungen
```

### Kostenanalyse und Chargeback (AIGW-117)

Die Seite „Kosten“ und `GET /admin/api/v1/costs/…` lesen die Rollups (`observe.rollups` muss an
sein; Konzept [07-beobachtung.md](../konzept/07-beobachtung.md) §14). Abschnitt `billing`:

- `currency` (Standard `USD`): Abrechnungswährung als ISO-4217-Code. Nur Beschriftung in
  Kennzahlen und Export – das Gateway rechnet nicht um; Preise (und Budgets) müssen in dieser
  Währung gepflegt sein. Preisvorschläge und Katalog liefern USD.
- `cost_center_key` (Standard `costCenter`): Schlüssel in `spec.metadata` von API-Key, Team
  und Organisation, der die Kostenstelle trägt. Der Chargeback-Export nimmt den Wert vom Key,
  sonst vom Team, sonst von der Organisation; Labels (`metadata.labels`) und weitere Metadaten
  der Objekte erscheinen als Spalten `label.<name>` bzw. `meta.<name>`.

```yaml
billing:
  currency: EUR
  cost_center_key: kostenstelle
---
kind: Team
metadata: {name: platform, labels: {bereich: it}}
spec: {organization: acme, metadata: {kostenstelle: "4711"}}
```

Export per API (CSV, je Monat, nach Team; gestreamt):

```bash
curl -H "Authorization: Bearer $AIGW_ADMIN_TOKEN" -o kosten.csv \
  "https://aigw.example/admin/api/v1/costs/export?dimension=team&granularity=month&from=2026-10-01T00:00:00Z&to=2026-11-01T00:00:00Z"
```

### Azure OpenAI mit Entra ID

Statt eines API-Keys meldet sich das Gateway per Entra ID an – empfohlen über Zugangsdaten
mit `spec.entra` (eine Stelle für alle Deployments, AIGW-84), alternativ wie bisher mit
`settings.auth: entraId` direkt am Deployment (Details in
[06-streaming-und-anbieter.md](../konzept/06-streaming-und-anbieter.md)):

```yaml
kind: ProviderCredential
metadata: {name: foundry-mi}
spec:
  provider: azure-openai          # bzw. anthropic (Claude in Foundry)
  entra:
    credential: managedIdentity   # workloadIdentity | clientSecret | clientCertificate
    clientId: 11111111-2222-3333-4444-555555555555   # benutzerzugewiesene Identität
  # data: {clientSecret: {valueFrom: {env: AZURE_CLIENT_SECRET}}}   # nur Service Principal
---
kind: Deployment
spec: {provider: azure-openai, credential: foundry-mi, …}   # keine entra*-Einstellungen nötig
```

Die Tabelle nennt die Einstellungen der Inline-Variante; mit Zugangsdaten stehen dieselben
Angaben in `spec.entra` (`credential`, `tenantId`, `clientId`, `authority`, `scope`,
`sendX5c`). Die
Admin-UI zeigt unter **Anbieter** je Endpunkt Anmeldeart, Deployments und Zustand – auch für
Inline-Deployments ohne Zugangsdaten.

| Variante | Deployment-Einstellungen | Zugangsdaten / Umgebung |
|---|---|---|
| Service Principal mit Secret | `auth: entraId` | `tenantId`, `clientId`, `clientSecret` |
| Service Principal mit Zertifikat | `auth: entraId`, optional `entraSendX5c: true` (Subject Name + Issuer) | `tenantId`, `clientId`, `clientCertificate` (PEM mit Zertifikat(skette) und RSA-Schlüssel, unverschlüsselt oder `ENCRYPTED PRIVATE KEY`), dazu ggf. `clientCertificatePassword` |
| Workload Identity (AKS) | `auth: entraId`, `entraCredential: workloadIdentity` | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_FEDERATED_TOKEN_FILE`, optional `AZURE_AUTHORITY_HOST` (setzt der AKS-Webhook) |
| Managed Identity (VM, AKS-Knoten, App Service, Container Apps, Azure Arc) | `auth: entraId`, `entraCredential: managedIdentity`, optional `entraClientId` (nicht bei Arc) | keine; App Service: `IDENTITY_ENDPOINT`/`IDENTITY_HEADER`; Arc: `IDENTITY_ENDPOINT`/`IMDS_ENDPOINT` (setzt der Arc-Agent) |

- **Workload/Managed Identity freigeben (AIGW-90):** beide Varianten nutzen die Identität des
  Gateway-Prozesses und sind nur mit diesem Schalter erlaubt (Standard `false`):

  ```yaml
  upstream:
    entra:
      ambient_identity: true   # bzw. AIGW_UPSTREAM__ENTRA__AMBIENT_IDENTITY=true
  ```

  Ohne Schalter sind solche Deployments „nicht nutzbar“ (Log: „Deployment nicht nutzbar –
  ausgelassen“ mit dem Schlüssel in der Meldung), beim Start warnt das Log mit allen
  betroffenen Zugangsdaten/Deployments. Service Principals brauchen den Schalter nicht.
- **Verschlüsselter Zertifikatsschlüssel (AIGW-94):** PEM mit `ENCRYPTED PRIVATE KEY`
  (PKCS#8, PBES2/PBKDF2/AES-CBC, z. B. `openssl pkcs8 -topk8 -v2 aes-256-cbc -in key.pem`),
  Passwort im Feld `clientCertificatePassword`. Per Datei in GitOps-Manifesten:

  ```yaml
  kind: ProviderCredential
  metadata: {name: aoai-cert}
  spec:
    provider: azure-openai
    entra: {credential: clientCertificate, tenantId: contoso.onmicrosoft.com,
            clientId: 11111111-2222-3333-4444-555555555555, sendX5c: true}
    data:
      clientCertificate: {valueFrom: {file: /run/secrets/aigw/aoai-cert.pem}}
      clientCertificatePassword: {valueFrom: {file: /run/secrets/aigw/aoai-cert.pass}}
  ```

  (`valueFrom.file` nur unter `gitops.secret_dir`.) Altes OpenSSL-Format
  (`Proc-Type: 4,ENCRYPTED`) und PBES1/3DES werden mit Hinweis abgelehnt – nach PKCS#8
  umwandeln. `sendX5c: true` schickt die Zertifikatskette (`x5c`) mit; nötig, wenn die
  App-Registrierung Zertifikate über Subject Name + Issuer statt Fingerabdruck akzeptiert.
- **Azure Arc (Server außerhalb Azure, AIGW-94):** Der Arc-Agent setzt `IDENTITY_ENDPOINT`
  und `IMDS_ENDPOINT`; ai-gw beantwortet die Challenge mit der Schlüsseldatei aus
  `/var/opt/azcmagent/tokens` (nur dort, nur `.key`, ≤ 4 KiB). Der Dienstbenutzer von ai-gw
  braucht Leserechte (Gruppe `himds`), dazu `upstream.entra.ambient_identity: true`. Nur
  systemzugewiesene Identität (`entraClientId` weglassen).
- Sovereign Clouds: `entraAuthority` (z. B. `https://login.microsoftonline.us`) und
  `entraScope` (z. B. `https://cognitiveservices.azure.us/.default`).
- Die Identität braucht auf der Azure-OpenAI-Ressource die Rolle **Cognitive Services OpenAI
  User**; Workload/Managed Identity erhalten nur Cognitive-Services-Scopes.
- Den Metadatendienst (`169.254.169.254`) muss man **nicht** in `admin.allow_private_upstreams`
  freischalten – Managed Identity nimmt genau diesen Host gezielt aus.
- Fehler beim Token-Abruf: 502 `upstream_authentication_error`; die Ursache (z. B.
  `AADSTS7000215` = falsches Secret) steht als Warnung im Log.
- Beispiel: [`config/examples/providers/resources.yaml`](../../config/examples/providers/resources.yaml).
- **Claude in Azure AI Foundry** nutzt dieselben Einstellungen mit `provider: anthropic` und
  `endpoint: https://<ressource>.services.ai.azure.com/anthropic` (Rolle „Cognitive Services
  User“), Beispiel mit GPT und Claude über eine benutzerzugewiesene Managed Identity:
  [`config/examples/providers/resources-azure-foundry.yaml`](../../config/examples/providers/resources-azure-foundry.yaml).
- **Azure Container Apps:** Die App setzt `IDENTITY_ENDPOINT` (`http://localhost:<port>/msi/token`)
  und `IDENTITY_HEADER`; ai-gw nutzt dann das App-Service-Protokoll (api-version 2019-08-01,
  Header `X-IDENTITY-HEADER`, bei benutzerzugewiesener Identität `client_id` =
  `entraClientId`). `localhost` wird dafür gezielt von den SSRF-Regeln ausgenommen – nichts in
  `admin.allow_private_upstreams` eintragen.

### Workload- und Agent-Identitäten (JWT)

AIGW-115, Konzept [18-workload-identitaeten.md](../konzept/18-workload-identitaeten.md):
Datenebene und MCP-Gateway nehmen neben API-Keys Access-Tokens vertrauenswürdiger Aussteller
an (z. B. **Microsoft Entra Agent ID**). Ohne `auth.jwt.issuers` ist das aus.

```yaml
auth:
  jwt:
    issuers:
      - name: entra-prod                 # Name für workloadIdentities[].issuer am Key
        profile: entra                   # entra | generic
        tenant_id: "<tenant-guid>"       # entra: Pflicht (GUID, nicht common/organizations)
        audiences: ["api://ai-gw", "<client-id>"]   # Pflicht: v1-Tokens = App-ID-URI, v2 = Client-ID
        # authority: https://login.microsoftonline.com   # souveräne Clouds: anpassen (+ issuers)
        # issuers: [...]                 # entra: Standard sts.windows.net/<tid>/ und <authority>/<tid>/v2.0
        # jwks_uri: ...                  # entra: Standard <authority>/<tid>/discovery/v2.0/keys
        # algorithms: [RS256, ES256]     # Teilmenge; none/HS* nie
        # identity_claims: [azp, appid]  # Kennung für die Zuordnung (generic: [sub])
        # user_claim: oid                # nur Attribution delegierter Tokens
        required_roles: [Gateway.Invoke] # optional, App-only (roles)
        # required_scopes: [access_as_user]   # optional, OBO/Agent-User (scp)
        # require_agent: true            # nur Agent-Identitäten (xms_act_fct enthält 11)
      - name: keycloak
        profile: generic
        issuers: ["https://sso.example.org/realms/agents"]   # JWKS per Discovery
        audiences: ["ai-gw"]
    clock_skew_secs: 60          # höchstens 300
    jwks_cache_secs: 3600
    jwks_refresh_min_secs: 30    # Nachladen bei unbekannter kid höchstens so oft
    http_timeout_secs: 10
    max_jwks_bytes: 262144
    max_token_bytes: 16384
    token_cache_entries: 10000   # geprüfte Tokens bis exp (0 = aus)
    # ca_file: /etc/aigw/idp-ca.pem
```

- Zuordnung am Key: `spec.workloadIdentities: [{issuer: entra-prod, subject: <App-ID der
  Agent-Identität>, description: …}]` (UI: Key bearbeiten → „Workload-/Agent-Identitäten“).
  Eine Identität gehört zu höchstens einem Key (`409`); Rotation nimmt sie zum neuen Key mit.
- Antworten: ungültiges/abgelaufenes Token `401 invalid_api_key`; gültiges Token, aber
  Identität keinem aktiven Key zugeordnet bzw. Rolle/Scope/Agent fehlt `403
  identity_not_allowed`. Gründe stehen nur im `debug`-Log (ohne Token).
- Das Token geht nie an Upstreams; im Request-Log steht die Identität (`workload`), nie das
  Token. Widerruf wirkt sofort nur über den Key (Identität entfernen, sperren); das Token
  selbst gilt beim Aussteller bis `exp`.
- Startprüfung: falsche Werte (fehlende `audiences`, `tenant_id` keine GUID, unerlaubtes
  Verfahren, doppelte Namen/Aussteller) brechen den Start ab. ENV-Overrides wie üblich,
  z. B. `AIGW_AUTH__JWT__CLOCK_SKEW_SECS=30`.

### Postgres mit TLS (z. B. Azure Database for PostgreSQL)

`store.url` nimmt die libpq-Parameter `sslmode` und `sslrootcert` an (TLS über rustls):

| `sslmode` | Verhalten |
|---|---|
| `disable` | ohne TLS (Azure Flexible Server lehnt das mit `require_secure_transport=on` ab) |
| `prefer` (Standard) | TLS, wenn der Server es anbietet, Zertifikat ungeprüft |
| `require` | nur TLS, Zertifikat **ungeprüft** (Schutz nur gegen passives Mitlesen) |
| `verify-ca` | TLS, Kette geprüft |
| `verify-full` | TLS, Kette **und** Hostname geprüft – empfohlen |

- Geprüft wird gegen die **eingebetteten** Wurzeln (webpki-roots, Mozilla-Liste), nicht gegen
  den Zertifikatsspeicher des Images – im distroless-Image funktioniert `verify-full` daher
  ohne weitere Dateien. Die Wurzeln der aktuellen Kette von Azure Database for PostgreSQL
  (DigiCert Global Root G2, Microsoft RSA/ECC Root Certificate Authority 2017) sind enthalten
  (Test `azure_postgres_roots_are_embedded`). Die alte **DigiCert Global Root CA** (G1) hat
  Mozilla entfernt: Läuft ein Server noch mit dieser Kette, deren PEM als `sslrootcert`
  einhängen (oder auf `sslmode=require` ausweichen).
- Eigene CA (z. B. selbst betriebener Postgres): `sslrootcert=/pfad/ca.pem` – ergänzt die
  eingebetteten Wurzeln.
- Beispiel Azure:
  `AIGW_STORE__URL=postgres://aigw:<passwort>@meinserver.postgres.database.azure.com:5432/aigw?sslmode=verify-full`
  (Flexible Server: Benutzer ohne `@server`-Suffix; Passwort URL-kodiert, z. B. aus einem
  Container-Apps-Secret per Umgebungsvariable). Entra-ID-Anmeldung an Postgres wird noch nicht
  unterstützt.

### Verbindungsgrenzen

Je Listener eine Höchstzahl gleichzeitiger Verbindungen, dazu Lese-Timeouts und
HTTP/2-Grenzen für beide Listener (S-32, AIGW-80; `0` = aus bzw. unbegrenzt):

```yaml
server:
  data:
    max_connections: 10000   # darüber: sofort 503 + Schließen
  control:
    max_connections: 1024
  http:
    header_read_timeout_secs: 10    # HTTP/1: erstes Byte bis vollständiger Kopf
    body_read_timeout_secs: 30      # höchste Pause im Anfragekörper
    body_min_bytes_per_sec: 1024    # Mindestrate nach body_read_timeout_secs Vorlauf
    idle_timeout_secs: 120          # Keep-Alive-Leerlauf (> Leerlauf-Timeout des LB)
    http2_max_concurrent_streams: 256
    http2_keepalive_interval_secs: 30
    http2_keepalive_timeout_secs: 20
```

ENV z. B. `AIGW_SERVER__DATA__MAX_CONNECTIONS=20000`,
`AIGW_SERVER__HTTP__IDLE_TIMEOUT_SECS=300`. Laufende Antworten (SSE-Streams) brechen die
Lese-Timeouts nicht ab. `idle_timeout_secs` größer wählen als das Leerlauf-Timeout eines
vorgeschalteten Load Balancers. Limits **je Client-IP** und TLS bleiben Aufgabe des Reverse
Proxys. Wirkung, Metriken (`aigw_server_*`) und Hinweise: [haertung.md](haertung.md).

### Prompt-Filter und Prompt-Leck (AIGW-109/111/112)

Plugin-Bindungen (Admin-API, GitOps, UI „Guardrails & Plugins“) – Details:
[13-guardrails.md](../konzept/13-guardrails.md) §4 und §4a.

```yaml
plugin: prompt-filter
config:
  direction: request            # Wort-/Regex-Regeln: request (Standard) | response | both
  words: {terms: ["Projekt Falke"], direction: both}   # je Regel überschreibbar
  regex: [{name: ticket, pattern: "INC-\\d{6}", action: flag, direction: response}]
  invisible:                    # unsichtbare Unicode-Zeichen (fehlt = aus)
    action: strip               # strip (Standard) | reject | flag
    allowJoiners: true          # ZWJ/ZWNJ zwischen sichtbaren Nicht-ASCII-Zeichen erlauben
  streamWindowBytes: 256        # Ausgabe-Regeln im Stream: zurückgehaltene Bytes (16–4096)
---
plugin: prompt-leak             # eigene Plugin-Art, empfohlen order 90 (innerste Ausgabeschicht)
config:
  action: block                 # block (200 content_filter/refusal, F160) | flag (zählen, Token entfernen)
  canary: {rotation: request}   # request (Standard, je Anfrage zufällig) | stable (Prompt-Caching)
  verbatim: {ngram: 8, minNgrams: 12, ratio: 0.5, action: flag}   # optional, Standard aus
```

- Injection-Heuristik und Grenzen gelten nur für die Eingabe; Ausgabe-Ablehnungen folgen
  `outputRejectMode` der Bindung (Standard `finish`).
- `prompt-leak` wirkt nur auf Anfragen mit System-/Developer-Prompt. Das Canary-Token steht
  nie in Antworten, Fehlermeldungen oder dem Payload-Log; der Antwort-Cache rechnet den
  Schlüssel ohne Token. `rotation: request` macht Prompt-Caching der Anbieter ab dem
  System-Prompt wirkungslos – deshalb nutzt das Profil „Streng“ `stable`.
- Metrik `aigw_guardrail_hits_total{plugin="prompt-filter",category="invisible_tags|invisible_zero_width|invisible_bidi|invisible_format|invisible_variation",action="strip|reject|flag"}`,
  `{plugin="prompt-leak",category="canary|verbatim",action="block|flag"}`.

### WASM-Plugins

Plugins der Art `wasm` (AIGW-63, [Konzept](../konzept/17-wasm-plugins.md)) laufen in einer
Sandbox mit Grenzen je Aufruf. Module lädt ein globaler `admin` über die Admin-API hoch
(`PUT /admin/api/v1/wasm-modules/{name}?sha256=…`, UI: Guardrails & Plugins → WASM-Module) oder
sie kommen aus Dateien der Startkonfiguration (Weg für den Modus „nur YAML“):

```yaml
wasm:
  default_fuel: 100000000     # CPU-Grenze je Aufruf; Bindung: fuel (≤ max_fuel)
  default_memory_mb: 32       # Bindung: maxMemoryMb (≤ max_memory_mb)
  pool_instances: 256         # gleichzeitige Instanzen (Pooling-Allocator)
  modules:
    - name: keyword-guard
      path: /etc/aigw/plugins/keyword-guard.wasm
      sha256: 9c1e…           # Abweichung bricht den Start ab
      key_id: release-2026    # Signatur (AIGW-101): aigw wasm sign …
      signature: 3q2+7w…
  trusted_keys:               # öffentliche Ed25519-Schlüssel (PEM/Base64); leer = keine Prüfung
    - id: release-2026
      public_key: MCowBQYDK2VwAyEA…
  require_signature: null     # Standard: Pflicht, sobald trusted_keys/sigstore gesetzt; false = Übergang
  sigstore:                   # keyless (AIGW-106): Trust-Root offline, erlaubte Identitäten
    trusted_root: /etc/aigw/sigstore/trusted_root.json
    identities:
      - issuer: 'https://token\.actions\.githubusercontent\.com'
        subject: 'https://github\.com/mupf-dev/plugins/\.github/workflows/release\.yml@refs/tags/v.*'
    require_inclusion_proof: true
  profile: standard           # standard | strict („Streng“: Signatur Pflicht, Vier-Augen an)
  four_eyes: null             # Vier-Augen-Freigabe; null = nach profile (strict: an)
```

Signaturen (AIGW-101): Mit `trusted_keys` müssen Module beim Upload, beim Import aus
`wasm.modules` und vor dem Laden eine gültige Ed25519-Signatur tragen (Format, Werkzeuge
`aigw wasm keygen|sign` und Schlüsselwechsel: [Konzept §3a](../konzept/17-wasm-plugins.md)).

Sigstore (AIGW-106): Mit `sigstore` genügt statt eines eigenen Schlüssels ein Bundle aus
`cosign sign-blob --bundle` (Fulcio-Zertifikat + Rekor-Eintrag), das zu einer der `identities`
passt (`issuer`/`subject` als reguläre Ausdrücke, vollständig verankert). Das Trust-Root wird
nur beim Start gelesen, zur Laufzeit gibt es keine Netzaufrufe. Module aus Dateien:
`wasm.modules[].sigstore_bundle: <Pfad>`. Prüfung offline: `aigw wasm verify-sigstore`. Details,
Grenzen (Rekor v1, kein DSSE): [Konzept §3b](../konzept/17-wasm-plugins.md).

Vier-Augen-Freigabe (AIGW-106): Mit `four_eyes` (bzw. `profile: strict`) werden Uploads über
die Admin-API nur eingereicht und erst aktiv, wenn ein **anderer** globaler `admin` sie freigibt
(`POST /admin/api/v1/wasm-modules/{name}/approve?sha256=…`, UI: Button „Freigeben“). Module aus
`wasm.modules` sind ohne Freigabe aktiv. Ablauf, RBAC, Audit und GitOps:
[Konzept §3c](../konzept/17-wasm-plugins.md). `profile: strict` verlangt `trusted_keys` oder
`sigstore` (sonst Startabbruch) und verbietet `require_signature: false`.

`wasm.enabled: false` (bzw. ein Build ohne Feature `wasm`) lässt Bindungen der Art `wasm`
prüfbar, Aufrufe schlagen aber fehl (→ `failMode`). Kann das System den Adressraum des
Pooling-Allocators nicht reservieren, fällt die Laufzeit mit Warnung auf Instanzen bei Bedarf
zurück (`wasm.pooling: false` erzwingt das).

### Structured Output prüfen (`schema-guard`)

Die Plugin-Art `schema-guard` (AIGW-113, [Konzept §13](../konzept/13-guardrails.md)) prüft
Modellantworten gegen das JSON-Schema der Anfrage (OpenAI `response_format` `json_schema`/
`json_object`, Anthropic `output_config.format`, erzwungene Tools). Es ist eine Bindung wie
jede andere (Admin-API `plugin-bindings`, GitOps, UI „Guardrails & Plugins“); die
Konfiguration wird beim Schreiben geprüft (`422` bzw. `aigw config validate`):

```yaml
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: structured-output}
spec:
  plugin: schema-guard
  scope: {level: model-group, target: gpt-4o}
  outputRejectMode: error    # 502 schema_validation_failed statt 200 + content_filter
  config:
    action: retry            # flag | reject (Standard) | retry
    maxRetries: 1            # 1–3, nur mit retry
    tools: forced            # off | forced (Standard) | all
    stream: live             # live (Standard: Prüfung am Ende) | buffer (puffern, auch retry)
    invalidSchema: reject    # ungeeignetes Schema der Anfrage: reject (400) | skip
    limits: {maxSchemaBytes: 65536, maxSchemaDepth: 32, maxOutputBytes: 1048576}
```

Hinweise: Wiederholungen (`retry`) sind eigene Modellaufrufe derselben Modellgruppe und werden
wie Plugin-Modellaufrufe abgerechnet (Kosten, Budgets, Token-Limits; `plugin_usage` im
Request-Log), die `usage` der Antwort bleibt die des ersten Aufrufs. `stream: buffer` puffert
**alle** gestreamten Anfragen der betroffenen Kette – Bindung eng fassen. Externe `$ref`
werden nie aufgelöst; Schemas mit externen Verweisen, Rückverweisen in `pattern` oder über den
Grenzen lehnt das Gateway mit `400 response_schema_rejected` ab. Standard-Zeitlimit der
Bindung 60 s (`timeoutMs`), Fehlermodus `closed`.

## 2. Master-Key

Zugangsdaten der Anbieter und `Secret`-Ressourcen liegen AES-256-GCM-verschlüsselt in der
Datenbank. Der Master-Key (32 Byte, hex oder Base64) liegt **nie** in der Datenbank.

```sh
aigw keygen                               # auf stdout (hex)
aigw keygen --format base64
aigw keygen --out /etc/aigw/master-key    # neue Datei, Rechte 0600
```

Quelle beim Start: `secrets.master_key_file` (Vorrang) oder die Variable aus
`secrets.master_key_env` (Standard `AIGW_MASTER_KEY`). Ohne Master-Key startet `aigw serve`
nicht. Er wird auch für den Snapshot-Cache (`snapshot.cache_file`) und den persistierten
semantischen Cache verwendet.

**Aufbewahrung:** getrennt von Datenbank-Backups (wer beides hat, hat alle Zugangsdaten), aber
selbst gesichert – ohne Master-Key sind die gespeicherten Zugangsdaten verloren und müssen neu
eingespielt werden (GitOps: erneutes `apply`).

### Rotation

1. Neuen Key erzeugen: `aigw keygen --out /etc/aigw/master-key.new`.
2. Auf **allen** Instanzen: neuer Key als `secrets.master_key_file`, alter Key in
   `secrets.previous_key_files` (nur zum Entschlüsseln); rollierend neu starten.
3. Einmalig umschlüsseln: `aigw config rekey -c aigw.yaml` – verschlüsselt alle Zugangsdaten
   und Secrets mit fremder Key-ID neu (je Datensatz ein Audit-Eintrag, Akteur `rekey`).
   Laufende Instanzen übernehmen das über den normalen Reload.
4. Erneut ausführen, bis `Neu verschlüsselt: 0` gemeldet wird; dann den alten Key aus
   `previous_key_files` entfernen, rollierend neu starten und den alten Key vernichten.

Der Snapshot-Cache wird mit dem aktuellen Key neu geschrieben; ein alter Cache, der sich nicht
mehr entschlüsseln lässt, wird beim Kaltstart nicht verwendet.

## 3. Ressourcen und GitOps

Ressourcen werden über die Admin-UI/-API oder als YAML gepflegt
([09-gitops.md](../konzept/09-gitops.md)):

```sh
aigw config export -c aigw.yaml --out bestand.yaml
aigw config diff   -c aigw.yaml bestand.yaml --exit-code     # Drift-Erkennung in CI
aigw config apply  -c aigw.yaml bestand.yaml [--prune] [--dry-run]
```

- Geheimnisse stehen nur als Verweis (`valueFrom: {env: AIGW_SECRET_…}` oder `{file: …}`) im
  Manifest. Über die Admin-API (`POST /admin/api/v1/config:apply`) sind Dateien nur aus
  `gitops.secret_dir` erlaubt.
- **Höchstens 1 024 YAML-Dokumente je Datei** (Grenze des YAML-Parsers `serde-saphyr`,
  Fehlermeldung „budget breached: Documents“). Große Bestände auf mehrere Dateien verteilen –
  sinnvoll nach Art oder Team, z. B. `10-organisationen.yaml`, `20-modelle.yaml`,
  `30-keys-team-a.yaml` – und nacheinander anwenden (abhängige Arten zuerst). `--prune` ist
  bei aufgeteilten Beständen **nicht** verwendbar: jede Datei gilt als vollständiger
  Soll-Zustand, `--prune` würde die Ressourcen der anderen Dateien löschen. Gelöschtes dann
  gezielt über die Admin-API/-UI entfernen.
- `gitops.read_only_admin: true` sperrt die Schreibrouten der Admin-API (Modus „nur YAML“).
- API-Keys werden ohne `--api-keys include` weder exportiert noch angewendet (F4).

### Guardrails: Link-Schutz und Sprachen

Plugin-Bindungen sind Ressourcen (Admin-API/-UI oder GitOps); Admin-API und `aigw config
validate|apply` prüfen die Konfiguration (unbekannte Felder, Domains, Sprachcodes → `422`).
Beispiel in `config/examples/guardrails/resources.yaml`, Details in
[13-guardrails.md](../konzept/13-guardrails.md) §11/§12.

```yaml
# AIGW-110: Datenabfluss über Links/Bilder in Antworten verhindern
plugin: link-guard
config:
  allowDomains: [example.com, "*.example.com"]  # "*." = nur Subdomains
  images: block            # Standard: Bilder zu fremden Domains entfernen
  links: allow             # block = auch Links zu fremden Domains
  blockQueryParams: false  # true = fremde URLs mit ?… oder user@host blockieren
  maxPathLength: 100       # optional
  action: strip            # strip | defang | reject | flag
  direction: output        # output | input (Tool-Ergebnisse) | both
---
# AIGW-114: erlaubte/gesperrte Sprachen (ISO 639-1)
plugin: language
config:
  allow: [de, en]          # und/oder deny: [..]
  action: reject           # reject (400 language_not_allowed) | flag
  direction: input         # input | output | both
  roles: [user]
  minChars: 20
  minConfidence: 0.5
```

Das Profil „Streng“ enthält ab Version 2 eine gesperrte `link-guard`-Bindung (Bilder zu fremden
Domains entfernen); `language` ist in keinem Profil.

