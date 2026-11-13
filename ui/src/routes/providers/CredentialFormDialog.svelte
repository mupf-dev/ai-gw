<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export type CredentialMode = 'create' | 'replace' | 'patch';
</script>

<script lang="ts">
  // Anbieter-Zugangsdaten (Admin-API §2.4). Werte sind nur schreibbar – das Formular zeigt nie gespeicherte Werte.
  // - Anlegen/Ersetzen (POST/PUT): Anbieter, Pflichtfelder der gewählten Alternative, optionale/weitere Felder.
  // - Werte ändern (PATCH, JSON Merge Patch): einzelne Felder neu setzen oder entfernen, der Rest bleibt erhalten.
  import { Checkbox, RadioGroup } from 'bits-ui';
  import { untrack } from 'svelte';
  import { credentials, CREDENTIAL_FIELDS, FIELD_NAME, LONG_FIELDS, NAME_HINT, NAME_RE, PROVIDERS, type Credential, type S } from '../../lib/api/resources';
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
      const alt = Math.max(0, req.alternatives.findIndex((a) => a.fields.every((f) => r.data.spec.fields.includes(f))));
      form = { name: n, provider: r.data.spec.provider, alt: String(alt) };
    } catch (e) {
      handleError(e, { action: 'Zugangsdaten laden' });
      open = false;
    } finally {
      loading = false;
    }
  }

  const req = $derived(CREDENTIAL_FIELDS[form.provider]);
  const required = $derived(req.alternatives[Number(form.alt)]?.fields ?? []);
  const optional = $derived(req.optional.filter((f) => !required.includes(f)));
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
        errors = e;
        if (Object.keys(e).length) return;
        if (!Object.keys(data).length) {
          formError = 'Keine Änderung angegeben.';
          return;
        }
        // Merge Patch: null entfernt ein Feld (das generierte Schema kennt null als Wert nicht).
        await credentials.patch(n, { spec: { data: data as Record<string, S['CredentialValue']> } }, etag);
      } else {
        const data = collect();
        if (!data) return;
        const body: S['ProviderCredentialInput'] = { metadata: { name: n, labels: loaded?.metadata.labels ?? {} }, spec: { provider: form.provider, data } };
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
                      <span class="block font-mono text-xs text-fg-muted">{a.fields.join(' + ')}</span>
                    </span>
                  {/snippet}
                </RadioGroup.Item>
              {/each}
            </RadioGroup.Root>
          </fieldset>
        {:else if req.alternatives.length === 0}
          <p class="text-sm text-fg-muted">Für diesen Anbieter sind beliebige Felder erlaubt (z. B. <code class="font-mono">apiKey</code> für einen geschützten Endpunkt).</p>
        {/if}
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
