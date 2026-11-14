# WIP AIGW-85 Preisvorschläge – nächste Schritte

Stand: Backend (aigw-billing::suggest, Admin-API /price-suggestions[:accept], Konfig billing.price_suggestions,
settings.pricing-Validierung, Binary-Verdrahtung, periodischer Abgleich), Katalog crates/aigw-billing/catalog/prices.yaml,
scripts/price-catalog-check.py, UI (prices/PriceSuggestions.svelte), e2e (aigw85.spec.js + fake-retail.py in run.sh),
Doku (11 §4a, 08, betrieb/konfiguration), CHANGELOG, THIRD-PARTY.

Billing-Unit-Tests grün. Offen:
1. Admin-Test (crates/aigw-admin/tests/price_suggestions_api.rs) + clippy + `cargo fmt --all` auf der Box laufen lassen
   (letzter Fix: BTreeMap-Schlüssel &str statt ProviderKind), Fehler beheben.
2. `scripts/gen-api.sh` (ui/openapi.json + schema.d.ts neu) per --fetch holen; UI `pnpm check`/build.
3. Volle Abnahme: AIGW_STRICT=1 AIGW_TEST_POSTGRES=docker AIGW_TEST_REDIS=docker scripts/check.sh,
   scripts/runtime-smoke.sh, AIGW_E2E_PORT=<frei> bash ui/e2e/run.sh.
4. Danach WIP-NEXT.md löschen, auf main rebasen, Bericht.

Echte Retail-API (lokal geprüft, swedencentral, USD/1M): gpt-5.4-mini Global in 0.75 / cached 0.075 / out 4.5,
DataZone 0.825 / 0.0825 / 4.95. Claude (sonnet-5-5) nicht in der Retail-API (Foundry-Claude über Marketplace zu
Anthropic-Listenpreisen) → Katalog-Ersatz.
