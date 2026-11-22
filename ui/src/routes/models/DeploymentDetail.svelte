<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Detailseite eines Deployments (D8/F136, /deployments/{name}): Anbieter (Mock gekennzeichnet, F85), Modell,
  // Endpunkt, Zugangsdaten, Priorität/Gewicht/Kapazität, strukturierte Einstellungen; Nutzung und Audit.
  import { Tabs } from 'bits-ui';
  import { deployments, modelGroups, providerLabel, type Deployment } from '../../lib/api/resources';
  import AsyncState from '../../lib/admin/AsyncState.svelte';
  import AuditTrail from '../../lib/admin/AuditTrail.svelte';
  import DetailLayout from '../../lib/admin/DetailLayout.svelte';
  import { Loader } from '../../lib/admin/errors.svelte';
  import UsagePanel from '../../lib/admin/UsagePanel.svelte';
  import { num } from '../../lib/format';
  import { detailPath } from '../../lib/nav';
  import { canWrite } from '../../lib/rbac.svelte';
  import { href } from '../../lib/router.svelte';
  import { toast } from '../../lib/toast.svelte';
  import Badge from '../../lib/ui/Badge.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Card from '../../lib/ui/Card.svelte';
  import DeploymentFormDialog from './DeploymentFormDialog.svelte';
  import { DEPLOYMENT_SECTIONS } from './settings';
  import SettingsView from './SettingsView.svelte';

  let { name }: { name: string } = $props();
  const data = new Loader<Deployment>(async () => (await deployments.get(name)).data);
  let formOpen = $state(false);
  let groups = $state<string[]>([]);
  async function edit() {
    groups = (await modelGroups.list()).map((g) => g.metadata.name);
    formOpen = true;
  }
  const writable = $derived(canWrite({ kind: 'catalog' }));
</script>

<AsyncState loader={data}>
  {#snippet children(d)}
    <DetailLayout
      yaml="kinds=Deployment&name={encodeURIComponent(d.metadata.name)}"
      back={detailPath('model-group', d.spec.modelGroup)}
      backLabel="Modellgruppe {d.spec.modelGroup}"
      title={d.metadata.name}
      subtitle="{providerLabel(d.spec.provider)} · {d.spec.providerModel}"
      tabs={[
        { value: 'overview', label: 'Übersicht' },
        { value: 'usage', label: 'Nutzung' },
        { value: 'audit', label: 'Audit' },
      ]}
      testid="deployment-detail"
    >
      {#snippet badge()}
        <Badge tone={d.spec.enabled === false ? 'neutral' : 'success'} dot>{d.spec.enabled === false ? 'deaktiviert' : 'aktiv'}</Badge>
        {#if d.spec.provider === 'mock'}<Badge tone="warning" icon="sparkles">Mock-Anbieter</Badge>{/if}
      {/snippet}
      {#snippet actions()}
        {#if writable}<Button size="sm" icon="pencil" onclick={edit} data-testid="edit-deployment">Bearbeiten</Button>{/if}
      {/snippet}
      <Tabs.Content value="overview" class="flex flex-col gap-4">
        {#if d.spec.provider === 'mock'}
          <p class="rounded-md border border-warning-fg/30 bg-warning-bg px-3 py-2 text-sm text-warning-fg" role="status" data-testid="mock-provider-hint">
            Eingebauter Echo-Anbieter für Tests und Demos – keine echten Modellantworten. In Produktion mit <span class="font-mono">admin.allow_mock: false</span> sperren (F85).
          </p>
        {/if}
        <div class="grid gap-4 lg:grid-cols-2">
          <Card title="Anbieter">
            <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
              <dt class="text-fg-muted">Modellgruppe</dt>
              <dd><a class="font-mono text-sm hover:text-accent-text" href={href(detailPath('model-group', d.spec.modelGroup))}>{d.spec.modelGroup}</a></dd>
              <dt class="text-fg-muted">Anbieter</dt><dd>{providerLabel(d.spec.provider)}</dd>
              <dt class="text-fg-muted">Modell</dt><dd class="font-mono text-sm">{d.spec.providerModel}</dd>
              <dt class="text-fg-muted">Endpunkt</dt><dd class="font-mono text-sm break-all">{d.spec.endpoint ?? 'Standard des Anbieters'}</dd>
              <dt class="text-fg-muted">Zugangsdaten</dt><dd class="font-mono text-sm">{d.spec.credential ?? '–'}</dd>
            </dl>
          </Card>
          <Card title="Routing">
            <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
              <dt class="text-fg-muted">Priorität</dt><dd class="num">{d.spec.priority ?? 0}</dd>
              <dt class="text-fg-muted">Gewicht</dt><dd class="num">{d.spec.weight ?? 1}</dd>
              <dt class="text-fg-muted">Kapazität</dt>
              <dd class="num">{d.spec.capacity?.rpm ? `${num(d.spec.capacity.rpm)} RPM` : '–'}{d.spec.capacity?.tpm ? ` · ${num(d.spec.capacity.tpm)} TPM` : ''}</dd>
            </dl>
          </Card>
        </div>
        <Card><SettingsView settings={d.spec.settings ?? {}} sections={DEPLOYMENT_SECTIONS} /></Card>
      </Tabs.Content>
      <Tabs.Content value="usage"><UsagePanel filter={{ deployment: name }} logsQuery="deployment={encodeURIComponent(name)}" /></Tabs.Content>
      <Tabs.Content value="audit"><Card flush><AuditTrail query={{ kind: 'deployment', object: name }} showObject={false} /></Card></Tabs.Content>
    </DetailLayout>
    <DeploymentFormDialog bind:open={formOpen} editName={name} group={d.spec.modelGroup} {groups} onsaved={() => (toast('success', 'Deployment gespeichert'), data.reload())} />
  {/snippet}
</AsyncState>
