<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Statt einer Verwaltungsseite, solange ohne Mock-Modus keine Anmeldung besteht (App.svelte: needsAuth).
  import AuthGate from '../lib/admin/AuthGate.svelte';
  import { pageForPath } from '../lib/nav';
  import { route } from '../lib/router.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';

  const page = $derived(pageForPath(route.path));
</script>

<PageHeader title={page?.label ?? 'Anmeldung'} description={page?.description} />
<AuthGate>{#snippet children()}{/snippet}</AuthGate>
