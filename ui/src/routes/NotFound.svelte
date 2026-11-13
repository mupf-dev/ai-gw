<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { href, route } from '../lib/router.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
</script>

<h1 tabindex="-1" class="sr-only">Seite nicht gefunden</h1>
<Card>
  <EmptyState icon="search" title="Seite nicht gefunden" text="Unter {route.path} gibt es keine Seite.">
    <Button href={href('/')} variant="primary" icon="home">Zur Übersicht</Button>
  </EmptyState>
</Card>
