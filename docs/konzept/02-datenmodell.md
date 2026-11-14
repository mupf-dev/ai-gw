# ai-gw – Datenmodell, Migrationen und Konfiguration (AIGW-9)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht) · Store-Umbau AIGW-73 (F7, F42, F54, F3, F34)
Grundlage: [00-entscheidungen.md](00-entscheidungen.md) (E4, E7), [01-architektur.md](01-architektur.md) §13–§14

## 1. Überblick

| Schicht | Ort | Inhalt |
|---|---|---|
| Startkonfiguration | `config/*.yaml`, `aigw_core::config` | nur, was vor dem Öffnen der DB nötig ist: Listener, Datenbank, Zähler-Backend, Master-Key, Logging |
| Ressourcen-Modell | `aigw_core::resource` | Rust-Typen = Quelle der Wahrheit für DB, Admin-API (utoipa → OpenAPI → TS) und GitOps-YAML |
| Persistenz | `aigw-store` (`migrations/`, `repo/`) | Schema, CRUD in Transaktionen mit `config_version` + `audit_log`, Verschlüsselung, Key-Erzeugung |
| Snapshot | `aigw_core::snapshot`, `aigw_store::load_snapshot` | unveränderlicher RAM-Stand mit Indizes für den Hot Path |

## 2. Startkonfiguration (YAML, E7)

```yaml
server:
  listen: "127.0.0.1:4000"
store:
  url: "sqlite://aigw.db?mode=rwc"   # oder postgres://benutzer@host/db
  max_connections: 10
  reload_poll_secs: 30               # Absicherungs-Abfrage von config_version (0 = aus), §14
counters:
  backend: local                     # local | postgres-sync | redis
  # redis_url: "redis://127.0.0.1:6379/0"
secrets:
  # master_key_file: /run/secrets/aigw-master-key
  master_key_env: AIGW_MASTER_KEY    # Variable mit dem Key, wenn keine Datei angegeben ist
  previous_key_files: []             # nur zum Entschlüsseln während einer Rotation
log:
  level: info                        # Filter wie RUST_LOG
  format: text                       # text | json
```

- Alle Felder optional (Standardwerte), unbekannte Felder sind ein Fehler (`deny_unknown_fields`).
- **Umgebungsvariablen** überschreiben jeden Wert: `AIGW_<ABSCHNITT>__<FELD>` (doppelter Unterstrich
  als Trenner, tiefere Ebenen ebenso), z. B. `AIGW_STORE__URL`, `AIGW_STORE__MAX_CONNECTIONS=20`,
  `AIGW_SECRETS__PREVIOUS_KEY_FILES=/a,/b` (Listen kommagetrennt). Zahlen und Wahrheitswerte werden
  passend zum Feldtyp umgewandelt; Tippfehler in Abschnitt/Feld führen zum Startabbruch mit Nennung
  der Variable. Variablen ohne `__` (`AIGW_CONFIG`, `AIGW_LOG`, `AIGW_TEST_POSTGRES_URL` …) sind
  keine Overrides.
- Hinweis YAML: Werte mit `:` am Ende (z. B. `sqlite::memory:`) in Anführungszeichen setzen.

### 2.1 Wahl des YAML-Parsers

Kriterien: aktiv gepflegt, kein `unsafe`-C (auch kein nach Rust übersetztes libyaml), serde-kompatibel
inkl. `deny_unknown_fields`, Mehrfachdokumente (`---`) für GitOps, Lizenz AGPL-kompatibel.

| Kandidat | Stand (11/2026) | Bewertung |
|---|---|---|
| `serde_yaml` | archiviert (2024) | ausgeschlossen (bereits in `deny.toml` gebannt) |
| `serde_yml` | RUSTSEC-Hinweise, Pflege unklar | ausgeschlossen |
| `serde_norway`, `serde_yaml_ng` | Forks von serde_yaml | basieren auf `unsafe-libyaml` (c2rust-Übersetzung von libyaml, durchgehend `unsafe`) |
| `yaml_serde` (YAML-Organisation) | gepflegt | basiert auf `libyaml-rs` (ebenfalls übersetztes libyaml), eigenes `unsafe` |
| `saphyr` / `yaml-rust2` | gepflegt, reines Rust | nur Parser/DOM, keine serde-Integration |
| **`serde-saphyr` 2.x** | aktiv gepflegt, MIT/Apache-2.0 | **gewählt**: serde direkt auf dem reinen Rust-Parser `granit-parser` (`#![forbid(unsafe_code)]`), panikfrei, gute Fehlermeldungen mit Zeile/Spalte, `from_multiple`/`to_string_multiple`, eingebaute Budgets gegen YAML-Bomben (Anker-Expansion, Tiefe, Größe) |

Abgeschaltet: Default-Features (`comments`), aktiv nur `deserialize` und `serialize`. Risiko: kleineres
Projekt mit wenigen Maintainern – der Parser ist hinter `Config::from_yaml` bzw.
`Manifest::parse_yaml` gekapselt und damit austauschbar.

## 3. Ressourcen-Modell

Jede Ressource = `metadata` (Name, Labels, `createdAt`/`updatedAt`) + `spec`; gelesen zusätzlich `id`
(UUIDv7, zeitlich sortierbar). Verweise auf andere Ressourcen stehen in `spec` **als Name**, in der
Datenbank als ID-Fremdschlüssel; das Repository löst Namen beim Schreiben auf und liefert beim Lesen
wieder Namen. Namen sind je Art global eindeutig (`a-z0-9-._`, max. 128 Zeichen).

| Art (`kind`) | Spezifikation (Auszug) |
|---|---|
| `Organization` | displayName, limits, budget, metadata |
| `Team` | organization, displayName, allowedModels, limits, budget, metadata |
| `User` | oidcIssuer + oidcSubject, email, displayName, roles (Rolle, optional auf Org/Team) |
| `ApiKey` | organization (Pflicht, F3), team?, user?, expiresAt?, allowedModels, limits, budget, metadata; Status (vom Server): prefix, state (`active`/`revoked`), lastUsedAt |
| `ProviderCredential` | provider, data (Feld → `value` bzw. `valueFrom`); gelesen nur provider, Feldnamen, keyId; `valueFrom`-Verweise werden je Feld mitgespeichert (F54) |
| `ModelGroup` | aliases, routing (`weighted`/`least-latency`/`least-busy`/`cost`), fallbacks (modelGroup + Auslöser), limits, settings |
| `Deployment` | modelGroup, provider, endpoint?, providerModel, credential?, weight, priority, capacity (rpm/tpm), enabled, settings |
| `Price` | provider?, model, validFrom, perMillionTokens (input, output, cachedInput, cacheWrite, reasoning, audio-/imageInput, audioOutput), perRequest?, perImage? |
| `PluginBinding` | scope (level `global`/`organization`/`team`/`api-key`/`model-group` + target), plugin, order, enabled, config (JSON) |
| `McpServer` | url, transport (`streamable-http`/`sse`/`stdio`), credential?, enabled, settings |
| `McpGrant` | server, subject (level `team`/`api-key` + target), tools (leer = alle) |
| `ToolPolicy` | scope (Ebene + Ziel), tools/servers (Glob, `!`), action (`allow`, `block-strip`, `block-reject`, `observe`, `require-approval`), unapproved, locked, enabled ([16-tools.md](16-tools.md)) |

Bausteine: `LimitSpec` (metric `requests`/`tokens`/`input-tokens`/`output-tokens`/`concurrency`,
limit, window `second`/`minute`/`hour`/`day`, optional modelGroup), `Budget` (amount, period
`day`/`week`/`month`/`total`, warnAtPercent, hardLimit).

**Geldbeträge** (`Amount`): Festkomma in Nano-Einheiten (10⁻⁹) der Abrechnungswährung als `i64` –
exakt summierbar, nach außen als Dezimaltext (`"2.50"`, in YAML auch als Zahl erlaubt). Preise gelten
je 1 Mio. Tokens.

**Geheimnisse**: `SecretString` gibt in `Debug`/`Serialize` nur `<redacted>` aus und lehnt
`<redacted>` als Eingabe ab (Schutz vor GET→PUT-Rundreisen). In GitOps-YAML sind nur Verweise erlaubt:
`valueFrom: {env: NAME}` oder `valueFrom: {file: /pfad}`; Klartext (`value`) nur über die Admin-API.

### 3.1 GitOps-YAML (`apiVersion: aigw/v1alpha1`)

```yaml
apiVersion: aigw/v1alpha1
kind: ProviderCredential
metadata:
  name: openai-prod
spec:
  provider: openai
  data:
    apiKey:
      valueFrom:
        env: OPENAI_API_KEY
---
apiVersion: aigw/v1alpha1
kind: ModelGroup
metadata:
  name: gpt-4o
  labels: {tier: premium}
spec:
  aliases: [gpt-4o-latest]
  routing: least-latency
  fallbacks:
    - modelGroup: claude-sonnet
      on: [rate-limit, unavailable]
---
apiVersion: aigw/v1alpha1
kind: Deployment
metadata:
  name: gpt-4o-openai
spec:
  modelGroup: gpt-4o
  provider: openai
  providerModel: gpt-4o-2024-08-06
  credential: openai-prod
  weight: 3
  capacity: {rpm: 500, tpm: 300000}
---
apiVersion: aigw/v1alpha1
kind: Price
metadata:
  name: gpt-4o-2024-08
spec:
  provider: openai
  model: gpt-4o-2024-08-06
  validFrom: 2024-08-06T00:00:00Z
  perMillionTokens: {input: "2.50", cachedInput: "1.25", output: "10"}
---
apiVersion: aigw/v1alpha1
kind: Team
metadata:
  name: platform
spec:
  organization: acme
  allowedModels: [gpt-4o]
  limits:
    - {metric: tokens, limit: 100000, window: minute}
  budget: {amount: "500", period: month, warnAtPercent: [80, 100]}
---
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata:
  name: pii-platform
spec:
  scope: {level: team, target: platform}
  plugin: pii
  order: 10
  config: {mode: mask}
```

`Manifest::parse_yaml` liest Mehrfachdokumente und prüft `apiVersion`, `kind`, unbekannte Felder,
Namen und die Geheimnisregel; `Manifest::to_yaml` schreibt sie (Rundreise getestet). Die Anwendung
(`aigw config apply`, Diff/`--prune`) beschreibt [09-gitops.md](09-gitops.md) (AIGW-33). `ResourceKind::ALL` liefert die
Abhängigkeitsreihenfolge für Import/Export.

## 4. Datenbankschema

Seit AIGW-73 (Entscheidung F7) **zwei Migrationssätze** mit nativen Typen je Datenbank:
`crates/aigw-store/migrations/sqlite/` und `crates/aigw-store/migrations/postgres/`, gleiche
Versionsnummern, gleiche Tabellen und Spalten (`aigw_store::migrator(Backend)`, eingebettet, keine
Compile-Zeit-Makros, kein DB-Zugriff beim Bauen).

| Inhalt | Postgres | SQLite |
|---|---|---|
| IDs | `UUID` (v7) | `TEXT` (Bindestrich-Form) |
| Zeitpunkte | `TIMESTAMPTZ` (auf Millisekunden gekürzt geschrieben) | `INTEGER` Unix-Millisekunden (UTC) |
| Listen/Objekte (Labels, Limits, Budget, Settings, Audit vorher/nachher, Payload …) | `JSONB` | JSON als `TEXT` |
| Wahrheitswerte | `BOOLEAN` | `INTEGER` 0/1 |
| Geld | `BIGINT` Nano-Einheiten | `INTEGER` Nano-Einheiten |
| Chiffrat, Nonce | `BYTEA` | `BLOB` |
| Ressourcennamen | `TEXT COLLATE "C"` (Bytefolge wie in Rust → stabile Keyset-Pagination) | `TEXT` (binär) |
| Rollup-Schlüssel ohne Wert | Null-UUID bzw. `''` (Teil des Primärschlüssels) | `''` |

**Geld als `BIGINT` statt `NUMERIC`:** `Amount` ist i64 in Nano-Einheiten (exakt, ±9,2 Mrd.
Währungseinheiten); Summen in Dashboards laufen über Millionen Rollup-Zeilen, `BIGINT`-Summen sind
dort deutlich schneller als `NUMERIC`. Preise je 1 Mio. Tokens bleiben Dezimaltext in `JSONB`.

**Frischer Schnitt (AIGW-73):** Die früheren portablen Migrationen 0001–0007 (ein Satz für beide
Datenbanken über den `Any`-Treiber) sind in je eine `0001_init.sql` pro Dialekt zusammengefasst und
um F3, F34 und F54 ergänzt. Es gab noch keine Produktivdaten; ältere Datenbanken werden **nicht**
migriert: `Store::migrate` bricht mit `StoreError::IncompatibleSchema` und dem Hinweis ab, die
Datenbank neu anzulegen und die Konfiguration per `aigw config export`/`apply` zu übertragen.
Weitere Migrationen: `0002_snapshot_journal.sql` (AIGW-75, Index `audit_log (config_version)`
für den inkrementellen Snapshot-Neubau, F16). Nächste freie Migrationsnummer: **0003** (in beiden
Sätzen anlegen).

**Zugriff (Enum-Dispatch):** Das interne Modul `aigw_store::db` kapselt `Pool`/`Tx`/`DbRow` als
Enums über `SqlitePool`/`PgPool`. Eine Abfrage ist SQL-Text plus typisierte Werte (`Val`: `Uuid`,
`Ms` (Zeitpunkt), `Json`, `Bool`, `Bytes`, `Dim`, `Int`, `Text`), gebunden erst beim Ausführen an
den jeweiligen Treiber; Zeilen werden mit typisierten Zugriffen gelesen (`uuid`, `ms`, `json`,
`bool` …). Begründung gegenüber einem Trait-Objekt bzw. Generics über `sqlx::Database`: genau zwei
Backends (geschlossene Menge), `Store` bleibt ein konkreter Typ mit unveränderter Schnittstelle (die
Aufrufer in Admin, GitOps, Analytics, Observe, Billing-Ledger, Events, Cache und Audit bleiben
unverändert), keine Trait-Grenzen von sqlx in den Signaturen, kein `dyn`/`async-trait`. Das SQL ist
überwiegend gemeinsam (`$n`-Platzhalter, `ON CONFLICT`, `RETURNING`, `TRUE/FALSE`); Abweichungen
wählt der Code per `Backend` (Zeit-Buckets: Roh-Bucket + Zusammenfassen im Speicher vs. Ganzzahl-Division, Label-Filter `JSONB`
vs. `json_each`, Teilstring `strpos` vs. `instr`, Platzhalter `?` bei SQLite-Massen-INSERTs).
LISTEN/NOTIFY nutzt einen eigenen `PgListener` mit eigener Verbindung; `aigw-counters`
(`postgres-sync`) und `aigw-cache` (`pgvector`) behalten ihren eigenen `PgPool`.

### 4.1 ER-Übersicht

```mermaid
erDiagram
    organization ||--o{ team : hat
    organization ||--o{ api_key : "besitzt (F3)"
    team ||--o{ api_key : besitzt
    app_user ||--o{ api_key : besitzt
    model_group ||--o{ deployment : routet
    model_group ||--o{ model_alias : "Name + Aliasse"
    provider_credential ||--o{ deployment : authentifiziert
    provider_credential ||--o{ mcp_server : authentifiziert
    mcp_server ||--o{ mcp_grant : gibt_frei
    plugin_binding }o--o| organization : "level/target_id"
    plugin_binding }o--o| team : "level/target_id"
    plugin_binding }o--o| api_key : "level/target_id"
    plugin_binding }o--o| model_group : "level/target_id"
    mcp_grant }o--|| team : "subject (team)"
    mcp_grant }o--|| api_key : "subject (api-key)"
    price }o..o{ deployment : "provider + model"
```

### 4.2 Tabellen

| Tabelle | Zweck / Besonderheiten |
|---|---|
| `config_version` | genau eine Zeile; jede Admin-Transaktion erhöht `version` als **erste** Anweisung (bei SQLite sofort Schreibsperre, kein Lock-Upgrade) |
| `audit_log` | wer, was (`create`/`update`/`delete`/`rotate` …), Art, ID, Name, Version, vorher/nachher als JSON (ohne Geheimnisse), Geltungsbereich (Organisation/Team vorher und nachher, AIGW-66) |
| `organization`, `team`, `app_user` | Mandanten; `user` ist in Postgres reserviert, daher `app_user`; OIDC (Issuer, Subjekt) eindeutig |
| `api_key` | `prefix` (eindeutig, sichtbar), `key_hash` (SHA-256 hex, eindeutig), `state`, `organization_id` (FK, **Pflicht**, F3), `team_id`/`user_id` (FK), Ablauf, Limits, Budget, Modelle, Metadaten, `last_used_at`. Zusammengesetzter FK `(team_id, organization_id) → team (id, organization_id) ON UPDATE CASCADE`: mit Team ist die Organisation die des Teams; zieht ein Team um, ziehen seine Keys mit |
| `provider_credential` | `provider`, `fields` (nur Namen), `refs` (F54: `valueFrom` je Feld, nur Namen, unverschlüsselt), `key_id`, `nonce`, `ciphertext` |
| `model_group`, `model_alias` | `model_alias` hält Name **und** Aliasse aller Gruppen in einem eindeutigen Namensraum |
| `deployment` | FK auf Modellgruppe und Zugangsdaten; `weight`, `priority`, `rpm`, `tpm`, `enabled` |
| `price` | eindeutig je (`provider`, `model`, `valid_from`); `provider = ''` = alle Anbieter; Token-Preise als JSON |
| `plugin_binding` | polymorpher Verweis (`level`, `target_id`) ohne FK; Löschen des Ziels löscht Bindungen in derselben Transaktion |
| `mcp_server`, `mcp_grant` | Freigabe je Team/Key (polymorph wie oben), `tools` als JSON-Liste |
| `tool_policy` | Ressource `ToolPolicy` (Ebene polymorph wie `plugin_binding`); wird mit dem Ziel gelöscht (Migration 0008) |
| `tool_inventory`, `tool_inventory_history` | Tool-Inventar je (Organisation, Team, Key, Name): Zähler, Hashes, Inhalte nur bei passendem `payloadMode`; Verlauf neu/geändert – keine Konfiguration, nicht im Snapshot |
| `counter_state` | nur Postgres (`postgres-sync`): (Schlüssel, Fensterbeginn, Instanz) → Wert |
| `budget_usage` | (Ebene, Ziel, Zeitraum, Beginn) → Betrag (Nano) |
| `request_log` | ein Eintrag je Anfrage (ID = `x-aigw-request-id`), Tokens, Kosten, Latenz/TTFT, Cache, Abbruch; Indizes nach Zeit, Key, Team (Details [07-beobachtung.md](07-beobachtung.md)) |
| `request_payload` | optionaler Payload (gefiltert/maskiert) je Anfrage, eigene Aufbewahrungsfrist (Migration 0002) |
| `usage_rollup`, `usage_rollup_hour` | Minuten-/Stunden-Rollups je Key/Team/Org/Modellgruppe/Deployment (ohne Wert: `''` bzw. Null-UUID, da Teil des PK); Latenz-/TTFT-Histogramm als feste Spalten für additive UPSERTs |
| `observe_batch` | F34: IDs angewendeter Writer-Batches (`batch_id` PK, `applied_at`); ein Batch wird nur angewendet, wenn seine ID neu ist – Wiederholungen nach unklarem Commit zählen Rollups nicht doppelt. Aufbewahrung 7 Tage (Retention) |
| `admin_session`, `admin_login_state`, `admin_api_key`, `admin_auth_state` | Anmeldung an der Admin-API (AIGW-32) |
| `event_dedup`, `cache_vector` | Ereignis-Deduplizierung (AIGW-61), Warmstart des semantischen Caches (AIGW-40) |

Löschen: Fremdschlüssel verhindern das Löschen verwendeter Ressourcen (Fehler `Conflict`), z. B.
eine Modellgruppe mit Deployments; Aliasse und Freigaben eines MCP-Servers fallen per Kaskade weg.

## 5. Repository

- `Store::create/update/get/get_by_id/list::<Spec>()` für alle Arten außer Keys und Zugangsdaten,
  `Store::delete(actor, kind, name)` für alle Arten.
- **Listen mit SQL-Pagination (F42):** `Store::list_page::<Spec>(&ListQuery)`,
  `list_api_keys_page`, `list_credentials_page`. Sortierung nach Name, Keyset-Cursor
  (`name > $cursor`), Suche (`?q=`: Name plus Suchspalten der Art, Modellgruppen auch Aliasse),
  Label-Selektor (`JSONB` bzw. `json_each`), ressourcenspezifische Filter (`ListFilter`) und der
  lesbare Geltungsbereich des Aufrufers (`AuditScope`: Organisationen/Teams) laufen im `WHERE`;
  gelesen werden höchstens `limit + 1` Zeilen. Die Admin-API übersetzt Rollen in den Bereich
  (`resources::read_scope`, gleiche Semantik wie `Principal::can(Read, …)`).
- **API-Keys (F3):** `organization` ist Pflicht (Schema, `aigw_core::validate::api_key_organization`,
  Admin-API und GitOps); mit Team muss es die Organisation des Teams sein. Keys ohne Team erben im
  Snapshot (`KeyCtx.organization`) Limits, Budgets und Plugins der Organisation; die Grenzen
  (AIGW-67) prüfen sie direkt gegen die Organisation.
- Jede Änderung: **eine** Transaktion → `config_version + 1` → Änderung → `audit_log` → Commit.
  Fehlgeschlagene Änderungen (Konflikt, ungültiger Verweis) hinterlassen weder Version noch Audit.
  Das `audit_log` ist damit zugleich das **Änderungsjournal** des Snapshots (AIGW-75, F16): Der
  Reloader liest die Einträge `(aktive Version, neue Version]` und lädt nur die betroffenen
  Objekte neu. Wer neue Schreibwege baut, muss je geändertem Snapshot-Objekt einen Audit-Eintrag
  mit `object_id` schreiben (sonst greift erst die Konsistenzprüfung); fehlt für eine Version jeder
  Eintrag, baut der Reloader vollständig neu.
- **API-Keys**: `create_api_key` erzeugt `sk-aigw-<12 Zeichen Präfix>-<43 Zeichen Base62 aus 32 Byte
  Zufall>` (aws-lc-rs `SystemRandom`), speichert nur Präfix und SHA-256 und gibt den Klartext einmalig
  zurück (`CreatedApiKey`). Dazu `get_api_key`, `list_api_keys`, `update_api_key`, `revoke_api_key`,
  `touch_api_keys` (letzte Nutzung, ohne Versionserhöhung).
- **Zugangsdaten**: AES-256-GCM über aws-lc-rs (gleicher Provider wie rustls, kein zusätzliches
  Krypto-Crate). Klartext-Felder als JSON verschlüsselt, zufällige 96-Bit-Nonce je Schreibvorgang,
  AAD = Ressourcen-ID (Chiffrat lässt sich nicht in andere Zeilen kopieren). Master-Key: 32 Byte hex
  aus Datei oder Umgebungsvariable, nie in der DB. `key_id` = erste 8 Byte von
  SHA-256(`"aigw-master-key-id"` ‖ Key). **Rotation**: neuen Key als `master_key_file`, alten unter
  `previous_key_files`, dann `rotate_credentials` (verschlüsselt alle Datensätze mit fremder `key_id`
  neu, je Datensatz ein Audit-Eintrag `rotate`).
- **`valueFrom`-Verweise (F54):** Kommt ein Feld aus `valueFrom` (`env`/`file`), speichert
  `provider_credential.refs` den Verweis (nur den Namen) mit; `PATCH` mit Klartext entfernt den
  Verweis des Felds. Der GitOps-Export gibt die Verweise originalgetreu aus (Felder ohne Verweis
  weiter als Platzhalter `AIGW_SECRET_…`), der Plan zeigt geänderte Verweise als sichtbare Änderung
  `spec.data.<feld>.valueFrom`.

## 6. Snapshot

`load_snapshot(&Store, &Keyring) -> Snapshot` liest alle Ressourcen und baut:

- `keys`: SHA-256 → `KeyCtx` (Key, Team, Organisation des Keys – F3, auch ohne Team; nur `active`,
  Ablauf prüft der Hot Path),
- `model_groups`: Name **und** Alias → `ModelGroupCtx` (aktive Deployments nach Priorität, mit
  entschlüsselten Zugangsdaten),
- Preise je (Anbieter, Modell) nach `validFrom` (`Snapshot::price_at`), Plugin-Bindungen nach Ebene
  und Reihenfolge, MCP-Server und -Freigaben, Organisationen/Teams nach ID.

Konsistenz: Version vor und nach dem Lesen vergleichen, bei Änderung neu laden (optimistisch). Das
Verteilen per `ArcSwap` und LISTEN/NOTIFY ist AIGW-10.

## 7. Tests

- `aigw-core`: Konfiguration (Standardwerte, unbekannte Felder, ENV-Overrides inkl. Fehlerfälle,
  Beispiel-Configs), Manifeste (Mehrfachdokument, Rundreise, Geheimnisregel, Versionen), Beträge.
- `aigw-store`: Krypto (Rundreise, AAD-Bindung, Manipulation, Rotation), Key-Format/Base62/SHA-256,
  Integrationstests (`tests/store.rs`, `tests/backends.rs` u. a.: Migrationen, CRUD aller Arten,
  Konflikte, Rollback, Audit + Version, Snapshot, Rotation, Löschregeln, SQL-Pagination, F3, F34,
  F54, Typ-Rundreisen) laufen **gemeinsam** gegen SQLite in-memory, SQLite-Datei und – mit
  `AIGW_TEST_POSTGRES_URL` – Postgres (je Lauf eigene Wegwerf-Datenbank).
- `scripts/check.sh`: `AIGW_TEST_POSTGRES=docker` startet dafür einen `postgres:17`-Container.
