<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Leerer Zustand: Icon, Titel, erklärender Text, nächste Aktion(en). Auch für „noch nicht verfügbar“.
  import type { Snippet } from 'svelte';
  import Icon, { type IconName } from './Icon.svelte';

  let {
    icon = 'box',
    title,
    text,
    compact = false,
    children,
  }: { icon?: IconName; title: string; text?: string; compact?: boolean; children?: Snippet } = $props();
</script>

<div class={['flex flex-col items-center text-center', compact ? 'gap-2 px-4 py-8' : 'gap-3 px-6 py-16']}>
  <div class="grid size-11 place-items-center rounded-lg border border-line bg-surface-2 text-fg-muted">
    <Icon name={icon} size={20} />
  </div>
  <div class="max-w-md">
    <h3 class="text-md font-semibold text-fg">{title}</h3>
    {#if text}<p class="mt-1 text-base text-fg-muted">{text}</p>{/if}
  </div>
  {#if children}<div class="mt-1 flex flex-wrap justify-center gap-2">{@render children()}</div>{/if}
</div>
