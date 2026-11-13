<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Kopfzeile: Menü (mobil), Organisation/Team, Suche (⌘K), Mock-Hinweis, Hilfe, Nutzer-Menü.
  import { isMock } from '../mode.svelte';
  import { setMockMode } from '../prefs.svelte';
  import { auth, requestLogin } from '../session.svelte';
  import Badge from '../ui/Badge.svelte';
  import Icon from '../ui/Icon.svelte';
  import Tooltip from '../ui/Tooltip.svelte';
  import { btn, kbd } from '../ui/styles';
  import ContextSwitcher from './ContextSwitcher.svelte';
  import UserMenu from './UserMenu.svelte';

  let { onmenu, onsearch }: { onmenu: () => void; onsearch: () => void } = $props();
  const mac = /Mac|iPhone|iPad/.test(navigator.platform);
</script>

<header class="sticky top-0 z-30 flex h-14 shrink-0 items-center gap-2 border-b border-line bg-topbar px-3 sm:px-4">
  <button type="button" class={btn('ghost', 'icon', 'lg:hidden')} aria-label="Navigation öffnen" onclick={onmenu} data-testid="menu-button">
    <Icon name="menu" size={18} />
  </button>
  <ContextSwitcher />
  <span class="mx-1 hidden h-5 w-px bg-line sm:block" aria-hidden="true"></span>
  <button
    type="button"
    onclick={onsearch}
    class="group flex h-9 min-w-0 flex-1 items-center gap-2 rounded-md border border-line bg-surface-2 px-3 text-base text-fg-subtle transition-colors hover:border-line-strong sm:max-w-sm"
    data-testid="search-button"
  >
    <Icon name="search" size={15} />
    <span class="flex-1 truncate text-left">Suchen …</span>
    <span class="hidden items-center gap-0.5 sm:inline-flex"><span class={kbd}>{mac ? '⌘' : 'Strg'}</span><span class={kbd}>K</span></span>
  </button>
  <div class="ml-auto flex items-center gap-1.5">
    {#if auth.loaded && !auth.session}
      <button type="button" class={btn('secondary', 'sm')} onclick={() => requestLogin()} data-testid="login-button">
        <Icon name="lock" size={14} /> Anmelden
      </button>
    {/if}
    {#if isMock()}
      <Tooltip text="Beispieldaten – keine echten Messwerte. Klicken zum Ausschalten.">
        {#snippet trigger({ props })}
          <button {...props} type="button" class="rounded-sm" onclick={() => setMockMode('off')} data-testid="mock-badge">
            <Badge tone="warning" icon="sparkles">Mock-Daten</Badge>
          </button>
        {/snippet}
      </Tooltip>
    {/if}
    <Tooltip text="Hilfe & Dokumentation">
      {#snippet trigger({ props })}
        <button {...props} type="button" class={btn('ghost', 'icon', 'hidden sm:inline-flex')} aria-label="Hilfe" data-testid="help-button">
          <Icon name="help" size={17} />
        </button>
      {/snippet}
    </Tooltip>
    <UserMenu />
  </div>
</header>
