<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Strukturierte Einstellungen (D9/F137) mit JSON-Ansicht „Erweitert“: Felder schreiben direkt in `value`
  // (leeres Feld = Standard, Schlüssel entfällt); unbekannte Schlüssel bleiben erhalten. In „Erweitert“ wird das
  // ganze Objekt als JSON bearbeitet; `invalid` meldet ungültiges JSON bzw. ungültige Zahlen an das Formular.
  import { Tabs } from 'bits-ui';
  import Select from '../../lib/ui/Select.svelte';
  import { cx, input, tabsList, tabsTrigger, textarea } from '../../lib/ui/styles';
  import { getPath, setPath, unknownKeys, type SettingField, type SettingSection } from './settings';

  let {
    value = $bindable({}),
    invalid = $bindable(false),
    sections,
    testid = 'settings-editor',
  }: { value?: Record<string, unknown>; invalid?: boolean; sections: SettingSection[]; testid?: string } = $props();

  let mode = $state<'form' | 'json'>('form');
  let json = $state('');
  let jsonError = $state<string | null>(null);
  let bad = $state<Record<string, boolean>>({});
  // Eingabetexte je Zahlenfeld (damit „0.“ während der Eingabe nicht verschwindet).
  let texts = $state<Record<string, string>>({});

  $effect(() => {
    invalid = !!jsonError || Object.values(bad).some(Boolean);
  });

  function switchMode(m: string) {
    if (m === 'json') {
      json = Object.keys(value ?? {}).length ? JSON.stringify(value, null, 2) : '';
      jsonError = null;
      mode = 'json';
    } else {
      if (jsonError) return; // erst gültiges JSON
      texts = {};
      mode = 'form';
    }
  }
  function onJson(t: string) {
    json = t;
    if (!t.trim()) {
      value = {};
      jsonError = null;
      return;
    }
    try {
      const v = JSON.parse(t);
      if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error('kein Objekt');
      value = v;
      jsonError = null;
    } catch {
      jsonError = 'Kein gültiges JSON-Objekt.';
    }
  }

  const str = (f: SettingField) => {
    if (f.path in texts) return texts[f.path];
    const v = getPath(value, f.path);
    return v === undefined || v === null ? '' : Array.isArray(v) ? v.join(', ') : String(v);
  };
  function setNumber(f: SettingField, t: string) {
    texts[f.path] = t;
    const s = t.trim().replace(',', '.');
    if (!s) {
      bad[f.path] = false;
      value = setPath(value, f.path, undefined);
      return;
    }
    const n = Number(s);
    const ok = Number.isFinite(n) && (f.decimal || Number.isInteger(n)) && (f.min === undefined || n >= f.min) && (f.max === undefined || n <= f.max);
    bad[f.path] = !ok;
    if (ok) value = setPath(value, f.path, n);
  }
  function setChoice(f: SettingField, v: string) {
    value = setPath(value, f.path, v === '' ? undefined : f.kind === 'bool' ? v === 'true' : v);
  }
  function setList(f: SettingField, t: string) {
    texts[f.path] = t;
    const list = t.split(/[\s,]+/).map((x) => x.trim()).filter(Boolean);
    value = setPath(value, f.path, list.length ? list : undefined);
  }
  const extra = $derived(unknownKeys(value ?? {}, sections));
  const rangeHint = (f: SettingField) => [f.min !== undefined ? `≥ ${f.min}` : '', f.max !== undefined ? `≤ ${f.max}` : ''].filter(Boolean).join(', ');
</script>

<div class="flex flex-col gap-3" data-testid={testid}>
  <Tabs.Root value={mode} onValueChange={switchMode}>
    <Tabs.List class={cx(tabsList, 'mb-3')}>
      <Tabs.Trigger value="form" class={tabsTrigger} disabled={!!jsonError} data-testid="settings-form-tab">Formular</Tabs.Trigger>
      <Tabs.Trigger value="json" class={tabsTrigger} data-testid="settings-json-tab">Erweitert (JSON)</Tabs.Trigger>
    </Tabs.List>
    <Tabs.Content value="form" class="flex flex-col gap-4">
      {#each sections as sec (sec.id)}
        <fieldset class="rounded-lg border border-line p-3" data-testid="settings-{sec.id}">
          <legend class="px-1 text-sm font-medium text-fg">{sec.title}</legend>
          <p class="mb-2.5 text-xs text-fg-muted">{sec.description}</p>
          <div class="grid gap-3 sm:grid-cols-2">
            {#each sec.fields.filter((f) => !f.when || f.when(value ?? {})) as f (f.path)}
              {@const id = `set-${f.path.replace('.', '-')}`}
              <div class="flex flex-col gap-1">
                <label for={id} class="text-sm font-medium text-fg">{f.label}</label>
                {#if f.kind === 'number'}
                  <input
                    {id}
                    class={input}
                    type="text"
                    inputmode={f.decimal ? 'decimal' : 'numeric'}
                    value={str(f)}
                    placeholder={f.def ? `Standard ${f.def}` : 'Standard'}
                    aria-invalid={bad[f.path] || undefined}
                    oninput={(e) => setNumber(f, e.currentTarget.value)}
                  />
                {:else if f.kind === 'list'}
                  <input {id} class={input} type="text" value={str(f)} oninput={(e) => setList(f, e.currentTarget.value)} />
                {:else}
                  <Select
                    {id}
                    value={str(f)}
                    onchange={(v) => setChoice(f, v)}
                    items={[
                      { value: '', label: `Standard${f.def ? ` (${f.kind === 'choice' ? (f.choices?.find((c) => c.value === f.def)?.label ?? f.def) : f.def})` : ''}` },
                      ...(f.kind === 'bool'
                        ? [
                            { value: 'true', label: 'an' },
                            { value: 'false', label: 'aus' },
                          ]
                        : (f.choices ?? [])),
                    ]}
                  />
                {/if}
                {#if bad[f.path]}
                  <p class="text-xs text-danger-fg">Ungültige Zahl{rangeHint(f) ? ` (${rangeHint(f)})` : ''}.</p>
                {:else if f.hint}
                  <p class="text-xs text-fg-subtle">{f.hint}</p>
                {/if}
              </div>
            {/each}
          </div>
        </fieldset>
      {/each}
      {#if extra.length}
        <p class="text-xs text-fg-muted" data-testid="settings-extra">Weitere Schlüssel (nur unter „Erweitert“ bearbeitbar): <span class="font-mono">{extra.join(', ')}</span></p>
      {/if}
    </Tabs.Content>
    <Tabs.Content value="json" class="flex flex-col gap-1.5">
      <textarea
        class={[textarea, 'min-h-48']}
        value={json}
        oninput={(e) => onJson(e.currentTarget.value)}
        spellcheck="false"
        aria-label="Einstellungen als JSON"
        aria-invalid={!!jsonError || undefined}
        data-testid="settings-json"
        placeholder={'{\n  "retry": {"maxAttempts": 2}\n}'}
      ></textarea>
      {#if jsonError}<p class="text-xs text-danger-fg" role="alert">{jsonError}</p>{:else}<p class="text-xs text-fg-subtle">Das ganze settings-Objekt; unbekannte Felder in retry/circuitBreaker/healthCheck lehnt der Server ab (422).</p>{/if}
    </Tabs.Content>
  </Tabs.Root>
</div>
