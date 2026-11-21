<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // API-Key anlegen oder bearbeiten (Admin-API §2.1). Anlegen → einmalige Klartext-Anzeige (SecretDialog).
  // Bearbeiten lädt den Key samt ETag und schreibt mit If-Match (412 → Konflikt-Dialog „neu laden“).
  // Budget und RPM/TPM werden auf `spec.budget` bzw. `spec.limits` (requests/tokens je Minute) abgebildet;
  // andere Limits des Keys bleiben unverändert erhalten.
  // Organisation ist Pflicht (F3): mit Team die des Teams, ohne Team frei wählbar (nur globale Rollen).
  import { Checkbox, RadioGroup } from 'bits-ui';
  import { untrack } from 'svelte';
  import { apiKeys, modelGroups, NAME_HINT, NAME_RE, type ApiKey, type S } from '../../lib/api/resources';
  import { fromLocalInput, handleError, toLocalInput } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import { catalog, ctx, label, orgOfTeam } from '../../lib/context.svelte';
  import { isMock } from '../../lib/mode.svelte';
  import { canWrite, canWriteKey } from '../../lib/rbac.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import { checkbox } from '../../lib/ui/styles';
  import InheritFields from '../../lib/admin/InheritFields.svelte';
  import { fromRows, toRows, type LimitRow } from '../../lib/admin/limits';
  import LimitsEditor from '../../lib/admin/LimitsEditor.svelte';
  import WorkloadIdentitiesEditor, { type IdentityRow } from '../../lib/admin/WorkloadIdentitiesEditor.svelte';
  import { auth } from '../../lib/session.svelte';

  let {
    open = $bindable(false),
    editName = null,
    oncreated,
    onsaved,
  }: {
    open?: boolean;
    /** null = anlegen, sonst Name des zu bearbeitenden Keys. */
    editName?: string | null;
    oncreated: (r: S['CreatedApiKey']) => void;
    onsaved: () => void;
  } = $props();

  const NO_TEAM = '__none__';
  const blank = () => ({
    name: '',
    team: ctx.teamId || (canWrite({ kind: 'global' }) ? NO_TEAM : (writableTeams()[0]?.metadata.name ?? NO_TEAM)),
    org: ctx.orgId || (catalog.orgs[0]?.metadata.name ?? ''),
    user: '',
    access: 'all' as 'all' | 'some',
    models: [] as string[],
    budget: '',
    period: 'month' as S['BudgetPeriod'],
    rpm: '',
    tpm: '',
    extraLimits: [] as LimitRow[],
    payloadMode: '',
    exposeReasoning: '',
    expires: '90',
    expiresAt: '',
    identities: [] as IdentityRow[],
  });
  let form = $state(blank());
  let loaded = $state<ApiKey | null>(null);
  let etag = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});
  let formError = $state<string | null>(null);
  let busy = $state(false);
  let groups = $state<string[]>([]);
  let loading = $state(false);
  // Vererbung Key → Team → Organisation (F30/F185) für die Anzeige „erben – …“.
  const inheritParents = $derived.by(() => {
    const team = form.team === NO_TEAM ? null : catalog.teams.find((t) => t.metadata.name === form.team);
    const orgName = team?.spec.organization ?? form.org;
    const org = catalog.orgs.find((o) => o.metadata.name === orgName);
    return [
      ...(team ? [{ label: `Team ${team.metadata.name}`, spec: team.spec }] : []),
      ...(org ? [{ label: `Organisation ${org.metadata.name}`, spec: org.spec }] : []),
    ];
  });

  function writableTeams() {
    return catalog.teams.filter((t) => canWriteKey(t.metadata.name, t.spec.organization));
  }

  $effect(() => {
    if (!open) return;
    const name = editName;
    untrack(() => reset(name));
  });

  function reset(name: string | null) {
    errors = {};
    formError = null;
    modelGroups
      .list()
      .then((g) => (groups = g.map((x) => x.metadata.name)))
      .catch(() => (groups = []));
    if (name) void load(name);
    else {
      loaded = null;
      etag = null;
      form = blank();
    }
  }

  /** RPM/TPM-Schnellfelder: requests/tokens je Minute ohne Modellgruppe; alles andere im Limit-Editor. */
  const quick = (l: S['LimitSpec']) => (l.window ?? 'minute') === 'minute' && !l.modelGroup && (l.metric === 'requests' || l.metric === 'tokens');
  const limitOf = (k: ApiKey, metric: S['LimitMetric']) => k.spec.limits?.find((l) => l.metric === metric && l.window === 'minute' && !l.modelGroup)?.limit;

  async function load(name: string) {
    loading = true;
    try {
      const r = await apiKeys.get(name);
      const k = r.data;
      loaded = k;
      etag = r.etag;
      form = {
        name: k.metadata.name,
        team: k.spec.team ?? NO_TEAM,
        org: k.spec.organization,
        user: k.spec.user ?? '',
        access: k.spec.allowedModels?.length ? 'some' : 'all',
        models: [...(k.spec.allowedModels ?? [])],
        budget: k.spec.budget?.amount ?? '',
        period: k.spec.budget?.period ?? 'month',
        rpm: String(limitOf(k, 'requests') ?? ''),
        tpm: String(limitOf(k, 'tokens') ?? ''),
        extraLimits: toRows((k.spec.limits ?? []).filter((l) => !quick(l))),
        payloadMode: k.spec.payloadMode ?? '',
        exposeReasoning: k.spec.exposeReasoning == null ? '' : String(k.spec.exposeReasoning),
        expires: 'custom',
        expiresAt: toLocalInput(k.spec.expiresAt),
        identities: (k.spec.workloadIdentities ?? []).map((w) => ({ issuer: w.issuer, subject: w.subject, description: w.description ?? '' })),
      };
    } catch (e) {
      handleError(e, { action: 'Key laden' });
      open = false;
    } finally {
      loading = false;
    }
  }

  const optNum = (s: string) => (s.trim() === '' ? null : Number(s.replace(',', '.')));

  function validate(): boolean {
    const e: Record<string, string> = {};
    if (!editName) {
      if (!form.name.trim()) e.name = 'Bitte einen Namen angeben.';
      else if (!NAME_RE.test(form.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.';
    }
    if (form.team === NO_TEAM && !form.org) e.org = 'Bitte eine Organisation wählen.';
    if (form.access === 'some' && form.models.length === 0) e.models = 'Mindestens eine Modellgruppe wählen.';
    for (const [k, l] of [
      ['budget', 'Budget'],
      ['rpm', 'RPM'],
      ['tpm', 'TPM'],
    ] as const) {
      const v = optNum(form[k]);
      if (v !== null && (!Number.isFinite(v) || v <= 0 || (k !== 'budget' && !Number.isInteger(v)))) e[k] = `${l} muss eine positive ${k === 'budget' ? 'Zahl' : 'ganze Zahl'} sein.`;
    }
    if (form.expires === 'custom' && form.expiresAt && !fromLocalInput(form.expiresAt)) e.expiresAt = 'Ungültiges Datum.';
    if (fromRows(form.extraLimits) === null) e.limits = 'Grenzen müssen ganze Zahlen ≥ 1 sein.';
    const ids = form.identities.filter((r) => r.subject.trim());
    if (ids.some((r) => !NAME_RE.test(r.issuer))) e.identities = 'Bitte für jede Identität einen Aussteller wählen.';
    else if (new Set(ids.map((r) => `${r.issuer}/${r.subject.trim().toLowerCase()}`)).size !== ids.length) e.identities = 'Jede Identität nur einmal angeben.';
    errors = e;
    return Object.keys(e).length === 0;
  }

  function spec(): S['ApiKeySpec'] {
    const base: Partial<S['ApiKeySpec']> = loaded?.spec ?? {};
    const keep = fromRows(form.extraLimits) ?? [];
    const rpm = optNum(form.rpm);
    const tpm = optNum(form.tpm);
    const limits: S['LimitSpec'][] = [
      ...keep,
      ...(rpm ? [{ metric: 'requests' as const, window: 'minute' as const, limit: rpm }] : []),
      ...(tpm ? [{ metric: 'tokens' as const, window: 'minute' as const, limit: tpm }] : []),
    ];
    const amount = form.budget.trim().replace(',', '.');
    const expiresAt =
      form.expires === 'never' ? null : form.expires === 'custom' ? fromLocalInput(form.expiresAt) : new Date(Date.now() + Number(form.expires) * 86_400_000).toISOString().replace(/\.\d{3}Z$/, 'Z');
    return {
      ...base,
      organization: effectiveOrg(),
      team: form.team === NO_TEAM ? null : form.team,
      user: form.user.trim() || null,
      allowedModels: form.access === 'all' ? [] : form.models,
      budget: amount ? { ...(base.budget ?? { hardLimit: true, warnAtPercent: [80] }), amount, period: form.period } : null,
      limits,
      expiresAt,
      payloadMode: (form.payloadMode || null) as S['PayloadMode'] | null,
      exposeReasoning: form.exposeReasoning === '' ? null : form.exposeReasoning === 'true',
      workloadIdentities: form.identities
        .filter((r) => r.subject.trim())
        .map((r) => ({ issuer: r.issuer, subject: r.subject.trim(), ...(r.description.trim() ? { description: r.description.trim() } : {}) })),
    };
  }

  async function submit(ev: SubmitEvent) {
    ev.preventDefault();
    formError = null;
    if (!validate()) {
      document.querySelector<HTMLElement>('[data-testid="key-form-dialog"] [aria-invalid="true"]')?.focus();
      return;
    }
    busy = true;
    try {
      if (editName && loaded) {
        await apiKeys.update(editName, { metadata: { name: editName, labels: loaded.metadata.labels ?? {} }, spec: spec() }, etag);
        open = false;
        onsaved();
      } else {
        const r = await apiKeys.createWithSecret({ metadata: { name: form.name.trim() }, spec: spec() });
        open = false;
        oncreated(r.data);
      }
    } catch (e) {
      handleError(e, { action: editName ? 'Key speichern' : 'Key erstellen', form: (m) => (formError = m), reload: () => editName && load(editName) });
    } finally {
      busy = false;
    }
  }

  /** Organisation des Keys: mit Team die des Teams, sonst die gewählte. */
  function effectiveOrg(): string {
    return (form.team !== NO_TEAM ? orgOfTeam(form.team) : null) ?? form.org;
  }
  const orgItems = $derived(catalog.orgs.map((o) => ({ value: o.metadata.name, label: label(o) })));

  function toggleModel(m: string, on: boolean) {
    form.models = on ? [...form.models, m] : form.models.filter((x) => x !== m);
  }
  const teamItems = $derived([
    ...(canWrite({ kind: 'global' }) ? [{ value: NO_TEAM, label: 'Kein Team', hint: 'Freigaben nur über den Key' }] : []),
    ...writableTeams().map((t) => ({ value: t.metadata.name, label: label(t), hint: t.spec.organization })),
  ]);
</script>

<Dialog
  bind:open
  title={editName ? `Key „${editName}“ bearbeiten` : 'Key erstellen'}
  description={editName ? 'Änderungen wirken nach dem Hot Reload sofort (ohne Neustart).' : 'Virtueller API-Key für Anwendungen; der Klartext wird nur einmal angezeigt.'}
  size="lg"
  testid="key-form-dialog"
>
  {#if loading}
    <div class="h-40 animate-pulse rounded-md bg-skeleton" aria-busy="true"></div>
  {:else}
    <form id="key-form" class="grid gap-5 sm:grid-cols-2" onsubmit={submit} novalidate>
      {#if !editName}
        <Field label="Name" required hint={NAME_HINT} error={errors.name} class="sm:col-span-2">
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.name} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="support-bot-prod" autocomplete="off" />
          {/snippet}
        </Field>
      {/if}
      <Field label="Team" hint="Team-Freigaben und -Limits gelten zusätzlich.">
        {#snippet children({ id, describedby })}
          <Select {id} {describedby} bind:value={form.team} items={teamItems} />
        {/snippet}
      </Field>
      {#if form.team === NO_TEAM}
        <Field label="Organisation" required hint="Keys ohne Team erben Limits, Budget und Plugins der Organisation." error={errors.org}>
          {#snippet children({ id, describedby })}
            <Select {id} {describedby} bind:value={form.org} items={orgItems} />
          {/snippet}
        </Field>
      {:else}
        <Field label="Organisation" hint="Organisation des Teams.">
          {#snippet children({ id, describedby })}
            <Input {id} value={effectiveOrg()} aria-describedby={describedby} readonly />
          {/snippet}
        </Field>
      {/if}
      <Field label="Ablauf" hint="Abgelaufene Keys werden abgewiesen (401).">
        {#snippet children({ id, describedby })}
          <Select
            {id}
            {describedby}
            bind:value={form.expires}
            items={[
              { value: '30', label: 'nach 30 Tagen' },
              { value: '90', label: 'nach 90 Tagen' },
              { value: '365', label: 'nach 1 Jahr' },
              { value: 'custom', label: 'Datum wählen …' },
              { value: 'never', label: 'nie', hint: 'nicht empfohlen' },
            ]}
          />
        {/snippet}
      </Field>
      {#if form.expires === 'custom'}
        <Field label="Läuft ab am" optional error={errors.expiresAt} class="sm:col-span-2">
          {#snippet children({ id, describedby, invalid })}
            <Input {id} type="datetime-local" bind:value={form.expiresAt} aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
      {/if}
      <Field label="Nutzer" optional hint="Besitzender Nutzer (Name), erscheint im Request-Log." class="sm:col-span-2">
        {#snippet children({ id, describedby })}
          <Input {id} bind:value={form.user} aria-describedby={describedby} autocomplete="off" placeholder="z. B. anna" />
        {/snippet}
      </Field>

      <fieldset class="flex flex-col gap-2 sm:col-span-2">
        <legend class="mb-1.5 text-sm font-medium text-fg">Modellzugriff</legend>
        <RadioGroup.Root bind:value={form.access} class="grid gap-2 sm:grid-cols-2" aria-label="Modellzugriff">
          {#each [{ v: 'all', t: 'Alle Modellgruppen', d: 'die das Team erlaubt' }, { v: 'some', t: 'Ausgewählte', d: 'nur explizit freigegebene' }] as o (o.v)}
            <RadioGroup.Item
              value={o.v}
              class="flex items-start gap-2.5 rounded-lg border border-line p-3 text-left hover:border-line-strong data-[state=checked]:border-accent data-[state=checked]:bg-accent-soft"
            >
              {#snippet children({ checked })}
                <span class={['mt-0.5 grid size-4 shrink-0 place-items-center rounded-full border', checked ? 'border-accent' : 'border-line-strong']}>
                  {#if checked}<span class="size-2 rounded-full bg-accent"></span>{/if}
                </span>
                <span>
                  <span class="block text-base font-medium text-fg">{o.t}</span>
                  <span class="block text-sm text-fg-muted">{o.d}</span>
                </span>
              {/snippet}
            </RadioGroup.Item>
          {/each}
        </RadioGroup.Root>
        {#if form.access === 'some'}
          <div class="mt-1 grid gap-1.5 rounded-lg border border-line bg-surface-2 p-3 sm:grid-cols-2" role="group" aria-label="Modellgruppen">
            {#each [...new Set([...groups, ...form.models])] as m (m)}
              <label class="flex items-center gap-2 text-base">
                <Checkbox.Root checked={form.models.includes(m)} onCheckedChange={(c) => toggleModel(m, c)} class={checkbox}>
                  {#snippet children({ checked })}
                    {#if checked}<Icon name="check" size={12} />{/if}
                  {/snippet}
                </Checkbox.Root>
                <span class="font-mono text-sm">{m}</span>
              </label>
            {:else}
              <p class="text-sm text-fg-muted">Noch keine Modellgruppen angelegt.</p>
            {/each}
          </div>
          {#if errors.models}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.models}</p>{/if}
        {/if}
      </fieldset>

      <div class="grid grid-cols-[1fr_8rem] gap-3">
        <Field label="Budget" optional hint="Leer = unbegrenzt; harte Grenze." error={errors.budget}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.budget} inputmode="decimal" placeholder="z. B. 250" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
        <Field label="Zeitraum">
          {#snippet children({ id })}
            <Select
              {id}
              bind:value={form.period}
              items={[
                { value: 'day', label: 'Tag' },
                { value: 'week', label: 'Woche' },
                { value: 'month', label: 'Monat' },
                { value: 'total', label: 'gesamt' },
              ]}
            />
          {/snippet}
        </Field>
      </div>
      <div class="grid grid-cols-2 gap-3">
        <Field label="RPM" optional error={errors.rpm}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.rpm} inputmode="numeric" placeholder="unbegrenzt" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
        <Field label="TPM" optional error={errors.tpm}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.tpm} inputmode="numeric" placeholder="unbegrenzt" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
      </div>
      <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
        <legend class="mb-1 text-sm font-medium text-fg">Weitere Limits <span class="text-xs font-normal text-fg-subtle">(andere Fenster, z. B. Kalendermonat, oder je Modellgruppe)</span></legend>
        <LimitsEditor bind:value={form.extraLimits} {groups} />
        {#if errors.limits}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.limits}</p>{/if}
      </fieldset>
      <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
        <legend class="mb-1 text-sm font-medium text-fg">
          Workload-/Agent-Identitäten <span class="text-xs font-normal text-fg-subtle">(Anmeldung mit Access-Token statt Key, z. B. Microsoft Entra Agent ID)</span>
        </legend>
        <WorkloadIdentitiesEditor bind:value={form.identities} issuers={auth.workloadIssuers} />
        {#if errors.identities}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.identities}</p>{/if}
      </fieldset>
      <InheritFields bind:payloadMode={form.payloadMode} bind:exposeReasoning={form.exposeReasoning} parents={inheritParents} />
      <div class="sm:col-span-2"><FormError message={formError} /></div>
    </form>
  {/if}
  {#snippet footer()}
    {#if isMock()}<span class="mr-auto hidden text-xs text-fg-muted sm:block">Mock-Modus: der Key funktioniert nicht am echten Gateway.</span>{/if}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="key-form" icon={editName ? 'check' : 'key'} loading={busy} disabled={busy || loading} data-testid="key-form-submit">
      {editName ? 'Speichern' : 'Key erstellen'}
    </Button>
  {/snippet}
</Dialog>
