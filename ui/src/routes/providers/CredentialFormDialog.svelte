<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export type CredentialMode = 'create' | 'replace' | 'patch';
</script>

<script lang="ts">
  // Anbieter-Zugangsdaten (Admin-API §2.4). Werte sind nur schreibbar – das Formular zeigt nie gespeicherte Werte.
  // - Anlegen/Ersetzen (POST/PUT): Anbieter, Pflichtfelder der gewählten Alternative, optionale/weitere Felder.
  // - Werte ändern (PATCH, JSON Merge Patch): einzelne Felder neu setzen oder entfernen, der Rest bleibt erhalten.
  // - Entra ID (AIGW-84, Azure OpenAI/Anthropic): Art (Managed/Workload Identity, Service Principal) und nicht geheime
  //   IDs als `spec.entra`; nur Secret/Zertifikat sind Werte. Deployments mit `credential` übernehmen die Anmeldung.
  import { Checkbox, RadioGroup } from 'bits-ui';
  import { untrack } from 'svelte';
  import { credentials, CREDENTIAL_FIELDS, ENTRA_KINDS, FIELD_NAME, LONG_FIELDS, NAME_HINT, NAME_RE, PROVIDERS, type Credential, type S } from '../../lib/api/resources';
  import { handleError } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import { btn, checkbox, input, textarea } from '../../lib/ui/styles';

  let {
    open = $bindable(false),
    mode = 'create',
    name = null,
    onsaved,
  }: { open?: boolean; mode?: CredentialMode; name?: string | null; onsaved: (name: string) => void } = $props();

  let form = $state({ name: '', provider: 'openai' as S['ProviderKind'], alt: '0' });
  let values = $state<Record<string, string>>({});
  const NO_ENTRA = { tenantId: '', clientId: '', authority: '', scope: '', sendX5c: false };
  let entra = $state({ ...NO_ENTRA });
  let extra = $state<{ key: string; value: string }[]>([]);
  let remove = $state<Record<string, boolean>>({});
  let loaded = $state<Credential | null>(null);
  let etag = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});
  let formError = $state<string | null>(null);
  let busy = $state(false);
  let loading = $state(false);

  $effect(() => {
    if (!open) return;
    const n = name;
    const m = mode;
    untrack(() => {
      errors = {};
      formError = null;
      values = {};
      extra = [];
      remove = {};
      entra = { ...NO_ENTRA };
      if (m !== 'create' && n) void load(n);
      else {
        loaded = null;
        etag = null;
        form = { name: '', provider: 'openai', alt: '0' };
      }
    });
  });

  async function load(n: string) {
    loading = true;
    try {
      const r = await credentials.get(n);
      loaded = r.data;
      etag = r.etag;
      const req = CREDENTIAL_FIELDS[r.data.spec.provider];
      const e = r.data.spec.entra;
      const has = (f: string) => r.data.spec.fields.includes(f);
      let alt = e
        ? req.alternatives.findIndex((a) => a.entra === e.credential)
        : req.alternatives.findIndex((a) => !a.entra && a.fields.length > 0 && a.fields.every(has));
      // Ältere Form (IDs als Geheimwerte ohne spec.entra): Service-Principal-Variante vorschlagen.
      if (alt < 0 && !e) alt = req.alternatives.findIndex((a) => a.entra && a.fields.length > 0 && a.fields.every(has));
      entra = { tenantId: e?.tenantId ?? '', clientId: e?.clientId ?? '', authority: e?.authority ?? '', scope: e?.scope ?? '', sendX5c: e?.sendX5c ?? false };
      form = { name: n, provider: r.data.spec.provider, alt: String(Math.max(0, alt)) };
    } catch (e) {
      handleError(e, { action: 'Zugangsdaten laden' });
      open = false;
    } finally {
      loading = false;
    }
  }

  const req = $derived(CREDENTIAL_FIELDS[form.provider]);
  const required = $derived(req.alternatives[Number(form.alt)]?.fields ?? []);
  const entraKind = $derived(mode === 'patch' ? loaded?.spec.entra?.credential : req.alternatives[Number(form.alt)]?.entra);
  const servicePrincipal = $derived(entraKind === 'clientSecret' || entraKind === 'clientCertificate');
  const GUID_OR_DOMAIN = /^[A-Za-z0-9._-]{1,256}$/;

  /** `spec.entra` aus dem Formular (leere Felder weggelassen); `null` bei Fehlern (in `e` eingetragen). */
  function entraBody(e: Record<string, string>): S['EntraAuth'] | null {
    if (!entraKind) return null;
    const t = entra.tenantId.trim();
    const c = entra.clientId.trim();
    const a = entra.authority.trim();
    const sc = entra.scope.trim();
    if (servicePrincipal && !t) e.tenantId = 'Pflichtfeld für Service Principals.';
    if (servicePrincipal && !c) e.clientId = 'Pflichtfeld für Service Principals.';
    if (t && !GUID_OR_DOMAIN.test(t)) e.tenantId = 'GUID oder Domänenname.';
    if (c && !GUID_OR_DOMAIN.test(c)) e.clientId = 'GUID oder Domänenname.';
    if (a && !/^https:\/\/[^/?#]+\/?$/.test(a)) e.authority = 'Nur https://host, z. B. https://login.microsoftonline.us.';
    return {
      credential: entraKind,
      ...(t ? { tenantId: t } : {}),
      ...(c ? { clientId: c } : {}),
      ...(a && entraKind !== 'workloadIdentity' ? { authority: a } : {}),
      ...(sc ? { scope: sc } : {}),
      ...(entraKind === 'clientCertificate' && entra.sendX5c ? { sendX5c: true } : {}),
    };
  }
  const optional = $derived([...req.optional, ...(req.alternatives[Number(form.alt)]?.optional ?? [])].filter((f) => !required.includes(f)));
  const fieldLabel = (f: string) => f.replace(/([A-Z])/g, ' $1').replace(/^./, (c) => c.toUpperCase());

  function collect(): Record<string, S['CredentialValue']> | null {
    const e: Record<string, string> = {};
    const data: Record<string, S['CredentialValue']> = {};
    for (const f of required) {
      if (!values[f]?.length) e[f] = 'Pflichtfeld.';
      else data[f] = { value: values[f] };
    }
    for (const f of optional) if (values[f]?.length) data[f] = { value: values[f] };
    extra.forEach((x, i) => {
      if (!x.key && !x.value) return;
      if (!FIELD_NAME.test(x.key)) e[`extra-${i}`] = 'Feldname: Buchstabe, dann Buchstaben/Ziffern/_ (max. 64).';
      else if (!x.value) e[`extra-${i}`] = 'Wert fehlt.';
      else if (data[x.key]) e[`extra-${i}`] = 'Feld doppelt.';
      else data[x.key] = { value: x.value };
    });
    errors = e;
    return Object.keys(e).length ? null : data;
  }

  async function submit(ev: SubmitEvent) {
    ev.preventDefault();
    formError = null;
    if (mode === 'create' && !NAME_RE.test(form.name)) {
      errors = { name: 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.' };
      return;
    }
    const n = mode === 'create' ? form.name : (name ?? '');
    busy = true;
    try {
      if (mode === 'patch') {
        const data: Record<string, S['CredentialValue'] | null> = {};
        const e: Record<string, string> = {};
        for (const f of loaded?.spec.fields ?? []) {
          if (remove[f]) data[f] = null;
          else if (values[f]?.length) data[f] = { value: values[f] };
        }
        extra.forEach((x, i) => {
          if (!x.key && !x.value) return;
          if (!FIELD_NAME.test(x.key)) e[`extra-${i}`] = 'Feldname: Buchstabe, dann Buchstaben/Ziffern/_ (max. 64).';
          else if (!x.value) e[`extra-${i}`] = 'Wert fehlt.';
          else data[x.key] = { value: x.value };
        });
        const en = entraBody(e);
        errors = e;
        if (Object.keys(e).length) return;
        const entraChanged = !!en && JSON.stringify(en) !== JSON.stringify(loaded?.spec.entra ?? null);
        if (!Object.keys(data).length && !entraChanged) {
          formError = 'Keine Änderung angegeben.';
          return;
        }
        // Merge Patch: null entfernt ein Feld (das generierte Schema kennt null als Wert nicht).
        await credentials.patch(n, { spec: { data: data as Record<string, S['CredentialValue']>, ...(entraChanged && en ? { entra: en } : {}) } }, etag);
      } else {
        const data = collect();
        const e: Record<string, string> = { ...errors };
        const en = entraBody(e);
        errors = e;
        if (!data || Object.keys(e).length) return;
        const body: S['ProviderCredentialInput'] = {
          metadata: { name: n, labels: loaded?.metadata.labels ?? {} },
          spec: { provider: form.provider, data, ...(en ? { entra: en } : {}) },
        };
        if (mode === 'replace') await credentials.update(n, body, etag);
        else await credentials.create(body);
      }
      values = {};
      extra = [];
      open = false;
      onsaved(n);
    } catch (err) {
      handleError(err, { action: 'Zugangsdaten speichern', form: (m) => (formError = m), reload: () => name && load(name) });
    } finally {
      busy = false;
    }
  }
  const TITLE = { create: 'Zugangsdaten anlegen', replace: 'Alle Werte ersetzen', patch: 'Werte ändern' };
</script>

{#snippet secretInput(f: string, id: string, describedby: string | undefined, invalid: boolean)}
  {#if LONG_FIELDS.has(f)}
    <textarea {id} class={[textarea, 'min-h-24']} bind:value={values[f]} aria-describedby={describedby} aria-invalid={invalid || undefined} spellcheck="false" autocomplete="off"></textarea>
  {:else}
    <Input {id} type="password" bind:value={values[f]} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="new-password" spellcheck={false} />
  {/if}
{/snippet}

{#snippet entraInputs()}
  <fieldset class="grid gap-4 rounded-lg border border-line p-3 sm:grid-cols-2" data-testid="credential-entra">
    <legend class="px-1 text-sm font-medium text-fg">Entra ID · {ENTRA_KINDS.find((k) => k.value === entraKind)?.label}</legend>
    <p class="text-sm text-fg-muted sm:col-span-2">{ENTRA_KINDS.find((k) => k.value === entraKind)?.hint}. Diese Angaben sind nicht geheim und werden angezeigt.</p>
    <Field label="Client-ID" required={servicePrincipal} optional={!servicePrincipal} hint={entraKind === 'managedIdentity' ? 'benutzerzugewiesene Identität (leer = systemzugewiesen)' : 'Anwendungs-ID (GUID)'} error={errors.clientId}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={entra.clientId} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="11111111-2222-3333-4444-555555555555" autocomplete="off" spellcheck={false} />{/snippet}
    </Field>
    <Field label="Tenant-ID" required={servicePrincipal} optional={!servicePrincipal} hint="GUID oder Domäne" error={errors.tenantId}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={entra.tenantId} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="contoso.onmicrosoft.com" autocomplete="off" spellcheck={false} />{/snippet}
    </Field>
    {#if entraKind !== 'workloadIdentity'}
      <Field label="Authority" optional hint="nur Sovereign Clouds" error={errors.authority}>
        {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={entra.authority} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="https://login.microsoftonline.com" autocomplete="off" spellcheck={false} />{/snippet}
      </Field>
    {/if}
    <Field label="Scope" optional hint="Standard: Cognitive Services" error={errors.scope}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={entra.scope} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="https://cognitiveservices.azure.com/.default" autocomplete="off" spellcheck={false} />{/snippet}
    </Field>
    {#if entraKind === 'clientCertificate'}
      <label class="flex items-start gap-2 text-sm sm:col-span-2" data-testid="credential-entra-x5c">
        <Checkbox.Root bind:checked={entra.sendX5c} class="{checkbox} mt-0.5">
          {#snippet children({ checked })}{#if checked}<Icon name="check" size={12} />{/if}{/snippet}
        </Checkbox.Root>
        <span>
          <span class="block font-medium text-fg">Zertifikatskette mitsenden (x5c)</span>
          <span class="block text-fg-muted">Für die Anmeldung über Subject Name + Issuer (SNI), z. B. bei automatisch rotierten Zertifikaten.</span>
        </span>
      </label>
    {/if}
  </fieldset>
{/snippet}

<Dialog
  bind:open
  title={mode === 'create' ? TITLE.create : `${TITLE[mode]}: ${name}`}
  description="Werte werden sofort mit dem Master-Key verschlüsselt und nie wieder angezeigt."
  size="lg"
  testid="credential-form-dialog"
>
  {#if loading}
    <div class="h-40 animate-pulse rounded-md bg-skeleton" aria-busy="true"></div>
  {:else}
    <form id="credential-form" class="flex flex-col gap-5" onsubmit={submit} novalidate autocomplete="off">
      {#if mode !== 'patch'}
        <div class="grid gap-5 sm:grid-cols-2">
          {#if mode === 'create'}
            <Field label="Name" required hint={NAME_HINT} error={errors.name}>
              {#snippet children({ id, describedby, invalid })}
                <Input {id} bind:value={form.name} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="openai-prod" autocomplete="off" />
              {/snippet}
            </Field>
          {/if}
          <Field label="Anbieter" required hint={mode === 'replace' ? 'Wechsel nur, wenn kein Deployment die Zugangsdaten nutzt.' : undefined}>
            {#snippet children({ id, describedby })}
              <Select
                {id}
                {describedby}
                value={form.provider}
                items={PROVIDERS.map((p) => ({ value: p.value, label: p.label }))}
                onchange={(v) => {
                  form.provider = v as S['ProviderKind'];
                  form.alt = '0';
                }}
              />
            {/snippet}
          </Field>
        </div>
        {#if req.alternatives.length > 1}
          <fieldset>
            <legend class="mb-1.5 text-sm font-medium text-fg">Anmeldeverfahren</legend>
            <RadioGroup.Root bind:value={form.alt} class="grid gap-2 sm:grid-cols-2" aria-label="Anmeldeverfahren">
              {#each req.alternatives as a, i (a.label)}
                <RadioGroup.Item
                  value={String(i)}
                  class="flex items-start gap-2.5 rounded-lg border border-line p-3 text-left hover:border-line-strong data-[state=checked]:border-accent data-[state=checked]:bg-accent-soft"
                >
                  {#snippet children({ checked })}
                    <span class={['mt-0.5 grid size-4 shrink-0 place-items-center rounded-full border', checked ? 'border-accent' : 'border-line-strong']}>
                      {#if checked}<span class="size-2 rounded-full bg-accent"></span>{/if}
                    </span>
                    <span>
                      <span class="block text-base font-medium text-fg">{a.label}</span>
                      <span class="block font-mono text-xs text-fg-muted">{a.fields.length ? a.fields.join(' + ') : 'ohne gespeichertes Geheimnis'}</span>
                    </span>
                  {/snippet}
                </RadioGroup.Item>
              {/each}
            </RadioGroup.Root>
          </fieldset>
        {:else if req.alternatives.length === 0}
          <p class="text-sm text-fg-muted">Für diesen Anbieter sind beliebige Felder erlaubt (z. B. <code class="font-mono">apiKey</code> für einen geschützten Endpunkt).</p>
        {/if}
        {#if entraKind}{@render entraInputs()}{/if}
        <div class="grid gap-4 sm:grid-cols-2" data-testid="credential-fields">
          {#each required as f (f)}
            <Field label={fieldLabel(f)} required hint={f} error={errors[f]} class={LONG_FIELDS.has(f) ? 'sm:col-span-2' : ''}>
              {#snippet children({ id, describedby, invalid })}{@render secretInput(f, id, describedby, invalid)}{/snippet}
            </Field>
          {/each}
          {#each optional as f (f)}
            <Field label={fieldLabel(f)} optional hint={f}>
              {#snippet children({ id, describedby, invalid })}{@render secretInput(f, id, describedby, invalid)}{/snippet}
            </Field>
          {/each}
        </div>
      {:else if loaded}
        {#if entraKind}{@render entraInputs()}{/if}
        <div class="flex flex-col gap-2" data-testid="credential-fields">
          <p class="text-sm text-fg-muted">Leere Felder bleiben unverändert. Gespeicherte Werte werden nie angezeigt.</p>
          {#each loaded.spec.fields as f (f)}
            <div class="grid items-center gap-2 rounded-lg border border-line bg-surface-2 p-2.5 sm:grid-cols-[10rem_1fr_auto]">
              <label for="patch-{f}" class="font-mono text-sm">{f}</label>
              <input
                id="patch-{f}"
                type="password"
                class={input}
                bind:value={values[f]}
                disabled={remove[f]}
                placeholder="•••••• (unverändert)"
                autocomplete="new-password"
              />
              <label class="flex items-center gap-1.5 text-sm text-danger-fg">
                <Checkbox.Root bind:checked={remove[f]} class={checkbox}>
                  {#snippet children({ checked })}{#if checked}<Icon name="check" size={12} />{/if}{/snippet}
                </Checkbox.Root>
                entfernen
              </label>
            </div>
          {/each}
        </div>
      {/if}

      <fieldset class="flex flex-col gap-2">
        <legend class="mb-1 text-sm font-medium text-fg">Weitere Felder <span class="text-xs font-normal text-fg-subtle">(optional)</span></legend>
        {#each extra as x, i (i)}
          <div class="grid grid-cols-[1fr_1fr_auto] gap-2">
            <input class={[input, 'font-mono']} aria-label="Feldname {i + 1}" placeholder="feldName" bind:value={x.key} aria-invalid={!!errors[`extra-${i}`] || undefined} />
            <input class={input} type="password" aria-label="Wert {i + 1}" placeholder="Wert" bind:value={x.value} autocomplete="new-password" />
            <button type="button" class={btn('ghost', 'icon')} aria-label="Feld {i + 1} entfernen" onclick={() => (extra = extra.filter((_, j) => j !== i))}><Icon name="x" /></button>
          </div>
          {#if errors[`extra-${i}`]}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors[`extra-${i}`]}</p>{/if}
        {/each}
        <div><Button size="sm" icon="plus" onclick={() => (extra = [...extra, { key: '', value: '' }])}>Feld hinzufügen</Button></div>
      </fieldset>
      <FormError message={formError} />
    </form>
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="credential-form" icon="lock" loading={busy} disabled={busy || loading} data-testid="credential-form-submit">
      {mode === 'create' ? 'Anlegen' : 'Speichern'}
    </Button>
  {/snippet}
</Dialog>
