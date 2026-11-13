<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // „Key erstellen“: Formular (Formularmuster mit Validierung beim Absenden) → einmalige Klartext-Anzeige.
  import { Checkbox, RadioGroup } from 'bits-ui';
  import { ctx, currentOrg } from '../../lib/context.svelte';
  import { createKey } from '../../lib/data.svelte';
  import { MODEL_GROUPS } from '../../lib/mock/data';
  import { toast } from '../../lib/toast.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import CodeBlock from '../../lib/ui/CodeBlock.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import SecretReveal from '../../lib/ui/SecretReveal.svelte';
  import Select from '../../lib/ui/Select.svelte';

  let { open = $bindable(false) }: { open?: boolean } = $props();

  const blank = () => ({
    name: '',
    teamId: ctx.teamId || currentOrg().teams[0]?.id || '',
    access: 'all' as 'all' | 'some',
    models: [] as string[],
    budget: '',
    rpm: '600',
    tpm: '',
    expires: '90',
  });
  let form = $state(blank());
  let errors = $state<Record<string, string>>({});
  let secret = $state<string | null>(null);
  let createdName = $state('');
  let confirmed = $state(false);

  $effect(() => {
    if (open && !secret) {
      form = blank();
      errors = {};
    }
  });

  const optNum = (s: string) => (s.trim() === '' ? null : Number(s));

  function validate(): boolean {
    const e: Record<string, string> = {};
    if (!form.name.trim()) e.name = 'Bitte einen Namen angeben.';
    else if (!/^[a-z0-9][a-z0-9-]{2,47}$/.test(form.name)) e.name = '3–48 Zeichen: Kleinbuchstaben, Ziffern, Bindestrich.';
    if (!form.teamId) e.teamId = 'Bitte ein Team wählen.';
    if (form.access === 'some' && form.models.length === 0) e.models = 'Mindestens eine Modellgruppe wählen.';
    for (const [k, label] of [
      ['budget', 'Budget'],
      ['rpm', 'RPM'],
      ['tpm', 'TPM'],
    ] as const) {
      const v = optNum(form[k]);
      if (v !== null && (!Number.isFinite(v) || v <= 0)) e[k] = `${label} muss eine positive Zahl sein.`;
    }
    errors = e;
    return Object.keys(e).length === 0;
  }

  function submit(ev: SubmitEvent) {
    ev.preventDefault();
    if (!validate()) {
      document.querySelector<HTMLElement>('[aria-invalid="true"]')?.focus();
      return;
    }
    const res = createKey({
      name: form.name,
      teamId: form.teamId,
      models: form.access === 'all' ? 'all' : form.models,
      budgetLimit: optNum(form.budget),
      rpm: optNum(form.rpm),
      tpm: optNum(form.tpm),
      expiresInDays: form.expires === 'never' ? null : Number(form.expires),
    });
    secret = res.secret;
    createdName = res.key.name;
    confirmed = false;
  }

  function finish() {
    secret = null;
    open = false;
    toast('success', 'Key erstellt', `„${createdName}“ ist aktiv.`);
  }

  function toggleModel(alias: string, on: boolean) {
    form.models = on ? [...form.models, alias] : form.models.filter((m) => m !== alias);
  }
</script>

{#if secret}
  <Dialog bind:open title="Key erstellt" description="„{createdName}“ ist sofort einsatzbereit." dismissible={false} testid="key-created-dialog">
    <SecretReveal {secret} bind:confirmed />
    <div class="mt-4">
      <CodeBlock
        lang="bash"
        title="Schnellstart (OpenAI-kompatibel)"
        code={`curl https://gateway.acme.example/v1/chat/completions \\
  -H "Authorization: Bearer $AIGW_API_KEY" \\
  -H "Content-Type: application/json" \\
  -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "Hallo!"}]}'`}
      />
    </div>
    {#snippet footer()}
      <Button variant="primary" disabled={!confirmed} onclick={finish} data-testid="key-done">Fertig</Button>
    {/snippet}
  </Dialog>
{:else}
  <Dialog bind:open title="Key erstellen" description="Virtueller API-Key für Anwendungen eines Teams." size="lg" testid="create-key-dialog">
    <form id="create-key" class="grid gap-5 sm:grid-cols-2" onsubmit={submit} novalidate>
      <Field label="Name" required hint="Eindeutig je Organisation, z. B. support-bot-prod." error={errors.name} class="sm:col-span-2">
        {#snippet children({ id, describedby, invalid })}
          <Input {id} bind:value={form.name} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="support-bot-prod" autocomplete="off" />
        {/snippet}
      </Field>
      <Field label="Team" required error={errors.teamId}>
        {#snippet children({ id, describedby, invalid })}
          <Select {id} {describedby} {invalid} bind:value={form.teamId} items={currentOrg().teams.map((t) => ({ value: t.id, label: t.name }))} />
        {/snippet}
      </Field>
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
              { value: 'never', label: 'nie', hint: 'nicht empfohlen' },
            ]}
          />
        {/snippet}
      </Field>

      <fieldset class="flex flex-col gap-2 sm:col-span-2">
        <legend class="mb-1.5 text-sm font-medium text-fg">Modellzugriff</legend>
        <RadioGroup.Root bind:value={form.access} class="grid gap-2 sm:grid-cols-2" aria-label="Modellzugriff">
          {#each [{ v: 'all', t: 'Alle Modellgruppen', d: 'inkl. künftig angelegter' }, { v: 'some', t: 'Ausgewählte', d: 'nur explizit freigegebene' }] as o (o.v)}
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
            {#each MODEL_GROUPS as m (m.alias)}
              <label class="flex items-center gap-2 text-base">
                <Checkbox.Root
                  checked={form.models.includes(m.alias)}
                  onCheckedChange={(c) => toggleModel(m.alias, c)}
                  class="grid size-4 shrink-0 place-items-center rounded-sm border border-line-strong bg-surface data-[state=checked]:border-accent data-[state=checked]:bg-accent data-[state=checked]:text-on-accent"
                >
                  {#snippet children({ checked })}
                    {#if checked}<Icon name="check" size={12} />{/if}
                  {/snippet}
                </Checkbox.Root>
                <span class="font-mono text-sm">{m.alias}</span>
              </label>
            {/each}
          </div>
          {#if errors.models}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.models}</p>{/if}
        {/if}
      </fieldset>

      <Field label="Budget pro Monat" optional hint="Leer = unbegrenzt. Ab 80 % Warnung, bei 100 % gesperrt." error={errors.budget}>
        {#snippet children({ id, describedby, invalid })}
          <Input {id} bind:value={form.budget} inputmode="decimal" suffix="EUR" placeholder="z. B. 250" aria-describedby={describedby} aria-invalid={invalid || undefined} />
        {/snippet}
      </Field>
      <div class="grid grid-cols-2 gap-3">
        <Field label="RPM" optional error={errors.rpm}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.rpm} inputmode="numeric" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
        <Field label="TPM" optional error={errors.tpm}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.tpm} inputmode="numeric" placeholder="unbegrenzt" aria-describedby={describedby} aria-invalid={invalid || undefined} />
          {/snippet}
        </Field>
      </div>
    </form>
    {#snippet footer()}
      <span class="mr-auto hidden text-xs text-fg-muted sm:block">Mock-Modus: der Key funktioniert nicht am echten Gateway.</span>
      <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
      <Button variant="primary" type="submit" form="create-key" icon="key" data-testid="create-key-submit">Key erstellen</Button>
    {/snippet}
  </Dialog>
{/if}
