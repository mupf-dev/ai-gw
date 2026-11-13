<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detailseite einer Organisation (D8/F136, /organizations/{name}): Übersicht (Limits, Payload-Modus/Reasoning,
  // Budgetverbrauch, Teams, Plugin-Bindungen), Nutzung, Audit-Verlauf.
  import { Tabs } from 'bits-ui';
  import { listPage } from '../../lib/api/client';
  import { organizations, teams, type Organization, type Team } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import AuditTrail from '../../lib/admin/AuditTrail.svelte';
  import BudgetMeters from '../../lib/admin/BudgetMeters.svelte';
  import DetailLayout from '../../lib/admin/DetailLayout.svelte';
  import { Loader } from '../../lib/admin/errors.svelte';
  import InheritedSettings from '../../lib/admin/InheritedSettings.svelte';
  import LimitList from '../../lib/admin/LimitList.svelte';
  import LimitWarnings from '../../lib/admin/LimitWarnings.svelte';
  import PluginBindings from '../../lib/admin/PluginBindings.svelte';
  import UsagePanel from '../../lib/admin/UsagePanel.svelte';
  import { detailPath } from '../../lib/nav';
  import { canWrite } from '../../lib/rbac.svelte';
  import { href } from '../../lib/router.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import EntityFormDialog from './EntityFormDialog.svelte';

  let { name }: { name: string } = $props();

  const data = new Loader(async () => {
    const [org, ts] = await Promise.all([organizations.get(name).then((r) => r.data as Organization), listPage<Team>(teams.path, { organization: name }, 100)]);
    return { org, teams: ts };
  });
  let formOpen = $state(false);
  const writable = $derived(canWrite({ kind: 'organization', organization: name }));
</script>

<AsyncState loader={data}>
  {#snippet children({ org, teams: ts })}
    <DetailLayout
      yaml="organization={encodeURIComponent(org.metadata.name)}"
      back="/teams?tab=org"
      backLabel="Alle Organisationen"
      title={org.metadata.name}
      subtitle={org.spec.displayName ?? undefined}
      tabs={[
        { value: 'overview', label: 'Übersicht' },
        { value: 'usage', label: 'Nutzung' },
        { value: 'audit', label: 'Audit' },
      ]}
      testid="org-detail"
    >
      {#snippet actions()}
        {#if writable}<Button size="sm" icon="pencil" onclick={() => (formOpen = true)} data-testid="edit-entity">Bearbeiten</Button>{/if}
      {/snippet}
      <Tabs.Content value="overview" class="flex flex-col gap-4">
        <LimitWarnings kind="organization" name={name} />
        <div class="grid gap-4 lg:grid-cols-2">
          <Card title="Einstellungen">
            <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
              <InheritedSettings spec={org.spec} />
            </dl>
          </Card>
          <Card title="Limits und Budget"><LimitList limits={org.spec.limits ?? []} budget={org.spec.budget} /></Card>
        </div>
        {#if org.spec.budget}
          <Card flush title="Budgetverbrauch" description="laufender Zeitraum"><BudgetMeters level="organization" {name} /></Card>
        {/if}
        <Card flush title="Teams" description="{ts.items.length}{ts.nextCursor ? '+' : ''} Teams">
          {#snippet actions()}<Button size="sm" variant="ghost" iconRight="chevron-right" href={href(`/teams?tab=team&organization=${encodeURIComponent(name)}`)}>Alle</Button>{/snippet}
          <ul class="divide-y divide-line" data-testid="org-teams">
            {#each ts.items as t (t.metadata.name)}
              <li class="flex items-center gap-3 px-4 py-2">
                <a class="min-w-0 flex-1 truncate hover:text-accent-text" href={href(detailPath('team', t.metadata.name))}>{t.spec.displayName || t.metadata.name}</a>
                <span class="font-mono text-xs text-fg-muted">{t.metadata.name}</span>
              </li>
            {:else}
              <li class="px-4 py-3 text-sm text-fg-muted">Noch keine Teams.</li>
            {/each}
          </ul>
        </Card>
        <Card flush title="Plugins" description="Bindungen an der Organisation mit wirksamem Fehlermodus"><PluginBindings level="organization" target={name} /></Card>
      </Tabs.Content>
      <Tabs.Content value="usage"><UsagePanel filter={{ organization: name }} logsQuery="organization={encodeURIComponent(name)}" /></Tabs.Content>
      <Tabs.Content value="audit"><Card flush><AuditTrail query={{ kind: 'organization', object: name }} showObject={false} /></Card></Tabs.Content>
    </DetailLayout>
  {/snippet}
</AsyncState>

<EntityFormDialog bind:open={formOpen} kind="org" editName={name} onsaved={() => data.reload()} />
