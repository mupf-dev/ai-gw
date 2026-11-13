<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Navigationsinhalt der Seitenleiste (Desktop fest, mobil im Sheet). Läuft in .scheme-dark.
  import { NAV, NAV_FOOTER, type Page } from '../nav';
  import { isGlobalAdmin } from '../rbac.svelte';
  import { href, route } from '../router.svelte';
  import Icon from '../ui/Icon.svelte';
  import Logo from './Logo.svelte';

  let { onnavigate }: { onnavigate?: () => void } = $props();
  const active = (p: Page) => (p.path === '/' ? route.path === '/' : route.path === p.path || route.path.startsWith(`${p.path}/`));
</script>

{#snippet link(p: Page)}
  <a
    href={href(p.path)}
    onclick={onnavigate}
    aria-current={active(p) ? 'page' : undefined}
    class="group relative flex h-8 items-center gap-2.5 rounded-md px-2.5 text-base text-fg-muted transition-colors hover:bg-surface-hover hover:text-fg aria-[current=page]:bg-accent-soft aria-[current=page]:font-medium aria-[current=page]:text-accent-soft-fg"
  >
    <span
      class="absolute top-1.5 bottom-1.5 -left-2 w-0.5 rounded-full bg-accent opacity-0 group-aria-[current=page]:opacity-100"
      aria-hidden="true"
    ></span>
    <Icon name={p.icon} size={16} />
    <span class="flex-1 truncate">{p.label}</span>
    {#if !p.ready}<span class="text-2xs text-fg-subtle opacity-0 group-hover:opacity-100">bald</span>{/if}
  </a>
{/snippet}

<div class="flex h-full flex-col">
  <div class="flex h-14 shrink-0 items-center px-4">
    <a href={href('/')} onclick={onnavigate} class="rounded-md" aria-label="ai-gw – Übersicht"><Logo /></a>
  </div>
  <nav class="scrollbar-thin flex flex-1 flex-col gap-4 overflow-y-auto px-3 py-2" aria-label="Hauptnavigation">
    {#each NAV as group, gi (gi)}
      <div class="flex flex-col gap-0.5">
        {#if group.label}<div class="label-caps px-2.5 pb-1 text-fg-subtle">{group.label}</div>{/if}
        {#each group.items.filter((p) => !p.adminOnly || isGlobalAdmin()) as p (p.id)}{@render link(p)}{/each}
      </div>
    {/each}
  </nav>
  <div class="flex flex-col gap-0.5 border-t border-sidebar-line px-3 py-3">
    {#each NAV_FOOTER as p (p.id)}{@render link(p)}{/each}
  </div>
</div>
