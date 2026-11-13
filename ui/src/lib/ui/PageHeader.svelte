<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Seitenkopf: Titel (h1, erhält Fokus nach Navigation), Beschreibung, Aktionen rechts.
  import type { Snippet } from 'svelte';

  let {
    title,
    description,
    badge,
    actions,
  }: { title: string; description?: string; badge?: Snippet; actions?: Snippet } = $props();
</script>

<div class="mb-6 flex flex-col gap-x-6 gap-y-3 md:flex-row md:items-end">
  <div class="min-w-0 flex-1">
    <div class="flex items-center gap-2">
      <h1 tabindex="-1" class="text-xl font-semibold tracking-tight text-fg">{title}</h1>
      {@render badge?.()}
    </div>
    {#if description}<p class="mt-1 max-w-3xl text-base text-fg-muted">{description}</p>{/if}
  </div>
  {#if actions}<div class="flex flex-wrap items-center gap-2">{@render actions()}</div>{/if}
</div>
