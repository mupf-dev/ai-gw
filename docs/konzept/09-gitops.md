# ai-gw – GitOps: YAML-Export, Diff und Apply (AIGW-33)

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
aigw config export   -c aigw.yaml [--out aigw-resources.yaml] [--api-keys include|ignore]
aigw config validate aigw-resources.yaml [-c aigw.yaml]
aigw config diff     -c aigw.yaml aigw-resources.yaml [--prune] [-o text|json] [-v] [--exit-code]
aigw config apply    -c aigw.yaml aigw-resources.yaml [--prune] [--dry-run] [-o text|json] [--actor gitops]
```

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
Plan gegen config_version 12 (prune: ja, API-Keys: include)
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
  ProviderCredential → ModelGroup → Deployment → Price → ApiKey → McpServer → McpGrant →
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
  Verweise auf eigene Variablen/Dateien ändern.
- **API-Keys** ohne Klartext (der ist nach der Erzeugung nicht mehr bekannt). Mit
  `--api-keys ignore` fehlen sie ganz.

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
     (Anbieter, Modell, `validFrom`), OIDC-Identität (Issuer, Subjekt), Key-Präfix.
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

### 4.3 API-Keys (Option `--api-keys`, Empfehlung aus AIGW-9)

| Fall | `include` (Standard) | `ignore` |
|---|---|---|
| Key im Manifest und in der DB | Spezifikation/Labels anwenden; Geheimnis bleibt – außer `spec.secret` gibt einen anderen Klartext vor (dann neuer Hash/Präfix, alter Klartext ungültig) | `skip` (Warnung) |
| Key nur im Manifest, mit `spec.secret.valueFrom` | anlegen mit diesem Klartext (Format `sk-aigw-<12>-<43>`) | `skip` |
| Key nur im Manifest, ohne `spec.secret` | `skip` mit Warnung (ein erzeugter Klartext wäre nirgends abrufbar) | `skip` |
| Key nur in der DB | `retain`, mit `--prune` gelöscht | immer `retain` |

`spec.secret` ist eine GitOps-Erweiterung des Manifests (nicht Teil von `ApiKeySpec` der
Admin-API); Klartext (`value`) ist dort wie überall in YAML verboten. Der Key-Zustand
(`active`/`revoked`) wird von GitOps nicht verändert.

### 4.4 Geheimnisse

- `valueFrom: {env: NAME}` – Umgebungsvariable des Prozesses, der anwendet.
- `valueFrom: {file: /pfad}` – Dateiinhalt ohne abschließenden Zeilenumbruch.
- CLI: keine Einschränkung (läuft mit den Rechten des Betreibers).
- **Admin-API**: nur `env` mit Präfix `AIGW_SECRET_`, keine Dateien. Sonst könnte ein
  Admin-Token beliebige Variablen (z. B. `AIGW_MASTER_KEY`) oder Dateien des Servers in
  Zugangsdaten kopieren und über einen Upstream-Endpunkt abfließen lassen.
- Fehlende Variablen/Dateien sind Validierungsfehler – es wird nichts angewendet.

## 5. Admin-API

| Route | Berechtigung | Inhalt |
|---|---|---|
| `GET /admin/api/v1/config:export?apiKeys=include\|ignore` | `Read` | `200 application/yaml` (`Cache-Control: no-store`) |
| `POST /admin/api/v1/config:apply?dryRun=&prune=&apiKeys=` | `Write` (auch Trockenlauf: er löst Server-Geheimnisse auf und vergleicht sie) | Körper YAML; `200` mit `{plan, applied, configVersion}`; `422` mit allen Fehlern im `detail`; `409` bei zwischenzeitlicher Änderung |

Konventionen wie in [08-admin-api.md](08-admin-api.md) (Problem Details, unbekannte
Parameter → `400`, Bearer-Anmeldung). Neue Berechtigungen wurden nicht eingeführt.

## 6. Modus „nur YAML“

```yaml
gitops:
  read_only_admin: true      # bzw. AIGW_GITOPS__READ_ONLY_ADMIN=true
```

Alle schreibenden Anfragen (alles außer `GET`/`HEAD`/`OPTIONS`) unter `/admin/api/` antworten
mit `403` (`code: read-only`, Hinweis auf `aigw config apply`) – vor Anmeldung und
Berechtigungsprüfung. Ausgenommen ist `POST /admin/api/v1/config:apply` (der GitOps-Weg selbst,
z. B. aus CI). Die CLI schreibt direkt in die Datenbank und ist nicht betroffen. Die UI zeigt
den Modus noch nicht an (Folgeaufgabe: Flag in `GET /version` o. Ä.).

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
  Prune löscht alles.
- `crates/aigw-admin/tests/gitops_api.rs`: Export/Apply über HTTP, `422`, Geheimnis-Policy,
  Nur-YAML-Modus.
- Unit-Tests: Platzhalternamen, `spec.secret`, Dubletten, Geheimnis-Policy, CLI-Parser,
  Offline-Validierung des Beispiels.
