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
| `guardrails/` | PII (global, gesperrt), Prompt-Filter (Team), Azure Content Safety (Organisation) |
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
- **Request-Body:** `server.data.max_body_bytes` (32 MiB), Audio `max_audio_bytes` (25 MiB).
- **Mock-Anbieter:** `admin.allow_mock: true` ist Standard (Demos, Smoke-Tests). Für
  Produktion `false`, ebenso `admin.ui.mock: false`.

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
