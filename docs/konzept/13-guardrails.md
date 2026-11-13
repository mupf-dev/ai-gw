# ai-gw – Guardrails: PII, Prompt-Filter, externe Inhaltsprüfung (AIGW-37/38)

Stand: 2026-11-13 · Status: umgesetzt (Entwurf zur Durchsicht),
Grundlage: [01-architektur.md](01-architektur.md) §9, [12-plugins.md](12-plugins.md),
[07-beobachtung.md](07-beobachtung.md) §5 · Code: `crates/aigw-pii`,
`crates/aigw-plugins/src/builtin/{pii,prompt_filter,content_safety}.rs`,
`crates/aigw-proxy/src/guard_host.rs`

## 1. Überblick

| Plugin | Phase | Zweck |
|---|---|---|
| `pii` | Anfrage, Antwort, Stream | personenbezogene Daten/Geheimnisse erkennen; maskieren, entfernen, ablehnen, pseudonymisieren (mit Rückübersetzung) oder nur zählen |
| `prompt-filter` | Anfrage | Wort-/Phrasenlisten, Regex-Regeln, Prompt-Injection-Heuristik, Längen-/Anzahl-Grenzen |
| `content-safety` | Anfrage, Antwort (gepuffert), Stream (satzweise) | externe Prüfung: Azure AI Content Safety, OpenAI Moderation, Prüfmodell (Llama Guard o. ä.) über eine Modellgruppe des Gateways |

Alle drei sind normale Plugins der Kette (Bindungen, Vererbung, `order`, `failMode`,
`timeoutMs` wie in [12-plugins.md](12-plugins.md) §4). Typische Reihenfolge über `order`:
`prompt-filter` (billig, lehnt früh ab) → `pii` (maskiert) → `content-safety` (externer Aufruf
auf dem bereits maskierten Text).

Die Log-Maskierung (`observe.payload: masked`) nutzt **dieselbe Engine** (§6.3).

## 2. PII-Engine `aigw-pii`

### 2.1 Eigene Crate statt Modul

- Zwei Nutzer mit unterschiedlichen Abhängigkeiten: `aigw-plugins` (Hot Path) und
  `aigw-observe` (Writer, hängt an `aigw-store`/sqlx). Ein Modul in einer der beiden würde die
  andere Crate an deren Abhängigkeiten koppeln.
- Keine Abhängigkeiten (auch kein `regex`): ein Durchlauf über die Bytes, Erkenner nur an
  Kandidatenstellen; isoliert test- und messbar (`cargo bench -p aigw-pii`).
- Später auch für MCP-Argumente (AIGW-41) und Cache-Schlüssel nutzbar, ohne Plugin-Kette.

### 2.2 Kategorien

| Kategorie (`Category`) | Name in Konfiguration/Metrik | Platzhalter | Erkennung und Schutz vor Falsch-Positiven |
|---|---|---|---|
| E-Mail | `email` | `EMAIL` | Local-Part (ASCII + internationalisiert) `@` ≥ 2 Domain-Labels, TLD 2–24 Buchstaben; `scheme://user@host` ist keine E-Mail |
| Telefon | `phone` | `PHONE` | international `+`/`00` + 8–15 Ziffern; deutsch national `0[1-9]…` 9–13 Ziffern, Vorwahl 3–6 Stellen bzw. zusammenhängend 10–12; Trenner Leerzeichen, `-`, `/`, Klammern (`+49 (0)30 …`); keine Punkte (Daten), keine Dezimalzahlen |
| IBAN | `iban` | `IBAN` | Länderlänge (ISO 13616, 75 Länder) + mod 97; Vierergruppen mit Leerzeichen |
| Kreditkarte | `credit_card` | `CARD` | 13–19 Ziffern (Gruppen mit Leerzeichen/`-`), BIN-Bereiche (Visa, Mastercard inkl. 2-Serie, Amex, Discover, Diners, JCB, UnionPay, Maestro, Mir) + Luhn; Schnitt nur an Gruppengrenzen (`4111 … 1111 22` → `[CARD] 22`) |
| Steuer-ID | `tax_id_de` | `TAX_ID` | 11 Ziffern, erste ≠ 0, genau eine Ziffer 2-/3-mal (nicht dreimal hintereinander), Prüfziffer ISO 7064 MOD 11,10 |
| Sozialversicherungsnummer | `social_security_de` | `SVNR` | `BB TTMMJJ A SS P`, plausibles Datum, Prüfziffer (Gewichte 2 1 2 5 7 1 2 1 2 1 2 1) |
| Personalausweis | `id_card_de` | `ID_CARD` | 10 Zeichen, Seriennummern-Alphabet (Ziffern + `CFGHJKLMNPRTVWXYZ`), ≥ 3 Ziffern, Prüfziffer ICAO 9303; erster Buchstabe `L M N P R T V W X Y` |
| Reisepass | `passport_de` | `PASSPORT` | wie Ausweis, erster Buchstabe `C F G H J K` |
| IP-Adresse | `ip_address` | `IP` | IPv4 (Oktette ≤ 255, keine Versionsketten `1.2.3.4.5`); IPv6 per `Ipv6Addr`-Parser, ≥ 3 Gruppen (oder 2 mit Ziffer und 4er-Gruppe, z. B. `fe80::1`); keine MAC-Adressen/Uhrzeiten/`std::`-Pfade |
| URL-Zugangsdaten | `url_credentials` | `URL_CREDENTIALS` | `scheme://user:pass@host` – ersetzt wird nur `user:pass` |
| API-Key/Token | `api_key` | `API_KEY` | Präfixe `sk-`, `sk-proj-`, `sk-ant-`, `sk_live_`/`rk_live_`/`…_test_`, `whsec_`, `ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`, `github_pat_`, `xoxb-`/`xoxp-`/`xoxa-`/`xoxe-`/`xoxs-`, `xapp-`, `glpat-`, `hf_`, `npm_`, `pypi-`, `AKIA`/`ASIA` (20 Zeichen), `AIza` (39), JWT (`eyJ…`, drei Teile); Mindestlängen, ≥ 2 Ziffern und Buchstaben im Rumpf |
| (Entropie-Heuristik) | `api_key` | `API_KEY` | Token aus `[A-Za-z0-9_+/-]` (+ `=`-Auffüllung) mit 32–256 Zeichen, Groß- **und** Kleinbuchstaben je ≥ 25 % der Buchstaben, ≥ 2 Ziffernfolgen, Shannon-Entropie ≥ 4,0 Bit/Zeichen. Schließt Hex-Hashes, UUIDs, CamelCase-Bezeichner und Pfade aus |

**Nicht erkannt: Namen und Postadressen.** Ohne NER-Modell sind sie zu fehleranfällig
(„Frank“ als Name vs. Adjektiv, Straßennamen in Prosa). Erweiterungspunkt: externer
NER-/PII-Dienst über `content-safety` mit `provider: model` (ein Modell, das
`safe`/`unsafe` + Kategorien antwortet, z. B. `unsafe\nname,address`) oder über das
Webhook-Plugin (AIGW-61), das maskierten Text zurückgeben kann. Ein eigener Anbieter-Typ
„PII-Dienst“ (z. B. Azure AI Language PII, Presidio) ließe sich in `content-safety` analog
zu Azure/OpenAI ergänzen.

Bekannte Grenzen: Versionsnummern mit vier Teilen (`1.2.3.4`) gelten als IPv4; zufällige
10-stellige Codes mit passender ICAO-Prüfziffer (≈ 10 %) als Ausweis/Pass; deutsche
Festnetznummern ohne Trenner und mit 9 Ziffern werden nicht erkannt. Die Allowlist (§2.3)
nimmt bekannte Werte aus.

### 2.3 Schnittstelle

```rust
let engine = Engine::new(EngineConfig {
    categories: CategorySet::all(),                 // oder Auswahl
    allow: vec!["support@acme.example".into(), "*@example.com".into()],
    entropy: Some(EntropyConfig::default()),        // None = nur bekannte Präfixe/JWT
});
engine.scan(text, &mut findings);                    // Vec<Finding { category, start, end }>, aufsteigend, disjunkt
engine.replace(text, |finding, value, out| …);       // Cow::Borrowed ohne Fund
engine.mask_labels(text);                            // „[EMAIL]“, „[CARD]“ … (Log-Maskierung)
```

Allowlist: Vergleich ohne Groß-/Kleinschreibung und ohne Leerzeichen, `-`, `/`, Klammern
(`+49 30 1234567` = `+49-30-1234567`); führendes `*` vergleicht das Ende.

Rohe JSON-Bodies (Log-Maskierung) werden verstanden: `\n`, `\t`, `\r` vor einem Wort gelten
als Trenner (`"\n0171 1234567"` → `"\n[PHONE]"`).

## 3. Plugin `pii`

```yaml
apiVersion: aigw/v1alpha1
kind: PluginBinding
metadata: {name: pii-default}
spec:
  scope: {level: global}
  plugin: pii
  order: 20
  config:
    action: mask                 # Standard je Kategorie: mask | redact | reject | pseudonymize | log-only
    direction: both              # input | output | both (Standard: both)
    categories:                  # ohne Angabe: alle Kategorien mit `action`/`direction`
      email: pseudonymize
      phone: pseudonymize
      creditCard: reject
      iban: {action: redact, direction: input}
      apiKey: {action: mask, direction: both}
      ipAddress: log-only
    allow: ["support@acme.example", "*@example.com"]
    entropy: {enabled: true, minLength: 32, minBits: 4.0}
    streamWindowBytes: 128       # 16–4096
    outputRejectMode: finish     # Kettensteuerung: finish (Standard) | error, siehe 12-plugins.md §3a
```

- **Entropie-Heuristik (F164):** standardmäßig an (Kategorie `apiKey`, Token ohne bekanntes
  Präfix). Je Bindung abschaltbar mit `entropy: {enabled: false}` – bekannte Präfixe
  (`sk-…`, `ghp_…`, `AKIA…`, JWT …) werden weiter erkannt. Sinnvoll z. B. für Teams, deren
  Prompts viele zufällige IDs oder Hashes enthalten. Test
  `pii::tests::entropy_heuristic_can_be_disabled_per_binding`.

Kategorienamen: `email`, `phone`, `iban`, `creditCard`, `taxIdDe`, `socialSecurityDe`,
`idCardDe`, `passportDe`, `ipAddress`, `urlCredentials`, `apiKey` (auch `snake_case`).

| Aktion | Eingabe | Ausgabe |
|---|---|---|
| `mask` | `[EMAIL_1]`, je Anfrage und Wert stabil nummeriert | ebenso (Nummerierung fortgesetzt) |
| `redact` | `[REDACTED]` | ebenso |
| `reject` | `400 invalid_request_error`, Code `pii_detected`, Meldung nennt die Kategorie, **nie** den Wert | Standard (`outputRejectMode: finish`, F160): `200` mit entferntem Inhalt und `finish_reason: content_filter` bzw. `stop_reason: refusal`, Header `x-aigw-guardrail: pii`; Live-Stream: sauberes Ende mit `content_filter`/`refusal`. Mit `outputRejectMode: error`: `400 pii_detected` bzw. Fehlerereignis. Nichts von der Fundstelle wurde gesendet |
| `pseudonymize` | `[EMAIL_1]`, Zuordnung für die Antwort gemerkt | Platzhalter werden durch die Originale ersetzt (JSON, gepuffert, Live-Stream); neue Werte des Modells bleiben unverändert |
| `log-only` | unverändert, nur gezählt | ebenso |

- **Richtung:** je Plugin und je Kategorie überschreibbar. `pseudonymize` braucht `input`
  (sonst Konfigurationsfehler); in der Ausgabe bedeutet es Rückübersetzung.
- **Geprüfte Stellen:** alle Nachrichten (auch System), Text, Ablehnungen, Tool-Argumente,
  Tool-Ergebnisse. **Reasoning bleibt unberührt** (Signaturen der Anbieter würden ungültig).
- **Zuordnung** Platzhalter → Original liegt nur im Speicher der Anfrage (`Meta`, Schlüssel
  `aigw.pii`) und wird nie geloggt. Schlüssel mit Präfix `aigw.` filtert
  `MetaExt::external_view` zentral heraus – sie gehen nie an externe Plugins (Webhook-Payload
  enthält nur die externe Sicht, Test `webhook::internal_meta_never_leaves_the_gateway`).
  Mehrere `pii`-Bindungen teilen sich Nummerierung und Zuordnung (keine doppelten Platzhalter).
- **Reihenfolge (Zwiebelprinzip, F167b):** Antwort- und Stream-Haken laufen in umgekehrter
  Kettenreihenfolge. `pii` mit niedriger `order` pseudonymisiert die Anfrage als erstes und
  übersetzt die Antwort als letztes zurück – Webhooks und Prüfdienste mit höherer `order` sehen
  in beiden Richtungen nur Platzhalter (Test `webhook::response_webhook_sees_only_pseudonyms`).
  Daher `pii` vor (niedrigere `order` als) externe Plugins legen.
- **Live-Stream:** Ein `[` am Ende des Puffers, dem bis zu 31 Zeichen `A–Z0–9_` folgen, wird
  zurückgehalten, bis klar ist, ob ein Platzhalter daraus wird – sonst keine Verzögerung.
  Mit Ausgabe-Erkennung bleiben zusätzlich die letzten `streamWindowBytes` zurück
  (Funde über Chunk-Grenzen); ausgegeben wird bis zum letzten Leerraum davor, nie mitten
  in einem Fund. Kosten: ≈ `streamWindowBytes` / 4 Tokens Verzögerung.
  `TextWindow` (Gleitfenster der Kette) genügt dafür nicht, weil es Text verwirft statt ihn
  zurückzuhalten; der Puffer liegt im Stream-Zustand des Plugins (`ChunkCtx::state`).
- Blockwechsel (Text → Tool-Aufruf), `Stop`, `Usage` und das Stream-Ende geben den Puffer
  vorher aus.

## 4. Plugin `prompt-filter`

```yaml
plugin: prompt-filter
config:
  action: reject                     # reject | flag (Standard je Regel)
  roles: [user, tool]                # geprüfte Rollen (Standard); system/developer/assistant möglich
  words:
    terms: ["Projekt Falke", "streng geheim"]
    caseSensitive: false
    wholeWords: true
    action: flag
  regex:
    - {name: ticket, pattern: "(?i)\\bINC-\\d{6}\\b", action: flag}
  injection:
    threshold: 1.0                   # Summe der Gewichte erkannter Signale
    action: reject
    signals:                         # zusätzliche Signale
      - {name: kunde, pattern: "(?i)admin-modus", weight: 1.0}
  limits: {maxMessages: 100, maxChars: 200000, maxMessageChars: 50000}
```

- **Wortlisten:** Aho-Corasick (`aho-corasick`, Leftmost-Longest), ein Durchlauf für alle
  Begriffe. Ohne `caseSensitive` vergleicht der Automat ASCII ohne Groß-/Kleinschreibung;
  für Umlaute werden zusätzlich klein, GROSS und Großer Anfang eingetragen. `wholeWords`
  prüft Unicode-Wortgrenzen.
- **Regex:** `regex`-Crate (linear, keine Rückverfolgung), Größenlimit 1 MiB je Muster.
- **Prompt-Injection-Heuristik:** eingebaute Signale (eine `RegexSet`, ein Durchlauf):

  | Signal | Gewicht | Beispiele |
  |---|---|---|
  | `ignore_instructions` (+ `_de`) | 1,0 | „ignore all previous instructions“, „ignoriere die vorherigen Anweisungen“ |
  | `reveal_system_prompt` (+ `_de`) | 0,8 | „print your system prompt“, „zeige den System-Prompt“ |
  | `persona_override` | 0,8 | „you are now DAN, unrestricted“, „du bist ab sofort ohne Regeln“ |
  | `chat_template_tokens` | 0,7 | `<\|im_start\|>`, `[INST]`, `<<SYS>>` |
  | `jailbreak_terms` | 0,6 | „do anything now“, „developer mode enabled“ |
  | `fake_role_header` | 0,4 | Zeilenanfang `system:` / `### assistant:` |
  | `new_instructions` | 0,4 | „new instructions:“ |
  | `no_restrictions` | 0,3 | „without restrictions“, „ohne Zensur“ |

  Jedes Signal zählt einmal je Anfrage. Die Heuristik ist ein **Signal**, kein Schutz:
  umformulierte Angriffe entgehen ihr. Für mehr Abdeckung `content-safety` mit einem
  Prüfmodell (z. B. Prompt Guard) kombinieren.
- **Grenzen:** Nachrichtenanzahl (alle Rollen), Zeichen gesamt und je Nachricht (geprüfte
  Rollen, Unicode-Zeichen).
- **Antwort:** `reject` → `400` (Eingabe-Ablehnung, Header `x-aigw-guardrail: prompt-filter`),
  Code `prompt_rejected`, Meldung nennt die Regel
  (`words`, `regex:<name>`, `injection`, `limits:<feld>`), nie die Fundstelle. `flag` → nur
  Zähler und Log-Zeile (Info, mit Request-ID und Regel).

## 5. Plugin `content-safety` (AIGW-38)

```yaml
plugin: content-safety
order: 30
config:
  provider: azure                   # azure | openai | model
  endpoint: https://acme.cognitiveservices.azure.com
  credential: azure-content-safety  # Name einer ProviderCredential (Feld apiKey bzw. api_key)
  apiVersion: "2024-09-01"          # azure
  thresholds: {hate: 2, violence: 4}
  categories: [hate, violence, sexual, self_harm]   # nur diese blockieren (Standard: alle)
  direction: input                  # input | output | both (Standard: input)
  outputMode: buffered              # buffered | sentence
  action: reject                    # reject | flag
  roles: [user]
  maxChars: 10000
  sentenceTimeoutMs: 2000
  sentenceOnError: block            # block | allow
  failMode: closed                  # Kettensteuerung: Fehler/Zeitlimit der Anfrage-/Antwortprüfung
  timeoutMs: 1500                   # Kettensteuerung (Standard dieses Plugins: 3000)
```

| Anbieter | Aufruf | Auswertung |
|---|---|---|
| `azure` | `POST {endpoint}/contentsafety/text:analyze?api-version=…`, Header `Ocp-Apim-Subscription-Key`, `outputType: FourSeverityLevels` | `categoriesAnalysis[].severity ≥ thresholds[kategorie]` (Standard 4 = mittel); Kategorien `hate`, `self_harm`, `sexual`, `violence` |
| `openai` | `POST {endpoint ?? https://api.openai.com}/v1/moderations`, Bearer, `model` (Standard `omni-moderation-latest`) | mit Schwelle: `category_scores ≥ Schwelle`, sonst `categories[k] == true`; Namen normalisiert (`self-harm/intent` → `self_harm_intent`) |
| `model` | Chat über die Modellgruppe `modelGroup` **des Gateways** (Routing, Retries, Fallbacks; ohne Plugin-Kette, ohne Limits des Clients), `max_tokens` 64, Temperatur 0 | erste Zeile `safe` / `unsafe`, Kategorien in derselben (`unsafe: S1`) oder nächsten Zeile (`S1,S10`), klein geschrieben; sonst Plugin-Fehler |

- **Prompt-Vorlage** (`promptTemplate`): Platzhalter `{{role}}` (`User` bzw. `Agent`) und
  `{{conversation}}` (Pflicht). Standard: Llama-Guard-3-Kategorien S1–S14.
- **Geprüfter Text:** Eingabe = Texte der Rollen `roles`, verbunden, die letzten
  `maxChars` Bytes; Ausgabe = Antworttext, ebenso begrenzt.
- **Zugangsdaten:** nie in der Plugin-Konfiguration (`deny_unknown_fields` weist z. B.
  `apiKey` ab), nur der Name einer `ProviderCredential`. Aufgelöst wird **zur Laufzeit** aus
  dem aktuellen Snapshot (`Snapshot::credential_data`), Rotation wirkt sofort.
- **SSRF:** `endpoint` wird beim Bau der Bindung mit `aigw_core::upstream::UpstreamPolicy`
  geprüft (nur `https`, keine privaten/lokalen Ziele, keine `user:pass@` – Ausnahmen über
  `admin.allow_insecure_upstreams` / `admin.allow_private_upstreams`), vor jedem Aufruf
  erneut und zusätzlich bei jedem Verbindungsaufbau nach der Namensauflösung (gemeinsamer
  Outbound-Baustein `aigw-outbound`, AIGW-72: geprüft wird genau die Adresse, mit der
  verbunden wird – auch bei DNS-Rebinding). Keine Weiterleitungen, Antwort höchstens 256 KiB.
- **Ausgabe:**
  - `buffered` (Standard): `needs_buffered_output` – gestreamte Anfragen werden gepuffert
    (`x-aigw-buffered: plugins`), Prüfung auf der ganzen Antwort; Ablehnung je
    `outputRejectMode` als `200` mit `content_filter`/`refusal` (Standard) oder als normale
    Fehlerantwort vor dem ersten Byte (`error`).
  - `sentence`: Live-Stream; Text wird bis zum Satzende (`.`, `!`, `?`, Zeilenumbruch +
    Leerraum, mindestens 40 Zeichen) zurückgehalten, dann im Hintergrund geprüft (Kontext:
    bisherige Ausgabe, höchstens `maxChars`). Freigegeben wird, sobald die Prüfung des
    Satzes bestanden ist; bei `unsafe` endet der Stream sauber mit `content_filter`/`refusal`
    (bzw. mit Fehlerereignis `content_filtered` bei `outputRejectMode: error`), nichts
    Ungeprüftes wurde gesendet. Am Stream-Ende wird synchron
    auf offene Prüfungen gewartet (`block_in_place`, nur auf der Mehrthread-Laufzeit).
    Fehler/Zeitlimit einer Satzprüfung: `sentenceOnError` (`block` = Stream endet mit
    `plugin_error`, `allow` = Satz wird freigegeben) – der `failMode` der Bindung lässt sich
    im synchronen Chunk-Pfad nicht sauber anwenden (zurückgehaltene Ereignisse).
- **Antwort:** `reject` auf der Eingabe → `400`, Code `content_filtered`, Meldung mit
  Kategorien (`hate, violence`); auf der Ausgabe je `outputRejectMode` `200` mit
  `content_filter`/`refusal` (Standard) oder `400` (F160, [12-plugins.md](12-plugins.md) §3a);
  Header `x-aigw-guardrail: content-safety`. `flag` → nur Zähler und Log-Zeile.
- **Fehler** (Dienst nicht erreichbar, HTTP ≠ 2xx, Antwort nicht auswertbar, Zugangsdaten
  fehlen, Zeitlimit): Plugin-Fehler → `failMode` der Bindung (`closed`: `500 plugin_error`,
  `open`: Warnung, Anfrage läuft weiter). Standard ist `closed` (F148); jedes Greifen zählt
  `aigw_plugin_failmode_triggered_total{plugin="content-safety",mode}`.

### 5.1 Gateway-Host (`PluginHost`)

Plugins bekommen keinen eigenen HTTP-Client und keinen Zugriff auf den Snapshot. Die Kette
reicht über `Registry::with_builtin_host(host)` einen `PluginHost` an die Fabrik von
`content-safety`:

```rust
pub trait PluginHost: Send + Sync + Debug + 'static {
    fn credential(&self, name: &str) -> Option<Arc<CredentialData>>;   // aktueller Snapshot
    fn check_endpoint(&self, url: &str) -> Result<(), String>;          // SSRF beim Bau
    fn post_json(&self, call: HttpCall) -> BoxFuture<'static, Result<HttpReply, PluginError>>;
    fn chat(&self, model_group: &str, req: ChatRequest) -> BoxFuture<'static, Result<ChatResponse, PluginError>>;
}
```

`aigw_proxy::GatewayHost` implementiert ihn (eigener `UpstreamClient`-Pool, Antworten
≤ 256 KiB, Routing über `exec::execute` mit Deployment-Kapazität). `Registry::with_builtin()`
nutzt `NoHost` (Konfiguration prüfbar mit strengen SSRF-Regeln, Aufrufe schlagen fehl).
Das Webhook-Plugin (AIGW-61) kann denselben Host nutzen.

## 6. Beobachtung

### 6.1 Zähler im Request-Record

`CallInfo::guardrails` (`aigw_core::observe::GuardrailHits`, geteilt mit dem Recorder) sammelt
je Anfrage `GuardrailHit { plugin, category, action, count }` – **ohne Inhalte**. Der Recorder
übernimmt sie beim Abschluss in `RequestRecord::guardrails` (auch für Stream-Phasen und
Ablehnungen). Ohne Plugins wird nichts angelegt.

Persistenz im Request-Log (`request_log`) ist noch offen (eigene Spalte bzw. Migration, siehe
§9).

### 6.2 Metrik und Trace

- `aigw_guardrail_hits_total{plugin,category,action}` (Zähler, `/metrics` der Steuerebene);
  höchstens 500 Serien, darüber Kategorie `other` (Kategorien externer Dienste sind offen).
- Wurzel-Span: Attribut `aigw.guardrail_hits` (Summe).

### 6.3 Log-Maskierung

`aigw_observe::PiiMasker` (Standard-Masker des `ObserveBuilder`) nutzt `aigw_pii::Engine` mit
allen Kategorien und ersetzt Funde durch `[LABEL]` (`[EMAIL]`, `[CARD]`, `[IBAN]`, `[PHONE]`,
`[API_KEY]` …). Er ersetzt den bisherigen `RegexMasker` (E-Mail/Karte/IBAN). Entscheidung des
Users: Falsch-Positive der breiteren Erkennung im Log werden in Kauf genommen. Kürzen ohne
Lecks wie bisher ([07-beobachtung.md](07-beobachtung.md) §5).

## 7. Messwerte

`cargo bench -p aigw-pii --bench scan` (divan, Release, Buildbox AMD EPYC, geteilt, 1 Thread,
2026-11-13, Median). Prompt: deutsch-englische Prosa mit Zahlen, Daten, Code-Schnipsel und
etwa einem Fund je KB (E-Mail, Telefon, IBAN, Karte, IP, Token, Steuer-ID, Ausweis).

| Benchmark | Größe | Median | Durchsatz |
|---|---|---|---|
| `scan` (Prompt mit Funden) | 4 KB | 23 µs | ≈ 178 MB/s |
| `scan` | 64 KB | 378 µs | ≈ 173 MB/s |
| `scan_clean_prose` (ohne Funde) | 4 KB | 18 µs | ≈ 224 MB/s |
| `scan_clean_prose` | 64 KB | 298 µs | ≈ 220 MB/s |
| `mask_labels` (Erkennen + Ersetzen) | 4 KB | 23 µs | |

Ziel (4 KB < 50 µs) erreicht. Keine Allokation ohne Fund (`Vec` des Aufrufers, `Cow`).
Ausreißer bei 64 KB (bis 3 ms) stammen von paralleler Last auf der geteilten Box. Das Plugin
`pii` kostet zusätzlich das Durchlaufen der IR-Nachrichten und bei Funden das Neuaufbauen der
betroffenen Texte.

## 8. Tests

- `aigw-pii`: positive und negative Korpora je Kategorie (inkl. Daten, Uhrzeiten,
  Versionsnummern, Zeitstempel, Hashes, UUIDs, Pfade, CamelCase, Prosa, Code),
  Prüfziffern, Allowlist, Ersetzen, Nicht-ASCII.
- `aigw-plugins` (Einheiten): `pii` – Konfiguration, alle Modi, Tool-Argumente, Allowlist,
  Pseudonymisierung JSON und Stream über Chunk-Grenzen (Text und Tool-Argumente), Ausgabe-
  Erkennung über Chunk-Grenzen, Ablehnung im Stream; `prompt-filter` – Wörter, Regex,
  Grenzen, Injection (Angriffe/harmlose Texte), reject/flag; `content-safety` –
  Kategorienamen, Antworten des Prüfmodells, Satzgrenzen, Konfiguration inkl. SSRF.
- `aigw-proxy/tests/guardrails.rs` (Hot Path): `pii` in allen Modi in beiden
  Eingangsformaten, JSON und Stream (Anbieter streamt in 3-Byte-Stücken), Zähler im
  `RequestRecord`, Ausgabe-Maskierung im Live-Stream, Ausgabe-Ablehnung mit
  `content_filter`/`refusal` (JSON, gepuffert, Live-Stream) und mit `outputRejectMode: error`; `prompt-filter` beide Formate + flag;
  `content-safety` gegen lokale Fake-Dienste (Azure mit Schwellen, OpenAI mit/ohne
  Score-Schwelle, Prüfmodell über eine Modellgruppe, gepufferte Ausgabe, Satzmodus im
  Live-Stream, Zeitlimit mit fail-open/closed, fehlende Zugangsdaten, SSRF auch nach
  Namensauflösung).
- `aigw-observe`: Masker-Einheiten, JSON-Lines-Export mit mehreren Kategorien, Metrik.

## 9. Offene Fragen

Entschieden und umgesetzt (AIGW-69): Ausgabe-Ablehnung mit `content_filter`/`refusal` statt
400 (F160), Entropie-Heuristik an und je Bindung abschaltbar (F164), Prüfung der Bindungen in
Admin-API und GitOps (F165), Zwiebelprinzip (F167b), Fehlermodus je Plugin-Art (F148).
Offen: Persistenz der Zähler im Request-Log (F161, Folgeticket); Satzmodus mit
`sentenceOnError` statt `failMode` (F163, vorerst so).
