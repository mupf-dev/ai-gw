<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Overview: im Mock-Modus die Musterseite mit Beispieldaten (AIGW-48), sonst die echte Seite gegen die Admin-API
  // (AIGW-77). Beide als eigener Chunk – die Mock-Daten werden ohne Mock-Modus nicht geladen.
  import { isMock } from '../lib/mode.svelte';
  const load = () => (isMock() ? import('./overview/OverviewMock.svelte') : import('./overview/OverviewReal.svelte'));
</script>

{#await load() then mod}
  {@const Page = mod.default}
  <Page />
{/await}
