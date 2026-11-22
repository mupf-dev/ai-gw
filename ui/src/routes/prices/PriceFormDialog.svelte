<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Preisversion anlegen bzw. künftige Version ändern (Admin-API §2.5). Gültige Versionen sind unveränderlich –
  // dafür eine neue Version mit späterem „gültig ab“ anlegen (Vorlage: `template`).
  import { untrack } from 'svelte';
  import { prices, PROVIDERS, type Price, type S } from '../../lib/api/resources';
  import { fromLocalInput, handleError, toLocalInput } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';

  let {
    open = $bindable(false),
    editName = null,
    template = null,
    onsaved,
  }: { open?: boolean; editName?: string | null; template?: Price | null; onsaved: (name: string) => void } = $props();

  type TokenKey = keyof S['TokenPrices'];
  const TOKENS: { key: TokenKey; label: string }[] = [
    { key: 'input', label: 'Eingabe' },
    { key: 'cachedInput', label: 'Eingabe (Cache-Lesen)' },
    { key: 'output', label: 'Ausgabe' },
    { key: 'cacheWrite', label: 'Cache-Schreiben' },
    { key: 'reasoning', label: 'Reasoning' },
    { key: 'imageInput', label: 'Bild-Eingabe' },
    { key: 'audioInput', label: 'Audio-Eingabe' },
    { key: 'audioOutput', label: 'Audio-Ausgabe' },
  ];
  const ANY = '__any__';
  const blank = () => ({ provider: ANY as string, model: '', validFrom: '', tokens: {} as Record<string, string>, perRequest: '', perImage: '', perAudioMinute: '', perMillionCharacters: '' });
  let form = $state(blank());
  let loaded = $state<Price | null>(null);
  let etag = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});
  let formError = $state<string | null>(null);
  let busy = $state(false);

  function fill(p: Price, keepDate: boolean) {
    form = {
      provider: p.spec.provider ?? ANY,
      model: p.spec.model,
      validFrom: keepDate ? toLocalInput(p.spec.validFrom) : '',
      tokens: Object.fromEntries(Object.entries(p.spec.perMillionTokens ?? {}).filter(([, v]) => v != null).map(([k, v]) => [k, String(v)])),
      perRequest: p.spec.perRequest ?? '',
      perImage: p.spec.perImage ?? '',
      perAudioMinute: p.spec.perAudioMinute ?? '',
      perMillionCharacters: p.spec.perMillionCharacters ?? '',
    };
  }

  $effect(() => {
    if (!open) return;
    const n = editName;
    const t = template;
    untrack(() => {
      errors = {};
      formError = null;
      loaded = null;
      etag = null;
      if (n) void load(n);
      else if (t) fill(t, false);
      else form = blank();
    });
  });

  async function load(n: string) {
    try {
      const r = await prices.get(n);
      loaded = r.data;
      etag = r.etag;
      fill(r.data, true);
    } catch (e) {
      handleError(e, { action: 'Preis laden' });
      open = false;
    }
  }

  const AMOUNT = /^\d+(?:[.,]\d+)?$/;
  const amount = (s: string) => s.trim().replace(',', '.');

  async function submit(ev: SubmitEvent) {
    ev.preventDefault();
    const e: Record<string, string> = {};
    if (!form.model.trim()) e.model = 'Bitte das Modell beim Anbieter angeben.';
    const validFrom = fromLocalInput(form.validFrom);
    if (!validFrom) e.validFrom = 'Bitte einen Zeitpunkt angeben.';
    const perMillionTokens: S['TokenPrices'] = {};
    for (const t of TOKENS) {
      const v = form.tokens[t.key]?.trim();
      if (!v) continue;
      if (!AMOUNT.test(v)) e[t.key] = 'Dezimalzahl ≥ 0, z. B. 2.50';
      else perMillionTokens[t.key] = amount(v);
    }
    for (const k of ['perRequest', 'perImage', 'perAudioMinute', 'perMillionCharacters'] as const) if (form[k].trim() && !AMOUNT.test(form[k].trim())) e[k] = 'Dezimalzahl ≥ 0.';
    errors = e;
    formError = null;
    if (Object.keys(e).length || !validFrom) return;
    const spec: S['PriceSpec'] = {
      provider: form.provider === ANY ? null : (form.provider as S['ProviderKind']),
      model: form.model.trim(),
      validFrom,
      perMillionTokens,
      perRequest: form.perRequest.trim() ? amount(form.perRequest) : null,
      perImage: form.perImage.trim() ? amount(form.perImage) : null,
      perAudioMinute: form.perAudioMinute.trim() ? amount(form.perAudioMinute) : null,
      perMillionCharacters: form.perMillionCharacters.trim() ? amount(form.perMillionCharacters) : null,
    };
    busy = true;
    try {
      if (editName) {
        await prices.update(editName, { metadata: { name: editName, labels: loaded?.metadata.labels ?? {} }, spec }, etag);
        onsaved(editName);
      } else {
        const r = await prices.create({ metadata: { name: '' }, spec });
        onsaved(r.data.metadata.name);
      }
      open = false;
    } catch (err) {
      handleError(err, { action: 'Preis speichern', form: (m) => (formError = m), reload: () => editName && load(editName) });
    } finally {
      busy = false;
    }
  }
</script>

<Dialog
  bind:open
  title={editName ? `Künftige Version „${editName}“ ändern` : 'Preisversion anlegen'}
  description="Beträge je 1 Mio. Tokens in der Abrechnungswährung; leere Felder gelten als 0."
  size="lg"
  testid="price-form-dialog"
>
  <form id="price-form" class="grid gap-5 sm:grid-cols-2" onsubmit={submit} novalidate>
    <Field label="Anbieter" hint="„Alle Anbieter“ gilt für jedes Deployment mit diesem Modell.">
      {#snippet children({ id, describedby })}
        <Select {id} {describedby} bind:value={form.provider} items={[{ value: ANY, label: 'Alle Anbieter' }, ...PROVIDERS.map((p) => ({ value: p.value, label: p.label }))]} />
      {/snippet}
    </Field>
    <Field label="Modell beim Anbieter" required hint="wie Deployment.providerModel" error={errors.model}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={form.model} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="gpt-4o-2024-08-06" autocomplete="off" />
      {/snippet}
    </Field>
    <Field label="Gültig ab" required hint="Lokale Zeit; gespeichert als UTC. Vergangene Zeitpunkte = Import historischer Preise." error={errors.validFrom} class="sm:col-span-2">
      {#snippet children({ id, describedby, invalid })}
        <Input {id} type="datetime-local" bind:value={form.validFrom} aria-describedby={describedby} aria-invalid={invalid || undefined} />
      {/snippet}
    </Field>
    <fieldset class="grid gap-3 sm:col-span-2 sm:grid-cols-4">
      <legend class="mb-1.5 text-sm font-medium text-fg">Je 1 Mio. Tokens</legend>
      {#each TOKENS as t (t.key)}
        <Field label={t.label} error={errors[t.key]}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.tokens[t.key]} inputmode="decimal" placeholder="0" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
      {/each}
    </fieldset>
    <Field label="Je Anfrage" optional error={errors.perRequest}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={form.perRequest} inputmode="decimal" placeholder="0" aria-describedby={describedby} aria-invalid={invalid || undefined} />
      {/snippet}
    </Field>
    <Field label="Je erzeugtem Bild" optional error={errors.perImage}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={form.perImage} inputmode="decimal" placeholder="0" aria-describedby={describedby} aria-invalid={invalid || undefined} />
      {/snippet}
    </Field>
    <Field label="Je Audio-Minute (Transkription)" optional error={errors.perAudioMinute}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={form.perAudioMinute} inputmode="decimal" placeholder="0" aria-describedby={describedby} aria-invalid={invalid || undefined} />
      {/snippet}
    </Field>
    <Field label="Je 1 Mio. Zeichen (Sprachausgabe)" optional error={errors.perMillionCharacters}>
      {#snippet children({ id, describedby, invalid })}
        <Input {id} bind:value={form.perMillionCharacters} inputmode="decimal" placeholder="0" aria-describedby={describedby} aria-invalid={invalid || undefined} />
      {/snippet}
    </Field>
    <div class="sm:col-span-2"><FormError message={formError} /></div>
  </form>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="price-form" icon="check" loading={busy} disabled={busy} data-testid="price-form-submit">{editName ? 'Speichern' : 'Anlegen'}</Button>
  {/snippet}
</Dialog>
