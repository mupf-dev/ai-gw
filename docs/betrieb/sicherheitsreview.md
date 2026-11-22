# Sicherheitsreview ai-gw (AIGW-58)

Stand: 2026-11-13, Basis `main` f4de6bd, Branch `p7-58-security`; Status nachgeführt bis
AIGW-94 (2026-11-14). Defensives Review des
eigenen Codes; Befunde mit Schweregrad wurden – soweit ohne Kollision mit parallelen Paketen
möglich – direkt behoben und mit Regressionstests abgesichert. Betriebshinweise:
[haertung.md](haertung.md).

## 1 Umfang

| Bereich | Geprüft |
|---|---|
| API-Keys | Erzeugung/Entropie, Hashing, Lookup/Timing, Zustände active/suspended/revoked/abgelaufen, Admin-API-Keys, Leaks in Logs/Fehlern/Audit/Export |
| Secrets | Master-Key/Keyring (AES-256-GCM, Nonce, AAD), `aigw keygen`, Secret-Ressource, `provider_credential`, `SecretString`, GitOps-Export, Snapshot-Cache, Bootstrap-Token, Zeroize |
| SSRF | `aigw-outbound` (DNS-Auflösung, Rebinding, IPv6/IPv4-mapped, Redirects, Limits, TLS), statische Prüfung `aigw_core::upstream`, Webhooks, MCP, OIDC, OTLP, Anbieter, Bild-URLs |
| Admin-API | OIDC (state/nonce/PKCE, Token-Validierung, oid+tid), Sessions/Cookies/CSRF, RBAC inkl. gitops/auditor, IDOR bei Org/Team, Einladungen, Bootstrap, If-Match, Fehlertexte |
| Datenebene | Header-Weitergabe, Größenlimits, Streaming-DoS, Limits/Budgets, MCP-Gateway-Rechte, Plugin-Webhooks, Guardrails/PII im Streaming, Cache-Mandantentrennung |
| UI | CSP, XSS (`{@html}`, Markdown, Logs), Clickjacking, Security-Header, Token-Speicherung |
| Audit/Payload | Hash-Kette, Payload-Logging-Rechte (F203) |
| Abhängigkeiten | `cargo deny check advisories`, `pnpm audit` |

Nicht im Umfang: neuer UI-Code aus AIGW-52 (Editoren) und AIGW-54/55 (Playground, YAML-UI),
die parallel entstehen; Penetrationstest einer laufenden Installation; Fuzzing.

## 2 Bedrohungsmodell (kurz)

- **Anonyme Angreifer im Netz** erreichen die Datenebene (`:4000`), ggf. die Steuerebene
  (`:4001`). Ziele: DoS, Key-Raten, SSRF, Umgehen von Limits.
- **Inhaber eines Daten-Keys** (Mandant): Ziele: fremde Modelle/Daten (Cache, MCP-Sessions),
  Limits/Budgets/Guardrails umgehen, Gateway als SSRF-Sprungbrett.
- **Eingeschränkte Admin-Rollen** (`org-admin`, `team-admin`, `viewer`, `auditor`,
  `gitops`): Ziele: Rechteausweitung, Zugriff auf fremde Mandanten (IDOR), globale
  Secrets, Payloads anderer.
- **Kompromittierter oder bösartiger Upstream** (Anbieter, MCP-Server, Webhook-Ziel):
  Ziele: Speicher-DoS, Inhalte in den Gateway-Ursprung schleusen, Antwort-Header fälschen.
- **Angreifer mit DB- oder Dateizugriff** (Backup, Fehlkonfiguration): Ziele: Audit fälschen,
  alte Snapshots einspielen, Cache-Einträge umhängen. Der Master-Key liegt getrennt.
- Vertraut wird: Startkonfiguration (YAML/ENV), dem Betreiber (globaler `admin`), dem
  Master-Key-Speicher, dem IdP.

## 3 Befunde

Schweregrade: **hoch** – ausnutzbar mit geringen Rechten, deutlicher Schaden;
**mittel** – Voraussetzungen oder begrenzter Schaden; **niedrig** – Härtung/Defense in Depth;
**info** – Hinweis. Status: **behoben** (mit Test), **teilweise**, **offen** (Folgeticket),
**akzeptiert** (bewusst, dokumentiert).

### Hoch

| ID | Befund | Status |
|---|---|---|
| S-01 | **Abfluss globaler Secrets/Zugangsdaten über Plugin-Bindungen.** `org-admin`/`team-admin` durften in eigenen Bindungen (`content-safety`, `webhook`) auf jedes globale `Secret`/jede `ProviderCredential` verweisen und den `endpoint` frei wählen – der Gateway schickte den Anbieter-Key als Bearer an den Angreifer. | **behoben:** Bindungen mit `config.secret`/`config.credential` schreibt nur der globale `admin` (`403`), `plugin_bindings.rs::respect_secret_refs`; Test `admin_auth_api.rs` (F183-Abschnitt). |
| S-02 | **Body wird vor der Key-Prüfung gelesen.** axum liest den Body (bis `max_body_bytes`, Standard 32 MiB) vor dem Handler; anonyme Anfragen konnten so N × 32 MiB RAM belegen. | **behoben:** Middleware `auth_gate` (Datenebene und `/mcp`) prüft den Schlüssel vor dem Body; ohne gültigen Schlüssel wird der Body verworfen, der Handler antwortet wie gewohnt `401`. Test `aigw-proxy/tests/auth_gate.rs`. Verbindungsgrenzen und Lese-Timeouts: siehe S-32 (behoben). |
| S-03 | **content-safety prüfte nur das Ende des Texts** (letzte `maxChars`): schädlicher Inhalt am Anfang plus Auffüllen umging die Prüfung (Anfrage und gepufferte Antwort). | **behoben:** ganzer Text in Fenstern zu `maxChars` (10 % Überlappung, parallel, max. 16 Fenster; länger → Plugin-Fehler nach `failMode`). Test `windows_cover_whole_text`. Satzmodus im Stream war nicht betroffen. |

### Mittel

| ID | Befund | Status |
|---|---|---|
| S-04 | **SSRF-Blockliste: IPv6-Übergangsformate** fehlten: IPv4-compatible `::a.b.c.d`, lokales NAT64 `64:ff9b:1::/48`, 6to4 `2002::/16` mit privater IPv4, Teredo `2001::/32`. Relevant v. a. in IPv6-only-Clustern mit NAT64 (Cloud-Metadaten). | **behoben:** `upstream.rs` (`canonical`, `is_private_ip`); Tests `strict_policy`, `ip_check_after_resolution`. |
| S-05 | **Rolle `gitops` ist admin-gleichwertig:** `config:apply` schreibt auch `User` mit Rollen und OIDC-Verknüpfung. | **behoben (AIGW-90):** Ohne `gitops.manage_users: true` (Standard `false`) lehnt `config:apply` (auch `dryRun`) für die Rolle `gitops` das Anlegen, Ändern und Löschen von Nutzern mit Rollen oder OIDC-Verknüpfung mit `422` ab (je Nutzer ein Eintrag in `issues`); unveränderte/beibehaltene Nutzer und Nutzer ohne Rollen/OIDC bleiben erlaubt. Globaler `admin` und CLI unverändert. Admin-API-Keys sind keine GitOps-Ressource (nur `admin` schreibt sie). Tests `privileged_users_need_manage_users` (aigw-store), `admin_auth_api.rs` (S-05-Abschnitt). |
| S-06 | **Bootstrap-Token im Klartext über `tracing`** – landete in zentralem Logging. | **behoben:** einmalig direkt auf stderr, `tracing` nur ohne Token. Fehlversuche werden seit AIGW-99 gezählt und gedrosselt (S-29). |
| S-07 | **Snapshot-Cache ohne Altersgrenze:** Beim Kaltstart ohne DB galt jeder entschlüsselbare Cache – inzwischen gesperrte/widerrufene Keys wurden wieder gültig. | **behoben:** `snapshot.cache_max_age_secs` (Standard 86 400, `0` = unbegrenzt); Test `cache_max_age`. Rest: Einspielen einer älteren gültigen Datei innerhalb der Frist (Dateirechte, haertung.md). |
| S-08 | **Audit-Kette: unverkettete Einträge** (`seq IS NULL`) wurden nur gezählt; per DB eingeschobene Einträge ohne Kettenfelder ließen `ok` auf `true`. | **teilweise behoben:** unverkettete Einträge mit `at` ab Kettenbeginn sind ein Befund (Test in `restpunkte.rs`). Mit rückdatiertem `at` bleibt es bei der Zählung (`unchained`); Abgleich des Kettenkopfs mit externer Kopie empfohlen. |
| S-09 | **payloadMode durch `team-admin` steuerbar:** spezifischstes Feld gewinnt (F203) – ein Team-Admin kann Protokollierung für eigene Keys abschalten oder `full` erzwingen. | **behoben (AIGW-79):** `payloadModeMax` (off < metadata < masked < full) und `payloadModeLocked` an Organisation/Team; Prüfung beim Schreiben (Admin-API, GitOps: `422`/`409`/`cascade=clamp`, Team-Richtlinie nur `admin`/`org-admin`) und Begrenzung zur Laufzeit (`KeyCtx::payload_mode`). Tests `payload_policy_sqlite`, `payload_policy`, `payload_mode_clamped_by_max_and_lock`, e2e `aigw79.spec.js`. Ein globales Maximum gibt es nicht (global setzt nur der Betreiber den Standard `observe.payload`). |
| S-10 | **content-safety im Stream prüft keine Tool-Call-Argumente** (ohne Stream schon). | **behoben (AIGW-89):** Im Satzmodus eigener Puffer je Tool-Block (Variante + Index); Prüfung am Blockende bzw. in Fenstern zu `maxChars`, Start und Fragmente bleiben bis dahin zurückgehalten. Tests `content_safety_checks_tool_call_arguments_in_stream` (OpenAI und Anthropic). |
| S-11 | **PII im Reasoning:** Reasoning wird nicht geschwärzt (Signaturen) und ging bei `exposeReasoning` ungeprüft an den Client. | **behoben:** Bei aktiver PII-Ausgabeprüfung wird lesbares Reasoning verworfen (Stream) bzw. entfernt (Antwort); verschlüsseltes Reasoning bleibt. Test `reasoning_is_removed_when_output_is_checked`. |
| S-12 | **Limit `concurrency` wird still ignoriert** (UI bietet es an). | **behoben (AIGW-100):** `concurrency` wird je Key/Team/Organisation (optional je Modellgruppe) und je Modellgruppe durchgesetzt: `CounterBackend::acquire_slots` belegt atomar über alle Ebenen, die `SlotLease` in der `Admission` gibt bei Abschluss, Client-Abbruch, Fehler und Drop frei; `429 concurrency_limit_exceeded` mit `Retry-After: 1`. `local` exakt je Instanz, `redis` exakt im Cluster (sortierte Mengen mit Lease-Ablauf `counters.concurrency_lease_secs`, Verlängerung, lokaler Rückfall), `postgres-sync` je Instanz (Hinweis `concurrency-per-instance`). Hinweis `concurrency-not-enforced` entfernt. Tests: `aigw-counters` (`slots`, Redis über zwei Instanzen, Ablauf), `limits::tests::concurrency_*` (Freigabe, parallele Tasks, `abort`), `tests/limits.rs` (Stream hält Platz, Abbruch gibt frei). |
| S-13 | **Aufrufe des Prüfmodells** (`content-safety` mit `provider: model`, Satzmodus) laufen an Limits/Budget des Keys vorbei. | **behoben (AIGW-89):** Der Gateway-Host bucht Anbieter/Modell/Usage in `CallInfo::plugin_calls`, der Recorder rechnet sie dem Key/Team zu (Kosten in `cost`/`plugin_cost`, Budgets, Token-Limits). Obergrenze `maxChecks` je Anfrage (Standard 100; darüber `failMode` bzw. im Stream `sentenceOnError`). Tests `guard_model_calls_are_charged_to_the_key`, `content_safety_max_checks_in_stream_and_request`. |
| S-14 | **SSE-Parser ohne Obergrenze**, quadratische Suche bei langen Zeilen – ein bösartiger Upstream konnte Speicher/CPU binden. | **behoben:** `SseParser::pending_bytes`, Abbruch über 16 MiB je Ereignis (`upstream_stream_too_large`), lineare Suche. Test `pending_bytes_counts_line_and_open_event`. |

### Niedrig

| ID | Befund | Status |
|---|---|---|
| S-15 | Zugangsdaten-Header außerhalb der festen Liste (`x-goog-api-key`, `x-amz-security-token`, `x-csrf-token`, …) landeten im Payload-Log (`full`). | **behoben:** zusätzlich Namensheuristik (`auth`, `token`, `secret`, `api-key`, `signature`, `session`, …). |
| S-16 | Header-Plugin: Antwort-Header `set-cookie`, `access-control-*`, `x-ratelimit-*`, `retry-after`, HSTS/CSP setzbar; `forwarded`/`x-forwarded-*`/`x-real-ip` an Upstream setzbar. | **behoben** (`headers.rs`). |
| S-17 | `/v1/audio/speech` übernahm den Content-Type des Upstreams (z. B. `text/html` auf gemeinsamem Ursprung mit der UI). | **behoben:** nur `audio/*`/`application/octet-stream`, sonst Ersatz; `nosniff` auf Datenebenen-Antworten mit eigenem Content-Type. |
| S-18 | Admin-API-Antworten ohne Sicherheits-Header. | **behoben:** `nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy`, `Cache-Control: no-store`, CSP `default-src 'none'; frame-ancestors 'none'` (wenn nicht gesetzt). |
| S-19 | Namensregeln in `allow_private_upstreams` (`*.0.1`) passten auf IP-Literale. | **behoben.** |
| S-20 | `Debug` von `GeneratedKey`/`CreatedApiKey`/`CreatedAdminApiKey` zeigte den Klartext. | **behoben** (`***`). |
| S-21 | Vertex `settings.tokenUrl`/`token_uri` nicht statisch geprüft (Laufzeit-SSRF-Schutz greift). | **behoben (AIGW-99):** `aigw_core::validate::vertex_token_url` – nur `https` zu `oauth2.googleapis.com` bzw. `*.googleapis.com` (Port 443, ohne Zugangsdaten/Fragment); geprüft für `settings.tokenUrl` (Deployment) und `token_uri` in `serviceAccountJson` (Zugangsdaten, auch aufgelöste `valueFrom` in GitOps) – Admin-API `422`, GitOps `issues`. Tests `vertex_token_endpoint_s21`, `routing_api.rs`. |
| S-22 | `admin.allow_private_upstreams` gilt für alle Rollen (Team-Admin-Webhooks erreichen freigegebene interne Netze); Namensregel gibt auch 169.254/16 frei. | **behoben (AIGW-99):** Private Ziele nur für Plugin-Bindungen der Ebenen `global`/`model-group` (Schreibrecht nur globaler `admin`/GitOps); Bindungen auf Organisation/Team/Key (`webhook`, `content-safety`) werden beim Schreiben (Admin-API, GitOps, Guardrail-Profile) **und** beim Verbindungsaufbau (eigener Client ohne Freigaben, auch nach der Namensauflösung) nur gegen öffentliche Ziele geprüft. Namensregeln geben Link-Local (169.254.0.0/16, fe80::/10) nie frei – nur ein ausdrücklicher IP-/CIDR-Eintrag. Tests `private_targets_only_for_global_bindings_s22`, `private_webhook_targets_only_on_global_levels`, `private_targets_only_for_global_bindings_at_runtime`. |
| S-23 | Cookies `__Secure-` statt `__Host-` (Login-CSRF über Schwester-Subdomain). | **behoben (AIGW-99):** `__Host-aigw-session` / `__Host-aigw-login` mit `Path=/`, ohne `Domain`. Alte `__Secure-`-Cookies werden nicht mehr angenommen (sie blieben von Subdomains setzbar) und bei Login/Rücksprung/Logout mit ihrem alten Pfad gelöscht – bestehende Sessions melden sich nach dem Update einmal neu an. Tests `cookies_parse_and_format`, `oidc_login.rs`. |
| S-24 | IdP-Gruppenrollen gelten bis Session-Ende. | akzeptiert (`max_age` kurz halten) |
| S-25 | Einladungslink = Inhaber-Token; Fehlversuch (Konflikt) verbraucht ihn. | akzeptiert (einmalig, Ablauf, nur Hash gespeichert) |
| S-26 | Existenz-Orakel 404 vs. 403 für eingeschränkte Rollen. | akzeptiert |
| S-27 | Cache-Verschlüsselung: AAD ohne `partition_key` (DB-Schreiber kann Einträge umhängen). | **behoben (AIGW-100):** AAD = `aigw-cache:v2:` ‖ ID ‖ `:` ‖ Partition; umgehängte Einträge lassen sich nicht öffnen und werden beim Warmstart übersprungen. Migration 0010 (SQLite + Postgres) verwirft die alten Einträge (Cache). Test `seal_open_roundtrip_and_wrong_key` (ID, Partition, alte AAD). |
| S-28 | MCP: Sessions nur global begrenzt, `initialize`/Ressourcen ohne `limits.admit`, unbegrenzter Benachrichtigungskanal. | **behoben (AIGW-100):** `mcp.max_sessions_per_key` (100) und `mcp.max_sessions_per_team` (1000) zusätzlich zu `max_sessions` (`429 too_many_sessions`); `initialize`, Listen, Ressourcen und Prompts laufen durch `limits.admit` (nur `ping` nicht); Benachrichtigungskanäle begrenzt (`mcp.notification_buffer`, 64): `tools/call`-Strom verwirft neue Meldungen, `GET`-Strom die ältesten, jeweils mit Warnung. Tests `sessions_limited_per_key_and_team`, `key_limits_apply_to_tool_calls`, `per_key_and_team_limits`, `notification_channel_is_bounded`. |
| S-29 | Bootstrap-Abschaltung kann still scheitern; Fehlversuche nicht gezählt/gedrosselt. | **behoben (AIGW-99):** Abschaltung gilt sofort im Prozess, Speicherfehler → `error`-Log und Wiederholung bei jeder weiteren Prüfung; beim Start holt `reconcile_bootstrap` einen fehlenden Vermerk nach (globaler Admin-Key vorhanden), bei Fehler bleibt das Token aus (`error`). Fehlversuche werden gezählt: ab 10 je Minute wird jeder weitere um 1 s gebremst (Warnung im Log; das richtige Token wird nie gesperrt, damit Falscheingaben den Notfallzugang nicht blockieren; Admin-Key-Präfix zählt nicht). Tests `bootstrap_tests::*`. |
| S-30 | Webhook-Plugin `modify`: `n`/`max_tokens` nach der Admission nicht erneut geprüft. | **behoben (AIGW-99):** Nach den Pre-Plugins (alle Plugins, nicht nur Webhook) werden `n` und `max_tokens` je Variante auf die zugelassenen Werte begrenzt (Client-Wert bzw. `limits.default_max_tokens`; entfernte Grenze wird wieder gesetzt), mit `warn`-Log – Begrenzen statt Ablehnen, damit die Reservierung immer reicht und ein fehlerhafter Webhook nicht allen Verkehr sperrt. Tests `plugin_changes_stay_within_admission`, `modify_cannot_raise_n_or_max_tokens`. |
| S-31 | (unsicher) Freigabe und Routing-Tabelle werden getrennt geladen – bei Konfig-Wechsel kann ein umgehängter Alias kurz auf eine andere Gruppe zeigen. | **behoben (AIGW-100):** `ModelRoutes::route_for` sucht die Route über den kanonischen Namen der freigegebenen Gruppe in der Tabelle derselben `config_version` wie der Snapshot der Anfrage (ältere Tabelle → sofortiger Neuaufbau unter Sperre; neuere → vorige Tabelle). Test `route_for_uses_the_same_config_version_as_the_grant`. |
| S-32 | Keine globale Verbindungs-/Nebenläufigkeitsgrenze, kein expliziter Header-/Body-Lese-Timeout (Slowloris) im Server. | **behoben (AIGW-80):** eigene Accept-Schleife (`aigw/src/serve.rs`): `server.data|control.max_connections` (über der Grenze sofort `503` + Schließen, Accept blockiert nie), `server.http`: Kopf-Timeout ab dem ersten Byte, Körper-Pause und -Mindestrate, Keep-Alive-Leerlauf, HTTP/2 max. Streams und PING-Keepalive; laufende Antworten (SSE) ohne Lese-Timeout. Metriken `aigw_server_connections_*`, `aigw_server_timeouts_total`. Tests in `serve.rs` (Slowloris Kopf/Körper/Mindestrate, Grenze, Leerlauf, Streams über HTTP/1.1 und h2). Limits je Client-IP und TLS bleiben beim Reverse Proxy (haertung.md). |

### Nachträge (AIGW-90)

| ID | Befund | Status |
|---|---|---|
| S-33 | **Gateway-Identität für jedes Entra-Deployment** (Restrisiko AIGW-82/84): Wer Deployments bzw. Zugangsdaten anlegen durfte, konnte mit `managedIdentity`/`workloadIdentity` Tokens der Identität des Gateway-Prozesses (Scope Cognitive Services) an einen erlaubten Endpunkt schicken lassen – ohne dass der Betreiber das freigegeben hatte. | **behoben (AIGW-90):** Opt-in `upstream.entra.ambient_identity` (Standard `false`, ENV `AIGW_UPSTREAM__ENTRA__AMBIENT_IDENTITY`). Ohne Freigabe baut die Token-Quelle nicht (Deployment „nicht nutzbar“ mit Meldung inkl. Schlüssel), beim Start warnt das Log mit den betroffenen Zugangsdaten/Deployments. Restrisiko mit Freigabe wie bisher (Rolle der Identität eng fassen). Test `ambient_identity_requires_opt_in`. |
| S-34 | **MCP-Server mit nicht auflösbarem Geheimnis:** Admin-API prüfte nur `credential`, nicht `secret`; ein fehlendes Token-Feld fiel erst zur Laufzeit auf. | **behoben (AIGW-90):** Admin-API (Anlegen/Ändern) prüft, dass `credential` bzw. `secret` existiert und das Feld `settings.credentialField` (Standard `apiKey`, sonst `value`) enthält – `422` mit `issues`; GitOps prüft das Feld ebenso (Existenz schon bisher). Tests `mcp_api.rs`, `reference_fields_aigw90`. |

### Nachträge (AIGW-94)

| ID | Befund | Status |
|---|---|---|
| S-35 | **Azure-Arc-Challenge als Datei-Lesegerät:** Der Arc-Agent nennt im `WWW-Authenticate`-Header einen Dateipfad, dessen Inhalt als `Authorization` zurückgeht. Ein manipulierter lokaler Endpunkt (oder ein umgebogenes `IDENTITY_ENDPOINT`) könnte so beliebige Dateien des Gateway-Prozesses (z. B. Master-Key-Datei) abfragen. | **vorgebeugt (AIGW-94):** nur `.key`-Dateien direkt im Arc-Verzeichnis (`/var/opt/azcmagent/tokens`), Prüfung lexikalisch **und** nach Auflösen symbolischer Links, nur reguläre Dateien ≤ 4 KiB; sonst Auth-Fehler ohne Inhalt, Pfad nur im Log. Arc nur mit `upstream.entra.ambient_identity`. Tests `arc_key_file_rules`, `azure_arc_rejects_key_files_outside_the_agent_directory`. |
| S-36 | **Verschlüsselte Zertifikatsschlüssel:** Passwort und Schlüsselmaterial dürfen weder in Fehlermeldungen noch in `Debug`/Logs erscheinen; teure Schlüsselableitung als DoS beim Konfigurationsaufbau. | **umgesetzt (AIGW-94):** eigene, längengeprüfte DER-Auswertung, Meldungen ohne Geheimnisse (falsches Passwort = „Passwort falsch oder Schlüssel beschädigt“), Zwischenpuffer genullt, PBKDF2 höchstens 2 000 000 Iterationen; Passwort nur als Zugangsdaten-Wert (verschlüsselt gespeichert) bzw. `valueFrom`. Tests `encrypted_client_certificate_errors_without_secrets`, `decrypts_openssl_pbes2_fixtures`, `unsupported_and_limits`. |

### Nachträge (AIGW-101)

| ID | Befund | Status |
|---|---|---|
| S-37 | **WASM-Module ohne kryptografische Herkunft:** Freigabe war allein der Upload durch einen globalen `admin` (Restrisiko AIGW-63). Wer Schreibzugriff auf die Datenbank, den Snapshot-Cache oder ein Admin-Konto bekam, konnte ein beliebiges Modul in den Gateway-Prozess bringen, das Anfragen aller bindenden Mandanten sieht. | **umgesetzt (AIGW-101):** Ed25519-Signaturen über `aigw-wasm-module-v1:<sha256>` gegen `wasm.trusted_keys` (öffentliche Schlüssel nur in der Startkonfiguration, nicht per Admin-API änderbar). Mit Schlüsseln ist die Signatur standardmäßig **Pflicht** (`wasm.require_signature`); geprüft beim Upload (`422`), beim Import aus `wasm.modules` (Startabbruch) **und vor jedem Kompilieren** (SHA-256 des Inhalts neu berechnet → auch an der Admin-API vorbei eingeschleuste Module laden nicht; Metrik `aigw_wasm_signature_rejected_total`). Schlüsselwechsel: neuen Schlüssel ergänzen, neu signieren, alten entfernen. Ohne `trusted_keys` unverändert (Prüfung aus). Tests `verify_cases`, `key_rotation`, `signature_checked_before_loading`, `signatures_upload_list_and_rotation`, e2e `aigw101.spec.js`. Restrisiko: wer die Startkonfiguration ändern kann, kann Schlüssel tauschen; Signierschlüssel offline halten. |

### Nachträge (AIGW-106)

| ID | Befund | Status |
|---|---|---|
| S-40 | **WASM: Schlüsselverwaltung und Ein-Personen-Freigabe** (Folgepunkt S-37): Ed25519-Signaturen verlangen eigene Schlüssel (Erzeugen, Verwahren, Wechseln); ein einzelnes übernommenes Admin-Konto konnte ein korrekt signiertes, aber unerwünschtes Modul sofort aktivieren. | **umgesetzt (AIGW-106):** (1) **Sigstore keyless** (`wasm.sigstore`): Bundle aus `cosign sign-blob`, offline geprüft gegen ein Trust-Root aus der Startkonfiguration (Fulcio-Kette zum Log-Zeitpunkt mit EKU codeSigning, SCT, Rekor-SET und Inclusion-Proof mit Checkpoint, Eintrag = Digest/Signatur/Zertifikat des Bundles, Zeit nicht in der Zukunft) und erlaubte Identitäten (Issuer/Subjekt als vollständig verankerte reguläre Ausdrücke); dieselben Prüfpunkte wie S-37 (Upload, Startkonfiguration, vor jedem Kompilieren, Freigabe). Eigene schlanke Prüfung (`aigw-sigstore`, aws-lc-rs + rustls-webpki) statt Crate `sigstore` (zweiter Krypto-Stapel, `rsa`/RUSTSEC-2023-0071); getestet mit der Sigstore-Konformitätssuite (5 gültige, 23 ungültige echte Bundles, ohne Netz). (2) **Vier-Augen-Freigabe** (`wasm.four_eyes`, Standard im Profil `strict`): Uploads werden eingereicht (Tabelle `wasm_module_pending`, nicht im Snapshot) und erst durch einen anderen globalen `admin` aktiv; Person = Nutzer hinter Admin-Keys (beim Einreichen festgehalten), Freigabe nur mit dem SHA-256 der geprüften Version, Signaturen werden bei der Freigabe erneut geprüft, Audit `wasm-module-approval` (submit/approve/reject). Tests `conformance` (aigw-sigstore), `sigstore_policy`, `sigstore_checked_before_loading`, Store-Test `wasm` (beide Dialekte), Admin-Tests `four_eyes_*`, e2e `aigw106.spec.js`. **Restrisiko:** Keys, die der Bootstrap-Admin anlegt, gelten als eigene Personen (Bootstrap nach Einrichtung beenden); wer die Startkonfiguration ändert, umgeht beides (Module aus `wasm.modules` brauchen keine Freigabe, Trust-Root/Identitäten stehen dort) – Review der Konfiguration; zu weite Identitätsmuster; Rekor v2/TSA und DSSE noch nicht unterstützt. |

### Nachträge (AIGW-103)

| ID | Befund | Status |
|---|---|---|
| S-38 | **Plugins vergrößern den Prompt nach der Admission** (Folgepunkt S-30): Limits und harte Budgets wurden nur mit der Eingabe der Client-Anfrage reserviert; ein Pre-Plugin (`request-transform`, Webhook `modify`, WASM) konnte die Eingabe danach beliebig vergrößern, die Vorabprüfung sah das nicht (der Abgleich bucht erst nachträglich). | **behoben (AIGW-103):** Nach den Pre-Plugins wird die Eingabe mit dem Tokenizer der Modellgruppe neu geschätzt; ist sie gewachsen, wird die Differenz auf allen Token-/Eingabe-Limits und (harte Budgets) als Kosten nachreserviert – alle oder keine. Reicht ein Limit nicht, wird die Anfrage mit `429`/Budget-Fehler abgelehnt wie bei der Admission. Beim Abschluss zählt die Nachreservierung genau die Differenz, zusammen ergibt sich der tatsächliche Verbrauch. Gilt für Chat und Responses; Endpunkte ohne Chat-Ausgabe (Embeddings u. a.) unverändert. Test `plugin_grown_input_is_reserved_or_denied`. |

### Nachträge (AIGW-115)

| ID | Befund | Status |
|---|---|---|
| S-39 | **Neue Anmeldeart Access-Token (JWT) auf Datenebene und MCP** (Microsoft Entra Agent ID, Workload-Identitäten): Risiken Verfahrens-Verwechslung (`none`/HMAC mit öffentlichem Schlüssel), fremde Aussteller/Zielgruppen, Schlüsselnachladen als DoS-/SSRF-Hebel, Token im Log oder beim Upstream, Zuordnung allein über Agent-Claims, nicht widerrufbare Tokens. | **umgesetzt (AIGW-115):** Verfahrens-Allowlist je Aussteller (nur RS256/ES256, `crit` abgelehnt, `jku`/`x5u` ignoriert), Signatur über aws-lc-rs, `iss` exakt, `aud` Pflicht, `exp` Pflicht, `nbf`/`iat`, Entra `tid`; Größenlimit vor jeder Auswertung (16 KiB). JWKS nur aus der Startkonfiguration (`https`, keine Weiterleitungen, Zeit-/Größenlimit über `aigw-outbound`), gedrosseltes Nachladen bei unbekannter `kid`, Single-Flight. Zuordnung nur über explizit eingetragene Identitäten am Key (eindeutig, `403` sonst, kein Auto-Provisioning), optional App-Rolle/Scope/Agent-Pflicht. Fehler ohne Detail (`401`/`403`), Token nie im Log (Header-Filter im Payload-Log), nie an Upstreams. Tests `aigw-jwt/tests/verifier.rs`, `aigw-proxy/tests/workload_auth.rs`, `agent_token_on_mcp_gateway`, `aigw-store/tests/workload.rs`. **Restrisiko:** Tokens gelten bis `exp` (kein Widerrufsabgleich mit Entra); sofort wirksam ist nur Entfernen der Identität bzw. Sperren des Keys. Ohne `required_roles` kann jede App des Tenants ein Token für das Gateway holen – zugeordnet werden aber nur eingetragene Identitäten. |

### Info

- Kein `zeroize`: `SecretString`, Klartext-Keys und entschlüsselte Credentials bleiben nach
  der Freigabe im Speicher (Ausnahme Snapshot-Cache-Puffer).
- OIDC `code_verifier`/`nonce` liegen 10 Minuten im Klartext in `admin_login_state`.
- `GET /admin/api/v1/auth/info` verrät ohne Anmeldung, ob das Bootstrap-Token aktiv ist.
- Katalog (Secrets-/Credential-Metadaten, Plugin-Bindungen) ist für jede wirksame Rolle
  lesbar – gewollt, aber mandantenübergreifend sichtbar.
- `admin.ui.trace_url` wird ohne Schemaprüfung als Link verwendet (nur Startkonfiguration).
- Standard-`failMode` des Webhook-Plugins ist `open` – Policy-Webhooks auf `closed` setzen.

## 4 Geprüft und in Ordnung (Auszug)

- **Keys:** 256 Bit aus `SystemRandom`, SHA-256-Hash (bei der Entropie angemessen),
  Lookup über Hash (kein Timing-Orakel), einheitliche `401`; nur aktive Keys im Snapshot,
  Ablauf je Anfrage. Admin-Keys je Anfrage aus der DB, Präfix getrennt.
- **Keyring:** AES-256-GCM, zufällige 96-Bit-Nonce, AAD = Objekt-ID; `aigw keygen` mit 0600
  und `create_new`; `SecretString` redigiert in `Debug`/`Serialize`; GitOps-Export nur mit
  Verweisen; `valueFrom.env` nur `AIGW_SECRET_*`, Dateien nur in `secret_dir`.
- **SSRF:** Prüfung nach der Auflösung, verbunden wird genau mit den geprüften Adressen
  (kein Rebinding), IP-Literale geprüft, Alt-Schreibweisen (`127.1`, `0x7f…`) abgelehnt,
  keine Redirects, keine Proxy-Variablen, Größen-/Zeitlimits, keine Dekompression,
  rustls ohne Abschaltmöglichkeit der Zertifikatsprüfung. Bild-URLs lädt der Gateway nicht.
- **OIDC/Sessions/CSRF:** state/nonce/PKCE (S256) 256 Bit, state einmalig, Signatur nur
  RS256/ES256/EdDSA über JWKS, `iss`/`aud`/`azp`/`exp`/`nbf`/`nonce`, Identität (iss, sub)
  bzw. (iss, oid)+`tid`; `return_to` nur `/admin…`; Session-ID 256 Bit, nur Hash in der DB,
  `__Host-`-Cookie mit `HttpOnly; Secure; SameSite=Strict; Path=/`, neue ID beim Login; CSRF-Header + `Sec-Fetch-Site` +
  `Origin` für alle unsicheren Methoden.
- **RBAC:** alter und neuer Bereich bei Updates geprüft (kein Verschieben in fremde Orgs),
  Listen per SQL gefiltert, Payloads nur `admin`/`auditor`, Audit-Verify nur global;
  Nutzer/Admin-Keys/Einladungen nur globaler `admin`; If-Match Pflicht (428/412);
  Nur-YAML-Modus blockiert Schreibzugriffe vor der Anmeldung.
- **Datenebene:** keine pauschale Header-Weitergabe an Upstreams, keine Upstream-Antwort-
  Header an Clients, CR/LF durch `HeaderValue` ausgeschlossen; Reservierung je Anfrage
  (`n` berücksichtigt, Abbruch rechnet geschätzt ab); Cache-Schlüssel mit Scope; MCP-Session
  an Key gebunden; Webhook-HMAC mit Zeitstempel und Vergleich in konstanter Zeit.
- **UI:** strenge CSP (`script-src 'self'`, `frame-ancestors 'none'`), kein `{@html}`/
  `innerHTML`, Links kodiert, Token nur im `sessionStorage`.
- **Audit-Kette:** Kopf per `UPDATE … RETURNING` serialisiert, `UNIQUE(seq)`, Anker bei
  Aufbewahrung, keine Secrets/Hashes im Payload.

## 5 Abhängigkeiten

- `cargo deny check advisories`: ok. Bewusste Ausnahme `RUSTSEC-2026-0459`
  (libsqlite3-sys über sqlx-sqlite, E8; nur lokal mit fremden DB-/WAL-Dateien ausnutzbar,
  erneut prüfen bis 2027-01-31).
- `pnpm audit`: 1 × low (`deepmerge` Prototype Poisoning, GHSA-gcqx-w48m-w2h4) nur im
  Build-Pfad `@sveltejs/vite-plugin-svelte`; `pnpm audit --prod`: keine. Akzeptiert.

## 6 Folgepunkte (Empfehlung)

1. ~~S-09 `payloadModeMax`/`locked` an Org/global (F203-Folgepunkt)~~ – behoben (AIGW-79).
2. ~~S-32 Verbindungs-/Nebenläufigkeitsgrenzen und Lese-Timeouts im Server~~ – behoben (AIGW-80).
3. ~~S-10/S-13 content-safety: Tool-Calls im Stream, Kosten des Prüfmodells zurechnen~~ –
   behoben (AIGW-89).
4. ~~S-12 `concurrency` durchsetzen~~ – behoben (AIGW-100).
5. ~~S-28 MCP-Grenzen je Key; S-27 Cache-AAD mit Partition~~ – behoben (AIGW-100);
   ~~S-23 `__Host-`-Cookies~~ – behoben (AIGW-99).
