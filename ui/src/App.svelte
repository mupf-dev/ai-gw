<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // App-Shell: feste dunkle Seitenleiste (ab lg), sonst Navigation im Sheet; Topbar; Seiten per dynamic import.
  import { Tooltip } from 'bits-ui';
  import type { Component } from 'svelte';
  import { tick } from 'svelte';
  import ConflictDialog from './lib/admin/ConflictDialog.svelte';
  import TokenDialog from './lib/admin/TokenDialog.svelte';
  import { loadCatalog } from './lib/context.svelte';
  import { dataKey, isMock } from './lib/mode.svelte';
  import Banners from './lib/admin/Banners.svelte';
  import { detailForPath, pageForPath, PAGES, type DetailKind } from './lib/nav';
  import { interceptLinks, route } from './lib/router.svelte';
  import { auth } from './lib/session.svelte';
  import CommandPalette from './lib/shell/CommandPalette.svelte';
  import SidebarNav from './lib/shell/SidebarNav.svelte';
  import Topbar from './lib/shell/Topbar.svelte';
  import { setMockMode } from './lib/prefs.svelte';
  import Icon from './lib/ui/Icon.svelte';
  import Sheet from './lib/ui/Sheet.svelte';
  import Toaster from './lib/ui/Toaster.svelte';

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  type Loader = () => Promise<{ default: Component<any> }>;
  const ROUTES: Partial<Record<keyof typeof PAGES, Loader>> = {
    overview: () => import('./routes/Overview.svelte'),
    keys: () => import('./routes/Keys.svelte'),
    models: () => import('./routes/Models.svelte'),
    providers: () => import('./routes/Providers.svelte'),
    secrets: () => import('./routes/Secrets.svelte'),
    guardrails: () => import('./routes/Guardrails.svelte'),
    mcp: () => import('./routes/Mcp.svelte'),
    prices: () => import('./routes/Prices.svelte'),
    teams: () => import('./routes/Teams.svelte'),
    'admin-keys': () => import('./routes/AdminKeys.svelte'),
    logs: () => import('./routes/Logs.svelte'),
    audit: () => import('./routes/Audit.svelte'),
    limits: () => import('./routes/Limits.svelte'),
    settings: () => import('./routes/Settings.svelte'),
    design: () => import('./routes/DesignSystem.svelte'),
  };
  // Detailseiten (D8/F136) je Objektart.
  const DETAILS: Record<DetailKind, Loader> = {
    key: () => import('./routes/keys/KeyDetail.svelte'),
    team: () => import('./routes/teams/TeamDetail.svelte'),
    organization: () => import('./routes/teams/OrgDetail.svelte'),
    'model-group': () => import('./routes/models/GroupDetail.svelte'),
    deployment: () => import('./routes/models/DeploymentDetail.svelte'),
  };
  const placeholder: Loader = () => import('./routes/Placeholder.svelte');
  const notFound: Loader = () => import('./routes/NotFound.svelte');
  const gate: Loader = () => import('./routes/LoginRequired.svelte');

  const page = $derived(pageForPath(route.path));
  const detail = $derived(detailForPath(route.path));
  // Verwaltungsseiten ohne Mock-Modus nur angemeldet – sonst würden ihre Abfragen 401 liefern.
  const gated = $derived(!!page?.needsAuth && !isMock() && !auth.session);
  const loader = $derived(gated ? gate : detail ? DETAILS[detail.kind] : page ? (ROUTES[page.id] ?? placeholder) : notFound);
  // Modul samt Pfad und Name laden: so erhält eine Seite nie den Namen der nächsten Route, solange deren Modul lädt.
  const pagePromise = $derived(
    auth.loaded ? ((load: Loader, path: string, name?: string) => load().then((mod) => ({ mod, path, name })))(loader, route.path, detail?.name) : new Promise<never>(() => {}),
  );

  // Organisationen/Teams für den Kontext-Umschalter bei jedem Wechsel der Datenquelle neu laden.
  $effect(() => {
    void dataKey();
    if (auth.loaded && (isMock() || auth.session)) void loadCatalog();
  });

  let navOpen = $state(false);
  let paletteOpen = $state(false);

  $effect(() => {
    document.title = `${detail ? `${detail.name} · ` : ''}${page?.label ?? 'Nicht gefunden'} · ai-gw`;
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
      {#if isMock()}
        <!-- Mock-Modus deutlich kennzeichnen (User-Entscheidung D6) -->
        <div class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-warning-fg/30 bg-warning-bg px-4 py-2 text-sm text-warning-fg sm:px-6 lg:px-8" role="status" data-testid="mock-banner">
          <Icon name="sparkles" size={14} />
          <span class="min-w-0 flex-1"><strong class="font-semibold">Mock-Modus:</strong> Beispieldaten, keine echten Werte – Änderungen bleiben in diesem Browser-Tab.</span>
          <button type="button" class="font-medium underline underline-offset-2 hover:no-underline" onclick={() => setMockMode('off')}>Mock ausschalten</button>
        </div>
      {/if}
      <Banners />
      <main id="main" class="mx-auto w-full max-w-[90rem] flex-1 px-4 py-6 sm:px-6 lg:px-8 lg:py-8">
        {#await pagePromise}
          <div class="h-8 w-48 animate-pulse rounded-md bg-skeleton" aria-label="Lade Seite"></div>
        {:then p}
          {@const Page = p.mod.default}
          {#key `${p.path}|${dataKey()}`}
            <Page name={p.name} />
          {/key}
        {/await}
      </main>
    </div>
    </div>
  </div>
  <CommandPalette bind:open={paletteOpen} />
  <TokenDialog />
  <ConflictDialog />
  <Toaster />
</Tooltip.Provider>
