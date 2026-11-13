<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Platzhalter-Wortmarke: Tor-Symbol (zwei Pfeiler, ein Durchgang) in Akzentfarbe + „ai-gw“.
  let { compact = false }: { compact?: boolean } = $props();
</script>

<span class="inline-flex items-center gap-2">
  <svg viewBox="0 0 24 24" class="size-6 shrink-0" aria-hidden="true">
    <rect x="1" y="1" width="22" height="22" rx="5" class="fill-accent" />
    <path d="M7 18V9.5a5 5 0 0 1 10 0V18" class="fill-none stroke-on-accent" stroke-width="2.2" stroke-linecap="round" />
    <path d="M12 12.5v5.5" class="fill-none stroke-on-accent" stroke-width="2.2" stroke-linecap="round" />
  </svg>
  {#if !compact}
    <span class="flex flex-col leading-none">
      <span class="text-md font-semibold tracking-tight text-fg">ai-gw</span>
      <span class="mt-0.5 text-2xs text-fg-subtle">AI Gateway</span>
    </span>
  {/if}
</span>
