<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Modellgruppe anlegen/bearbeiten (Admin-API §2.2): Aliasse, Routing-Strategie, Fallback-Kette mit Auslösern,
  // Einstellungen (retry, circuitBreaker, healthCheck – vom Server mit dem Parser der Datenebene geprüft).
  import { Checkbox } from 'bits-ui';
  import { untrack } from 'svelte';
  import { FALLBACK_TRIGGERS, modelGroups, NAME_HINT, NAME_RE, ROUTING, type ModelGroup, type S } from '../../lib/api/resources';
  import { handleError, splitList } from '../../lib/admin/errors.svelte';
  import FormError from '../../lib/admin/FormError.svelte';
  import Button from '../../lib/ui/Button.svelte';
  import Dialog from '../../lib/ui/Dialog.svelte';
  import Field from '../../lib/ui/Field.svelte';
  import Icon from '../../lib/ui/Icon.svelte';
  import Input from '../../lib/ui/Input.svelte';
  import Select from '../../lib/ui/Select.svelte';
  import { btn, checkbox, textarea } from '../../lib/ui/styles';

  let {
    open = $bindable(false),
    editName = null,
    groups,
    onsaved,
  }: { open?: boolean; editName?: string | null; groups: string[]; onsaved: (name: string) => void } = $props();

  const blank = () => ({ name: '', aliases: '', routing: 'weighted' as S['RoutingStrategy'], fallbacks: [] as S['Fallback'][], settings: '' });
  let form = $state(blank());
  let loaded = $state<ModelGroup | null>(null);
  let etag = $state<string | null>(null);
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
      const r = await modelGroups.get(name);
      loaded = r.data;
      etag = r.etag;
      const s = r.data.spec;
      form = {
        name,
        aliases: (s.aliases ?? []).join(', '),
        routing: s.routing ?? 'weighted',
        fallbacks: (s.fallbacks ?? []).map((f) => ({ modelGroup: f.modelGroup, on: [...(f.on ?? [])] })),
        settings: s.settings && Object.keys(s.settings).length ? JSON.stringify(s.settings, null, 2) : '',
      };
    } catch (e) {
      handleError(e, { action: 'Modellgruppe laden' });
      open = false;
    } finally {
      loading = false;
    }
  }

  function parseSettings(): Record<string, unknown> | null {
    if (!form.settings.trim()) return {};
    try {
      const v = JSON.parse(form.settings);
      if (v && typeof v === 'object' && !Array.isArray(v)) return v;
    } catch {
      /* unten */
    }
    return null;
  }

  async function submit(ev: SubmitEvent) {
    ev.preventDefault();
    const e: Record<string, string> = {};
    if (!editName && !NAME_RE.test(form.name)) e.name = 'Ungültiger Name: a-z, 0-9, „-“, „.“, „_“, beginnt alphanumerisch.';
    const aliases = splitList(form.aliases);
    if (aliases.some((a) => /[\s\p{Cc}]/u.test(a) || a.length > 256)) e.aliases = 'Aliasse ohne Leer-/Steuerzeichen, max. 256 Zeichen.';
    const settings = parseSettings();
    if (settings === null) e.settings = 'Kein gültiges JSON-Objekt.';
    if (form.fallbacks.some((f) => !f.modelGroup)) e.fallbacks = 'Jeder Fallback braucht eine Ziel-Gruppe.';
    errors = e;
    formError = null;
    if (Object.keys(e).length) return;
    const name = editName ?? form.name;
    const spec: S['ModelGroupSpec'] = {
      ...(loaded?.spec ?? {}),
      aliases,
      routing: form.routing,
      fallbacks: form.fallbacks.map((f) => ({ modelGroup: f.modelGroup, on: f.on?.length ? f.on : [] })),
      settings: (settings ?? {}) as S['ModelGroupSpec']['settings'],
    };
    busy = true;
    try {
      if (editName) await modelGroups.update(name, { metadata: { name, labels: loaded?.metadata.labels ?? {} }, spec }, etag);
      else await modelGroups.create({ metadata: { name }, spec });
      open = false;
      onsaved(name);
    } catch (err) {
      handleError(err, { action: 'Modellgruppe speichern', form: (m) => (formError = m), reload: () => editName && load(editName) });
    } finally {
      busy = false;
    }
  }

  function toggleTrigger(i: number, t: S['FallbackTrigger'], on: boolean) {
    const cur = form.fallbacks[i].on ?? [];
    form.fallbacks[i].on = on ? [...cur, t] : cur.filter((x) => x !== t);
  }
  function move(i: number, d: -1 | 1) {
    const f = [...form.fallbacks];
    [f[i], f[i + d]] = [f[i + d], f[i]];
    form.fallbacks = f;
  }
  const targets = $derived(groups.filter((g) => g !== (editName ?? form.name)));
</script>

<Dialog bind:open title={editName ? `Modellgruppe „${editName}“ bearbeiten` : 'Modellgruppe anlegen'} description="Öffentlicher Modellname; Deployments dahinter werden nach Priorität und Strategie gewählt." size="lg" testid="group-form-dialog">
  {#if loading}
    <div class="h-40 animate-pulse rounded-md bg-skeleton" aria-busy="true"></div>
  {:else}
    <form id="group-form" class="grid gap-5 sm:grid-cols-2" onsubmit={submit} novalidate>
      {#if !editName}
        <Field label="Name" required hint={NAME_HINT} error={errors.name}>
          {#snippet children({ id, describedby, invalid })}
            <Input {id} bind:value={form.name} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="gpt-4o" autocomplete="off" />
          {/snippet}
        </Field>
      {/if}
      <Field label="Routing-Strategie">
        {#snippet children({ id })}
          <Select {id} bind:value={form.routing} items={ROUTING.map((r) => ({ value: r.value, label: r.label, hint: r.hint }))} />
        {/snippet}
      </Field>
      <Field label="Aliasse" optional hint="Weitere Modellnamen der Clients, mit Komma getrennt (z. B. openai/gpt-4o). Namen und Aliasse sind global eindeutig." error={errors.aliases} class="sm:col-span-2">
        {#snippet children({ id, describedby, invalid })}
          <Input {id} bind:value={form.aliases} aria-describedby={describedby} aria-invalid={invalid || undefined} placeholder="gpt-4o-latest, openai/gpt-4o" autocomplete="off" />
        {/snippet}
      </Field>

      <fieldset class="flex flex-col gap-2 sm:col-span-2" data-testid="fallbacks">
        <legend class="mb-1.5 text-sm font-medium text-fg">Fallback-Kette <span class="text-xs font-normal text-fg-subtle">(in Reihenfolge, ohne Zyklen)</span></legend>
        {#each form.fallbacks as f, i (i)}
          <div class="flex flex-col gap-2 rounded-lg border border-line bg-surface-2 p-2.5">
            <div class="flex items-center gap-2">
              <span class="num w-5 text-sm text-fg-subtle">{i + 1}.</span>
              <div class="min-w-0 flex-1">
                <Select label="Ziel-Gruppe {i + 1}" bind:value={f.modelGroup} items={targets.map((g) => ({ value: g, label: g }))} placeholder="Ziel-Gruppe wählen …" />
              </div>
              <button type="button" class={btn('ghost', 'icon-sm')} aria-label="Fallback {i + 1} nach oben" disabled={i === 0} onclick={() => move(i, -1)}><Icon name="arrow-up" size={14} /></button>
              <button type="button" class={btn('ghost', 'icon-sm')} aria-label="Fallback {i + 1} nach unten" disabled={i === form.fallbacks.length - 1} onclick={() => move(i, 1)}><Icon name="arrow-down" size={14} /></button>
              <button type="button" class={btn('ghost', 'icon-sm')} aria-label="Fallback {i + 1} entfernen" onclick={() => (form.fallbacks = form.fallbacks.filter((_, j) => j !== i))}><Icon name="x" size={14} /></button>
            </div>
            <div class="flex flex-wrap gap-x-4 gap-y-1.5 pl-7" role="group" aria-label="Auslöser für Fallback {i + 1} (keiner = jeder Fehler)">
              {#each FALLBACK_TRIGGERS.filter((t) => t.value !== 'any') as t (t.value)}
                <label class="flex items-center gap-1.5 text-sm">
                  <Checkbox.Root checked={(f.on ?? []).includes(t.value)} onCheckedChange={(c) => toggleTrigger(i, t.value, c)} class={checkbox}>
                    {#snippet children({ checked })}{#if checked}<Icon name="check" size={12} />{/if}{/snippet}
                  </Checkbox.Root>
                  {t.label}
                </label>
              {/each}
            </div>
          </div>
        {/each}
        {#if errors.fallbacks}<p class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{errors.fallbacks}</p>{/if}
        <div>
          <Button size="sm" icon="plus" disabled={targets.length === 0} onclick={() => (form.fallbacks = [...form.fallbacks, { modelGroup: '', on: [] }])}>Fallback hinzufügen</Button>
        </div>
      </fieldset>

      <Field
        label="Einstellungen (JSON)"
        optional
        hint={'retry {maxAttempts, initialBackoffMs, maxBackoffMs, budgetMs, maxRetryAfterMs} · circuitBreaker {enabled, failureRate, minRequests, windowSecs, openSecs} · healthCheck {enabled, intervalSecs, timeoutMs, unhealthyThreshold}. Unbekannte Felder lehnt der Server ab (422).'}
        error={errors.settings}
        class="sm:col-span-2"
      >
        {#snippet children({ id, describedby, invalid })}
          <textarea
            {id}
            class={[textarea, 'min-h-28']}
            bind:value={form.settings}
            aria-describedby={describedby}
            aria-invalid={invalid || undefined}
            spellcheck="false"
            placeholder={'{\n  "retry": {"maxAttempts": 2, "initialBackoffMs": 100}\n}'}
          ></textarea>
        {/snippet}
      </Field>
      <div class="sm:col-span-2"><FormError message={formError} /></div>
    </form>
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="group-form" icon="check" loading={busy} disabled={busy || loading} data-testid="group-form-submit">
      {editName ? 'Speichern' : 'Anlegen'}
    </Button>
  {/snippet}
</Dialog>
