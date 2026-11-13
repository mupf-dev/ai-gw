<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detailseite eines Objekts (D8/F136): Zurück zur Liste, Kopf mit Name, Status und Aktionen, Tabs
  // (Übersicht, Nutzung, Audit …). Der aktive Tab steht in der URL (?tab=), damit Links und Neuladen ihn behalten.
  import { Tabs } from 'bits-ui';
  import type { Snippet } from 'svelte';
  import { canReadGlobal } from '../rbac.svelte';
  import { href, navigate, route } from '../router.svelte';
  import Icon from '../ui/Icon.svelte';
  import { btn, cx, tabsList, tabsTrigger } from '../ui/styles';

  let {
    back,
    backLabel,
    title,
    subtitle,
    badge,
    actions,
    tabs,
    tab = $bindable(''),
    children,
    testid = 'detail',
    yaml,
  }: {
    /** Auswahl für den YAML-Export dieses Objekts (Query für /config, AIGW-55), z. B. `kinds=Team&name=x`. */
    yaml?: string;
    back: string;
    backLabel: string;
    title: string;
    subtitle?: string;
    badge?: Snippet;
    actions?: Snippet;
    tabs: { value: string; label: string }[];
    tab?: string;
    children: Snippet;
    testid?: string;
  } = $props();

  const wanted = new URLSearchParams(route.search).get('tab');
  if (!tab) tab = tabs.some((t) => t.value === wanted) ? (wanted as string) : tabs[0].value;

  function change(v: string) {
    tab = v;
    const path = route.path + (v === tabs[0].value ? '' : `?tab=${v}`);
    navigate(path, true);
  }
</script>

<div data-testid={testid}>
  <a href={href(back)} class="mb-3 inline-flex items-center gap-1 text-sm text-fg-muted hover:text-fg" data-testid="detail-back">
    <Icon name="arrow-left" size={14} />
    {backLabel}
  </a>
  <header class="mb-5 flex flex-wrap items-start gap-3">
    <div class="min-w-0 flex-1">
      <div class="flex flex-wrap items-center gap-2">
        <h1 tabindex="-1" class="truncate font-mono text-xl font-semibold text-fg outline-none">{title}</h1>
        {@render badge?.()}
      </div>
      {#if subtitle}<p class="mt-1 text-base text-fg-muted">{subtitle}</p>{/if}
    </div>
    {#if actions || (yaml && canReadGlobal())}
      <div class="flex flex-wrap items-center gap-2">
        {#if yaml && canReadGlobal()}
          <a href={href(`/config?${yaml}`)} class={btn('ghost', 'sm')} data-testid="detail-yaml"><Icon name="file-text" size={14} />YAML</a>
        {/if}
        {@render actions?.()}
      </div>
    {/if}
  </header>
  <Tabs.Root value={tab} onValueChange={change}>
    <Tabs.List class={cx(tabsList, 'scrollbar-thin mb-4 overflow-x-auto')}>
      {#each tabs as t (t.value)}
        <Tabs.Trigger value={t.value} class={tabsTrigger} data-testid="tab-{t.value}">{t.label}</Tabs.Trigger>
      {/each}
    </Tabs.List>
    {@render children()}
  </Tabs.Root>
</div>
