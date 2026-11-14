<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: CC-BY-4.0 -->
# WIP AIGW-86 – nächster Schritt

- Läuft/lief: `check.sh` (STRICT, Postgres+Redis) mit `--no-fail-fast` und Skip von
  `serve::tests::slow_body_is_cut_by_gap_and_by_min_rate` (zeitkritisch, scheitert unter Last in
  `cargo test --workspace`, einzeln grün; nicht von AIGW-86 berührt), danach runtime-smoke und
  `ui/e2e/run.sh` mit `AIGW_E2E_PORT=4986` (neuer Test `ui/e2e/tests/aigw86.spec.js`).
  Ergebnis: /tmp/p986-check.log, /tmp/p986-smoke.log, /tmp/p986-e2e.log auf der Box.
- Danach: Screenshots `ui/e2e/results/screens/echt-guardrail-profil*.png` nach `docs/konzept/ux/`
  übernehmen (optional), auf aktuelles main rebasen, Check grün, diese Datei löschen.
- Bereits grün: neue Admin-API-Tests `guardrail_profiles_api` (2), Unit-Tests
  `guardrail_profiles` (5), svelte-check 0 Fehler, OpenAPI/schema.d.ts neu erzeugt.
