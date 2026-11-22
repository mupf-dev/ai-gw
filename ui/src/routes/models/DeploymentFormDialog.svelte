<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Deployment anlegen/bearbeiten (Admin-API §2.3): Anbieter, Anbieter-Modell, Endpunkt (SSRF-geprüft),
  // Zugangsdaten desselben Anbieters, Priorität (0 vor 1), Gewicht, Kapazität (RPM/TPM), aktiv, Einstellungen.
  import { Switch } from 'bits-ui';
  import { untrack } from 'svelte';
  import { credentials, deployments, NAME_HINT, NAME_RE, PROVIDERS, type Credential, type Deployment, type S } from '../../lib/api/resources';
  import { handleError } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import { switchRoot, switchThumb } from '../../lib/ui/styles';
  import SettingsEditor from './SettingsEditor.svelte';
  import { DEPLOYMENT_SECTIONS } from './settings';

  let {
    open = $bindable(false),
    editName = null,
    group,
    groups,
    onsaved,
  }: { open?: boolean; editName?: string | null; group: string; groups: string[]; onsaved: () => void } = $props();

  const NONE = '__none__';
  const blank = () => ({
    name: '',
    modelGroup: group,
    provider: 'openai' as S['ProviderKind'],
    providerModel: '',
    endpoint: '',
    credential: NONE,
    priority: '0',
    weight: '1',
    rpm: '',
    tpm: '',
    enabled: true,
    settings: {} as Record<string, unknown>,
  });
  let form = $state(blank());
  let settingsInvalid = $state(false);
  let loaded = $state<Deployment | null>(null);
  let etag = $state<string | null>(null);
  let creds = $state<Credential[]>([]);
  let errors = $state<Record<string, string>>({});
  let formError = $state<string | null>(null);
  let busy = $state(false);
  let loading = $state(false);

  $effect(() => {
    if (!open) return;
    const n = editName;
    untrack(() => {
      errors = {};
      formError = null;
      credentials
        .list()
        .then((c) => (creds = c))
        .catch(() => (creds = []));
      if (n) void load(n);
      else {
        loaded = null;
        etag = null;
        form = blank();
      }
    });
  });

  async function load(name: string) {
    loading = true;
    try {
      const r = await deployments.get(name);
      const s = r.data.spec;
      loaded = r.data;
      etag = r.etag;
      form = {
        name,
        modelGroup: s.modelGroup,
        provider: s.provider,
        providerModel: s.providerModel,
        endpoint: s.endpoint ?? '',
        credential: s.credential ?? NONE,
        priority: String(s.priority ?? 0),
        weight: String(s.weight ?? 1),
        rpm: s.capacity?.rpm ? String(s.capacity.rpm) : '',
        tpm: s.capacity?.tpm ? String(s.capacity.tpm) : '',
        enabled: s.enabled !== false,
        settings: { ...((s.settings ?? {}) as Record<string, unknown>) },
      };
    } catch (e) {
      handleError(e, { action: 'Deployment laden' });
      open = false;
    } finally {
      loading = false;
    }
  }

  const provider = $derived(PROVIDERS.find((p) => p.value === form.provider));
  const SETTINGS_HINT: Partial<Record<S['ProviderKind'], string>> = {
    'azure-openai': '{"apiVersion": "2024-10-21"} – Pflicht bei apiStyle „deployment“ (Standard); {"apiStyle": "v1"} ohne apiVersion.',
    bedrock: '{"region": "eu-central-1"} – Pflicht.',
    vertex: '{"project": "…", "location": "europe-west4"}',
  };
  const credItems = $derived([
    { value: NONE, label: 'Keine Zugangsdaten', hint: form.provider === 'mock' || form.provider === 'ollama' ? 'üblich für diesen Anbieter' : 'nur ohne Authentifizierung' },
    ...creds.filter((c) => c.spec.provider === form.provider).map((c) => ({ value: c.metadata.name, label: c.metadata.name, hint: c.spec.fields.join(', ') })),
  ]);
  $effect(() => {
    // Anbieterwechsel: Zugangsdaten eines anderen Anbieters verwerfen.
    const p = form.provider;
    untrack(() => {
      if (form.credential !== NONE && creds.length && !creds.some((c) => c.metadata.name === form.credential && c.spec.provider === p)) form.credential = NONE;
    });
  });

  const int = (s: string) => (s.trim() === '' ? null : Number(s));

  async function submit(ev: SubmitEvent) {
    ev.preventDefault();
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(form.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.';
    if (!form.providerModel.trim()) e.providerModel = 'Bitte das Modell beim Anbieter angeben.';
    if (provider?.endpoint === 'required' && !form.endpoint.trim()) e.endpoint = `Für ${provider.label} ist ein Endpunkt Pflicht.`;
    const prio = int(form.priority);
    if (prio === null || !Number.isInteger(prio) || prio < 0) e.priority = 'Ganze Zahl ≥ 0.';
    const weight = int(form.weight);
    if (weight === null || !Number.isInteger(weight) || weight < 1 || weight > 1_000_000) e.weight = '1 – 1 000 000 (abschalten über „aktiv“).';
    for (const k of ['rpm', 'tpm'] as const) {
      const v = int(form[k]);
      if (v !== null && (!Number.isInteger(v) || v <= 0)) e[k] = 'Positive ganze Zahl.';
    }
    const settings = $state.snapshot(form.settings);
    if (settingsInvalid) {
      e.settings = 'Einstellungen prüfen (ungültige Zahl bzw. kein gültiges JSON).';
    }
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    const name = editName ?? form.name;
    const spec: S['DeploymentSpec'] = {
      ...(loaded?.spec ?? {}),
      modelGroup: form.modelGroup,
      provider: form.provider,
      providerModel: form.providerModel.trim(),
      endpoint: form.endpoint.trim() || null,
      credential: form.credential === NONE ? null : form.credential,
      priority: prio ?? 0,
      weight: weight ?? 1,
      capacity: { rpm: int(form.rpm), tpm: int(form.tpm) },
      enabled: form.enabled,
      settings: settings as S['DeploymentSpec']['settings'],
    };
    busy = true;
    try {
      if (editName) await deployments.update(name, { metadata: { name, labels: loaded?.metadata.labels ?? {} }, spec }, etag);
      else await deployments.create({ metadata: { name }, spec });
      open = false;
      onsaved();
    } catch (err) {
      handleError(err, { action: 'Deployment speichern', form: (m) => (formError = m), reload: () => editName && load(editName) });
    } finally {
      busy = false;
    }
  }
</script>

<Dialog bind:open title={editName ? `Deployment „${editName}“ bearbeiten` : 'Deployment anlegen'} description="Konkretes Anbieter-Modell hinter der Modellgruppe." size="lg" testid="deployment-form-dialog">
  {#if loading}
    <div class="h-40 animate-pulse rounded-md bg-skeleton" aria-busy="true"></div>
  {:else}
    <form id="deployment-form" class="grid gap-5 sm:grid-cols-2" onsubmit={submit} novalidate>
      {#if !editName}
        <Field label="Name" required hint={NAME_HINT} error={errors.name}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.name} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="gpt-4o-azure-swc" autocomplete="off" />
          {/snippet}
        </Field>
      {/if}
      <Field label="Modellgruppe">
        {#snippet children({ id })}
          <Select {id} bind:value={form.modelGroup} items={groups.map((g) => ({ value: g, label: g }))} />
        {/snippet}
      </Field>
      <Field label="Anbieter" required>
        {#snippet children({ id })}
          <Select {id} bind:value={form.provider} items={PROVIDERS.map((p) => ({ value: p.value, label: p.label }))} />
        {/snippet}
      </Field>
      <Field
        label="Modell beim Anbieter"
        required
        hint={form.provider === 'azure-openai' ? 'Azure-Deployment-Name.' : form.provider === 'mock' ? 'mock, mock-error oder mock-stream-error.' : 'z. B. gpt-4o-2024-11-20'}
        error={errors.providerModel}
      >
        {#snippet children({ id, describedby, invalid })}
          <Input {id} bind:value={form.providerModel} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" />
        {/snippet}
      </Field>
      {#if provider?.endpoint !== 'none'}
        <Field
          label="Endpunkt"
          required={provider?.endpoint === 'required'}
          optional={provider?.endpoint === 'optional'}
          hint="https:// (http nur für freigeschaltete Hosts); private/interne Ziele nur mit admin.allow_private_upstreams."
          error={errors.endpoint}
          class="sm:col-span-2"
        >
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.endpoint} type="url" aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="https://…" autocomplete="off" />
          {/snippet}
        </Field>
      {/if}
      <Field label="Zugangsdaten" hint="Nur Zugangsdaten desselben Anbieters." class="sm:col-span-2">
        {#snippet children({ id, describedby })}
          <Select {id} {describedby} bind:value={form.credential} items={credItems} />
        {/snippet}
      </Field>
      <div class="grid grid-cols-2 gap-3 sm:col-span-2 md:grid-cols-4">
        <Field label="Priorität" hint="0 vor 1 (z. B. PTU vor PAYG)." error={errors.priority}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.priority} inputmode="numeric" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
        <Field label="Gewicht" hint="Anteil bei „Gewichtet“." error={errors.weight}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.weight} inputmode="numeric" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
        <Field label="Kapazität RPM" optional error={errors.rpm}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.rpm} inputmode="numeric" placeholder="–" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
        <Field label="Kapazität TPM" optional error={errors.tpm}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.tpm} inputmode="numeric" placeholder="–" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
      </div>
      <section class="flex flex-col gap-1.5 sm:col-span-2" aria-label="Einstellungen">
        <h3 class="text-sm font-medium text-fg">Einstellungen</h3>
        <p class="text-xs text-fg-subtle">Anbieterspezifisches unter „Erweitert“: {SETTINGS_HINT[form.provider] ?? 'z. B. {"maxTokensField": "max_completion_tokens"}.'}</p>
        <SettingsEditor bind:value={form.settings} bind:invalid={settingsInvalid} sections={DEPLOYMENT_SECTIONS} testid="deployment-settings" />
        {#if errors.settings}<p class="text-xs text-danger-fg">{errors.settings}</p>{/if}
      </section>
      <div class="flex items-center gap-3 sm:col-span-2">
        <Switch.Root id="dep-enabled" bind:checked={form.enabled} class={switchRoot}><Switch.Thumb class={switchThumb} /></Switch.Root>
        <label for="dep-enabled" class="text-base">Aktiv <span class="text-sm text-fg-muted">– deaktivierte Deployments bleiben gespeichert, werden aber nicht geroutet</span></label>
      </div>
      <div class="sm:col-span-2"><FormError message={formError} /></div>
    </form>
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="deployment-form" icon="check" loading={busy} disabled={busy || loading} data-testid="deployment-form-submit">
      {editName ? 'Speichern' : 'Anlegen'}
    </Button>
  {/snippet}
</Dialog>
