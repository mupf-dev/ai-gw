<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Befehlspalette (⌘K / Strg+K) im Bits-UI-Dialog. Die Liste ist ein eigenes APG-Muster (Combobox + Listbox,
  // aria-activedescendant), weil Bits UI Command statische style-Attribute rendert (CSP style-src-attr) –
  // Befund aus dem cloud-project-Spike, für bits-ui 2.20 erneut geprüft.
  import { Dialog } from 'bits-ui';
  import { tick } from 'svelte';
  import { isMock, mockAvailable } from '../mode.svelte';
  import { isGlobalAdmin } from '../rbac.svelte';
  import { prefs, setMockMode, setTheme } from '../prefs.svelte';
  import { MOCK, MODEL_GROUPS } from '../mock/data';
  import { NAV, NAV_FOOTER, PAGES } from '../nav';
  import { navigate } from '../router.svelte';
  import Icon, { type IconName } from '../ui/Icon.svelte';
  import { kbd } from '../ui/styles';

  let { open = $bindable(false) }: { open?: boolean } = $props();
  let search = $state('');
  let activeId = $state('');
  let inputEl = $state<HTMLInputElement | null>(null);

  interface Cmd {
    id: string;
    label: string;
    hint?: string;
    icon: IconName;
    keywords: string;
    run: () => void;
  }
  interface Group {
    label: string;
    items: Cmd[];
  }

  const groups = $derived.by<Group[]>(() => [
    {
      label: 'Seiten',
      items: [...NAV.flatMap((g) => g.items), ...NAV_FOOTER, PAGES.design].filter((p) => !p.adminOnly || isGlobalAdmin()).map((p) => ({
        id: `nav-${p.id}`,
        label: p.label,
        hint: p.ready ? undefined : 'in Planung',
        icon: p.icon,
        keywords: `${p.keywords ?? ''} ${p.description}`,
        run: () => navigate(p.path),
      })),
    },
    {
      label: 'Aktionen',
      items: [
        { id: 'act-key', label: 'Key erstellen', icon: 'plus' as IconName, keywords: 'neu api key schlüssel anlegen', run: () => navigate('/keys?new=1') },
        { id: 'act-logs-err', label: 'Fehlerhafte Requests anzeigen', icon: 'alert' as IconName, keywords: 'logs fehler 5xx 429', run: () => navigate('/logs?status=error') },
        {
          id: 'act-theme',
          label: prefs.theme === 'dark' ? 'Helles Design' : 'Dunkles Design',
          icon: (prefs.theme === 'dark' ? 'sun' : 'moon') as IconName,
          keywords: 'theme darstellung hell dunkel dark light',
          run: () => setTheme(prefs.theme === 'dark' ? 'light' : 'dark'),
        },
        ...(mockAvailable() ? [{
          id: 'act-mock',
          label: isMock() ? 'Mock-Daten ausschalten' : 'Mock-Daten einschalten',
          icon: 'sparkles' as IconName,
          keywords: 'mock demo beispieldaten',
          run: () => setMockMode(isMock() ? 'off' : 'on'),
        }] : []),
      ],
    },
    {
      label: 'Modellgruppen',
      items: isMock()
        ? MODEL_GROUPS.map((m) => ({
            id: `model-${m.alias}`,
            label: m.alias,
            hint: `${m.deployments.length} Deployments`,
            icon: 'route' as IconName,
            keywords: m.deployments.map((d) => `${d.provider} ${d.upstreamModel}`).join(' '),
            run: () => navigate(`/models?group=${encodeURIComponent(m.alias)}`),
          }))
        : [],
    },
    {
      label: 'Keys',
      items: isMock()
        ? MOCK.keys.map((k) => ({
            id: `key-${k.id}`,
            label: k.name,
            hint: `${k.prefix}…${k.last4}`,
            icon: 'key' as IconName,
            keywords: k.teamId,
            run: () => navigate(`/keys?q=${encodeURIComponent(k.name)}`),
          }))
        : [],
    },
  ]);

  const filtered = $derived.by(() => {
    const q = search.trim().toLowerCase();
    return groups
      .map((g) => ({
        ...g,
        items: q ? g.items.filter((c) => `${c.label} ${c.keywords} ${c.hint ?? ''}`.toLowerCase().includes(q)).slice(0, 8) : g.items.slice(0, g.label === 'Seiten' ? 20 : 4),
      }))
      .filter((g) => g.items.length);
  });
  const flat = $derived(filtered.flatMap((g) => g.items));

  $effect(() => {
    if (!flat.some((c) => c.id === activeId)) activeId = flat[0]?.id ?? '';
  });
  $effect(() => {
    if (open) {
      search = '';
      tick().then(() => inputEl?.focus());
    }
  });

  function run(c: Cmd) {
    open = false;
    c.run();
  }
  function move(d: number) {
    const i = flat.findIndex((c) => c.id === activeId);
    const next = flat[(i + d + flat.length) % flat.length];
    if (next) {
      activeId = next.id;
      document.getElementById(`cmd-${next.id}`)?.scrollIntoView({ block: 'nearest' });
    }
  }
  function onkeydown(e: KeyboardEvent) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      move(1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      move(-1);
    } else if (e.key === 'Enter') {
      const c = flat.find((x) => x.id === activeId);
      if (c) {
        e.preventDefault();
        run(c);
      }
    }
  }
</script>

<Dialog.Root bind:open>
  <Dialog.Portal>
    <Dialog.Overlay class="anim-fade fixed inset-0 z-40" />
    <Dialog.Content
      class="anim-pop fixed top-[12vh] left-1/2 z-50 flex max-h-[70vh] w-[calc(100vw-2rem)] max-w-xl -translate-x-1/2 flex-col overflow-hidden rounded-xl border border-line bg-surface shadow-dialog outline-none"
      data-testid="command-palette"
    >
      <Dialog.Title class="sr-only">Befehlspalette</Dialog.Title>
      <div class="flex items-center gap-2 border-b border-line px-4">
        <Icon name="search" class="text-fg-subtle" />
        <input
          bind:this={inputEl}
          bind:value={search}
          {onkeydown}
          class="h-12 flex-1 bg-transparent text-md text-fg outline-none placeholder:text-fg-subtle"
          placeholder="Seite, Aktion, Modell oder Key suchen …"
          role="combobox"
          aria-expanded="true"
          aria-controls="cmd-list"
          aria-activedescendant={activeId ? `cmd-${activeId}` : undefined}
          aria-autocomplete="list"
          aria-label="Befehl suchen"
        />
        <span class={kbd}>Esc</span>
      </div>
      <div id="cmd-list" role="listbox" aria-label="Ergebnisse" class="scrollbar-thin flex-1 overflow-y-auto p-1.5">
        {#each filtered as g (g.label)}
          <div role="group" aria-labelledby="cmd-g-{g.label}">
            <div id="cmd-g-{g.label}" class="label-caps px-2.5 pt-2.5 pb-1 text-fg-subtle">{g.label}</div>
            {#each g.items as c (c.id)}
              <div
                id="cmd-{c.id}"
                role="option"
                tabindex="-1"
                aria-selected={c.id === activeId}
                class="flex h-9 cursor-default items-center gap-2.5 rounded-md px-2.5 text-base text-fg aria-selected:bg-surface-hover"
                onclick={() => run(c)}
                onkeydown={() => {}}
                onpointermove={() => (activeId = c.id)}
              >
                <Icon name={c.icon} size={15} class="text-fg-muted" />
                <span class="flex-1 truncate">{c.label}</span>
                {#if c.hint}<span class="truncate text-xs text-fg-subtle">{c.hint}</span>{/if}
                {#if c.id === activeId}<Icon name="chevron-right" size={14} class="text-fg-subtle" />{/if}
              </div>
            {/each}
          </div>
        {:else}
          <p class="px-3 py-8 text-center text-base text-fg-muted">Keine Treffer für „{search}“.</p>
        {/each}
      </div>
      <div class="flex items-center gap-3 border-t border-line bg-surface-2 px-4 py-2 text-xs text-fg-muted">
        <span class="inline-flex items-center gap-1"><span class={kbd}>↑</span><span class={kbd}>↓</span> wählen</span>
        <span class="inline-flex items-center gap-1"><span class={kbd}>↵</span> ausführen</span>
      </div>
    </Dialog.Content>
  </Dialog.Portal>
</Dialog.Root>
