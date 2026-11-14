# ai-gw – GitOps: YAML-Export, Diff und Apply (AIGW-33, AIGW-74, AIGW-55)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht)
Grundlage: [01-architektur.md](01-architektur.md) §14 · [02-datenmodell.md](02-datenmodell.md) §3.1 (Manifest-Format `aigw/v1alpha1`) · [08-admin-api.md](08-admin-api.md)

## 1. Überblick

Die gesamte Ressourcen-Konfiguration (Organisationen, Teams, Nutzer, Zugangsdaten, Modellgruppen,
Deployments, Preise, Plugin-Bindungen, MCP-Server/-Freigaben, API-Keys) lässt sich als
YAML-Mehrfachdokument exportieren, in Git pflegen und wieder anwenden.

| Weg | Befehl / Route | Code |
|---|---|---|
| CLI | `aigw config export\|diff\|apply\|validate` | `crates/aigw/src/cli_config.rs` |
| Admin-API | `GET /admin/api/v1/config:export`, `POST /admin/api/v1/config:apply` | `crates/aigw-admin/src/gitops.rs` |
| Kern | Export, Plan (Diff), Apply | `crates/aigw-store/src/gitops/` |
| Nur-YAML-Modus | `gitops.read_only_admin: true` | `crates/aigw-admin/src/read_only.rs` |

Beispiel-Manifest: [`config/examples/gitops.yaml`](../../config/examples/gitops.yaml).

## 2. CLI

```sh
aigw config export   -c aigw.yaml [--out aigw-resources.yaml] [--api-keys ignore|include]
aigw config validate aigw-resources.yaml [-c aigw.yaml]
aigw config diff     -c aigw.yaml aigw-resources.yaml [--prune] [-o text|json] [-v] [--exit-code]
aigw config apply    -c aigw.yaml aigw-resources.yaml [--prune] [--dry-run] [-o text|json] [--actor gitops]
```

- `--api-keys` gilt für alle Befehle, **Standard `ignore`** (F4): API-Keys sind Laufzeitdaten
  der Teams und werden ohne ausdrückliches `--api-keys include` weder exportiert noch angewendet
  noch gelöscht.
- `-c/--config` ist die Startkonfiguration (wie bei `aigw serve`, auch `AIGW_CONFIG`); daraus
  kommen Datenbank und Master-Key. `diff`/`apply` brauchen den Master-Key, weil Zugangsdaten
  über ihren Klartext verglichen und beim Schreiben verschlüsselt werden; `export` nicht.
- Manifest `-` liest von der Standardeingabe.
- `validate` ohne `--config` prüft offline (YAML-Syntax, `apiVersion`/`kind`, unbekannte Felder,
  Namen, Geheimnisregel, Regeln der Spezifikation, Dubletten). Mit `--config` zusätzlich alles,
  was `apply` vorab prüft (Verweise, Geheimnisse, Eindeutigkeit) – ohne zu schreiben.
- Alle Befehle mit `--config` migrieren die Datenbank bei Bedarf (idempotent, wie `serve`).
- Exit-Codes: `0` Erfolg, `1` Fehler (alle Validierungsfehler auf stderr), `2` nur bei
  `diff --exit-code` mit ausstehenden Änderungen (Drift-Erkennung in CI).
- Laufende Server übernehmen Änderungen über den Reloader (§14): bei Postgres sofort per
  `NOTIFY`, bei SQLite über die Versionsabfrage (`store.reload_poll_secs`, Standard 30 s).

Textausgabe von `diff`:

```text
Plan gegen config_version 12 (prune: ja, API-Keys: ignore)
  + ModelGroup "mistral-large" anlegen
  ~ ProviderCredential "openai-prod" ändern
      spec.data.apiKey: (Geheimnis geändert)
  ~ Team "platform" ändern
      spec.budget.amount: "500" → "600"
  - Deployment "claude-sonnet-anthropic" löschen
Zusammenfassung: 1 anlegen, 2 ändern, 1 löschen, 10 unverändert
```

`-o json` liefert denselben Plan maschinenlesbar (`changes[]` mit `kind`, `name`, `action`,
`fields[]` = `{path, before, after, secret}`, dazu `summary` und `warnings`). Geheime Werte
erscheinen nie, nur `secret: true`.

## 3. Export

- Alle Arten in Abhängigkeitsreihenfolge (`ResourceKind::ALL`: Organization → Team → User →
  ProviderCredential → Secret → ModelGroup → Deployment → Price → ApiKey → McpServer → McpGrant →
  PluginBinding), je Art nach Name (Bytefolge, unabhängig von der DB-Kollation).
- Feldreihenfolge: `apiVersion`, `kind`, `metadata`, `spec`; innerhalb von `spec` alphabetisch
  (feste Ordnung über die JSON-Zwischenform). Standardwerte werden weitgehend weggelassen.
- **Ohne Server-Felder**: keine IDs, `createdAt`/`updatedAt`, Key-Präfix/-Hash, Key-Zustand,
  `lastUsedAt`, `keyId` der Zugangsdaten. Damit ist der Export bei unverändertem Bestand
  byte-identisch (getestet: export → apply → export).
- **Zugangsdaten** nur als Verweis: Kam ein Feld beim Schreiben aus `valueFrom` (GitOps oder
  Admin-API), steht dieser Verweis **originalgetreu** im Export (F54, AIGW-73: gespeichert in
  `provider_credential.refs`, nur Namen, unverschlüsselt). Felder ohne Verweis (Klartext über
  die Admin-API, auch nach `PATCH` mit Klartext) erscheinen als Platzhalter
  `valueFrom: {env: AIGW_SECRET_<ZUGANG>_<FELD>}` (Großbuchstaben, camelCase → `SNAKE_CASE`,
  nie `__`, damit die Variable nicht als Konfig-Override `AIGW_<ABSCHNITT>__<FELD>` gilt).
  Der Kopfkommentar listet alle Platzhalter. Vor `apply` diese Variablen setzen oder die
  Verweise auf eigene Variablen/Dateien ändern. Dasselbe gilt für **`Secret`** (F149,
  allgemeine Geheimnisse für Webhooks, Guardrail-Dienste, MCP-Server): in YAML nur
  `valueFrom`, Verweise originalgetreu (`secret.refs`), sonst Platzhalter.
- **API-Keys** nur mit `--api-keys include` (bzw. `?apiKeys=include`), dann ohne Klartext (der
  ist nach der Erzeugung nicht mehr bekannt). Standard (`ignore`, F4): sie fehlen ganz.
- **Teil-Export (AIGW-55, Admin-API):** `kinds=ModelGroup,Deployment` (auch kebab-case),
  `name=<name>` (genau eine Ressource je Art), `organization=<org>` (die Organisation, ihre Teams
  und API-Keys sowie Plugin-Bindungen und MCP-Freigaben auf Ebene der Organisation, ihrer Teams
  und Keys) bzw. `team=<team>` (das Team, seine Keys, deren Bindungen/Freigaben). Katalog-
  Ressourcen (Modellgruppen, Deployments, Zugangsdaten, Preise …) und Nutzer gehören zu keiner
  Organisation. Der Kopfkommentar markiert den Teil-Export: **nur ohne `--prune` anwenden**, sonst
  würde alles außerhalb der Auswahl gelöscht. Unbekannte Art → `400`.

## 4. Diff und Apply

### 4.1 Ablauf

1. Manifest lesen und offline prüfen (wie `validate`). Alle Fehler werden gesammelt.
2. Ist-Zustand konsistent lesen (Version vor/nach dem Lesen gleich), inkl. entschlüsselter
   Zugangsdaten und Key-Hashes.
3. Geheimnisse auflösen (`valueFrom: env|file`, **zur Apply-Zeit**, im Prozess, der anwendet).
4. Endzustand bilden (Manifest + ohne `--prune` alle bestehenden Ressourcen) und vollständig
   prüfen:
   - Verweise per Name: Team → Organisation, Rollen → Organisation/Team, Key → Team/Nutzer,
     Deployment → Modellgruppe/Zugangsdaten, MCP-Server → Zugangsdaten, Freigabe → Server und
     Team/Key, Plugin-Bindung → Ziel der Ebene. `allowedModels`, `fallbacks[].modelGroup` und
     `limits[].modelGroup` müssen eine Modellgruppe (Name oder Alias) treffen. Bei Verweisen auf
     Ressourcen, die `--prune` löschen würde, nennt die Meldung das.
   - Eindeutigkeit: Modellnamen und Aliasse über alle Gruppen, Preis je
     (Anbieter, Modell, `validFrom`), OIDC-Identität (Issuer, Kennung), Key-Präfix.
   - Regeln wie in der Admin-API (`aigw_core::validate`), u. a.: Zugangsdaten passen zum
     Deployment (gleicher Anbieter oder Kompatibilitätsliste, F65), `settings.caFile` nur
     textuell innerhalb `upstream.ca_dir` (F177; Symlinks prüft der Gateway beim Routenaufbau),
     Nutzer mit vollständiger OIDC-Identität (`oidcIssuer` + `oidcSubject`) und Rolle `gitops`
     nur global (F90, F55). Gelöschte Modellgruppen bzw. entfernte Aliasse, auf die noch
     `allowedModels`/`limits` zeigen, sind Fehler (kein `cascade` in GitOps, F66).
5. Plan je Ressource: `create`, `update` (mit geänderten Feldern), `delete` (nur `--prune`),
   `unchanged`, `retain` (nicht im Manifest, bleibt ohne `--prune`), `skip` (siehe Keys).
6. `--dry-run`/`diff` enden hier. Gibt es nichts zu tun, wird nichts geschrieben (keine neue
   Version).
7. **Eine Transaktion**: `config_version + 1` (genau einmal) als erste Anweisung; danach muss
   die neue Version = gelesene Version + 1 sein, sonst Abbruch mit „zwischenzeitlich geändert“
   (Admin-API `409`, nichts angewendet). Dann alle Anlagen/Änderungen in
   Abhängigkeitsreihenfolge, danach alle Löschungen in umgekehrter Reihenfolge; Aliasse
   geänderter/gelöschter Modellgruppen werden vorab freigegeben (Aliasse können zwischen
   Gruppen wandern). Je Änderung ein `audit_log`-Eintrag (vorher/nachher ohne Geheimnisse),
   Akteur `gitops` (CLI, änderbar per `--actor`) bzw. `gitops:<principal>` (Admin-API).
   Schlägt ein Schritt fehl (z. B. ein DB-Konflikt, den die Vorprüfung nicht erkennt), wird
   zurückgerollt – **alles oder nichts**.

Abweichung von der Reihenfolge im Auftrag (… PluginBinding → MCP → Key): Plugin-Bindungen und
MCP-Freigaben können auf API-Keys zeigen, deshalb kommen Keys **vor** MCP und Bindungen
(`ResourceKind::ALL`).

### 4.2 Vergleich

- Verglichen werden `metadata.labels` und `spec` (Server-Felder werden ignoriert). Zeitpunkte
  werden wie gespeichert normalisiert (UTC, Millisekunden).
- Zugangsdaten: Anbieter und Labels wie oben, Werte über den Klartext (bestehend entschlüsselt
  vs. aufgelöstes `valueFrom`); im Plan nur `spec.data.<feld>` mit `secret: true`. Geänderte
  Verweise selbst (F54) erscheinen sichtbar als `spec.data.<feld>.valueFrom` (vorher/nachher,
  nur Namen) und werden beim `apply` gespeichert.
- API-Keys: `spec.organization` ist Pflicht (F3); mit Team muss es die Organisation des Teams
  sein, sonst ein Fehler am Dokument.
- `apply` ersetzt die Spezifikation vollständig (wie `PUT`), Felder, die im Manifest fehlen,
  fallen auf ihre Standardwerte zurück.

### 4.3 API-Keys (Option `--api-keys`, Standard `ignore` – F4)

| Fall | `include` | `ignore` (Standard) |
|---|---|---|
| Key im Manifest und in der DB | Spezifikation/Labels anwenden; Geheimnis bleibt – außer `spec.secret` gibt einen anderen Klartext vor (dann neuer Hash/Präfix, alter Klartext ungültig) | `skip` (Warnung) |
| Key nur im Manifest, mit `spec.secret.valueFrom` | anlegen mit diesem Klartext (Format `sk-aigw-<12>-<43>`) | `skip` |
| Key nur im Manifest, ohne `spec.secret` | `skip` mit Warnung (ein erzeugter Klartext wäre nirgends abrufbar) | `skip` |
| Key nur in der DB | `retain`, mit `--prune` gelöscht | immer `retain` |

`spec.secret` ist eine GitOps-Erweiterung des Manifests (nicht Teil von `ApiKeySpec` der
Admin-API); Klartext (`value`) ist dort wie überall in YAML verboten. Der Key-Zustand
(`active`/`suspended`/`revoked`, F37) wird von GitOps weder exportiert noch verändert – Sperren
und Widerrufen sind Betriebsaktionen der Admin-API.

### 4.4 Geheimnisse

- `valueFrom: {env: NAME}` – Umgebungsvariable des Prozesses, der anwendet.
- `valueFrom: {file: /pfad}` – Dateiinhalt ohne abschließenden Zeilenumbruch.
- CLI: keine Einschränkung (läuft mit den Rechten des Betreibers).
- **Admin-API**: nur `env` mit Präfix `AIGW_SECRET_`; Dateien nur mit `gitops.secret_dir`
  (F52) und nur darin: relative Pfade gelten in diesem Verzeichnis, absolute müssen darin
  liegen, `..` ist verboten, Symlinks werden aufgelöst und dürfen nicht hinausführen (die
  Fehlermeldung nennt nie den Inhalt). Ohne `secret_dir` keine Dateien. Sonst könnte ein
  Admin-Token beliebige Variablen (z. B. `AIGW_MASTER_KEY`) oder Dateien des Servers in
  Zugangsdaten kopieren und über einen Upstream-Endpunkt abfließen lassen.

  ```yaml
  gitops:
    secret_dir: /run/aigw/secrets   # bzw. AIGW_GITOPS__SECRET_DIR
  ```
- Fehlende Variablen/Dateien sind Validierungsfehler – es wird nichts angewendet.

## 5. Admin-API

| Route | Berechtigung | Inhalt |
|---|---|---|
| `GET /admin/api/v1/config:export?apiKeys=ignore\|include&kinds=&name=&organization=&team=` | `Read` auf `GitOps`: globale `admin`, `viewer`, `auditor`, `gitops` | `200 application/yaml` (`Cache-Control: no-store`); Standard `apiKeys=ignore` (F4); Teil-Export §3 |
| `POST /admin/api/v1/config:apply?dryRun=&prune=&apiKeys=` | `Write` auf `GitOps`: globaler `admin`, `gitops` (auch Trockenlauf: er löst Server-Geheimnisse auf und vergleicht sie) | Körper YAML; `200` mit `{plan, applied, configVersion}`; `422` mit allen Fehlern im `detail` und einzeln in `issues` (`document`, `kind`, `name`, `line`, `column`, `message`, AIGW-55); `409` bei zwischenzeitlicher Änderung; Standard `apiKeys=ignore` |

**Zeilenbezug (AIGW-55):** YAML-Syntaxfehler tragen Zeile und Spalte des Parsers
(`serde-saphyr`), alle übrigen Fehler (unbekannte Felder, Regeln der Spezifikation, Dubletten,
Verweise, Geheimnisse) die erste Inhaltszeile ihres Dokuments (`aigw_store::gitops::locate`,
Dokumente getrennt durch `---`). Die Admin-UI markiert diese Zeilen im Editor und springt per
Klick dorthin.

**Admin-UI (AIGW-55):** Seite „YAML-Import/-Export“ (`/config`): Export gesamt bzw. nach Art,
Organisation, Team oder einzelner Ressource mit Vorschau und Download (Detailseiten von Key, Team,
Organisation, Modellgruppe und Deployment verlinken mit „YAML“ auf den passenden Teil-Export);
Import mit Editor (Zeilennummern), Datei laden, Prune/API-Keys, Trockenlauf mit Diff
(`PlanView`), Fehlerliste mit Zeilenbezug und Anwenden erst nach Bestätigung (mit Löschungen nur
nach Eingabe von „ANWENDEN“). Export sehen globale Leserollen und `gitops`, Import globale
`admin` und `gitops` – auch im Modus „nur YAML“. Geheimwerte erscheinen nie im Klartext.

Konventionen wie in [08-admin-api.md](08-admin-api.md) (Problem Details, unbekannte
Parameter → `400`, Bearer-Anmeldung). `config:apply` braucht kein `If-Match` (F38): die
Versionsprüfung des Plans (§4.1, Schritt 7) übernimmt diese Rolle.

**Rolle `gitops` (F55):** für Admin-API-Keys der CI-Pipeline – nur `config:export` und
`config:apply` (inkl. `dryRun`), keine anderen Routen (`403`, Listen leer). Nur global wirksam
(mit `organization`/`team` → `422`).

**Nutzer mit Rechten (S-05, AIGW-90):** Ein `User` mit `roles` oder OIDC-Verknüpfung
(`oidcIssuer`/`oidcSubject`) darf über `config:apply` nur ein globaler `admin` anlegen, ändern
oder (mit `prune`) löschen – die Rolle `gitops` nur mit `gitops.manage_users: true` in der
Startkonfiguration (Standard `false`). Sonst bricht der Plan (auch `dryRun`) mit `422` ab, je
betroffenem Nutzer ein Eintrag in `issues` („Nutzer mit Rollen oder OIDC-Verknüpfung
anlegen/ändern/löschen: nur ein globaler admin …“). Erlaubt bleiben unveränderte Nutzer (ein
exportiertes Manifest lässt sich also wieder anwenden), beibehaltene Nutzer (ohne `prune`) und
Nutzer ohne Rollen/OIDC. Die CLI (`aigw config apply`) ist nicht betroffen
(`ApplyOptions::manage_users`, Standard `true`). Admin-API-Keys sind keine GitOps-Ressource.

```yaml
gitops:
  manage_users: false        # bzw. AIGW_GITOPS__MANAGE_USERS=true
```

Beispiel eines CI-Keys:

```json
POST /admin/api/v1/admin-api-keys
{"name": "gitops-ci", "description": "GitOps-Pipeline", "roles": [{"role": "gitops"}],
 "expiresAt": "2027-06-30T00:00:00Z"}
```

## 6. Modus „nur YAML“

```yaml
gitops:
  read_only_admin: true      # bzw. AIGW_GITOPS__READ_ONLY_ADMIN=true
```

Alle schreibenden Anfragen (alles außer `GET`/`HEAD`/`OPTIONS`) unter `/admin/api/` antworten
mit `403` (`code: read-only`, Hinweis auf `aigw config apply`) – vor Anmeldung und
Berechtigungsprüfung. Ausgenommen ist `POST /admin/api/v1/config:apply` (der GitOps-Weg selbst,
z. B. aus CI). Die CLI schreibt direkt in die Datenbank und ist nicht betroffen. Die UI erfährt
den Modus über `GET /admin/api/v1/auth/info` (`readOnly`, F56) und blendet Schreibaktionen aus.

## 7. Grenzen

- Ein Name gehört genau einer Ressource je Art; Umbenennen = Löschen + Anlegen (`--prune`).
- Eindeutigkeitskonflikte, bei denen ein **gelöschtes** Objekt einen Wert belegt, den ein
  **neues** Objekt braucht (z. B. Preis mit gleichem Anbieter/Modell/`validFrom` unter neuem
  Namen), scheitern in der Datenbank (Anlagen laufen vor Löschungen) → Rollback, nichts
  angewendet; in zwei Schritten anwenden.
- Mehrere Instanzen + SQLite gibt es nicht; bei Postgres verhindert die Versionsprüfung, dass
  ein veralteter Plan angewendet wird.

## 8. Tests

- `crates/aigw-store/tests/gitops.rs` (SQLite in-memory, SQLite-Datei, Postgres mit
  `AIGW_TEST_POSTGRES_URL`): Beispiel-Manifest trocken/echt anwenden (eine Version, 14
  Audit-Einträge mit Akteur `gitops`, Geheimnisse nicht im Audit), erneutes Anwenden ohne
  Änderung, Rundreise export → apply → export identisch, Diff-Fälle (update mit Feldpfad,
  create, retain, Geheimnis geändert), Prune (inkl. Ablehnung bei noch verwendeten
  Ressourcen), wandernde Aliasse, Fehler in einem Dokument → nichts angewendet, fehlende
  Variable, Geheimnis aus Datei, eingeschränkte Herkunft, alle Key-Fälle, leeres Manifest +
  Prune löscht alles. `privileged_users_need_manage_users` (S-05): Nutzer mit Rollen/OIDC ohne
  `manage_users` weder anlegen, ändern, verknüpfen, Rechte entziehen noch per Prune löschen;
  unverändert und ohne Rollen erlaubt.
- `crates/aigw-admin/tests/gitops_api.rs`: Export/Apply über HTTP, `422`, Geheimnis-Policy,
  Nur-YAML-Modus (auch `auth/info.readOnly`), Teil-Export (Art, Name, Organisation, Team) und
  `issues` mit Zeile (Feldfehler, Verweisfehler, YAML-Syntax).
- `ui/e2e/tests/aigw54.spec.js`: Export-Vorschau ohne Geheimwerte, Download, Teil-Export von der
  Team-Detailseite, Fehler mit Zeilenbezug im Editor, Trockenlauf, Anwenden mit Bestätigung.
- `crates/aigw-admin/tests/admin_auth_api.rs` (SQLite und Postgres): Rolle `gitops`, `viewer`
  nur Export, `gitops.secret_dir` (innerhalb, `..`, außerhalb, Symlink), API-Keys standardmäßig
  `skip`, Kompatibilitätsliste der Zugangsdaten, S-05 (gitops-Key: Nutzer mit Rolle/OIDC → `422`
  mit `issues`, auch `dryRun`; globaler admin darf; unverändert erneut anwenden erlaubt).
- Unit-Tests: Platzhalternamen, `spec.secret`, Dubletten, Geheimnis-Policy, CLI-Parser,
  Offline-Validierung des Beispiels.
