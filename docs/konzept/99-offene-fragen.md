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
51. [x] (= F4, ignore)
52. [x] GitOps-Secrets Admin-API: A nur AIGW_SECRET_* + optional gitops.secret_dir für file: (Umsetzung nötig)
53. [x] Nur-YAML: A 403 read-only, config:apply erlaubt
54. [x] valueFrom-Verweise mitspeichern (unverschlüsselt) für originalgetreuen Export: A (Umsetzung nötig, mit F7)
55. [x] Rolle gitops: A nur config:export/apply (+dry-run) (Umsetzung nötig)
56. [x] auth/info + readOnly + bootstrap, UI blendet aus + Bootstrap-Warnbanner: A (Umsetzung nötig)

## AIGW-16/17/20 Anthropic/Ollama/Mistral
57. [x] Anthropic 429: A RateLimit, Quota nur billing_error
58. [x] reasoningMode: A Standard adaptive
59. [x] Thinking-Budget: C je Deployment wählbar (raise-max-tokens Std. | cap-budget) (Umsetzung nötig)
60. [x] anthropic-beta: B settings.clientBetas allow-all|allowlist|deny (Std. allow-all) (Umsetzung nötig)
61. [x] Mistral/Magistral: A Folgepunkt – Denkblöcke → IR-Reasoning, reasoning_effort abbilden (Umsetzung nötig)

## AIGW-65 Admin-API Modelle
62. [x] (= F52; REST-CRUD ohne valueFrom)
63. [x] SSRF nach DNS: A auch Anbieter-Client, gemeinsamer Outbound-Baustein für alle ausgehenden Verbindungen (Umsetzung nötig)
64. [x] (= F45)
65. [x] Credentials: B Kompatibilitätsliste (openai → openai-compatible/vllm/ollama) (Umsetzung nötig)
66. [x] Löschen von Modellgruppen mit Freigaben blockieren (409) (Empf. ja)  → Empfehlung übernommen (2026-11-13)
67. [x] Rückwirkende Preise nur mit backfill-Flag (Empf. optional)  → Empfehlung übernommen (2026-11-13)
68. [x] Fehlerhafte Preisversion nur per neuer Version korrigieren (Empf. ja)  → Empfehlung übernommen (2026-11-13)
69. [x] Fallback-Zyklen ablehnen + Tiefenlimit (Empf. ja)  → Empfehlung übernommen (2026-11-13)
70. [x] Prune darf gültige Preisversionen löschen (Empf. ja)  → Empfehlung übernommen (2026-11-13)
71. [x] Ollama ohne Endpoint braucht localhost-Freischaltung (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-18/19 Bedrock/Vertex
72. [x] Neue WireFormat-Varianten (Empf. ja)  → Empfehlung übernommen (2026-11-13)
73. [x] Provider::prepare-Hook (Empf. ja)  → Empfehlung übernommen (2026-11-13)
74. [x] thoughtSignature in Tool-Call-ID (Empf. ja)  → Empfehlung übernommen (2026-11-13)
75. [x] Vertex-Location Standard global (Empf.)  → Empfehlung übernommen (2026-11-13)
76. [x] Volles JSON-Schema für Gemini (Empf. ja)  → Empfehlung übernommen (2026-11-13)
77. [x] Eine Anbieterart vertex für beide Google-APIs (Empf. ja)  → Empfehlung übernommen (2026-11-13)
78. [x] Gemeinsamer HTTP-Client für Token-Abrufe (Empf. ja)  → Empfehlung übernommen (2026-11-13)
79. [x] Bedrock http(s)-Bild-URLs ablehnen (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-24–26 Routing
80. [x] Fallback: B nur auf freigegebene Gruppen, sonst überspringen (Header x-aigw-fallback-skipped), Ausnahme settings.fallbackIgnoresGrants (Umsetzung nötig)
81. [x] Nur Kette der Ursprungsgruppe (Empf. ja)  → Empfehlung übernommen (2026-11-13)
82. [x] Stream-Header erst nach erstem Event (Empf. ja)  → Empfehlung übernommen (2026-11-13)
83. [x] Alle gesperrt wegen Retry-After/Kapazität → 429 statt 503 (Empf. ja)  → Empfehlung übernommen (2026-11-13)
84. [x] Breaker-Standardwerte (Empf. ja)  → Empfehlung übernommen (2026-11-13)
85. [x] mock im Produktivbetrieb erlaubt, sperrbar (Empf.)  → Empfehlung übernommen (2026-11-13)
86. [x] attempts im request_log + Metriken (Empf. Folgeticket)  → Empfehlung übernommen (2026-11-13)

## AIGW-32 OIDC/Rollen
87. [x] Höchstlaufzeit Admin-Keys konfigurierbar (Empf.)  → Empfehlung übernommen (2026-11-13)
88. [x] Admin-Keys nur globale Admins (Empf. ja)  → Empfehlung übernommen (2026-11-13)
89. [x] org-admin darf eigene Org nicht ändern (Empf. ja)  → Empfehlung übernommen (2026-11-13)
90. [x] OIDC-Verknüpfung: B provision_users an, link_by_email aus (keine E-Mail-Verknüpfung als Standard) – Verknüpfung über unveränderliche ID: iss+sub, bei Entra tid+oid (konfigurierbarer identity_claim), Einladungslink (Umsetzung nötig)
91. [x] Bootstrap aus erst bei globaler Admin-Rolle (Empf. ja)  → Empfehlung übernommen (2026-11-13)
92. [x] IdP-Gruppen erst beim nächsten Login (Empf. ja)  → Empfehlung übernommen (2026-11-13)
93. [x] Eingeschränkte Rollen sehen Credential-Metadaten (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-44/45 Beobachtung
94. [x] OTLP eigen statt opentelemetry-Crates (Empf. ja)  → Empfehlung übernommen (2026-11-13)
95. [x] nur HTTP/protobuf, gzip später (Empf. ja)  → Empfehlung übernommen (2026-11-13)
96. [x] sampling_ratio 1.0 wenn an (Empf.)  → Empfehlung übernommen (2026-11-13)
97. [x] parent_based true (Empf.)  → Empfehlung übernommen (2026-11-13)
98. [x] traceparent: B Standard je Anbietertyp (an: vllm/ollama/openai-compatible; aus: externe Cloud), settings.propagateTraceContext je Deployment (Umsetzung nötig)
99. [x] deployment-Label als Name (Empf. ja)  → Empfehlung übernommen (2026-11-13)
100. [x] trace_id im Request-Log (Empf. ja)  → Empfehlung übernommen (2026-11-13)
101. [x] gen_ai.provider.name zusätzlich (Empf. ja)  → Empfehlung übernommen (2026-11-13)
102. [x] OTLP-Retry ja, App-Logs eigenes Ticket (Empf.)  → Empfehlung übernommen (2026-11-13)
103. [x] OTel-Collector-Gegentest: A freigegeben – otel/opentelemetry-collector-contrib (feste Version) in check.sh über Schalter (Umsetzung nötig)

## AIGW-30/31 Limits/Budgets
104. [x] budget_exceeded 429 statt 402 (Empf. 429)  → Empfehlung übernommen (2026-11-13)
105. [x] Retry-After bis nächster Zeitraum (Empf. ja)  → Empfehlung übernommen (2026-11-13)
106. [x] Budgets: B Kosten vorab reservieren, enforcement strict (Std.)|soft je Budget (Umsetzung nötig)
107. [x] Zeitzone global (Empf. ja)  → Empfehlung übernommen (2026-11-13)
108. [x] Ersatzpreise cached→input, reasoning→output (Empf. ja)  → Empfehlung übernommen (2026-11-13)
109. [x] Deployment-TPM vorab reservieren (Empf. ja)  → Empfehlung übernommen (2026-11-13)
110. [x] Budget-Warnungen dedupliziert mit Webhook (in AIGW-61 beauftragt)  → Empfehlung übernommen (2026-11-13)
111. [x] Fallback verbraucht kein Kontingent der Zielgruppe (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-66 Audit
112. [x] Eingeschränkte admin/auditor sehen Bereich, viewer nichts (Empf. ja)  → Empfehlung übernommen (2026-11-13)
113. [x] Audit beim Login: B admin.auth.audit_required (Std. true = fail-closed) (Umsetzung nötig)
114. [x] Fehlgeschlagene Bearer-Logins nur Metrik (Empf. ja)  → Empfehlung übernommen (2026-11-13)
115. [x] Arten-Schreibweise vereinheitlichen (Empf. ja)  → Empfehlung übernommen (2026-11-13)
116. [x] Aufbewahrung + Hash-Kette als nächstes Paket (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-36 Plugins
117. [x] Steuerfelder als eigene Spec-Felder (Empf. ja, Folgeticket)  → Empfehlung übernommen (2026-11-13)
118. [x] Bindungen beim Schreiben validieren (Empf. ja)  → Empfehlung übernommen (2026-11-13)
119. [x] order über alle Ebenen (Empf. ja)  → Empfehlung übernommen (2026-11-13)
120. [x] Modell-Umleitung gesperrt (Empf. ja)  → Empfehlung übernommen (2026-11-13)
121. [x] Plugin-Respond ohne Usage verbraucht keine Token-Limits (Empf. ja)  → Empfehlung übernommen (2026-11-13)

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
122. [x] Ohne settings.capabilities nur Chat (Empf. ja)  → Empfehlung übernommen (2026-11-13)
123. [x] capabilities später typisiertes Feld (Empf. ja)  → Empfehlung übernommen (2026-11-13)
124. [x] Responses zustandslos, store fehlt = false, native optional (Empf. ja)  → Empfehlung übernommen (2026-11-13)
125. [x] Bild-URLs nie nachladen (Empf. ja)  → Empfehlung übernommen (2026-11-13)
126. [x] Audio-Limit 25 MiB (Empf. ja)  → Empfehlung übernommen (2026-11-13)
127. [x] Bedrock Titan 256 Eingaben, 8 parallel (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-67 Team-Grenzen (angehängt)
128. [x] force für globalen Admin (Empf. ja)  → Empfehlung übernommen (2026-11-13)
129. [x] kein force/cascade in GitOps (Empf. ja, später --allow-bound-violations)  → Empfehlung übernommen (2026-11-13)
130. [x] verschiedene Zeiträume nicht vergleichen, ggf. Warnung (Empf. ja)  → Empfehlung übernommen (2026-11-13)
131. [x] gesperrte/abgelaufene Keys zählen mit (Empf. ja)  → Empfehlung übernommen (2026-11-13)
132. [x] nur neue Verstöße blockieren (Empf. ja)  → Empfehlung übernommen (2026-11-13)
133. [x] Audit-Format force (Empf. so lassen)  → Empfehlung übernommen (2026-11-13)
134. [x] clamp ohne Modelle → 409 (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-49 UI (angehängt)
135. [x] D7 Mock-Schalter per admin.ui.mock ausblendbar (Empf. ja)  → Empfehlung übernommen (2026-11-13)
136. [x] D8 Detailseiten statt Dialoge (Empf. ja, mit 46/66)  → Empfehlung übernommen (2026-11-13)
137. [x] D9 strukturierte Formulare für Routing-Settings (Empf. ja)  → Empfehlung übernommen (2026-11-13)
138. [x] D10 Routing-Regeln entfallen (Empf. ja)  → Empfehlung übernommen (2026-11-13)
139. [x] D11 serverseitige Pagination ab ~1000 (Empf. ja)  → Empfehlung übernommen (2026-11-13)
140. [x] (= F56)
- AIGW-46-Folge: Proxy füllt RequestRecord.client_request_id aus x-request-id/x-client-request-id (F11)

## AIGW-46 Analytics (angehängt)
141. [x] 24h/5m auf SQLite 380 ms – 5-Min-Rollups erst bei Bedarf / Auto-Step 15 min (Empf.)  → Empfehlung übernommen (2026-11-13)
142. [x] provider über aktuelle Deployment-Konfig, gelöschte = unknown (Empf. so lassen)  → Empfehlung übernommen (2026-11-13)
143. [x] step=1d nach billing.timezone (Empf. ja)  → Empfehlung übernommen (2026-11-13)
144. [x] deltaPoints für Quoten zusätzlich (Empf. ja)  → Empfehlung übernommen (2026-11-13)
145. [x] fremde Einzelanfrage 403 (Empf. 403)  → Empfehlung übernommen (2026-11-13)
146. [x] Anfragen ohne Team für Org-Rollen sichtbar – mit F3 (Org Pflicht) möglich (Empf. jetzt ja)  → Empfehlung übernommen (2026-11-13)
147. [x] clientIp nur admin/auditor (Empf. ja)  → Empfehlung übernommen (2026-11-13)

## AIGW-61 Webhook (angehängt)
148. [x] failMode: B content-safety closed, webhook open (je Bindung überschreibbar), UI-Anzeige + Metrik/Alarm (Umsetzung nötig) – gilt auch für 166
149. [x] Eigene Ressource Secret/WebhookSecret statt ProviderCredential (Empf. ja)  → Empfehlung übernommen (2026-11-13)
150. [x] EventSubscription als Ressource zusätzlich zur Startkonfig (Empf. später)  → Empfehlung übernommen (2026-11-13)
151. [x] Dedup höchstens-einmal; Outbox später für kritische Ereignisse (Empf. ja)  → Empfehlung übernommen (2026-11-13)
152. [x] reject mit exaktem Status (ApiError optionaler Status) (Empf. ja)  → Empfehlung übernommen (2026-11-13)
153. [x] mTLS je Bindung nach MVP (Empf. ja)  → Empfehlung übernommen (2026-11-13)
154. [x] Budget-Ereignisse mit Namen anreichern (Empf. ja)  → Empfehlung übernommen (2026-11-13)
- AIGW-41-Folgen (eigene Tickets): Plugin-Kette auf MCP-Tool-Args/Ergebnisse; Admin-Route MCP-Upstream-Status; OAuth für MCP-Upstreams; stdio-E2E + Interop mit offiziellen Clients

## AIGW-41 MCP (angehängt)
155. [x] Team-Freigabe Obergrenze, Key schränkt ein, Ausschluss gewinnt (Empf. ja)  → Empfehlung übernommen (2026-11-13)
156. [x] Ausschlüsse als !muster in tools (Empf. ja)  → Empfehlung übernommen (2026-11-13)
157. [x] MCP-Sessions: Sticky Sessions dokumentiert, geteilter Speicher später (Empf. ja)  → Empfehlung übernommen (2026-11-13)
158. [x] Upstream-Abbruch bei Client-Verbindungsabbruch (Empf. ja, Resume später)  → Empfehlung übernommen (2026-11-13)
159. [x] Rechte für Ressourcen/Prompts nur Server-Ebene oder feiner? (Empf. Server-Ebene)  → Empfehlung übernommen (2026-11-13)
- AIGW-37-Folgen: SICHERHEIT – Meta-Schlüssel aigw.* (Pseudonym-Map) nie an externe Plugins/Webhook senden; Registry::validate in Admin-API + config validate; guardrails-Spalte im request_log; runtime-smoke Guardrails

## AIGW-37/38 Guardrails (angehängt)
160. [x] Ausgabe-Ablehnung: B wie Azure – 200 + finish_reason content_filter / stop_reason refusal + Header x-aigw-guardrail; Eingabe bleibt 400; je Bindung auf 400 umstellbar (Umsetzung nötig)
161. [x] Guardrail-Zähler in request_log (JSON-Spalte) (Empf. ja, Folgeticket)  → Empfehlung übernommen (2026-11-13)
162. [x] aigw.*-Meta für externe Plugins ausblenden (Empf. ja – Sicherheit)  → Empfehlung übernommen (2026-11-13)
163. [x] Satzmodus sentenceOnError vorerst (Empf. ja)  → Empfehlung übernommen (2026-11-13)
164. [x] Entropie-Heuristik an, je Bindung abschaltbar (Empf. ja)  → Empfehlung übernommen (2026-11-13)
165. [x] Plugin-Bindungen beim Speichern validieren (Empf. ja) – vgl. 118  → Empfehlung übernommen (2026-11-13)
166. [x] content-safety 3 s + closed, UI-Anzeige (Empf. ja)  → Empfehlung übernommen (2026-11-13)
- F42: Admin-Listen mit SQL-Pagination (Keyset-Cursor, Filter in SQL) – im Store-Umbau F7
- F44: settings.timeouts je Modellgruppe (connect/firstByte/idle/total) – prüfen, ob durch AIGW-24 bereits vorhanden
- F46: Crate aigw-tokenizer: BPE (o200k/cl100k eingebettet), HF tokenizer.json (Llama/Mistral), Heuristik mit Sprachfaktor; genutzt für Abbruch-Usage + Limit-Reservierung (Prompt-Schätzung)
- F47: RequestRecord/request_log.error_class (feine Klasse), Log-Explorer-Filter, analytics groupBy=error_class
- F48: upstream.roots (webpki|system|both) + ca_file global/je Deployment (settings.caFile), einheitlich mit OIDC-Client
- F49: settings.responseModel upstream|requested je Modellgruppe, Header x-aigw-model immer
- F52: gitops.secret_dir – file:-Verweise über Admin-API nur innerhalb dieses Verzeichnisses (Pfad-Normalisierung, keine Symlinks nach außen)
- F54: provider_credential speichert valueFrom-Verweise je Feld (env/file-Name), Export gibt sie originalgetreu aus
- F55: Rolle gitops (RBAC), nur config:export/config:apply; Admin-Keys mit dieser Rolle für CI
- F56: GET /auth/info liefert readOnly + bootstrap; UI: Schreibaktionen im Nur-YAML-Modus ausblenden, Warnbanner bei aktivem Bootstrap
- F59: settings.thinkingBudgetPolicy raise-max-tokens (Std.) | cap-budget je Deployment
- F60: settings.clientBetas (allow-all|allowlist|deny) + betaAllowlist je Modellgruppe
- F61: Mistral-Adapter: Magistral-content-Blöcke (thinking) → IR-Reasoning, reasoning_effort/prompt_mode abbilden
- F63: Gemeinsamer Outbound-Baustein (Resolver mit ip_allowed, keine Redirects, Größenlimits) für Provider/Webhook/MCP/OIDC/Content-Safety; Provider-Client umstellen
- F65: Kompatibilitätsliste Credential-Typ ↔ Deployment-Anbieter in aigw_core::validate (Admin + GitOps)

## Umsetzungspunkte aus übernommenen Empfehlungen (66–166)
- F66: Modellgruppe/Alias löschen → 409 mit verweisenden Teams/Keys, ?cascade=remove (Audit)
- F69: Fallback-Tiefenlimit (max. 8, bereits in AIGW-24) – prüfen
- F83: alle Deployments nur wegen Retry-After/Kapazität gesperrt → 429 mit Retry-After statt 503
- F85: mock-Anbieter per Konfig sperrbar (admin.allow_mock), UI-Kennzeichnung
- F86: attempts-Metriken (Retries/Fallbacks/Breaker-Zustand) in /metrics
- F87: admin.auth.max_admin_key_lifetime (optional)
- F99: Prometheus-Label deployment = Name statt ID
- F100: trace_id im request_log + Link Log↔Trace
- F101: gen_ai.provider.name zusätzlich zu gen_ai.system
- F102: OTLP-Export Retry mit Backoff bei 429/503
- F108: Schema-Kommentar Preise: Ersatzpreise cached→input, reasoning→output
- F115: Audit-Arten-Schreibweise vereinheitlichen (kebab-case)
- F116: Paket Audit-Aufbewahrung + Hash-Kette
- F117/F118/F165: Plugin-Steuerfelder als Spec-Felder; Registry::validate in Admin-API + GitOps
- F123: ModelGroupSpec.capabilities typisiert (mit UI)
- F135–F139: UI D7–D11 (admin.ui.mock, Detailseiten, strukturierte Routing-Formulare, serverseitige Pagination)
- F141: Auto-Step bei 24h → 15 min
- F143/F144/F146/F147: Analytics 1d nach billing.timezone, deltaPoints, Org-Rollen sehen teamlose Keys ihrer Org (F3), clientIp nur admin/auditor
- F149/F152/F154: Ressource Secret (verschlüsselt, GitOps valueFrom); ApiError mit optionalem exaktem Status; Budget-Ereignisse mit Namen
- F161/F162/F164: guardrails-Spalte request_log; aigw.*-Meta nie an externe Plugins (läuft in AIGW-37/38); entropy.enabled je Bindung
- F80: Fallback prüft Freigaben (Key/Team) der Zielgruppe; Ausnahme settings.fallbackIgnoresGrants; Header x-aigw-fallback-skipped
- F90: OIDC-Identität = (issuer, identity_claim); Standard sub, Profil entra: oid + tid (Tenant-Prüfung); Nutzer vorab mit dieser ID anlegbar (UI/API/GitOps); Einladungslink (einmalig, Ablauf); link_by_email Standard aus
- F98: propagate-Standard je ProviderKind (intern an, Cloud aus), Override settings.propagateTraceContext

## AIGW-39/40 Cache – Empfehlungen übernommen (2026-11-13)
167. [x] Cache als Pipeline-Schritt statt Plugin-Respond → Empfehlung übernommen
168. [x] Usage im Body bei Treffer = Original, Header kennzeichnet Treffer → Empfehlung übernommen
169. [x] Embedding-Kosten des semantischen Caches auf den Key buchen → Empfehlung übernommen (Umsetzung nötig)
170. [x] Partition über ganzen Verlauf → Empfehlung übernommen
171. [x] fehlende temperature = nicht cachen → Empfehlung übernommen
172. [x] Warmstart-Persistenz Standard an (verschlüsselt) → Empfehlung übernommen
173. [x] check.sh mit pgvector/redis-stack-Images → Empfehlung übernommen; Redis-Stack-Image auf feste Version pinnen (Umsetzung nötig)
174. [x] Cache für Embeddings/Responses als Folgeaufgabe → Empfehlung übernommen
175. [x] ef_search 64 Standard, 128 dokumentiert → Empfehlung übernommen
- F169/F173/F174: Embedding-Kosten des Caches auf Key buchen; redis-stack-server-Image versionieren; Cache für Embeddings (exakt) + Responses
- F103: check.sh: AIGW_TEST_OTELCOL=docker startet otel/opentelemetry-collector-contrib:<feste Version>, Export von Traces/Logs prüfen (Collector-Debug-Exporter/Datei)
- F106: Budget-Reservierung vor Upstream (geschätzte Kosten aus Prompt-Tokens+max_tokens×Preis), Abgleich nach Usage; Budget.enforcement strict|soft (Std. strict); Redis exakt, sonst Abgleich-Takt
- F113: admin.auth.audit_required (Std. true): Login/Logout scheitert, wenn Audit-Eintrag nicht geschrieben werden kann (gleiche Transaktion wie Session)
167b. [x] Antwort-Haken: A Zwiebelprinzip – Antwort-/Stream-Haken in umgekehrter Reihenfolge (Umsetzung nötig, Sicherheit)
- F148: Standard-failMode je Plugin-Art (content-safety closed, webhook open), UI zeigt failMode, Metrik aigw_plugin_failmode_triggered_total{plugin,mode}
- F160: Ausgabe-Guardrails beenden mit content_filter/refusal (200, auch Stream sauber), Header x-aigw-guardrail; Bindung outputRejectMode: finish|error
- F167b: Plugin-Kette: on_response/on_chunk/on_stream_end in umgekehrter order (Zwiebel), Test pii→webhook sieht nur Pseudonyme

## AIGW-72 Outbound – Empfehlungen übernommen (2026-11-13)
176. [x] OIDC nutzt upstream.roots (Std. webpki) + oidc.ca_file – Release-Note → Empfehlung übernommen
177. [x] settings.caFile nur aus upstream.ca_dir (wie gitops.secret_dir) → Empfehlung übernommen (Umsetzung nötig, Sicherheit)
178. [x] gesperrtes Anbieter-Ziel = ErrorClass::Connect → Empfehlung übernommen
179. [x] OTLP-Retry darf Writer verzögern, Entkopplung später → Empfehlung übernommen
180. [x] Collector-Test UID 10001 erst bei CI angehen → Empfehlung übernommen
- F177: upstream.ca_dir – settings.caFile nur innerhalb (Pfad-Normalisierung, keine Symlinks nach außen)

## AIGW-69 – Empfehlungen übernommen (2026-11-13)
181. [x] Usage nach Live-Stream-Abbruch per Tokenizer (F46) → Empfehlung übernommen
182. [x] Kein Header im Live-Stream, nur finish_reason/stop_reason → Empfehlung übernommen
183. [x] Plugin-Bindungen je Scope (Org/Team-Admins, locked respektiert) → Folgeticket (Umsetzung nötig)
184. [x] Reine Stream-Plugins im gepufferten Modus nach Antwort-Haken → Empfehlung übernommen
- F183: Plugin-Bindungen mit RBAC-Ziel je Scope-Ebene (org-admin/team-admin), locked von oben respektiert
