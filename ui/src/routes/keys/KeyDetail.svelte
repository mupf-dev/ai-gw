<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Detailseite eines API-Keys (D8/F136, /keys/{name}): Übersicht (Zuordnung, Modelle, Limits, Budgetverbrauch,
  // Plugin-Bindungen mit failMode), Nutzung (Analytics), Audit-Verlauf; Aktionen Bearbeiten, Rotieren, Sperren/
  // Entsperren, endgültig Widerrufen (F37), Löschen.
  import { Tabs } from 'bits-ui';
  import { apiKeys, type ApiKey, type S } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import AuditTrail from '../../lib/admin/AuditTrail.svelte';
  import BudgetMeters from '../../lib/admin/BudgetMeters.svelte';
  import DetailLayout from '../../lib/admin/DetailLayout.svelte';
  import { Loader } from '../../lib/admin/errors.svelte';
  import LimitWarnings from '../../lib/admin/LimitWarnings.svelte';
  import PluginBindings from '../../lib/admin/PluginBindings.svelte';
  import SecretDialog from '../../lib/admin/SecretDialog.svelte';
  import UsagePanel from '../../lib/admin/UsagePanel.svelte';
  import { orgOfTeam, teamLabel } from '../../lib/context.svelte';
  import { ago, dateTime, num } from '../../lib/format';
  import { detailPath } from '../../lib/nav';
  import { canWriteKey } from '../../lib/rbac.svelte';
  import { href, navigate } from '../../lib/router.svelte';
  import { toast } from '../../lib/toast.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import CodeBlock from '../../lib/ui/CodeBlock.svelte';
  import KeyActionDialog from './KeyActionDialog.svelte';
  import KeyFormDialog from './KeyFormDialog.svelte';
  import { keyStatus, type KeyAction } from './keyState';
  import RotateKeyDialog from './RotateKeyDialog.svelte';

  let { name }: { name: string } = $props();

  const key = new Loader<ApiKey>(async () => (await apiKeys.get(name)).data);
  let formOpen = $state(false);
  let rotateOpen = $state(false);
  let act = $state<KeyAction | null>(null);
  let actOpen = $state(false);
  let secret = $state<string | null>(null);
  let secretTitle = $state('');

  const writable = $derived(key.data ? canWriteKey(key.data.spec.team, key.data.spec.organization ?? orgOfTeam(key.data.spec.team)) : false);
  function ask(a: KeyAction) {
    act = a;
    actOpen = true;
  }
  const PERIOD: Record<S['BudgetPeriod'], string> = { day: 'Tag', week: 'Woche', month: 'Monat', total: 'gesamt' };
  const WINDOW: Record<string, string> = { minute: 'Minute', hour: 'Stunde', day: 'Tag' };
  const METRIC: Record<string, string> = { requests: 'Anfragen', tokens: 'Tokens', 'input-tokens': 'Eingabe-Tokens', 'output-tokens': 'Ausgabe-Tokens' };
</script>

<AsyncState loader={key}>
  {#snippet children(k)}
    {@const st = keyStatus(k)}
    <DetailLayout
      yaml="kinds=ApiKey&name={encodeURIComponent(k.metadata.name)}"
      back="/keys"
      backLabel="Alle Keys"
      title={k.metadata.name}
      subtitle="sk-aigw-{k.status.prefix}… · {k.spec.team ? `Team ${teamLabel(k.spec.team)}` : 'ohne Team'} · Organisation {k.spec.organization}"
      tabs={[
        { value: 'overview', label: 'Übersicht' },
        { value: 'usage', label: 'Nutzung' },
        { value: 'audit', label: 'Audit' },
      ]}
      testid="key-detail"
    >
      {#snippet badge()}<Badge tone={st.tone} dot>{st.label}</Badge>{/snippet}
      {#snippet actions()}
        {#if writable && k.status.state !== 'revoked'}
          <Button size="sm" icon="pencil" onclick={() => (formOpen = true)}>Bearbeiten</Button>
          {#if k.status.state === 'active'}
            <Button size="sm" icon="refresh" onclick={() => (rotateOpen = true)}>Rotieren</Button>
            <Button size="sm" icon="ban" onclick={() => ask('suspend')} data-testid="key-suspend">Sperren</Button>
          {:else}
            <Button size="sm" icon="check-circle" onclick={() => ask('resume')} data-testid="key-resume">Entsperren</Button>
          {/if}
          <Button size="sm" variant="danger" icon="x-circle" onclick={() => ask('revoke')} data-testid="key-revoke">Endgültig widerrufen</Button>
        {/if}
        {#if writable}
          <Button size="sm" variant="ghost" icon="trash" aria-label="Key löschen" onclick={() => ask('delete')} />
        {/if}
      {/snippet}

      <Tabs.Content value="overview" class="flex flex-col gap-4">
        <LimitWarnings kind="api-key" name={name} />
        {#if k.status.state === 'revoked'}
          <p class="rounded-md border border-danger-fg/30 bg-danger-bg px-3 py-2 text-sm text-danger-fg" role="status">Dieser Key ist endgültig widerrufen und lässt sich nicht mehr ändern, entsperren oder rotieren.</p>
        {:else if k.status.state === 'suspended'}
          <p class="rounded-md border border-warning-fg/30 bg-warning-bg px-3 py-2 text-sm text-warning-fg" role="status">Vorübergehend gesperrt – Anfragen werden mit 401 abgewiesen, bis der Key entsperrt wird.</p>
        {/if}
        <div class="grid gap-4 lg:grid-cols-2">
          <Card title="Zuordnung">
            <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
              <dt class="text-fg-muted">Organisation</dt>
              <dd><a class="font-mono text-sm hover:text-accent-text" href={href(detailPath('organization', k.spec.organization))}>{k.spec.organization}</a></dd>
              <dt class="text-fg-muted">Team</dt>
              <dd>{#if k.spec.team}<a class="font-mono text-sm hover:text-accent-text" href={href(detailPath('team', k.spec.team))}>{k.spec.team}</a>{:else}<span class="text-fg-muted">kein Team</span>{/if}</dd>
              <dt class="text-fg-muted">Nutzer</dt>
              <dd class="font-mono text-sm">{k.spec.user ?? '–'}</dd>
              <dt class="text-fg-muted">Ablauf</dt>
              <dd class="num">{k.spec.expiresAt ? dateTime(Date.parse(k.spec.expiresAt)) : 'nie'}</dd>
              <dt class="text-fg-muted">Zuletzt genutzt</dt>
              <dd class="num">{k.status.lastUsedAt ? ago(Date.parse(k.status.lastUsedAt)) : 'nie'}</dd>
              <dt class="text-fg-muted">Angelegt</dt>
              <dd class="num">{k.metadata.createdAt ? dateTime(Date.parse(k.metadata.createdAt)) : '–'}</dd>
            </dl>
          </Card>
          <Card title="Modelle und Limits">
            <div class="flex flex-col gap-3">
              <div>
                <h3 class="label-caps mb-1.5 text-fg-subtle">Erlaubte Modelle</h3>
                <div class="flex flex-wrap gap-1">
                  {#each k.spec.allowedModels ?? [] as m (m)}<a href={href(detailPath('model-group', m))}><Badge mono>{m}</Badge></a>{:else}<Badge tone="outline">Alle, die das Team erlaubt</Badge>{/each}
                </div>
              </div>
              <div>
                <h3 class="label-caps mb-1.5 text-fg-subtle">Ratenlimits</h3>
                {#if k.spec.limits?.length}
                  <ul class="flex flex-col gap-1 text-sm">
                    {#each k.spec.limits as l, i (i)}<li class="num">{num(l.limit)} {METRIC[l.metric] ?? l.metric} je {WINDOW[l.window ?? 'minute'] ?? l.window}{l.modelGroup ? ` · ${l.modelGroup}` : ''}</li>{/each}
                  </ul>
                {:else}<p class="text-sm text-fg-muted">keine eigenen (Team-/Organisationslimits gelten)</p>{/if}
              </div>
              <div>
                <h3 class="label-caps mb-1.5 text-fg-subtle">Budget</h3>
                <p class="num text-sm">{k.spec.budget ? `${k.spec.budget.amount} je ${PERIOD[k.spec.budget.period]}${k.spec.budget.hardLimit === false ? ' (nur Warnung)' : ''}` : 'unbegrenzt'}</p>
              </div>
            </div>
          </Card>
        </div>
        {#if k.spec.budget}
          <Card flush title="Budgetverbrauch" description="laufender Zeitraum"><BudgetMeters level="api-key" name={k.metadata.name} /></Card>
        {/if}
        <Card flush title="Plugins" description="Bindungen direkt an diesem Key mit wirksamem Fehlermodus"><PluginBindings level="api-key" target={k.metadata.name} /></Card>
        {#if k.spec.metadata && Object.keys(k.spec.metadata).length}
          <CodeBlock title="Metadaten" code={k.spec.metadata} maxHeight="max-h-48" />
        {/if}
      </Tabs.Content>
      <Tabs.Content value="usage">
        <UsagePanel filter={{ key: k.metadata.name }} logsQuery="key={encodeURIComponent(k.metadata.name)}" />
      </Tabs.Content>
      <Tabs.Content value="audit">
        <Card flush><AuditTrail query={{ kind: 'api-key', object: k.metadata.name }} showObject={false} /></Card>
      </Tabs.Content>
    </DetailLayout>
  {/snippet}
</AsyncState>

<KeyFormDialog bind:open={formOpen} editName={name} oncreated={() => {}} onsaved={() => (toast('success', 'Key gespeichert'), key.reload())} />
<RotateKeyDialog
  bind:open={rotateOpen}
  {name}
  onrotated={(r) => {
    secretTitle = `Key „${r.newKey.key.metadata.name}“ erstellt (Rotation)`;
    secret = r.newKey.secret;
    void key.reload();
  }}
/>
<SecretDialog bind:secret title={secretTitle} description="Der alte Key läuft nach der Übergangszeit ab." ondone={() => navigate(detailPath('key', name))} />
<KeyActionDialog
  bind:open={actOpen}
  action={act}
  {name}
  ondone={(a) => {
    if (a === 'delete') navigate('/keys');
    else void key.reload();
  }}
/>
