<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Logs: im Mock-Modus die Musterseite mit Beispieldaten (AIGW-48), sonst die echte Seite gegen die Admin-API
  // (AIGW-77). Beide als eigener Chunk – die Mock-Daten werden ohne Mock-Modus nicht geladen.
  import { isMock } from '../lib/mode.svelte';
  const load = () => (isMock() ? import('./logs/LogsMock.svelte') : import('./logs/LogsReal.svelte'));
</script>

{#await load() then mod}
  {@const Page = mod.default}
  <Page />
{/await}
