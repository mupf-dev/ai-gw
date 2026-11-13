<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Modelle & Routen (AIGW-49): Master/Detail – Modellgruppen (Aliasse, Strategie, Fallbacks, Einstellungen) und
  // ihre Deployments nach Prioritätsgruppen (Anbieter, Modell, Gewichtsanteil, Kapazität, aktiv).
  // Schreiben nur für globale Admins (Katalog); Laufzeit-Kennzahlen (Zustand, p95) folgen mit AIGW-46.
  import { DropdownMenu, Switch } from 'bits-ui';
  import { deployments, modelGroups, providerLabel, ROUTING, FALLBACK_TRIGGERS, type Deployment, type ModelGroup } from '../lib/api/resources';
  import AsyncState from '../lib/admin/AsyncState.svelte';
  import ConfirmDialog from '../lib/admin/ConfirmDialog.svelte';
  import { formErrorOf, handleError, Loader } from '../lib/admin/errors.svelte';
  import { short } from '../lib/format';
  import { canWrite } from '../lib/rbac.svelte';
  import { navigate, route } from '../lib/router.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import CodeBlock from '../lib/ui/CodeBlock.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, switchRoot, switchThumb } from '../lib/ui/styles';
  import DeploymentFormDialog from './models/DeploymentFormDialog.svelte';
  import GroupFormDialog from './models/GroupFormDialog.svelte';

  const data = new Loader(async () => {
    const [g, d] = await Promise.all([modelGroups.list(), deployments.list()]);
    return { groups: g, deployments: d };
  });
  let selected = $state(new URLSearchParams(route.search).get('group') ?? '');
  let filter = $state('');
  let groupOpen = $state(false);
  let groupEdit = $state<string | null>(null);
  let depOpen = $state(false);
  let depEdit = $state<string | null>(null);
  let del = $state<{ kind: 'group' | 'deployment'; name: string } | null>(null);
  let delOpen = $state(false);

  const writable = $derived(canWrite({ kind: 'catalog' }));
  const groups = $derived(data.data?.groups ?? []);
  const shown = $derived(groups.filter((g) => !filter || `${g.metadata.name} ${(g.spec.aliases ?? []).join(' ')}`.toLowerCase().includes(filter.toLowerCase())));
  const group = $derived(groups.find((g) => g.metadata.name === selected) ?? groups[0]);
  const depsOf = (g: string) => (data.data?.deployments ?? []).filter((d) => d.spec.modelGroup === g);
  const deps = $derived(group ? depsOf(group.metadata.name) : []);
  const tiers = $derived.by(() => {
    const m = new Map<number, Deployment[]>();
    for (const d of deps) m.set(d.spec.priority ?? 0, [...(m.get(d.spec.priority ?? 0) ?? []), d]);
    return [...m.entries()].sort((a, b) => a[0] - b[0]);
  });
  const strategy = (g: ModelGroup) => ROUTING.find((r) => r.value === (g.spec.routing ?? 'weighted'));

  function select(name: string) {
    selected = name;
    navigate(`/models?group=${encodeURIComponent(name)}`, true);
  }
  function editGroup(name: string | null) {
    groupEdit = name;
    groupOpen = true;
  }
  function editDeployment(name: string | null) {
    depEdit = name;
    depOpen = true;
  }
  async function toggle(d: Deployment, enabled: boolean) {
    try {
      const { data: cur, etag } = await deployments.get(d.metadata.name);
      await deployments.update(d.metadata.name, { metadata: { name: d.metadata.name, labels: cur.metadata.labels ?? {} }, spec: { ...cur.spec, enabled } }, etag);
      toast('success', enabled ? 'Deployment aktiviert' : 'Deployment deaktiviert', d.metadata.name);
    } catch (e) {
      handleError(e, { action: 'Deployment umschalten', reload: () => void data.reload() });
    }
    void data.reload();
  }
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    const { kind, name } = del;
    const res = kind === 'group' ? modelGroups : deployments;
    try {
      const { etag } = await res.get(name);
      await res.remove(name, etag);
      toast('success', kind === 'group' ? 'Modellgruppe gelöscht' : 'Deployment gelöscht', name);
      if (kind === 'group') selected = '';
      void data.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, kind === 'group' ? 'Modellgruppe löschen' : 'Deployment löschen', () => void data.reload());
    }
  }
  const triggers = (on: string[] | undefined) => (on?.length ? on.map((t) => FALLBACK_TRIGGERS.find((x) => x.value === t)?.label ?? t).join(', ') : 'jeder Fehler');
</script>

<PageHeader title="Modelle & Routen" description="Modellgruppen sind die öffentlichen Modellnamen der Clients. Dahinter wählt das Gateway ein Deployment nach Priorität und Strategie; bei Fehlern greift die Fallback-Kette.">
  {#snippet actions()}
    {#if writable}
      <Button variant="primary" icon="plus" onclick={() => editGroup(null)} data-testid="create-group">Modellgruppe anlegen</Button>
    {/if}
  {/snippet}
</PageHeader>

  <AsyncState loader={data}>
    {#snippet children()}
      {#if groups.length === 0}
        <Card>
          <EmptyState icon="route" title="Noch keine Modellgruppen" text="Legen Sie eine Modellgruppe an und hängen Sie Deployments (Anbieter-Modelle) dahinter. Alternativ per GitOps (Einstellungen → GitOps).">
            {#if writable}<Button variant="primary" icon="plus" onclick={() => editGroup(null)}>Modellgruppe anlegen</Button>{/if}
          </EmptyState>
        </Card>
      {:else}
        <div class="grid gap-4 lg:grid-cols-[20rem_1fr]">
          <Card flush title="Modellgruppen" description="{groups.length} Gruppen">
            <div class="border-b border-line px-3 py-2">
              <label class="relative flex items-center">
                <span class="sr-only">Modellgruppen filtern</span>
                <span class="pointer-events-none absolute left-2.5 text-fg-subtle"><Icon name="search" size={14} /></span>
                <input type="search" bind:value={filter} placeholder="Name oder Alias …" class="h-8 w-full rounded-md border border-line bg-surface pr-2 pl-8 text-sm text-fg placeholder:text-fg-subtle" />
              </label>
            </div>
            <ul class="flex flex-col p-1.5" aria-label="Modellgruppen" data-testid="group-list">
              {#each shown as g (g.metadata.name)}
                {@const n = depsOf(g.metadata.name)}
                <li>
                  <button
                    type="button"
                    class={[
                      'flex w-full flex-col gap-1 rounded-md px-3 py-2 text-left hover:bg-surface-hover',
                      group?.metadata.name === g.metadata.name && 'bg-accent-soft hover:bg-accent-soft',
                    ]}
                    aria-current={group?.metadata.name === g.metadata.name ? 'true' : undefined}
                    onclick={() => select(g.metadata.name)}
                  >
                    <span class="flex items-center gap-2">
                      <span class="flex-1 truncate font-mono text-sm font-medium text-fg">{g.metadata.name}</span>
                      <span class="text-xs text-fg-muted">{n.filter((d) => d.spec.enabled !== false).length}/{n.length}</span>
                    </span>
                    <span class="flex items-center gap-1.5 text-xs text-fg-muted">
                      <span>{strategy(g)?.label}</span>
                      {#if g.spec.aliases?.length}<span>· {g.spec.aliases.length} Alias{g.spec.aliases.length > 1 ? 'se' : ''}</span>{/if}
                      {#if g.spec.fallbacks?.length}<span>· Fallback → {g.spec.fallbacks[0].modelGroup}</span>{/if}
                    </span>
                  </button>
                </li>
              {:else}
                <li class="px-3 py-6 text-center text-sm text-fg-muted">Kein Treffer.</li>
              {/each}
            </ul>
          </Card>

          {#if group}
            <div class="flex min-w-0 flex-col gap-4" data-testid="group-detail">
              <Card>
                <div class="flex flex-wrap items-start gap-3">
                  <div class="min-w-0 flex-1">
                    <h2 class="font-mono text-lg font-semibold text-fg">{group.metadata.name}</h2>
                    <div class="mt-1 flex flex-wrap gap-1">
                      {#each group.spec.aliases ?? [] as a (a)}<Badge mono tone="outline">{a}</Badge>{:else}<span class="text-sm text-fg-muted">keine Aliasse</span>{/each}
                    </div>
                  </div>
                  {#if writable}
                    <Button size="sm" icon="pencil" onclick={() => editGroup(group.metadata.name)} data-testid="edit-group">Bearbeiten</Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      icon="trash"
                      aria-label="Modellgruppe löschen"
                      onclick={() => {
                        del = { kind: 'group', name: group.metadata.name };
                        delOpen = true;
                      }}
                    />
                  {/if}
                </div>
                <dl class="mt-4 grid gap-x-6 gap-y-3 text-base sm:grid-cols-3">
                  <div><dt class="text-sm text-fg-muted">Strategie</dt><dd class="font-medium">{strategy(group)?.label}</dd></div>
                  <div><dt class="text-sm text-fg-muted">Deployments</dt><dd class="num font-medium">{deps.filter((d) => d.spec.enabled !== false).length} aktiv / {deps.length}</dd></div>
                  <div><dt class="text-sm text-fg-muted">Limits</dt><dd class="font-medium">{group.spec.limits?.length ? `${group.spec.limits.length} Limit(s)` : 'keine'}</dd></div>
                </dl>
                <div class="mt-4">
                  <h3 class="label-caps mb-1.5 text-fg-subtle">Fallback-Kette</h3>
                  {#if group.spec.fallbacks?.length}
                    <ol class="flex flex-wrap items-center gap-1.5 text-sm">
                      <li><Badge mono tone="accent">{group.metadata.name}</Badge></li>
                      {#each group.spec.fallbacks as f (f.modelGroup)}
                        <li class="flex items-center gap-1.5">
                          <Icon name="chevron-right" size={14} class="text-fg-subtle" />
                          <button type="button" class="rounded-sm" onclick={() => select(f.modelGroup)} title="bei: {triggers(f.on)}"><Badge mono>{f.modelGroup}</Badge></button>
                          <span class="text-xs text-fg-muted">({triggers(f.on)})</span>
                        </li>
                      {/each}
                    </ol>
                  {:else}
                    <p class="text-sm text-fg-muted">Keine Fallbacks – Fehler gehen nach den Retries an den Client.</p>
                  {/if}
                </div>
                {#if group.spec.settings && Object.keys(group.spec.settings).length}
                  <div class="mt-4"><CodeBlock title="Einstellungen" code={group.spec.settings} maxHeight="max-h-48" /></div>
                {/if}
              </Card>

              <Card flush title="Deployments" description="Priorität 0 wird vor 1 genutzt; innerhalb einer Priorität entscheidet die Strategie (Gewicht = Anteil).">
                {#snippet actions()}
                  {#if writable}<Button size="sm" variant="primary" icon="plus" onclick={() => editDeployment(null)} data-testid="create-deployment">Deployment</Button>{/if}
                {/snippet}
                {#if deps.length === 0}
                  <EmptyState compact icon="server" title="Keine Deployments" text="Ohne Deployment antwortet die Gruppe mit „nicht verfügbar“ (bzw. Fallback)." />
                {:else}
                  <div class="scrollbar-thin overflow-x-auto">
                    <table class="w-full text-left text-base" data-testid="deployments-table">
                      <caption class="sr-only">Deployments von {group.metadata.name}</caption>
                      <thead>
                        <tr class="border-b border-line bg-surface-2 text-sm text-fg-muted">
                          <th scope="col" class="h-9 px-3 font-medium">Deployment</th>
                          <th scope="col" class="hidden h-9 px-3 font-medium md:table-cell">Anbieter</th>
                          <th scope="col" class="h-9 px-3 text-right font-medium">Anteil</th>
                          <th scope="col" class="hidden h-9 px-3 text-right font-medium lg:table-cell">Kapazität</th>
                          <th scope="col" class="h-9 px-3 font-medium">Aktiv</th>
                          <th scope="col" class="h-9 w-10 px-3"><span class="sr-only">Aktionen</span></th>
                        </tr>
                      </thead>
                      {#each tiers as [prio, list] (prio)}
                        {@const total = list.filter((d) => d.spec.enabled !== false).reduce((s, d) => s + (d.spec.weight ?? 1), 0)}
                        <tbody>
                          <tr class="border-b border-line bg-surface-2/60">
                            <th scope="rowgroup" colspan="6" class="px-3 py-1.5 text-left text-xs font-medium text-fg-muted">Priorität {prio}{prio === tiers[0][0] ? ' · zuerst' : ' · wenn höhere Priorität ausfällt'}</th>
                          </tr>
                          {#each list as d (d.metadata.name)}
                            <tr class="border-b border-line last:border-b-0" data-key={d.metadata.name}>
                              <td class="px-3 py-2.5">
                                <span class="block font-mono text-sm font-medium text-fg">{d.metadata.name}</span>
                                <span class="block truncate font-mono text-xs text-fg-muted">{d.spec.providerModel}{d.spec.endpoint ? ` · ${d.spec.endpoint.replace(/^https?:\/\//, '')}` : ''}</span>
                              </td>
                              <td class="hidden px-3 py-2.5 md:table-cell">
                                <span class="block text-sm">{providerLabel(d.spec.provider)}</span>
                                <span class="block text-xs text-fg-muted">{d.spec.credential ?? 'ohne Zugangsdaten'}</span>
                              </td>
                              <td class="num px-3 py-2.5 text-right text-sm">
                                {d.spec.enabled === false || !total ? '–' : `${Math.round(((d.spec.weight ?? 1) / total) * 100)} %`}
                                <span class="block text-xs text-fg-muted">Gewicht {d.spec.weight ?? 1}</span>
                              </td>
                              <td class="num hidden px-3 py-2.5 text-right text-xs text-fg-muted lg:table-cell">
                                {d.spec.capacity?.rpm ? `${short(d.spec.capacity.rpm)} RPM` : '–'}<br />{d.spec.capacity?.tpm ? `${short(d.spec.capacity.tpm)} TPM` : ''}
                              </td>
                              <td class="px-3 py-2.5">
                                {#if writable}
                                  <Switch.Root checked={d.spec.enabled !== false} onCheckedChange={(c) => toggle(d, c)} class={switchRoot} aria-label="{d.metadata.name} aktiv">
                                    <Switch.Thumb class={switchThumb} />
                                  </Switch.Root>
                                {:else}
                                  <Badge tone={d.spec.enabled === false ? 'neutral' : 'success'} dot>{d.spec.enabled === false ? 'aus' : 'aktiv'}</Badge>
                                {/if}
                              </td>
                              <td class="px-3 py-2.5 text-right">
                                {#if writable}
                                  <DropdownMenu.Root>
                                    <DropdownMenu.Trigger class={btn('ghost', 'icon-sm')} aria-label="Aktionen für {d.metadata.name}"><Icon name="more" /></DropdownMenu.Trigger>
                                    <DropdownMenu.Portal>
                                      <DropdownMenu.Content class={cx(popover, 'w-44')} align="end" sideOffset={4}>
                                        <DropdownMenu.Item class={menuItem} onSelect={() => editDeployment(d.metadata.name)}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
                                        <DropdownMenu.Separator class={menuSeparator} />
                                        <DropdownMenu.Item
                                          class={cx(menuItem, 'text-danger-fg [&_svg]:text-danger-fg')}
                                          onSelect={() => {
                                            del = { kind: 'deployment', name: d.metadata.name };
                                            delOpen = true;
                                          }}><Icon name="trash" size={14} /> Löschen …</DropdownMenu.Item
                                        >
                                      </DropdownMenu.Content>
                                    </DropdownMenu.Portal>
                                  </DropdownMenu.Root>
                                {/if}
                              </td>
                            </tr>
                          {/each}
                        </tbody>
                      {/each}
                    </table>
                  </div>
                {/if}
              </Card>
            </div>
          {/if}
        </div>
      {/if}
    {/snippet}
  </AsyncState>

<GroupFormDialog
  bind:open={groupOpen}
  editName={groupEdit}
  groups={groups.map((g) => g.metadata.name)}
  onsaved={(n) => {
    toast('success', 'Modellgruppe gespeichert', n);
    selected = n;
    void data.reload();
  }}
/>
<DeploymentFormDialog
  bind:open={depOpen}
  editName={depEdit}
  group={group?.metadata.name ?? ''}
  groups={groups.map((g) => g.metadata.name)}
  onsaved={() => {
    toast('success', 'Deployment gespeichert');
    void data.reload();
  }}
/>
<ConfirmDialog bind:open={delOpen} title={del?.kind === 'group' ? 'Modellgruppe löschen?' : 'Deployment löschen?'} confirmLabel="Löschen" onconfirm={runDelete}>
  {#if del?.kind === 'group'}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht. Nur möglich ohne Deployments und wenn keine andere Gruppe sie als Fallback nutzt. Clients erhalten danach „Modell unbekannt“.</p>
  {:else if del}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht und nach dem Hot Reload nicht mehr geroutet. Zum vorübergehenden Abschalten stattdessen „aktiv“ ausschalten.</p>
  {/if}
</ConfirmDialog>
