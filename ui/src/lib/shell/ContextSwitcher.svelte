<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Organisation/Team-Umschalter (Bits UI DropdownMenu mit Radiogruppen). Team '' = alle Teams.
  import { DropdownMenu } from 'bits-ui';
  import { ctx, currentOrg } from '../context.svelte';
  import { ORGS } from '../mock/data';
  import { toast } from '../toast.svelte';
  import Icon from '../ui/Icon.svelte';
  import { cx, menuItem, menuLabel, menuSeparator, popover } from '../ui/styles';

  const org = $derived(currentOrg());
  const team = $derived(org.teams.find((t) => t.id === ctx.teamId));

  function setOrg(id: string) {
    if (id === ctx.orgId) return;
    ctx.orgId = id;
    ctx.teamId = '';
    toast('info', `Organisation gewechselt: ${currentOrg().name}`);
  }
  function setTeam(id: string) {
    ctx.teamId = id;
  }
</script>

<DropdownMenu.Root>
  <DropdownMenu.Trigger
    class="inline-flex h-9 max-w-full min-w-0 items-center gap-2 rounded-md px-2 text-left hover:bg-surface-hover data-[state=open]:bg-surface-hover"
    data-testid="context-switcher"
  >
    <span class="grid size-6 shrink-0 place-items-center rounded-sm bg-accent-soft text-xs font-semibold text-accent-soft-fg">
      {org.name.slice(0, 1)}
    </span>
    <span class="hidden min-w-0 flex-col leading-tight sm:flex">
      <span class="truncate text-sm font-medium text-fg">{org.name}</span>
      <span class="truncate text-2xs text-fg-muted">{team?.name ?? 'Alle Teams'}</span>
    </span>
    <Icon name="chevrons" size={14} class="text-fg-subtle" />
  </DropdownMenu.Trigger>
  <DropdownMenu.Portal>
    <DropdownMenu.Content class={cx(popover, 'w-72')} sideOffset={6} align="start">
      <DropdownMenu.Group>
        <DropdownMenu.GroupHeading class={menuLabel}>Organisation</DropdownMenu.GroupHeading>
        <DropdownMenu.RadioGroup value={ctx.orgId} onValueChange={setOrg}>
          {#each ORGS as o (o.id)}
            <DropdownMenu.RadioItem value={o.id} class={menuItem}>
              {#snippet children({ checked })}
                <span class="grid size-5 place-items-center rounded-sm bg-surface-2 text-2xs font-semibold text-fg-muted">{o.name.slice(0, 1)}</span>
                <span class="flex-1 truncate">{o.name}</span>
                <span class="text-2xs text-fg-subtle">{o.plan}</span>
                <Icon name="check" size={14} class={checked ? 'text-accent-text' : 'invisible'} />
              {/snippet}
            </DropdownMenu.RadioItem>
          {/each}
        </DropdownMenu.RadioGroup>
      </DropdownMenu.Group>
      <DropdownMenu.Separator class={menuSeparator} />
      <DropdownMenu.Group>
        <DropdownMenu.GroupHeading class={menuLabel}>Team</DropdownMenu.GroupHeading>
        <DropdownMenu.RadioGroup value={ctx.teamId} onValueChange={setTeam}>
          {#each [{ id: '', name: 'Alle Teams' }, ...org.teams] as t (t.id)}
            <DropdownMenu.RadioItem value={t.id} class={menuItem}>
              {#snippet children({ checked })}
                <Icon name={t.id ? 'users' : 'layers'} size={14} />
                <span class="flex-1 truncate">{t.name}</span>
                <Icon name="check" size={14} class={checked ? 'text-accent-text' : 'invisible'} />
              {/snippet}
            </DropdownMenu.RadioItem>
          {/each}
        </DropdownMenu.RadioGroup>
      </DropdownMenu.Group>
      <DropdownMenu.Separator class={menuSeparator} />
      <DropdownMenu.Item class={menuItem} onSelect={() => toast('info', 'Organisationen verwalten', 'Folgt mit „Teams & Nutzer“.')}>
        <Icon name="building" size={14} /> Organisationen verwalten
      </DropdownMenu.Item>
    </DropdownMenu.Content>
  </DropdownMenu.Portal>
</DropdownMenu.Root>
