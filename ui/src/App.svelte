<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // App-Shell: feste dunkle Seitenleiste (ab lg), sonst Navigation im Sheet; Topbar; Seiten per dynamic import.
  import { Tooltip } from 'bits-ui';
  import type { Component } from 'svelte';
  import { tick } from 'svelte';
  import { pageForPath, PAGES } from './lib/nav';
  import { interceptLinks, route } from './lib/router.svelte';
  import CommandPalette from './lib/shell/CommandPalette.svelte';
  import SidebarNav from './lib/shell/SidebarNav.svelte';
  import Topbar from './lib/shell/Topbar.svelte';
  import Sheet from './lib/ui/Sheet.svelte';
  import Toaster from './lib/ui/Toaster.svelte';

  type Loader = () => Promise<{ default: Component }>;
  const ROUTES: Partial<Record<keyof typeof PAGES, Loader>> = {
    overview: () => import('./routes/Overview.svelte'),
    keys: () => import('./routes/Keys.svelte'),
    models: () => import('./routes/Models.svelte'),
    logs: () => import('./routes/Logs.svelte'),
    settings: () => import('./routes/Settings.svelte'),
    design: () => import('./routes/DesignSystem.svelte'),
  };
  const placeholder: Loader = () => import('./routes/Placeholder.svelte');
  const notFound: Loader = () => import('./routes/NotFound.svelte');

  const page = $derived(pageForPath(route.path));
  const loader = $derived(page ? (ROUTES[page.id] ?? placeholder) : notFound);

  let navOpen = $state(false);
  let paletteOpen = $state(false);

  $effect(() => {
    document.title = `${page?.label ?? 'Nicht gefunden'} · ai-gw`;
  });

  // Fokus nach Seitenwechsel auf die Überschrift (Screenreader, Tastatur)
  let first = true;
  $effect(() => {
    void route.path;
    if (first) {
      first = false;
      return;
    }
    tick().then(() => setTimeout(() => document.querySelector<HTMLElement>('main h1')?.focus(), 30));
  });

  function onkeydown(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
      e.preventDefault();
      paletteOpen = !paletteOpen;
    }
  }
</script>

<svelte:window {onkeydown} />

<Tooltip.Provider>
  <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
  <div class="min-h-dvh" onclick={interceptLinks}>
    <a
      href="#main"
      class="sr-only z-[70] rounded-md bg-surface px-3 py-2 text-fg shadow-pop focus:not-sr-only focus:fixed focus:top-3 focus:left-3">Zum Inhalt springen</a
    >
    <Sheet bind:open={navOpen} side="left" size="sm" title="Navigation" hideTitle contentClass="scheme-dark bg-sidebar! border-sidebar-line!">
      <SidebarNav onnavigate={() => (navOpen = false)} />
    </Sheet>

    <div class="flex min-h-dvh">
    <!-- Seitenleiste läuft über die volle Seitenhöhe, der Inhalt klebt oben (sticky) -->
    <aside class="scheme-dark hidden w-60 shrink-0 border-r border-sidebar-line bg-sidebar lg:block" data-testid="sidebar">
      <div class="sticky top-0 h-dvh"><SidebarNav /></div>
    </aside>

    <div class="flex min-h-dvh min-w-0 flex-1 flex-col">
      <Topbar onmenu={() => (navOpen = true)} onsearch={() => (paletteOpen = true)} />
      <main id="main" class="mx-auto w-full max-w-[90rem] flex-1 px-4 py-6 sm:px-6 lg:px-8 lg:py-8">
        {#await loader()}
          <div class="h-8 w-48 animate-pulse rounded-md bg-skeleton" aria-label="Lade Seite"></div>
        {:then mod}
          {@const Page = mod.default}
          {#key route.path}
            <Page />
          {/key}
        {/await}
      </main>
    </div>
    </div>
  </div>
  <CommandPalette bind:open={paletteOpen} />
  <Toaster />
</Tooltip.Provider>
