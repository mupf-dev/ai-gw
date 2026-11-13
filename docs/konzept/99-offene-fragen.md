# ai-gw – Offene Fragen und Entscheidungen (Fragerunde 2026-11-13)

Status: [ ] offen, [x] entschieden (Ergebnis dahinter)

## AIGW-9 Datenmodell
1. [x] Namensraum: A global eindeutig für v1alpha1
2. [x] Währung: A eine je Installation, billing.currency (Standard USD) nur Anzeige
3. [x] Keys: A Team optional, Organisation PFLICHT (Umsetzung nötig)
4. [x] GitOps-Keys: B Standard ignore, nur mit --api-keys include (Umsetzung nötig)
5. [x] Credentials: A PUT + PATCH (Merge) so lassen
6. [x] Master-Key: A Hex + Base64, `aigw keygen` (Umsetzung nötig)
7. [x] DB-Treiber: B getrennte Treiber SQLite/Postgres mit nativen Typen (großer Umbau)

## AIGW-13 IR/Formate
8. [x] IR: A owned Strings, Optimierung in AIGW-57
9. [x] /v1/models: A nach anthropic-version-Header
10. [x] Reasoning: B Standard an als reasoning_content (auch Stream), je Gruppe/Key abschaltbar (Umsetzung nötig)
11. [x] Client-ID: A zusätzlich als client_request_id loggen/suchbar (Umsetzung nötig)
12. [x] n>1 Stream: B voll unterstützen, Nicht-OpenAI per parallelen Anfragen (Umsetzung nötig)

## AIGW-10 Snapshot
13. [x] Ohne Master-Key: A abbrechen + Hinweis keygen
14. [x] DB beim Start weg: B abbrechen + Folgeaufgabe verschlüsselter Snapshot-Cache auf Platte
15. [x] Stale-Snapshot: A /readyz degraded (200) ab 5 min, UI-Banner, Feld in /version (Umsetzung nötig)
16. [x] Snapshot: B jetzt inkrementeller Neubau (User: sonst vergessen) (Umsetzung nötig)

## AIGW-48 UI
17. [x] Akzent: A Violett #5b45d6
18. [x] Charts: uPlot (Zeitreihen) + eigene SVG + LayerChart wo sinnvoll (komplexere Typen), lazy geladen
19. [x] Schrift: A Systemschrift
20. [x] Zahlen: B alles Monospace wie cloud-project (Umsetzung nötig)
21. [x] Sidebar: A immer dunkel
22. [x] Mock: A Standard aus, per Schalter/?mock=1, gekennzeichnet (an AIGW-49 weitergeben)
23. [x] Screenshots: B nur hell+dunkel Desktop optimiert (~1 MB), Rest als e2e-Artefakt (Umsetzung nötig)

## AIGW-29 Zähler
24. [x] BSL-1.0: A erlauben
25. [x] Limit-Fenster: C wählbar – gleitend (30d etc.) oder Kalender (day/week/month, Zeitzone billing.timezone) (Umsetzung nötig)
26. [x] Zähler-Einstellungen: A in Startkonfig (Umsetzung nötig)
27. [x] Redis: A nur Einzelinstanz/Sentinel, Cluster-Modus → klare Fehlermeldung beim Start (Umsetzung nötig)
28. [x] Redis-Ausfall: B Deltas nachtragen (Umsetzung nötig)
29. [x] Sekundenlimit+postgres-sync: A Warnung Log + UI-Hinweis (Umsetzung nötig)

## AIGW-43 Request-Log
30. [x] Payload-Modus: A je Org/Team/Key mit Vererbung (Umsetzung nötig)
31. [x] JSONL: B eingebaute Rotation (Tag/Größe, keep, gzip) + SIGHUP (Umsetzung nötig)
32. [x] Rollup-Minute: A Anfragebeginn
33. [x] Rollups: C getrennt schaltbar observe.rollups (Std. an) (Umsetzung nötig)
34. [x] Rollups exakt: B Batch-ID-Tabelle (idempotent) (Umsetzung nötig)
35. [x] Fristen: A 7/30/400, Payload off
36. [x] KK-Falsch-Positive: A bis PII-Engine AIGW-37

## AIGW-28 Keys/Admin
37. [x] Key-Zustände: A suspended (umkehrbar) + revoked (endgültig) (Umsetzung nötig)
38. [x] If-Match: A Pflicht (428), Prüfung in DB-Transaktion, If-Match: * erlaubt (Umsetzung nötig)
39. [x] Unbekanntes Modell: C immer präzise (403 nicht freigegeben, 404 existiert nicht) (Umsetzung nötig)
40. [x] /v1/models: A kanonisch, Aliasse per Option (global + je Gruppe) (Umsetzung nötig)
41. [x] Rotation: A 24 h Übergang, Name+Zeitstempel
42. [x] Pagination: A jetzt echte SQL-Pagination (zusammen mit F7-Store-Umbau) (Umsetzung nötig)

## AIGW-14/15 Streaming/Anbieter
43. [x] Upstream-401/403: A → 502 ohne Anbietermeldung
44. [x] Timeouts: A 10/120/120/600 s, je Modellgruppe settings.timeouts überschreibbar (prüfen ob schon in AIGW-24 umgesetzt)
45. [x] Azure: A Deployment-URL Standard, apiVersion Pflicht, apiStyle v1 wählbar
46. [x] Usage-Schätzung: B eigene Tokenizer (OpenAI o200k/cl100k, Llama/Mistral), Heuristik+Sprachfaktor für Claude/Gemini (Umsetzung nötig)
47. [x] ErrorClass: A eigenes Feld error_class in request_log, Filter + Dashboard-Dimension (Umsetzung nötig)
48. [x] Upstream-CAs: B upstream.roots webpki|system|both + ca_file global und je Deployment (Umsetzung nötig)
49. [x] Modellname: C settings.responseModel upstream|requested (Std. upstream) + Header x-aigw-model (Umsetzung nötig)
50. [x] p50: A Optimierung in AIGW-57 nach Feature-Vollständigkeit

## AIGW-33 GitOps
51. [ ] ApiKeys Standard include vs ignore bei apply/prune (Empf. include)
52. [ ] Admin-API valueFrom nur AIGW_SECRET_* env (Empf. ja)
53. [ ] Nur-YAML-Modus 403, config:apply erlaubt (Empf. ja)
54. [ ] valueFrom-Verweise speichern für originalgetreuen Export (Empf. später)
55. [ ] Eigene Permission gitops (Empf. ja)
56. [ ] readOnly-Flag für UI (Empf. ja)

## AIGW-16/17/20 Anthropic/Ollama/Mistral
57. [ ] Anthropic 429 = RateLimit, Quota nur billing (Empf. ja)
58. [ ] reasoningMode Standard adaptive (Empf.)
59. [ ] Budget-Tabelle + max_tokens anheben (Empf. ja)
60. [ ] anthropic-beta von Clients durchreichen (Empf. ja, Allowlist später)
61. [ ] Mistral reasoning_effort/Magistral als Folgepunkt (Empf.)

## AIGW-65 Admin-API Modelle
62. [ ] valueFrom über Admin-API verboten (Empf. ja)
63. [ ] SSRF nach DNS-Auflösung im HTTP-Client (ip_allowed) (Empf. ja)
64. [ ] Azure apiVersion Pflicht (Empf. ja)
65. [ ] Credentials nur gleicher Anbieter (Empf. ja)
66. [ ] Löschen von Modellgruppen mit Freigaben blockieren (409) (Empf. ja)
67. [ ] Rückwirkende Preise nur mit backfill-Flag (Empf. optional)
68. [ ] Fehlerhafte Preisversion nur per neuer Version korrigieren (Empf. ja)
69. [ ] Fallback-Zyklen ablehnen + Tiefenlimit (Empf. ja)
70. [ ] Prune darf gültige Preisversionen löschen (Empf. ja)
71. [ ] Ollama ohne Endpoint braucht localhost-Freischaltung (Empf. ja)

## AIGW-18/19 Bedrock/Vertex
72. [ ] Neue WireFormat-Varianten (Empf. ja)
73. [ ] Provider::prepare-Hook (Empf. ja)
74. [ ] thoughtSignature in Tool-Call-ID (Empf. ja)
75. [ ] Vertex-Location Standard global (Empf.)
76. [ ] Volles JSON-Schema für Gemini (Empf. ja)
77. [ ] Eine Anbieterart vertex für beide Google-APIs (Empf. ja)
78. [ ] Gemeinsamer HTTP-Client für Token-Abrufe (Empf. ja)
79. [ ] Bedrock http(s)-Bild-URLs ablehnen (Empf. ja)

## AIGW-24–26 Routing
80. [ ] Fallback auf nicht freigegebene Gruppe erlaubt (Empf. ja)
81. [ ] Nur Kette der Ursprungsgruppe (Empf. ja)
82. [ ] Stream-Header erst nach erstem Event (Empf. ja)
83. [ ] Alle gesperrt wegen Retry-After/Kapazität → 429 statt 503 (Empf. ja)
84. [ ] Breaker-Standardwerte (Empf. ja)
85. [ ] mock im Produktivbetrieb erlaubt, sperrbar (Empf.)
86. [ ] attempts im request_log + Metriken (Empf. Folgeticket)

## AIGW-32 OIDC/Rollen
87. [ ] Höchstlaufzeit Admin-Keys konfigurierbar (Empf.)
88. [ ] Admin-Keys nur globale Admins (Empf. ja)
89. [ ] org-admin darf eigene Org nicht ändern (Empf. ja)
90. [ ] link_by_email + provision_users an (Empf. ja)
91. [ ] Bootstrap aus erst bei globaler Admin-Rolle (Empf. ja)
92. [ ] IdP-Gruppen erst beim nächsten Login (Empf. ja)
93. [ ] Eingeschränkte Rollen sehen Credential-Metadaten (Empf. ja)

## AIGW-44/45 Beobachtung
94. [ ] OTLP eigen statt opentelemetry-Crates (Empf. ja)
95. [ ] nur HTTP/protobuf, gzip später (Empf. ja)
96. [ ] sampling_ratio 1.0 wenn an (Empf.)
97. [ ] parent_based true (Empf.)
98. [ ] propagate an externe Anbieter (Empf. global true, je Deployment abschaltbar)
99. [ ] deployment-Label als Name (Empf. ja)
100. [ ] trace_id im Request-Log (Empf. ja)
101. [ ] gen_ai.provider.name zusätzlich (Empf. ja)
102. [ ] OTLP-Retry ja, App-Logs eigenes Ticket (Empf.)
103. [ ] Gegentest mit offiziellem OTel-Collector (Docker) freigeben?

## AIGW-30/31 Limits/Budgets
104. [ ] budget_exceeded 429 statt 402 (Empf. 429)
105. [ ] Retry-After bis nächster Zeitraum (Empf. ja)
106. [ ] Parallele Überschreitung von Budgets akzeptieren (Empf. ja)
107. [ ] Zeitzone global (Empf. ja)
108. [ ] Ersatzpreise cached→input, reasoning→output (Empf. ja)
109. [ ] Deployment-TPM vorab reservieren (Empf. ja)
110. [ ] Budget-Warnungen dedupliziert mit Webhook (in AIGW-61 beauftragt)
111. [ ] Fallback verbraucht kein Kontingent der Zielgruppe (Empf. ja)

## AIGW-66 Audit
112. [ ] Eingeschränkte admin/auditor sehen Bereich, viewer nichts (Empf. ja)
113. [ ] Audit-Ausfall beim Login fail-open (Empf. ja)
114. [ ] Fehlgeschlagene Bearer-Logins nur Metrik (Empf. ja)
115. [ ] Arten-Schreibweise vereinheitlichen (Empf. ja)
116. [ ] Aufbewahrung + Hash-Kette als nächstes Paket (Empf. ja)

## AIGW-36 Plugins
117. [ ] Steuerfelder als eigene Spec-Felder (Empf. ja, Folgeticket)
118. [ ] Bindungen beim Schreiben validieren (Empf. ja)
119. [ ] order über alle Ebenen (Empf. ja)
120. [ ] Modell-Umleitung gesperrt (Empf. ja)
121. [ ] Plugin-Respond ohne Usage verbraucht keine Token-Limits (Empf. ja)

## Umsetzungsänderungen aus Entscheidungen
- F3: ApiKey.organization Pflicht (Validierung Admin+GitOps, Migration/Altbestand, UI-Formular)
- F4: GitOps --api-keys Standard ignore (CLI + config:apply), Doku 09
- F6: Master-Key Base64 akzeptieren + `aigw keygen` (stdout oder Datei 0600)
- F7: Store auf getrennte Treiber (sqlx sqlite + postgres) mit nativen Typen (UUID, JSONB, timestamptz), zwei Migrationssätze, alle Repos/GitOps/Analytics/Audit/Ledger/Counters anpassen – eigenes Paket
- F10: OpenAI-Encoder gibt ReasoningDelta/Reasoning als reasoning_content aus (Standard an), settings.exposeReasoning=false zum Abschalten je Modellgruppe/Key
- F11: client_request_id (x-request-id/x-client-request-id) in RequestRecord/request_log/Trace + Filter im Log-Explorer
- F12: n>1 im Stream (index je Choice); Anbieter ohne n: n parallele Upstream-Anfragen, Usage summiert, Limits/Kosten entsprechend
- F14: Verschlüsselter Snapshot-Cache auf Platte (Master-Key), Kaltstart bei DB-Ausfall mit letztem Stand, Readiness „degraded“
- F15: /readyz degraded (HTTP 200) wenn Snapshot > snapshot.stale_after (Std. 5 min) veraltet; Feld snapshot in GET /version; UI-Banner
- F16: Inkrementeller Snapshot-Neubau (geänderte Ressourcen per audit/config_version-Delta), Voll-Neubau als Fallback + periodische Konsistenzprüfung
- F18: LayerChart zusätzlich für komplexere Diagrammtypen (z. B. Sankey/Treemap/Stacked), lazy je Seite, CSP prüfen, Bundle-Budget beachten
- F20: UI: alle Zahlen (auch KPI-Kacheln) in Monospace, Screenshots aktualisieren
- F22: UI Mock-Modus Standard aus (nur Schalter/?mock=1, deutlich gekennzeichnet) – beim Fortsetzen AIGW-49 mitteilen
- F23: docs/konzept/ux auf hell/dunkel Desktop optimiert reduzieren (WebP/optimiertes PNG), e2e erzeugt Rest als Artefakt (nicht eingecheckt)
- F25: LimitSpec.window zusätzlich calendar-day/-week/-month (Zeitzone wie Budgets), Counter-Schlüssel mit Periodenbeginn, Reset-Header korrekt; „month“ gleitend in „30d“ umbenennen
- F26: counters.sync_interval_ms / redis_pool_size / redis_timeout_ms in CountersConfig (+ENV)
- F27: Beim Start Redis-Cluster-Modus erkennen (INFO cluster) → klare Fehlermeldung
- F28: redis-Backend sammelt lokale Deltas während Ausfall und überträgt sie nach Rückkehr (nur noch laufende Fenster; abgelaufene Buckets verwerfen), Metrik
- F29: Warnung bei Sekundenlimits mit counters.backend=postgres-sync (Start-Log, Admin-API-Warnung, UI-Hinweis)
- F30: payloadMode in Org/Team/Key-Spec mit Vererbung, RequestObserver::payload_mode(&KeyCtx)
- F31: JsonLinesExporter: Rotation daily/max_size_mb, keep N, optional gzip, plus Reopen bei SIGHUP
- F33: observe.rollups true|false (Standard true), unabhängig von request_log; UI-Hinweis wenn aus (Dashboards leer)
- F34: Tabelle observe_batch(batch_id PK, applied_at) – Batch nur anwenden wenn ID neu, Aufbewahrung kurz (z. B. 7 Tage)
- F37: ApiKey-Status active|suspended|revoked; suspend/resume-Aktionen, revoke endgültig; API/UI/Audit/GitOps
- F38: If-Match Pflicht für PUT/PATCH/DELETE/Aktionen (428), Versionsvergleich im UPDATE (Cluster-sicher), If-Match: * erlaubt; UI/GitOps/Smoke anpassen
- F39: Datenebene: 404 model_not_found wenn Modell/Alias nicht existiert, 403 model_not_allowed wenn nicht freigegeben – auch für eingeschränkte Keys
- F40: server.data.list_aliases (Std. false) + ModelGroup settings.listAliases
- AIGW-21-Folgen: Chat-Handler auf pipeline.rs umstellen; Plugin-Haken für Embeddings/Bilder/Audio; Kosten für Bilder/Audio (perImage, perAudioMinute); n>1 auch in Responses; gestreamte Transkription/Bilder, Image-Edits

## AIGW-21 Multimodal (angehängt)
122. [ ] Ohne settings.capabilities nur Chat (Empf. ja)
123. [ ] capabilities später typisiertes Feld (Empf. ja)
124. [ ] Responses zustandslos, store fehlt = false, native optional (Empf. ja)
125. [ ] Bild-URLs nie nachladen (Empf. ja)
126. [ ] Audio-Limit 25 MiB (Empf. ja)
127. [ ] Bedrock Titan 256 Eingaben, 8 parallel (Empf. ja)

## AIGW-67 Team-Grenzen (angehängt)
128. [ ] force für globalen Admin (Empf. ja)
129. [ ] kein force/cascade in GitOps (Empf. ja, später --allow-bound-violations)
130. [ ] verschiedene Zeiträume nicht vergleichen, ggf. Warnung (Empf. ja)
131. [ ] gesperrte/abgelaufene Keys zählen mit (Empf. ja)
132. [ ] nur neue Verstöße blockieren (Empf. ja)
133. [ ] Audit-Format force (Empf. so lassen)
134. [ ] clamp ohne Modelle → 409 (Empf. ja)

## AIGW-49 UI (angehängt)
135. [ ] D7 Mock-Schalter per admin.ui.mock ausblendbar (Empf. ja)
136. [ ] D8 Detailseiten statt Dialoge (Empf. ja, mit 46/66)
137. [ ] D9 strukturierte Formulare für Routing-Settings (Empf. ja)
138. [ ] D10 Routing-Regeln entfallen (Empf. ja)
139. [ ] D11 serverseitige Pagination ab ~1000 (Empf. ja)
140. [ ] D12 auth/info um bootstrap + readOnly erweitern (Empf. ja)
- AIGW-46-Folge: Proxy füllt RequestRecord.client_request_id aus x-request-id/x-client-request-id (F11)

## AIGW-46 Analytics (angehängt)
141. [ ] 24h/5m auf SQLite 380 ms – 5-Min-Rollups erst bei Bedarf / Auto-Step 15 min (Empf.)
142. [ ] provider über aktuelle Deployment-Konfig, gelöschte = unknown (Empf. so lassen)
143. [ ] step=1d nach billing.timezone (Empf. ja)
144. [ ] deltaPoints für Quoten zusätzlich (Empf. ja)
145. [ ] fremde Einzelanfrage 403 (Empf. 403)
146. [ ] Anfragen ohne Team für Org-Rollen sichtbar – mit F3 (Org Pflicht) möglich (Empf. jetzt ja)
147. [ ] clientIp nur admin/auditor (Empf. ja)

## AIGW-61 Webhook (angehängt)
148. [ ] Webhook failMode Standard closed + UI-Anzeige (Empf. ja)
149. [ ] Eigene Ressource Secret/WebhookSecret statt ProviderCredential (Empf. ja)
150. [ ] EventSubscription als Ressource zusätzlich zur Startkonfig (Empf. später)
151. [ ] Dedup höchstens-einmal; Outbox später für kritische Ereignisse (Empf. ja)
152. [ ] reject mit exaktem Status (ApiError optionaler Status) (Empf. ja)
153. [ ] mTLS je Bindung nach MVP (Empf. ja)
154. [ ] Budget-Ereignisse mit Namen anreichern (Empf. ja)
- AIGW-41-Folgen (eigene Tickets): Plugin-Kette auf MCP-Tool-Args/Ergebnisse; Admin-Route MCP-Upstream-Status; OAuth für MCP-Upstreams; stdio-E2E + Interop mit offiziellen Clients

## AIGW-41 MCP (angehängt)
155. [ ] Team-Freigabe Obergrenze, Key schränkt ein, Ausschluss gewinnt (Empf. ja)
156. [ ] Ausschlüsse als !muster in tools (Empf. ja)
157. [ ] MCP-Sessions: Sticky Sessions dokumentiert, geteilter Speicher später (Empf. ja)
158. [ ] Upstream-Abbruch bei Client-Verbindungsabbruch (Empf. ja, Resume später)
159. [ ] Rechte für Ressourcen/Prompts nur Server-Ebene oder feiner? (Empf. Server-Ebene)
- AIGW-37-Folgen: SICHERHEIT – Meta-Schlüssel aigw.* (Pseudonym-Map) nie an externe Plugins/Webhook senden; Registry::validate in Admin-API + config validate; guardrails-Spalte im request_log; runtime-smoke Guardrails

## AIGW-37/38 Guardrails (angehängt)
160. [ ] Ausgabe-Ablehnung 400 statt 200+content_filter (Empf. 400, Option später)
161. [ ] Guardrail-Zähler in request_log (JSON-Spalte) (Empf. ja, Folgeticket)
162. [ ] aigw.*-Meta für externe Plugins ausblenden (Empf. ja – Sicherheit)
163. [ ] Satzmodus sentenceOnError vorerst (Empf. ja)
164. [ ] Entropie-Heuristik an, je Bindung abschaltbar (Empf. ja)
165. [ ] Plugin-Bindungen beim Speichern validieren (Empf. ja) – vgl. 118
166. [ ] content-safety 3 s + closed, UI-Anzeige (Empf. ja)
- F42: Admin-Listen mit SQL-Pagination (Keyset-Cursor, Filter in SQL) – im Store-Umbau F7
- F44: settings.timeouts je Modellgruppe (connect/firstByte/idle/total) – prüfen, ob durch AIGW-24 bereits vorhanden
- F46: Crate aigw-tokenizer: BPE (o200k/cl100k eingebettet), HF tokenizer.json (Llama/Mistral), Heuristik mit Sprachfaktor; genutzt für Abbruch-Usage + Limit-Reservierung (Prompt-Schätzung)
- F47: RequestRecord/request_log.error_class (feine Klasse), Log-Explorer-Filter, analytics groupBy=error_class
- F48: upstream.roots (webpki|system|both) + ca_file global/je Deployment (settings.caFile), einheitlich mit OIDC-Client
- F49: settings.responseModel upstream|requested je Modellgruppe, Header x-aigw-model immer
