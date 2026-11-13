<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Nutzer-Menü: Identität/Rolle, Darstellung (Schema), Mock-Modus, Einstellungen, Abmelden.
  // Angemeldet (Session/OIDC): echter Nutzer; sonst im Mock-Modus der Beispielnutzer.
  import { DropdownMenu } from 'bits-ui';
  import { USER } from '../context.svelte';
  import { isMock, mockAvailable } from '../mode.svelte';
  import { auth, identity, logout, METHOD_LABEL, requestLogin, roleLabel } from '../session.svelte';
  import { prefs, setMockMode, setTheme, THEMES, type Theme } from '../prefs.svelte';
  import { navigate } from '../router.svelte';
  import { toast } from '../toast.svelte';
  import Icon, { type IconName } from '../ui/Icon.svelte';
  import { cx, menuItem, menuLabel, menuSeparator, popover } from '../ui/styles';

  const THEME_ICON: Record<Theme, IconName> = { system: 'monitor', light: 'sun', dark: 'moon' };

  const me = $derived.by(() => {
    if (auth.session) {
      const id = identity(auth.session);
      return { ...id, detail: auth.session.email ?? METHOD_LABEL[auth.session.method] ?? auth.session.subject, role: roleLabel(auth.session.roles) };
    }
    if (isMock()) return { name: USER.name, initials: USER.initials, detail: USER.email, role: USER.role };
    return { name: 'Nicht angemeldet', initials: '?', detail: auth.oidc ? 'Anmeldung per OIDC' : 'Admin-Token erforderlich', role: '' };
  });
</script>

<DropdownMenu.Root>
  <DropdownMenu.Trigger
    class="grid size-8 place-items-center rounded-full bg-surface-2 text-xs font-semibold text-fg ring-1 ring-line hover:ring-line-strong data-[state=open]:ring-2 data-[state=open]:ring-accent"
    aria-label="Benutzermenü: {me.name}"
    data-testid="user-menu"
  >
    {me.initials}
  </DropdownMenu.Trigger>
  <DropdownMenu.Portal>
    <DropdownMenu.Content class={cx(popover, 'w-64')} sideOffset={6} align="end">
      <div class="flex items-center gap-3 px-2 py-2">
        <span class="grid size-9 place-items-center rounded-full bg-accent-soft text-sm font-semibold text-accent-soft-fg">{me.initials}</span>
        <div class="min-w-0">
          <p class="truncate text-base font-medium text-fg" data-testid="user-name">{me.name}</p>
          <p class="truncate text-xs text-fg-muted">{me.detail}</p>
          {#if me.role}<p class="truncate text-xs text-fg-subtle" data-testid="user-role">{me.role}</p>{/if}
        </div>
      </div>
      <DropdownMenu.Separator class={menuSeparator} />
      <DropdownMenu.Group>
        <DropdownMenu.GroupHeading class={menuLabel}>Darstellung</DropdownMenu.GroupHeading>
        <DropdownMenu.RadioGroup value={prefs.theme} onValueChange={(v) => setTheme(v as Theme)}>
          {#each THEMES as t (t.value)}
            <DropdownMenu.RadioItem value={t.value} class={menuItem} closeOnSelect={false}>
              {#snippet children({ checked })}
                <Icon name={THEME_ICON[t.value]} size={14} />
                <span class="flex-1">{t.label}</span>
                <Icon name="check" size={14} class={checked ? 'text-accent-text' : 'invisible'} />
              {/snippet}
            </DropdownMenu.RadioItem>
          {/each}
        </DropdownMenu.RadioGroup>
      </DropdownMenu.Group>
      {#if mockAvailable()}
      <DropdownMenu.Separator class={menuSeparator} />
      <DropdownMenu.CheckboxItem checked={isMock()} onCheckedChange={(c) => setMockMode(c ? 'on' : 'off')} class={menuItem} closeOnSelect={false}>
        {#snippet children({ checked })}
          <Icon name="sparkles" size={14} />
          <span class="flex-1">Mock-Daten</span>
          <span
            class={cx('relative h-4 w-7 rounded-full transition-colors', checked ? 'bg-accent' : 'bg-line-strong')}
            aria-hidden="true"
          >
            <span class={cx('absolute top-0.5 size-3 rounded-full bg-surface transition-[left]', checked ? 'left-3.5' : 'left-0.5')}></span>
          </span>
        {/snippet}
      </DropdownMenu.CheckboxItem>
      {/if}
      <DropdownMenu.Item class={menuItem} onSelect={() => navigate('/settings')}><Icon name="settings" size={14} /> Einstellungen</DropdownMenu.Item>
      <DropdownMenu.Item class={menuItem} onSelect={() => toast('info', 'API-Dokumentation', 'OpenAPI unter /admin/api/openapi.json')}>
        <Icon name="file-text" size={14} /> API-Dokumentation
      </DropdownMenu.Item>
      <DropdownMenu.Separator class={menuSeparator} />
      {#if auth.session}
        <DropdownMenu.Item class={menuItem} onSelect={() => void logout()} data-testid="logout">
          <Icon name="logout" size={14} /> Abmelden
        </DropdownMenu.Item>
      {:else if auth.loaded}
        <DropdownMenu.Item class={menuItem} onSelect={() => requestLogin()} data-testid="login">
          <Icon name="user" size={14} /> Anmelden
        </DropdownMenu.Item>
      {/if}
    </DropdownMenu.Content>
  </DropdownMenu.Portal>
</DropdownMenu.Root>
