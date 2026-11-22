<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Karte mit optionalem Kopf (Titel, Beschreibung, Aktionen). `flush`: Inhalt ohne Innenabstand (Tabellen).
  import type { Snippet } from 'svelte';
  import { card } from './styles';

  let {
    title,
    description,
    actions,
    flush = false,
    class: cls = '',
    children,
    testid,
  }: {
    title?: string;
    description?: string;
    actions?: Snippet;
    flush?: boolean;
    class?: string;
    children: Snippet;
    testid?: string;
  } = $props();
</script>

<section class={[card, 'flex min-w-0 flex-col', cls]} data-testid={testid}>
  {#if title || actions}
    <header class="flex flex-wrap items-center gap-x-3 gap-y-2 border-b border-line px-4 py-3">
      <div class="min-w-0 flex-1">
        {#if title}<h2 class="text-base font-semibold text-fg">{title}</h2>{/if}
        {#if description}<p class="text-sm text-fg-muted">{description}</p>{/if}
      </div>
      {@render actions?.()}
    </header>
  {/if}
  <div class={['min-w-0 flex-1', !flush && 'p-4']}>{@render children()}</div>
</section>
