<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Platzhalter für Seiten aus dem Katalog, die noch keine Musterseite haben.
  import { pageForPath } from '../lib/nav';
  import { href, route } from '../lib/router.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';

  const page = $derived(pageForPath(route.path));
</script>

{#if page}
  <PageHeader title={page.label} description={page.description}>
    {#snippet badge()}<Badge tone="outline">in Planung</Badge>{/snippet}
  </PageHeader>
  <Card>
    <EmptyState icon={page.icon} title="Diese Seite folgt" text="Inhalt und Aufbau sind im UX-Konzept (docs/konzept/04-ux-designsystem.md, Seitenkatalog) beschrieben.">
      <Button href={href('/')} variant="secondary" icon="home">Zur Übersicht</Button>
    </EmptyState>
  </Card>
{/if}
