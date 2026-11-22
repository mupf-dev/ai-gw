# WASM-Plugins (AIGW-63, AIGW-101, AIGW-106)

Eigene Plugin-Logik ohne Fork und ohne externen Dienst: Betreiber laden eine
**WebAssembly-Komponente** hoch, die das Gateway in einer Sandbox in der Plugin-Kette ausführt
(Architektur §9, Entscheidung „Webhook im MVP, WASM nach dem MVP“). Gegenüber dem Webhook
(Netzwerk-Rundreise, 200 ms Standard-Zeitlimit) läuft ein WASM-Plugin im Prozess (≈ 10–50 µs
je Aufruf, §9) und auch im Stream-Pfad.

```text
Admin-API PUT /wasm-modules/{name}?sha256=…   ─┐
wasm.modules (Datei, Startkonfiguration)        ├─► Tabelle wasm_module ─► Snapshot ─► Kette
                                                 ┘   (Audit, config_version)   (Hot Reload)
Bindung  plugin: wasm, config.module: <name>  ─► Fabrik: Modul aus Snapshot → kompilieren (Cache
                                                  nach SHA-256) → describe(config) → Instanz
Anfrage  ─► je Aufruf frische Instanz (Pooling-Allocator), Fuel + Epoch-Deadline + Speichergrenze
```

## 1. Schnittstelle (WIT)

Datei: [`crates/aigw-wasm/wit/plugin.wit`](../../crates/aigw-wasm/wit/plugin.wit), Paket
`aigw:plugin@0.1.0`, Welt `plugin`. Abgeleitet aus dem Plugin-Trait (§12-plugins):

| Trait | WIT |
|---|---|
| `phases()` | `describe(config) -> result<phases, string>` (einmal beim Aufbau der Kette; prüft zugleich die Konfiguration) |
| `on_request` / `on_response` / `on_chunk` | `handle(input) -> result<output, string>` mit `input.phase` = `request` \| `response` \| `chunk` |
| `ReqCtx.request`, `RespCtx.response`, Stream-Ereignis | `input.payload`: **JSON-IR** (`ChatRequest`, `ChatResponse`, `StreamEvent`, Doku 03) |
| `Meta` | `input.meta` (JSON-Objekt, nur externe Sicht ohne `aigw.*`); `output.meta` ersetzt die externen Schlüssel |
| `CallInfo` | `input.info`: Request-ID, Eingangsformat, Modellgruppe, Stream, Key/Team/Organisation (Namen) |
| Client-Header | `input.headers`: nur die in `includeHeaders` freigegebenen (nie `authorization`, Cookies …) |
| `upstream_headers`, `response_headers` | `output.upstream-headers` (nur Anfrage-Phase, Auth-/Hop-Header gesperrt), `output.response-headers` |
| `Flow`/`ChunkFlow` | `output.action`: `pass` (weiter), `modify(json)` (ändern), `block(rejection)` (Guardrail-Ablehnung, Header `x-aigw-guardrail: wasm`), `reject(rejection)` (Fehler 400–499), `respond(json)` (nur Anfrage: feste Antwort), `drop` (nur Stream: Ereignis verwerfen) |
| `PluginError` | `result::err(string)`, Trap, Grenzüberschreitung → `failMode` |

Warum JSON statt WIT-Records für die IR: Die IR hat viele optionale Felder, `extra` für
unbekannte Felder und wächst mit jedem Anbieter. Als JSON bleibt die WIT-Welt stabil (Version
`0.1.0`), Plugins nutzen beliebige JSON-Bibliotheken, und die Regeln für Änderungen sind
dieselben wie beim Webhook (`model`/`stream` unveränderlich, `hints`/`source` übernimmt das
Gateway). Kosten: ein Serialisieren und ggf. Parsen je Aufruf (Messwerte §9).

Host-Funktionen (`import host`): nur `log(level, message)` – gekürzt auf 1 KiB, höchstens 16
Meldungen je Aufruf, landet im Gateway-Log mit `wasm_module`.

## 2. Sandbox und Grenzen

| Grenze | Umsetzung | Konfiguration |
|---|---|---|
| CPU | **Fuel** (wasmtime zählt Instruktionen; leer → Trap `fuel`) | Bindung `fuel`, Standard `wasm.default_fuel` (10⁸), Obergrenze `wasm.max_fuel` |
| Zeit | **Epoch-Interrupt**: ein Thread erhöht alle `wasm.epoch_tick_ms` (2 ms) die Epoche; Deadline = `timeoutMs` der Bindung (Standard 100 ms) → Trap `timeout`. Anfrage-/Antwort-Phase zusätzlich mit dem Zeitlimit der Kette (`Hook::Pending`) | `timeoutMs` (Kettensteuerung) |
| Speicher | `ResourceLimiter` je Store: jeder lineare Speicher ≤ Grenze → sonst Trap `memory`; Pool-Slots in derselben Größe | Bindung `maxMemoryMb`, Standard `wasm.default_memory_mb` (32), Obergrenze `wasm.max_memory_mb` (256) |
| Instanzen | Pooling-Allocator mit `wasm.pool_instances` (256) gleichzeitigen Instanzen; erschöpft → Fehler `instantiate` → `failMode` | `wasm.pooling`, `wasm.pool_instances` |
| WASI | WASI 0.2 **ohne Freigaben**: keine Verzeichnisse (kein Dateisystem), keine Umgebung/Argumente, Sockets/DNS verboten (TCP/UDP aus, Adressprüfung lehnt alles ab), stdin leer, stdout/stderr verworfen. Erlaubt: Uhren, Zufall | – |
| Importe | Nur WASI 0.2 und `aigw:plugin/host`; jede andere Importanforderung (z. B. `wasi:http`) scheitert schon beim Aufbau (`instantiate_pre`) | – |
| Zustand | Instanz **je Aufruf** – kein Zustand zwischen Aufrufen oder Anfragen (Datenschutz, keine Seiteneffekte zwischen Mandanten); Zustand je Anfrage/Stream über `meta` | – |

Fehlerarten (Metrik `aigw_wasm_errors_total{kind}`): `fuel`, `timeout`, `memory`, `trap` (Panic,
`unreachable`, Speicherzugriff), `guest` (`result::err`), `output` (ungültige Ausgabe, verbotene
Änderung oder gesperrter Header), `instantiate`. Was daraus wird, entscheidet `failMode` der
Bindung – **Standard `closed`** (F148: Sicherheits-Plugins schließen). Ungültige Bindungen
(Modul fehlt, SHA passt nicht, Kompilieren/`describe` scheitert) behandelt der Index wie jede
ungültige Bindung (§12-plugins: übersprungen bzw. mit `failMode: closed` Kette gesperrt,
`500 plugin_config_invalid`).

Ausführungsmodell: Anfrage- und Antwort-Phase laufen über `spawn_blocking` (Tokio-Worker bleiben
frei; die Kette wartet höchstens `timeoutMs`, die Instanz bricht spätestens an der
Epoch-Deadline ab). Die Stream-Phase ist synchron (`on_chunk`) und läuft per `block_in_place`
auf dem Worker. Hintergrund: WASI-Aufrufe der Komponente (z. B. Panic-Ausgabe auf stderr)
blockieren in wasmtime-wasi intern auf Tokio und dürfen nicht im asynchronen Kontext laufen.

## 3. Module: Herkunft, Integrität, Freigabe

- **Tabelle `wasm_module`** (Migration 0009, SQLite + Postgres): Name, SHA-256, Größe,
  Beschreibung, Inhalt (BLOB/BYTEA), hochgeladen von, Zeiten. Module sind **keine** generische
  Ressource (Binärinhalt), gehören aber zum Snapshot: Jede Änderung erhöht `config_version`
  und schreibt einen Audit-Eintrag `wasm-module` mit `object_id` (Journal des inkrementellen
  Snapshots, 02-datenmodell). Im Audit stehen nur Metadaten, nie der Inhalt.
- **Upload über die Admin-API**: `PUT /admin/api/v1/wasm-modules/{name}?sha256=<hex>` mit
  `Content-Type: application/wasm`. Der SHA-256 ist Pflicht und muss zum Inhalt passen
  (Integrität der Übertragung; Tippfehler oder abgeschnittene Uploads fallen auf). Geprüft
  werden außerdem Name, Größe (`wasm.max_module_bytes`, 16 MiB) und das Komponenten-Format
  (Kern-Module ohne Component Model werden abgelehnt). Ersetzen braucht `If-Match`
  (ETag = SHA-256), Löschen ebenso und nur ohne verweisende Bindungen (`409`).
- **Freigabe nur durch globalen `admin`** (Katalog-Schreibrecht): Ein Modul läuft im
  Gateway-Prozess und sieht Anfragen aller Mandanten, die es binden. Org-/Team-Admins dürfen
  freigegebene Module **binden** (auf ihrer Ebene), aber keine hochladen. Ohne weitere
  Einstellungen ist der Upload durch einen globalen Admin die Freigabe; der SHA-256 in der
  Bindung pinnt die freigegebene Version. Dazu kommen wahlweise eine Ed25519-Signatur
  (`wasm.trusted_keys`, §3a, AIGW-101), eine Sigstore-Signatur ohne Schlüsselverwaltung
  (`wasm.sigstore`, §3b, AIGW-106) und die Vier-Augen-Freigabe (`wasm.four_eyes`, §3c,
  AIGW-106).
- **Datei aus der Startkonfiguration** (`wasm.modules: [{name, path, sha256}]`): beim Start
  in die Datenbank übernommen (Akteur `startup-config`; unverändert = kein Schreibvorgang).
  Abweichender SHA-256 bricht den Start ab, ebenso eine fehlende/ungültige Signatur (`key_id`,
  `signature`; §3a). Weg für den Modus „nur YAML“, in dem die Admin-API
  nicht schreibt.
- **GitOps**: Bindungen verweisen per Name und optional SHA-256
  (`config: {module: keyword-guard, sha256: …}`); Module selbst sind nicht Teil des YAML.
  Ein Verweis auf ein (noch) fehlendes Modul ist beim Apply erlaubt (das Modul kann danach
  kommen) und macht die Bindung bis dahin ungültig (Log, `failMode`). Die Admin-API prüft beim
  Anlegen/Ändern einer Bindung dagegen, dass Modul und SHA passen (`422`).

## 3a. Signaturen (AIGW-101)

Der Upload durch einen globalen Admin schützt nicht gegen Schreibzugriff auf Datenbank oder
Snapshot-Cache und nicht gegen ein übernommenes Admin-Konto. Signaturen binden ein Modul an
einen Schlüssel, der außerhalb des Gateways liegt (Build-System, offline).

**Format.** Ed25519 (RFC 8032) über die ASCII-Nachricht

```text
aigw-wasm-module-v1:<sha256 des Moduls, 64 Hex-Zeichen, klein>
```

ohne Zeilenumbruch. Das Präfix trennt die Domäne (eine Signatur gilt für nichts anderes) und
versioniert das Format; signiert wird der Hash, damit Werkzeuge große Module nicht zweimal lesen
müssen (SHA-256 ist kollisionsresistent, die Signatur bindet also den Inhalt). Übertragen wird
eine **abgesetzte Signatur**: Schlüssel-ID + 64 Byte Signatur als Base64 (Standard- oder
URL-Alphabet, mit/ohne `=`; gespeichert normalisiert im Standard-Alphabet).

| Weg | Angabe |
|---|---|
| Admin-API | `PUT /admin/api/v1/wasm-modules/{name}?sha256=…&keyId=…&signature=…` (Base64 prozentkodiert; `aigw wasm sign --format query` liefert die Parameter) |
| Startkonfiguration | `wasm.modules[]: {name, path, sha256, key_id, signature}` |
| Admin-UI | Upload-Dialog: Felder „Schlüssel-ID“ und „Signatur“ |

**Konfiguration.**

```yaml
wasm:
  trusted_keys:
    - id: release-2026
      public_key: |            # PEM, Base64 (32 Byte roh) oder Base64 SubjectPublicKeyInfo
        -----BEGIN PUBLIC KEY-----
        MCowBQYDK2VwAyEA…
        -----END PUBLIC KEY-----
  require_signature: null      # Standard: Pflicht, sobald trusted_keys gesetzt sind
```

| `trusted_keys` | `require_signature` | unsigniert | Signatur vorhanden |
|---|---|---|---|
| leer | `null`/`false` | erlaubt (wie vor AIGW-101) | nur gespeichert, nicht geprüft (`unchecked`) |
| leer | `true` | Konfigurationsfehler (Start bricht ab) | – |
| gesetzt | `null` (Standard) / `true` | **abgelehnt** | muss gültig sein |
| gesetzt | `false` (Übergang) | erlaubt (`unsigned`) | muss gültig sein |

Warum Pflicht als Standard: Wer Schlüssel hinterlegt, will signierte Module. Wäre ein
unsigniertes Modul weiterhin erlaubt, könnte genau der Angreifer, gegen den die Signatur
schützen soll, sie durch Weglassen umgehen. `false` ist nur für die Umstellung gedacht. Eine
**vorhandene** Signatur muss immer gültig sein (unbekannter Schlüssel oder falsche Signatur =
abgelehnt) – sonst wäre ein entfernter (zurückgezogener) Schlüssel wirkungslos.

**Prüfpunkte.**

1. **Upload** (Admin-API): gegen den SHA-256 des empfangenen Inhalts; Fehler `422`
   („Signatur fehlt“, „unbekannter Schlüssel“, „ungültig“).
2. **Startkonfiguration** (`wasm.modules`): Fehler bricht den Start ab.
3. **Laufzeit**, vor jedem Kompilieren (`Factory::build_in`): bei aktiver Prüfung wird der
   SHA-256 des Inhalts **neu berechnet** und die Signatur erneut geprüft. Ein Modul, das an
   der Admin-API vorbei in Datenbank oder Snapshot-Cache gelangt ist (oder dessen Schlüssel
   inzwischen entfernt wurde), macht die Bindung ungültig (Log, `failMode`; Standard
   `closed` → `500 plugin_config_invalid`). Metrik
   `aigw_wasm_signature_rejected_total{reason}` (`missing`, `unknown-key`, `invalid`,
   `digest`). Der Fingerabdruck der Bindung enthält die Signatur: neu signieren baut neu.

**Anzeige.** `GET /wasm-modules` und `GET /wasm-modules/{name}` liefern je Modul
`signature: {keyId?, status, loadable}` mit `status` = `valid` | `invalid` | `unknown-key` |
`unsigned` | `unchecked` (gegen die aktuelle Richtlinie), die Liste zusätzlich
`signatureRequired` und `trustedKeys` (IDs). Die UI zeigt das als Spalte „Signatur“
(„wird nicht geladen“, wenn `loadable: false`).

**Werkzeuge.**

```sh
aigw wasm keygen --out release.pem            # privat (PKCS#8-PEM, 0600); öffentlich → stdout
aigw wasm pubkey --key release.pem            # öffentlichen Schlüssel erneut ausgeben
aigw wasm sign --key release.pem --key-id release-2026 keyword-guard.wasm            # Base64
aigw wasm sign … --format query               # sha256=…&keyId=…&signature=… (für curl)
aigw wasm sign … --format json                # {sha256, keyId, signature}
aigw wasm verify --public-key release.pub --key-id release-2026 --signature … modul.wasm
```

Ohne `aigw`, mit OpenSSL ≥ 3 (Schlüssel aus `openssl genpkey -algorithm ed25519` funktionieren
auch mit `aigw wasm sign`):

```sh
SHA=$(sha256sum modul.wasm | cut -d' ' -f1)
printf 'aigw-wasm-module-v1:%s' "$SHA" > msg
openssl pkeyutl -sign -inkey release.pem -rawin -in msg | base64 -w0
```

`scripts/build-wasm-plugins.sh` signiert die gebauten Module, wenn `AIGW_WASM_SIGN_KEY` (Pfad
zum privaten Schlüssel) und `AIGW_WASM_SIGN_KEY_ID` gesetzt sind (Dateien `*.wasm.sig`).

**Schlüsselwechsel.** (1) Neuen Schlüssel zusätzlich in `trusted_keys` eintragen, neu starten;
(2) alle Module mit dem neuen Schlüssel signieren und erneut hochladen (gleicher Inhalt +
neue Signatur = neue Version, `If-Match` wie beim Ersetzen); (3) alten Schlüssel entfernen.
Die Liste zeigt danach für noch alt signierte Module `unknown-key` / `loadable: false`.
Schlüssel stehen bewusst **nur in der Startkonfiguration** (nicht per Admin-API änderbar):
ein Admin-Konto allein kann sich so keinen eigenen Schlüssel freigeben.

## 3b. Sigstore – keyless (AIGW-106)

Ed25519-Schlüssel (§3a) muss jemand erzeugen, aufbewahren, verteilen und wechseln. Mit
**Sigstore** signiert der Build mit einem kurzlebigen Zertifikat (Fulcio, 10 Minuten gültig),
das an eine **OIDC-Identität** gebunden ist – typischerweise den Release-Workflow eines
Repositorys (GitHub Actions: `https://github.com/<org>/<repo>/.github/workflows/release.yml@refs/tags/v1.2.3`)
oder eine E-Mail-Adresse. Die Signatur steht im öffentlichen Transparenz-Log (Rekor). Das
Gateway vertraut damit nicht einem Schlüssel, sondern einer **Identität**: „dieses Modul hat der
Release-Workflow von `mupf-dev/plugins` gebaut“.

**Signieren** (im CI, z. B. GitHub Actions mit `id-token: write`):

```sh
cosign sign-blob --yes --bundle keyword-guard.wasm.sigstore.json keyword-guard.wasm
```

cosign ab v3 schreibt standardmäßig Rekor-v2-Einträge mit RFC-3161-Zeitstempel; die Prüfung
im Gateway kennt Rekor **v1** (siehe Grenzen). Bis Rekor v2 unterstützt ist, mit
`--use-signing-config=false --rekor-url https://rekor.sigstore.dev` signieren (bzw. cosign
v2.x).

**Konfiguration** (nur Startkonfiguration, wie `trusted_keys`):

```yaml
wasm:
  sigstore:
    trusted_root: /etc/aigw/sigstore/trusted_root.json
    identities:
      - issuer: 'https://token\.actions\.githubusercontent\.com'
        subject: 'https://github\.com/mupf-dev/plugins/\.github/workflows/release\.yml@refs/tags/v.*'
    require_inclusion_proof: true   # Standard
```

- `trusted_root`: das **Trust-Root** im Sigstore-Format (`trusted_root.json`: Fulcio-CAs,
  Rekor- und CT-Logs mit Gültigkeitszeiträumen). Für die öffentliche Instanz z. B. nach
  `cosign initialize` aus `~/.sigstore/root/tuf-repo-cdn.sigstore.dev/targets/trusted_root.json`
  (oder `sigstore-python`/`sigstore-go` exportieren); bei eigener Sigstore-Instanz deren Datei.
  Das Gateway liest die Datei **beim Start** und macht zur Laufzeit **keine Netzaufrufe** (kein
  TUF, kein Rekor-Abruf). Aktualisieren = Datei tauschen und neu starten (wie bei
  CA-Wechseln der öffentlichen Instanz, selten).
- `identities`: mindestens eine; `issuer` und `subject` sind reguläre Ausdrücke und werden
  **vollständig** verglichen (`^(?:…)$`; ein Teiltreffer reicht nie). Punkte maskieren!
- Signatur Pflicht: wie bei `trusted_keys` ist mit `sigstore` die Signatur standardmäßig
  Pflicht (`require_signature: false` nur für den Übergang). Es genügt **ein** Verfahren:
  Ed25519 **oder** Sigstore; ist beides angegeben, muss beides gültig sein.

**Übertragen**: Admin-API als JSON-Umschlag (`Content-Type: application/json`,
`{"content": "<Base64 der Komponente>", "sigstoreBundle": {…}}`; die übrigen Query-Parameter
wie gehabt), Startkonfiguration `wasm.modules[].sigstore_bundle: <Pfad>`, UI-Upload-Dialog Feld
„Sigstore-Bundle“. Gespeichert wird das Bundle in `wasm_module.sigstore_bundle` (Migration
0014), im Snapshot und im Snapshot-Cache.

```sh
jq -n --rawfile c <(base64 -w0 keyword-guard.wasm) --slurpfile b keyword-guard.wasm.sigstore.json \
  '{content: $c, sigstoreBundle: $b[0]}' |
curl -X PUT -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' --data-binary @- \
  "$AIGW/admin/api/v1/wasm-modules/keyword-guard?sha256=$(sha256sum keyword-guard.wasm | cut -d' ' -f1)"
aigw wasm verify-sigstore --bundle keyword-guard.wasm.sigstore.json --trusted-root trusted_root.json \
  --issuer 'https://token\.actions\.githubusercontent\.com' --subject 'https://github\.com/mupf-dev/.*' keyword-guard.wasm
```

**Prüfung** (Crate `aigw-sigstore`, offline, dieselbe an allen Prüfpunkten aus §3a – Upload,
Startkonfiguration, vor jedem Kompilieren, Freigabe):

1. Bundle v0.1–v0.3 mit `messageSignature` (wie `cosign sign-blob`); `messageDigest` muss zum
   Modul passen.
2. **Signatur** des Moduls (ECDSA P-256 über SHA-256) mit dem Schlüssel des Zertifikats. Geprüft
   wird über den SHA-256 – zur Laufzeit wird er zuvor aus dem Inhalt neu berechnet (§3a).
3. **Zertifikatskette** gegen die Fulcio-CAs des Trust-Roots (rustls-webpki): Signaturen,
   Gültigkeit **zum Zeitpunkt des Log-Eintrags**, EKU `codeSigning`, kein CA-Zertifikat als
   Blatt, keine Wurzel im Bundle.
4. **SCT** (Certificate Transparency): mindestens ein gültiger eingebetteter SCT eines CT-Logs
   aus dem Trust-Root (sofern es CT-Logs enthält).
5. **Rekor-Eintrag** (`hashedrekord` 0.0.1): Log aus dem Trust-Root, Schlüssel zum Zeitpunkt
   gültig, **SET** (signierter Zeitstempel; Pflicht) und **Inclusion-Proof** mit signiertem
   Checkpoint (Standard Pflicht; `require_inclusion_proof: false` lässt ältere v0.1-Bundles nur
   mit SET zu – wie `cosign verify-blob --offline`). Der Eintrag muss genau Digest, Signatur
   und Zertifikat des Bundles enthalten; der Zeitpunkt darf nicht in der Zukunft liegen.
6. **Identität**: OIDC-Issuer (Fulcio-Erweiterung 1.3.6.1.4.1.57264.1.8 bzw. .1.1) und Subjekt
   (SAN: URI, E-Mail, Nutzername) gegen `identities`.

Ergebnis in Liste/Einzelabfrage: `signature.sigstore = {status: valid|invalid|identity,
subject, issuer, integratedTime, logIndex, message}`; Gesamtstatus `signature.status` (neu:
`identity` = gültig signiert, aber keine erlaubte Identität). Ablehnungen zählen in
`aigw_wasm_signature_rejected_total{reason="sigstore"|"identity"}`.

**Warum eine eigene Prüfung statt des Crates `sigstore`** (sigstore-rs, Apache-2.0): Die Lizenz
passt, aber das Crate ist nicht schlank – es bringt rund 30 zusätzliche Crates mit
(RustCrypto-Stapel `p256`/`p384`/`ecdsa`/`ed25519-dalek`/`rsa`/`x509-cert`/`scrypt`/
`crypto_secretbox`/`chrono`, Tokio-Werkzeuge; Bundle-Prüfung je nach Version mit `tough`/TUF
und HTTP-Client), also einen **zweiten Krypto-Provider** neben aws-lc-rs, und `rsa` trägt den
offenen Hinweis RUSTSEC-2023-0071 (Marvin), den cargo-deny meldet. Gebraucht wird hier nur
ein kleiner, fester Ausschnitt (Bundle lesen, Kette, SCT, SET, Merkle-Proof, Checkpoint) – das
leisten aws-lc-rs und rustls-webpki, die ohnehin im Baum sind, mit ≈ 1000 Zeilen eigenem Code
(`crates/aigw-sigstore`). Die Prüfung ist gegen die **Sigstore-Konformitätssuite** getestet
(echte Bundles der öffentlichen Instanz, 5 gültige und 23 ungültige Fälle, ohne Netz).

**Grenzen**: keine DSSE-Umschläge/Attestierungen (nur `messageSignature`), kein Rekor v2
(`hashedrekord` 0.0.2) und keine RFC-3161-Zeitstempel (TSA), keine Bundles mit festem Schlüssel
statt Zertifikat, Zertifikatsschlüssel nur ECDSA P-256 (Fulcio-Standard). Folgeticket: Rekor
v2 + TSA.

## 3c. Vier-Augen-Freigabe (AIGW-106)

Signaturen belegen die Herkunft, aber nicht, dass ein Mensch die Version für dieses Gateway
gewollt hat – und ein einzelnes übernommenes Admin-Konto kann ein (korrekt signiertes, aber
unerwünschtes) Modul sofort aktivieren. Mit der **Vier-Augen-Freigabe** wird ein neues Modul
bzw. eine neue Version erst aktiv, wenn eine **zweite berechtigte Person** freigibt.

| Einstellung | Wirkung |
|---|---|
| `wasm.four_eyes: null` (Standard) | nach Profil: `wasm.profile: strict` → an, `standard` → aus |
| `wasm.four_eyes: true` / `false` | ausdrücklich an / aus |
| `wasm.profile: strict` („Streng“) | Vier-Augen-Freigabe an (sofern nicht `four_eyes: false`), Signatur Pflicht: ohne `trusted_keys` und ohne `sigstore` startet das Gateway nicht, `require_signature: false` ist verboten |

**Ablauf (Admin-API, UI):**

1. `PUT /wasm-modules/{name}` (wie bisher, inkl. Signaturprüfung und `If-Match` gegen die
   aktive Version) **reicht ein**: Antwort `202` mit der Einreichung
   (`PendingWasmModule`: SHA-256, Hochlader, Signaturstatus, `replaces`, `canApprove`). Die
   aktive Version und der Snapshot bleiben unverändert. Höchstens eine Einreichung je Name;
   eine neue ersetzt die alte. Gleich der aktiven Version → `200`, nichts eingereicht.
2. Prüfen: `GET /wasm-modules` (Feld `pending`), `GET /wasm-modules/{name}` (Feld `pending`),
   `GET /wasm-modules/{name}/pending/content` (Download zum Prüfen).
3. `POST /wasm-modules/{name}/approve?sha256=<geprüfte Version>` → aktiv (`200`, Hot Reload).
   `sha256` ist Pflicht: freigegeben wird genau die geprüfte Version (sonst `409`). Die
   Signaturen werden dabei gegen die **aktuelle** Richtlinie erneut geprüft (`422`).
4. `DELETE /wasm-modules/{name}/pending` verwirft (jeder globale Admin, auch der Hochlader).

**Wer darf freigeben (RBAC):** nur ein globaler `admin` (wie Hochladen), der **nicht** der
Hochlader ist (`403`). Verglichen wird die **Person**: Ein Admin-API-Key zählt als der Nutzer,
der ihn angelegt hat (rekursiv), damit niemand mit Session und eigenem Key zwei Personen
spielt; die Person des Hochladers wird beim Einreichen festgehalten (ein später gelöschter Key
ändert nichts). Keys, die der Bootstrap-Admin angelegt hat, gelten als eigene Personen – sie
werden in der Einrichtung an verschiedene Halter ausgegeben, und der Bootstrap-Zugang endet
(`bootstrap: auto`) mit dem ersten Admin-Key. Die UI zeigt Einreichungen unter „Wartet auf
Freigabe“ mit Button **Freigeben** (nur wenn erlaubt, sonst Hinweis) und **Verwerfen**.

**Audit:** Einreichen/Freigeben/Verwerfen schreiben Einträge der Art `wasm-module-approval`
(Aktionen `submit`, `approve`, `reject`; mit Hochlader, SHA-256, Signaturen – ohne Inhalt).
Die Freigabe schreibt zusätzlich den üblichen `wasm-module`-Eintrag (Journal des Snapshots) mit
Akteur = Freigebender; das Modul trägt danach `approval: {uploadedBy, approvedBy, approvedAt}`.

**Bindungen:** Die Admin-API lehnt Bindungen auf ein nur eingereichtes Modul bzw. eine
eingereichte, noch nicht freigegebene `sha256` ab (`422` „wartet auf Freigabe“), damit eine
Bindung mit `failMode: closed` nicht bis zur Freigabe Anfragen sperrt.

**GitOps / Startkonfiguration:** Module sind nicht Teil des YAML; `config:apply` ändert an
Modulen nichts. Bindungen auf ein (noch) nicht freigegebenes Modul sind wie bisher beim Apply
erlaubt und bis zur Freigabe ungültig (Log, `failMode`). Module aus `wasm.modules` der
Startkonfiguration sind **ohne** Vier-Augen-Freigabe aktiv: Wer die Startkonfiguration ändert,
hat ohnehin Betreiberrechte – das zweite Augenpaar ist dort das Review der Konfiguration (Pull
Request). Im Modus „nur YAML“ gibt es keine Uploads und damit keine Einreichungen.

**Speicherung:** Tabelle `wasm_module_pending` (Migration 0014, beide Dialekte) – nicht Teil
des Snapshots; Spalten `uploaded_by`, `approved_by`, `approved_at` an `wasm_module`.

## 4. Bindung

```yaml
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: team-keywords}
spec:
  plugin: wasm
  scope: {level: team, target: research}
  failMode: closed          # Standard
  timeoutMs: 50             # Zeitgrenze je Aufruf (Standard 100 ms)
  config:
    module: keyword-guard
    sha256: 9c1e…           # optional: nur genau diese Version
    config: {block: [geheimprojekt], mask: [intern], responseHeaders: {x-guard: checked}}
    phases: [request]       # optional: Schnittmenge mit den Phasen aus describe
    includeHeaders: [x-client-app]
    fuel: 50000000
    maxMemoryMb: 16
```

Antwort-Phase ohne Stream-Phase → gestreamte Anfragen werden gepuffert
(`needs_buffered_output`, wie beim Webhook); meldet das Modul auch `chunk`, läuft es live auf
jedem Stream-Ereignis.

## 5. Kompilieren, Cache, Hot Reload

- **Eine Engine je Prozess** (Cranelift, `OptLevel::Speed`, Component Model, Fuel,
  Epoch-Interrupt), dazu ein Linker (WASI + Host) und ein **Cache kompilierter Komponenten nach
  SHA-256** (`wasm.compiled_cache_entries`, LRU). Mehrere Bindungen desselben Moduls teilen
  die Kompilierung; `InstancePre` ist vorbereitet (Importe aufgelöst), eine Instanz je Aufruf
  kostet mit Pooling-Allocator wenige Mikrosekunden.
- **Hot Reload über den Snapshot**: Upload/Löschen erhöht `config_version`, der Reloader baut
  den Snapshot (inkrementell: nur geänderte Module), die Plugin-Ketten bauen neu. Bindungen
  werden wiederverwendet, wenn Konfiguration **und** Fingerabdruck gleich sind – der
  Fingerabdruck der Plugin-Art `wasm` ist der SHA-256 des Moduls (`PluginFactory::fingerprint`).
  Ein neues Modul unter gleichem Namen baut die Bindungen also neu; laufende Aufrufe beenden
  mit der alten Instanz.
- **Snapshot-Cache auf Platte** (F14) enthält die Module (Base64, verschlüsselt): ein Kaltstart
  ohne Datenbank kann WASM-Plugins weiter ausführen.
- Pooling-Allocator reserviert Adressraum je Slot. Kann das System ihn nicht reservieren
  (z. B. `vm.overcommit_memory=2`), fällt die Laufzeit mit Warnung auf Instanzen bei Bedarf
  zurück (`wasm.pooling: false` erzwingt das).

## 6. Metriken

| Metrik | Labels |
|---|---|
| `aigw_wasm_calls_total` | `module`, `phase`, `outcome` (`pass`, `modify`, `block`, `reject`, `respond`, `drop`, `error`) |
| `aigw_wasm_errors_total` | `module`, `kind` |
| `aigw_wasm_call_duration_seconds` (Histogramm) | `module`, `phase` |
| `aigw_wasm_fuel_consumed_total` | `module` |
| `aigw_wasm_compile_total` | `outcome` (`ok`, `error`, `cached`) |
| `aigw_wasm_signature_rejected_total` | `reason` (`missing`, `unknown-key`, `invalid`, `digest`; AIGW-101; `sigstore`, `identity`; AIGW-106) |

Dazu wie bei allen Plugins `aigw_plugin_failmode_triggered_total{plugin="wasm"}`.

## 7. Beispiel und Werkzeuge

- [`plugins/keyword-guard`](../../plugins/keyword-guard): Stichwort-Blocker, Maskierung und
  Header-Setzer in Rust (`wit-bindgen`, Ziel `wasm32-wasip2`, ≈ 100 KiB).
- [`plugins/testkit`](../../plugins/testkit): Testmodi (Endlosschleife, Speicher, Trap …) für
  die Tests und die Latenzmessung.
- `scripts/build-wasm-plugins.sh` baut beide und legt sie unter `crates/aigw-wasm/tests/data/`
  ab (eingecheckt, damit `cargo test` kein WASM-Ziel braucht).
- Admin-UI: Plugins → „WASM-Module“ (Liste, Hochladen mit SHA-256-Berechnung im Browser,
  Löschen); im Bindungsformular die Plugin-Art `wasm` mit Modulauswahl.

## 8. Build-Feature `wasm`

wasmtime (mit Cranelift und WASI) vergrößert Binary und Bauzeit deutlich (Messwerte §9). Das
Feature `wasm` des Crates `aigw` ist **standardmäßig an**; `cargo build --no-default-features
--features mimalloc` baut ohne. Dann bleibt die Plugin-Art `wasm` prüfbar (Admin-API, GitOps),
Aufrufe schlagen aber fehl (→ `failMode`). `wasm.enabled: false` schaltet die Laufzeit zur
Laufzeit ab (gleiche Wirkung).

## 9. Messwerte

Siehe [messwerte/wasm-plugins.md](messwerte/wasm-plugins.md) (Latenz Leer-Plugin, Binärgröße,
Bauzeit).

## 10. Offen / später

- Sigstore: Rekor v2 (`hashedrekord` 0.0.2) mit RFC-3161-Zeitstempeln, DSSE/Attestierungen
  (§3b, Grenzen).
- Weitere Host-Funktionen (z. B. Zähler, Modellaufrufe über `PluginHost`) – bewusst nicht im
  ersten Schnitt, damit Module keine Seiteneffekte haben.
- Wiederverwendung von Instanzen je Anfrage (Zustand zwischen Phasen ohne `meta`).
- Asynchrone Ausführung (`call_async` mit Fuel-Yield) statt `spawn_blocking`.
