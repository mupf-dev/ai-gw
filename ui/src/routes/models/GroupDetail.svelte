<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detailseite einer Modellgruppe (D8/F136, /models/{name}): Übersicht (Strategie, Fallback-Kette, strukturierte
  // Einstellungen, Deployments nach Priorität mit Mock-Kennzeichnung F85), Nutzung, Plugins (failMode), Audit.
  import { DropdownMenu, Switch, Tabs } from 'bits-ui';
  import { deployments, FALLBACK_TRIGGERS, modelGroups, providerLabel, ROUTING, type Deployment, type ModelGroup } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import AuditTrail from '../../lib/admin/AuditTrail.svelte';
  import ConfirmDialog from '../../lib/admin/ConfirmDialog.svelte';
  import DetailLayout from '../../lib/admin/DetailLayout.svelte';
  import { formErrorOf, handleError, Loader } from '../../lib/admin/errors.svelte';
  import LimitWarnings from '../../lib/admin/LimitWarnings.svelte';
  import PluginBindings from '../../lib/admin/PluginBindings.svelte';
  import UsagePanel from '../../lib/admin/UsagePanel.svelte';
  import { short } from '../../lib/format';
  import { detailPath } from '../../lib/nav';
  import { canWrite } from '../../lib/rbac.svelte';
  import { href, navigate } from '../../lib/router.svelte';
  import { toast } from '../../lib/toast.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import EmptyState from '../../lib/ui/EmptyState.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, switchRoot, switchThumb } from '../../lib/ui/styles';
  import DeploymentFormDialog from './DeploymentFormDialog.svelte';
  import GroupFormDialog from './GroupFormDialog.svelte';
  import { GROUP_SECTIONS } from './settings';
  import SettingsView from './SettingsView.svelte';

  let { name }: { name: string } = $props();

  const data = new Loader(async () => {
    const [g, d, all] = await Promise.all([
      modelGroups.get(name).then((r) => r.data),
      deployments.list({ modelGroup: name }),
      modelGroups.list().then((l) => l.map((x) => x.metadata.name)),
    ]);
    return { group: g as ModelGroup, deps: d, groups: all };
  });
  let groupOpen = $state(false);
  let depOpen = $state(false);
  let depEdit = $state<string | null>(null);
  let del = $state<{ kind: 'group' | 'deployment'; name: string } | null>(null);
  let delOpen = $state(false);

  const writable = $derived(canWrite({ kind: 'catalog' }));
  const tiersOf = (deps: Deployment[]) => {
    const m = new Map<number, Deployment[]>();
    for (const d of deps) m.set(d.spec.priority ?? 0, [...(m.get(d.spec.priority ?? 0) ?? []), d]);
    return [...m.entries()].sort((a, b) => a[0] - b[0]);
  };
  const strategy = (g: ModelGroup) => ROUTING.find((r) => r.value === (g.spec.routing ?? 'weighted'));
  function editDeployment(n: string | null) {
    depEdit = n;
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
    const { kind, name: n } = del;
    const res = kind === 'group' ? modelGroups : deployments;
    try {
      const { etag } = await res.get(n);
      await res.remove(n, etag);
      toast('success', kind === 'group' ? 'Modellgruppe gelöscht' : 'Deployment gelöscht', n);
      if (kind === 'group') navigate('/models');
      else void data.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, kind === 'group' ? 'Modellgruppe löschen' : 'Deployment löschen', () => void data.reload());
    }
  }
  const triggers = (on: string[] | undefined) => (on?.length ? on.map((t) => FALLBACK_TRIGGERS.find((x) => x.value === t)?.label ?? t).join(', ') : 'jeder Fehler');
</script>

<AsyncState loader={data}>
  {#snippet children({ group, deps, groups })}
    {@const tiers = tiersOf(deps)}
    <DetailLayout
      yaml="kinds=ModelGroup&name={encodeURIComponent(group.metadata.name)}"
      back="/models"
      backLabel="Alle Modellgruppen"
      title={group.metadata.name}
      subtitle="{strategy(group)?.label} · {deps.filter((d) => d.spec.enabled !== false).length} von {deps.length} Deployments aktiv · Fähigkeiten: {group.spec.capabilities?.length ? group.spec.capabilities.join(', ') : 'chat'}"
      tabs={[
        { value: 'overview', label: 'Übersicht' },
        { value: 'usage', label: 'Nutzung' },
        { value: 'plugins', label: 'Plugins' },
        { value: 'audit', label: 'Audit' },
      ]}
      testid="group-detail"
    >
      {#snippet badge()}
        {#each group.spec.aliases ?? [] as a (a)}<Badge mono tone="outline">{a}</Badge>{/each}
      {/snippet}
      {#snippet actions()}
        {#if writable}
          <Button size="sm" icon="pencil" onclick={() => (groupOpen = true)} data-testid="edit-group">Bearbeiten</Button>
          <Button
            size="sm"
            variant="ghost"
            icon="trash"
            aria-label="Modellgruppe löschen"
            onclick={() => {
              del = { kind: 'group', name };
              delOpen = true;
            }}
          />
        {/if}
      {/snippet}
      <Tabs.Content value="overview" class="flex min-w-0 flex-col gap-4">
        <LimitWarnings kind="model-group" name={name} />
      <Card>
        <dl class="grid gap-x-6 gap-y-3 text-base sm:grid-cols-3">
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
                  <a class="rounded-sm" href={href(detailPath('model-group', f.modelGroup))} title="bei: {triggers(f.on)}"><Badge mono>{f.modelGroup}</Badge></a>
                  <span class="text-xs text-fg-muted">({triggers(f.on)})</span>
                </li>
              {/each}
            </ol>
          {:else}
            <p class="text-sm text-fg-muted">Keine Fallbacks – Fehler gehen nach den Retries an den Client.</p>
          {/if}
        </div>
        <div class="mt-4"><SettingsView settings={group.spec.settings ?? {}} sections={GROUP_SECTIONS} /></div>
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
                        <a class="block font-mono text-sm font-medium text-fg hover:text-accent-text" href={href(detailPath('deployment', d.metadata.name))}>{d.metadata.name}</a>
                        <span class="block truncate font-mono text-xs text-fg-muted">{d.spec.providerModel}{d.spec.endpoint ? ` · ${d.spec.endpoint.replace(/^https?:\/\//, '')}` : ''}</span>
                      </td>
                      <td class="hidden px-3 py-2.5 md:table-cell">
                        <span class="flex items-center gap-1.5 text-sm">{providerLabel(d.spec.provider)}{#if d.spec.provider === 'mock'}<Badge tone="warning" icon="sparkles">Mock-Anbieter</Badge>{/if}</span>
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
      </Tabs.Content>
      <Tabs.Content value="usage"><UsagePanel filter={{ modelGroup: name }} logsQuery="modelGroup={encodeURIComponent(name)}" /></Tabs.Content>
      <Tabs.Content value="plugins"><Card flush title="Plugins" description="Bindungen an der Modellgruppe mit wirksamem Fehlermodus"><PluginBindings level="model-group" target={name} /></Card></Tabs.Content>
      <Tabs.Content value="audit"><Card flush><AuditTrail query={{ kind: 'model-group', object: name }} showObject={false} /></Card></Tabs.Content>
    </DetailLayout>

    <GroupFormDialog bind:open={groupOpen} editName={name} {groups} onsaved={(n) => (toast('success', 'Modellgruppe gespeichert', n), data.reload())} />
    <DeploymentFormDialog bind:open={depOpen} editName={depEdit} group={name} {groups} onsaved={() => (toast('success', 'Deployment gespeichert'), data.reload())} />
  {/snippet}
</AsyncState>

<ConfirmDialog bind:open={delOpen} title={del?.kind === 'group' ? 'Modellgruppe löschen?' : 'Deployment löschen?'} confirmLabel="Löschen" onconfirm={runDelete}>
  {#if del?.kind === 'group'}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht. Nur möglich ohne Deployments und wenn keine andere Gruppe sie als Fallback nutzt. Clients erhalten danach „Modell unbekannt“.</p>
  {:else if del}
    <p>„<span class="font-mono text-fg">{del.name}</span>“ wird gelöscht und nach dem Hot Reload nicht mehr geroutet. Zum vorübergehenden Abschalten stattdessen „aktiv“ ausschalten.</p>
  {/if}
</ConfirmDialog>
