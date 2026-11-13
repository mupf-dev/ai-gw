<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detailseite eines Teams (D8/F136, /teams/{name}): Übersicht (Organisation, Modelle, Limits, Payload-Modus und
  // Reasoning mit geerbtem Wert, Budgetverbrauch, Keys des Teams, Plugin-Bindungen), Nutzung, Audit-Verlauf.
  import { Tabs } from 'bits-ui';
  import { listPage } from '../../lib/api/client';
  import { apiKeys, organizations, teams, type ApiKey, type Organization, type Team } from '../../lib/api/resources';
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
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import EntityFormDialog from './EntityFormDialog.svelte';
  import { keyStatus } from '../keys/keyState';

  let { name }: { name: string } = $props();

  const data = new Loader(async () => {
    const team = (await teams.get(name)).data as Team;
    const [org, keys] = await Promise.all([
      organizations
        .get(team.spec.organization)
        .then((r) => r.data as Organization)
        .catch(() => null),
      listPage<ApiKey>(apiKeys.path, { team: name }, 50),
    ]);
    return { team, org, keys };
  });
  let formOpen = $state(false);
  const writable = $derived(data.data ? canWrite({ kind: 'team', organization: data.data.team.spec.organization, team: name }) : false);
</script>

<AsyncState loader={data}>
  {#snippet children({ team, org, keys })}
    <DetailLayout
      back="/teams?tab=team"
      backLabel="Alle Teams"
      title={team.metadata.name}
      subtitle="{team.spec.displayName ? `${team.spec.displayName} · ` : ''}Organisation {team.spec.organization}"
      tabs={[
        { value: 'overview', label: 'Übersicht' },
        { value: 'usage', label: 'Nutzung' },
        { value: 'audit', label: 'Audit' },
      ]}
      testid="team-detail"
    >
      {#snippet actions()}
        <Button size="sm" icon="key" href={href(`/keys?team=${encodeURIComponent(name)}`)}>Keys</Button>
        {#if writable}<Button size="sm" icon="pencil" onclick={() => (formOpen = true)} data-testid="edit-entity">Bearbeiten</Button>{/if}
      {/snippet}
      <Tabs.Content value="overview" class="flex flex-col gap-4">
        <LimitWarnings kind="team" name={name} />
        <div class="grid gap-4 lg:grid-cols-2">
          <Card title="Einstellungen">
            <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
              <dt class="text-fg-muted">Organisation</dt>
              <dd><a class="font-mono text-sm hover:text-accent-text" href={href(detailPath('organization', team.spec.organization))}>{team.spec.organization}</a></dd>
              <dt class="text-fg-muted">Modelle</dt>
              <dd class="flex flex-wrap gap-1">{#each team.spec.allowedModels ?? [] as m (m)}<a href={href(detailPath('model-group', m))}><Badge mono>{m}</Badge></a>{:else}<Badge tone="outline">Alle Modelle</Badge>{/each}</dd>
              <InheritedSettings spec={team.spec} parents={[{ label: `Organisation ${team.spec.organization}`, spec: org?.spec ?? null }]} />
            </dl>
          </Card>
          <Card title="Limits"><LimitList limits={team.spec.limits ?? []} /></Card>
        </div>
        {#if team.spec.budget}
          <Card flush title="Budgetverbrauch" description="laufender Zeitraum"><BudgetMeters level="team" name={name} /></Card>
        {/if}
        <Card flush title="Keys" description={keys.nextCursor ? 'die ersten 50' : `${keys.items.length} Keys`}>
          {#snippet actions()}<Button size="sm" variant="ghost" iconRight="chevron-right" href={href(`/keys?team=${encodeURIComponent(name)}`)}>Alle</Button>{/snippet}
          <ul class="divide-y divide-line" data-testid="team-keys">
            {#each keys.items as k (k.metadata.name)}
              {@const st = keyStatus(k)}
              <li class="flex items-center gap-3 px-4 py-2">
                <a class="min-w-0 flex-1 truncate font-mono text-sm hover:text-accent-text" href={href(detailPath('key', k.metadata.name))}>{k.metadata.name}</a>
                <Badge tone={st.tone} dot>{st.label}</Badge>
              </li>
            {:else}
              <li class="px-4 py-3 text-sm text-fg-muted">Noch keine Keys in diesem Team.</li>
            {/each}
          </ul>
        </Card>
        <Card flush title="Plugins" description="Bindungen am Team mit wirksamem Fehlermodus"><PluginBindings level="team" target={name} /></Card>
      </Tabs.Content>
      <Tabs.Content value="usage"><UsagePanel filter={{ team: name }} logsQuery="team={encodeURIComponent(name)}" /></Tabs.Content>
      <Tabs.Content value="audit"><Card flush><AuditTrail query={{ kind: 'team', object: name }} showObject={false} /></Card></Tabs.Content>
    </DetailLayout>
  {/snippet}
</AsyncState>

<EntityFormDialog bind:open={formOpen} kind="team" editName={name} onsaved={() => data.reload()} />
