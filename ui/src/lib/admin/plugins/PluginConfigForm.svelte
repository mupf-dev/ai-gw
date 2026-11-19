<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  /** Plugins mit strukturiertem Formular (F214: Felder nach dem Konfigurationsschema der Plugins, 12/13-*.md). */
  export const STRUCTURED = new Set(['pii', 'prompt-filter', 'prompt-leak', 'content-safety', 'webhook', 'wasm']);
</script>

<script lang="ts">
  // Strukturierte Konfiguration der Guardrail-Plugins (pii, prompt-filter, content-safety) und des Webhooks.
  // Arbeitet direkt auf dem Konfigurationsobjekt: unbekannte/weitere Schlüssel bleiben erhalten (JSON-Ansicht
  // zeigt alles). Leere Felder entfernen den Schlüssel – es gilt dann der Standard des Plugins.
  // Geheimnisse nur als Verweis (Secret bzw. Anbieter-Zugangsdaten, F149) – nie Klartext.
  import type { SelectItem } from '../../ui/Select.svelte';
  import Field from '../../ui/Field.svelte';
  import Input from '../../ui/Input.svelte';
  import Select from '../../ui/Select.svelte';
  import Button from '../../ui/Button.svelte';
  import { textarea } from '../../ui/styles';

  type Obj = Record<string, unknown>;
  let {
    plugin,
    config = $bindable(),
    secrets = [],
    credentials = [],
    modelGroups = [],
    wasmModules = [],
    secretRefs = true,
  }: {
    plugin: string;
    config: Obj;
    secrets?: string[];
    credentials?: string[];
    modelGroups?: string[];
    /** AIGW-63: Namen der hochgeladenen WASM-Module (Auswahl für die Plugin-Art `wasm`). */
    wasmModules?: string[];
    /** AIGW-58: Verweise auf (globale) Secrets/Zugangsdaten darf nur ein globaler Admin setzen (sonst 403). */
    secretRefs?: boolean;
  } = $props();

  // ------------------------------------------------------------ Zugriff auf verschachtelte Schlüssel
  function obj(path: string[]): Obj | undefined {
    let o: unknown = config;
    for (const k of path) {
      if (!o || typeof o !== 'object') return undefined;
      o = (o as Obj)[k];
    }
    return o && typeof o === 'object' && !Array.isArray(o) ? (o as Obj) : undefined;
  }
  function get(path: string[]): unknown {
    const parent = path.length > 1 ? obj(path.slice(0, -1)) : config;
    return parent?.[path[path.length - 1]];
  }
  const empty = (v: unknown) => v === undefined || v === null || v === '' || (Array.isArray(v) && v.length === 0);
  function set(path: string[], v: unknown) {
    let o = config;
    for (const k of path.slice(0, -1)) {
      if (!o[k] || typeof o[k] !== 'object' || Array.isArray(o[k])) o[k] = {};
      o = o[k] as Obj;
    }
    const last = path[path.length - 1];
    if (empty(v)) delete o[last];
    else o[last] = v;
    // Leere Zwischenobjekte entfernen.
    for (let i = path.length - 1; i > 0; i--) {
      const p = obj(path.slice(0, i));
      if (p && Object.keys(p).length === 0) {
        const parent = i > 1 ? obj(path.slice(0, i - 1)) : config;
        if (parent) delete parent[path[i - 1]];
      }
    }
  }
  const str = (path: string[]) => {
    const v = get(path);
    return v === undefined || v === null ? '' : String(v);
  };
  /** Zahl (leer = Standard); Ungültiges bleibt Text, der Server meldet es (422). */
  const num = (v: string) => (v.trim() === '' ? undefined : Number.isFinite(Number(v)) ? Number(v) : v);
  const bool = (v: string) => (v === '' ? undefined : v === 'true');
  const boolStr = (path: string[]) => {
    const v = get(path);
    return v === true ? 'true' : v === false ? 'false' : '';
  };
  const lines = (path: string[]) => ((get(path) as string[] | undefined) ?? []).join('\n');
  const toLines = (v: string) => v.split('\n').map((x) => x.trim()).filter(Boolean);

  const STD: SelectItem = { value: '', label: 'Standard' };
  const ON_OFF: SelectItem[] = [STD, { value: 'true', label: 'an' }, { value: 'false', label: 'aus' }];
  const DIRECTION: SelectItem[] = [STD, { value: 'input', label: 'Eingabe' }, { value: 'output', label: 'Ausgabe' }, { value: 'both', label: 'beides' }];
  const FILTER_ACTION: SelectItem[] = [STD, { value: 'reject', label: 'reject – ablehnen' }, { value: 'flag', label: 'flag – nur zählen' }];
  const ROLES = ['system', 'developer', 'user', 'assistant', 'tool'];
  /** AIGW-111: Richtung der Wort-/Regex-Regeln (Standard: nur Anfrage). */
  const RULE_DIRECTION: SelectItem[] = [STD, { value: 'request', label: 'Anfrage' }, { value: 'response', label: 'Antwort' }, { value: 'both', label: 'beides' }];
  /** AIGW-109: unsichtbare Unicode-Zeichen (Abschnitt fehlt = aus). */
  const INVISIBLE_ACTION: SelectItem[] = [
    { value: '', label: 'aus (bzw. strip, wenn konfiguriert)' },
    { value: 'strip', label: 'strip – entfernen' },
    { value: 'reject', label: 'reject – ablehnen' },
    { value: 'flag', label: 'flag – nur zählen' },
  ];
  /** AIGW-112: prompt-leak. */
  const LEAK_ACTION: SelectItem[] = [STD, { value: 'block', label: 'block – Antwort blockieren (content_filter)' }, { value: 'flag', label: 'flag – zählen, Canary entfernen' }];

  // ------------------------------------------------------------ pii
  const PII_ACTIONS: SelectItem[] = [
    { value: 'mask', label: 'mask – Platzhalter [EMAIL_1]' },
    { value: 'redact', label: 'redact – [REDACTED]' },
    { value: 'reject', label: 'reject – ablehnen' },
    { value: 'pseudonymize', label: 'pseudonymize – mit Rückübersetzung' },
    { value: 'log-only', label: 'log-only – nur zählen' },
  ];
  const PII_CATEGORIES: [string, string][] = [
    ['email', 'E-Mail'],
    ['phone', 'Telefon'],
    ['iban', 'IBAN'],
    ['creditCard', 'Kreditkarte'],
    ['taxIdDe', 'Steuer-ID'],
    ['socialSecurityDe', 'Sozialversicherungsnr.'],
    ['idCardDe', 'Personalausweis'],
    ['passportDe', 'Reisepass'],
    ['ipAddress', 'IP-Adresse'],
    ['urlCredentials', 'URL-Zugangsdaten'],
    ['apiKey', 'API-Key/Token'],
  ];
  /**
   * Kategorien: ohne `categories` gelten alle mit Aktion/Richtung von oben; mit `categories` NUR die genannten
   * (Regel = Aktion als Text, `{}` = Aktion von oben, `{action?, direction}`).
   */
  const catMode = $derived(obj(['categories']) ? 'some' : 'all');
  function setCatMode(m: string) {
    set(['categories'], m === 'some' ? Object.fromEntries(PII_CATEGORIES.map(([c]) => [c, {}])) : undefined);
  }
  /** Regel einer Kategorie; Aktion `off` = nicht geprüft, '' = Aktion von oben. */
  function catRule(c: string): { action: string; direction: string } {
    const r = obj(['categories'])?.[c];
    if (typeof r === 'string') return { action: r, direction: '' };
    if (r && typeof r === 'object') return { action: String((r as Obj).action ?? ''), direction: String((r as Obj).direction ?? '') };
    return { action: 'off', direction: '' };
  }
  function setCat(c: string, action: string, direction: string) {
    const cats = obj(['categories']);
    if (!cats) return;
    if (action === 'off') delete cats[c];
    else cats[c] = direction ? { ...(action ? { action } : {}), direction } : action || {};
  }

  // ------------------------------------------------------------ prompt-filter: Regex-Regeln
  type Rule = { name: string; pattern: string; action?: string };
  const regex = $derived((get(['regex']) as Rule[] | undefined) ?? []);
  function setRegex(rows: Rule[]) {
    set(['regex'], rows.map((r) => ({ name: r.name, pattern: r.pattern, ...(r.action ? { action: r.action } : {}) })));
  }
  function toggleRole(r: string, on: boolean) {
    const cur = (get(['roles']) as string[] | undefined) ?? [];
    set(['roles'], on ? [...new Set([...cur, r])] : cur.filter((x) => x !== r));
  }

  // ------------------------------------------------------------ Verweise auf Geheimnisse
  const secretItems = $derived<SelectItem[]>([{ value: '', label: 'keins' }, ...secrets.map((s) => ({ value: s, label: s }))]);
  const credItems = $derived<SelectItem[]>([{ value: '', label: 'keine' }, ...credentials.map((s) => ({ value: s, label: s }))]);
  /** content-safety: secret ODER credential ODER Entra ID (auth: entraId + credential mit spec.entra, AIGW-89). */
  const csAuth = $derived(get(['auth']) === 'entraId' ? 'entra' : get(['secret']) ? 'secret' : get(['credential']) ? 'credential' : '');
  const CS_AUTH_LABEL: Record<string, string> = { secret: 'Secret', credential: 'Zugangsdaten', entra: 'Entra ID über Zugangsdaten' };
  /** webhook: secret = {secret, field?} | {credential, field?} | {env} | {file}. */
  const whKind = $derived.by(() => {
    const s = obj(['secret']);
    return s ? (['secret', 'credential', 'env', 'file'].find((k) => k in s) ?? '') : '';
  });
  function setWhKind(k: string) {
    if (k) config.secret = { [k]: k === 'secret' ? (secrets[0] ?? '') : k === 'credential' ? (credentials[0] ?? '') : '' };
    else set(['secret'], undefined);
  }
  // ------------------------------------------------------------ wasm (AIGW-63)
  const wasmItems = $derived<SelectItem[]>([{ value: '', label: 'Modul wählen' }, ...wasmModules.map((m) => ({ value: m, label: m }))]);
  /** `config.config` (Konfiguration für das Modul) als JSON-Text; ungültiges JSON bleibt bis zur Korrektur stehen. */
  let wasmCfgText = $state('');
  let wasmCfgError = $state<string | null>(null);
  $effect(() => {
    if (plugin === 'wasm' && wasmCfgError === null) wasmCfgText = JSON.stringify(get(['config']) ?? {}, null, 2);
  });
  function setWasmCfg(v: string) {
    wasmCfgText = v;
    try {
      const o = JSON.parse(v || '{}');
      wasmCfgError = null;
      set(['config'], o && typeof o === 'object' && !Array.isArray(o) && Object.keys(o).length === 0 ? undefined : o);
    } catch {
      wasmCfgError = 'Kein gültiges JSON.';
    }
  }

  // Thresholds „kategorie=wert“ je Zeile.
  const thresholds = $derived(
    Object.entries(obj(['thresholds']) ?? {})
      .map(([k, v]) => `${k}=${v}`)
      .join('\n'),
  );
  function setThresholds(v: string) {
    const out: Obj = {};
    for (const l of toLines(v)) {
      const [k, x] = l.split('=').map((s) => s.trim());
      if (k) out[k] = num(x ?? '') ?? x;
    }
    set(['thresholds'], Object.keys(out).length ? out : undefined);
  }
</script>

{#snippet text(label: string, path: string[], o: { hint?: string; placeholder?: string; list?: string; required?: boolean; numeric?: boolean; wide?: boolean } = {})}
  <Field {label} hint={o.hint} required={o.required} class={o.wide ? 'sm:col-span-2' : ''}>
    {#snippet children({ id, describedby })}
      <Input
        {id}
        aria-describedby={describedby}
        value={str(path)}
        oninput={(e) => set(path, o.numeric ? num(e.currentTarget.value) : e.currentTarget.value)}
        placeholder={o.placeholder ?? (o.numeric ? 'Standard' : '')}
        inputmode={o.numeric ? 'decimal' : undefined}
        list={o.list}
        autocomplete="off"
        data-field={path.join('.')}
      />
    {/snippet}
  </Field>
{/snippet}
{#snippet choice(label: string, path: string[], items: SelectItem[], hint?: string)}
  <Field {label} {hint}>
    {#snippet children({ id })}<Select {id} value={str(path)} onchange={(v) => set(path, v || undefined)} {items} />{/snippet}
  </Field>
{/snippet}
{#snippet flag(label: string, path: string[], hint?: string)}
  <Field {label} {hint}>
    {#snippet children({ id })}<Select {id} value={boolStr(path)} onchange={(v) => set(path, bool(v))} items={ON_OFF} />{/snippet}
  </Field>
{/snippet}
{#snippet list(label: string, path: string[], hint: string, placeholder = '')}
  <Field {label} {hint} class="sm:col-span-2">
    {#snippet children({ id, describedby })}
      <textarea {id} class={[textarea, 'min-h-20 font-mono']} aria-describedby={describedby} value={lines(path)} oninput={(e) => set(path, toLines(e.currentTarget.value))} {placeholder} spellcheck="false" data-field={path.join('.')}></textarea>
    {/snippet}
  </Field>
{/snippet}
{#snippet refNote(current: string)}
  <div class="flex flex-col gap-1 sm:col-span-2" data-testid="secret-ref-note">
    <span class="text-sm font-medium">{current}</span>
    <span class="text-xs text-fg-muted">Secrets und Zugangsdaten sind global – Bindungen mit Verweis darauf legt nur ein globaler Admin an (AIGW-58). Ohne Verweis speichern oder einen globalen Admin bitten.</span>
  </div>
{/snippet}
{#snippet section(title: string)}<h3 class="label-caps text-fg-subtle sm:col-span-2">{title}</h3>{/snippet}

<div class="grid gap-5 sm:col-span-2 sm:grid-cols-2" data-testid="plugin-config-form">
  {#if plugin === 'pii'}
    {@render choice('Aktion (Standard aller Kategorien)', ['action'], [{ value: '', label: 'Standard (mask)' }, ...PII_ACTIONS])}
    {@render choice('Richtung', ['direction'], DIRECTION, 'Standard: beides')}
    <Field label="Kategorien" hint="Auswahl: nur die aktivierten Kategorien werden geprüft">
      {#snippet children({ id })}<Select {id} value={catMode} onchange={setCatMode} items={[{ value: 'all', label: 'alle mit Aktion/Richtung von oben' }, { value: 'some', label: 'Auswahl je Kategorie' }]} />{/snippet}
    </Field>
    {#if catMode === 'some'}
    <div class="overflow-x-auto sm:col-span-2">
      <table class="w-full text-left text-sm" data-testid="pii-categories">
        <thead>
          <tr class="border-b border-line text-fg-muted">
            <th class="py-1.5 pr-3 font-medium">Kategorie</th>
            <th class="py-1.5 pr-3 font-medium">Aktion</th>
            <th class="py-1.5 font-medium">Richtung</th>
          </tr>
        </thead>
        <tbody>
          {#each PII_CATEGORIES as [c, label] (c)}
            {@const r = catRule(c)}
            <tr class="border-b border-line last:border-0">
              <td class="py-1.5 pr-3"><span class="block">{label}</span><span class="font-mono text-xs text-fg-subtle">{c}</span></td>
              <td class="w-56 py-1.5 pr-3"><Select size="sm" label="Aktion {label}" value={r.action} onchange={(v) => setCat(c, v, r.direction)} items={[{ value: 'off', label: 'aus' }, { value: '', label: 'wie oben' }, ...PII_ACTIONS]} /></td>
              <td class="w-40 py-1.5"><Select size="sm" label="Richtung {label}" value={r.direction} onchange={(v) => setCat(c, r.action, v)} items={[{ value: '', label: 'wie oben' }, ...DIRECTION.slice(1)]} /></td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
    {/if}
    {@render list('Ausnahmen (Allowlist)', ['allow'], 'Ein Wert je Zeile; „*“ als Platzhalter, z. B. *@example.com', 'support@acme.example')}
    {@render flag('Entropie-Heuristik', ['entropy', 'enabled'], 'unbekannte Tokens hoher Entropie als api_key (F164)')}
    {@render text('Entropie: Mindestlänge', ['entropy', 'minLength'], { numeric: true })}
    {@render text('Entropie: Bit je Zeichen', ['entropy', 'minBits'], { numeric: true })}
    {@render text('Stream-Fenster (Bytes)', ['streamWindowBytes'], { numeric: true, hint: '16–4096' })}
  {:else if plugin === 'prompt-filter'}
    {@render choice('Aktion (Standard je Regel)', ['action'], FILTER_ACTION)}
    {@render choice('Richtung der Wort-/Regex-Regeln', ['direction'], RULE_DIRECTION, 'Standard: Anfrage; Antwort/Stream wie pii (Stream mit Puffer)')}
    <fieldset class="flex flex-col gap-1.5">
      <legend class="text-sm font-medium">Geprüfte Rollen</legend>
      <div class="flex flex-wrap gap-3 pt-1.5">
        {#each ROLES as r (r)}
          {@const sel = (get(['roles']) as string[] | undefined) ?? []}
          <label class="flex items-center gap-1.5 text-sm"><input type="checkbox" class="accent-accent" checked={sel.includes(r)} onchange={(e) => toggleRole(r, e.currentTarget.checked)} /> {r}</label>
        {/each}
      </div>
      <span class="text-xs text-fg-muted">keine Auswahl = Standard (user, tool)</span>
    </fieldset>
    {@render section('Wortliste')}
    {@render list('Begriffe', ['words', 'terms'], 'Ein Begriff je Zeile', 'Projekt Falke')}
    {@render flag('Groß-/Kleinschreibung beachten', ['words', 'caseSensitive'])}
    {@render flag('Nur ganze Wörter', ['words', 'wholeWords'], 'Standard: an')}
    {@render choice('Aktion der Wortliste', ['words', 'action'], FILTER_ACTION)}
    {@render choice('Richtung der Wortliste', ['words', 'direction'], RULE_DIRECTION)}
    {@render section('Reguläre Ausdrücke')}
    <div class="flex flex-col gap-2 sm:col-span-2" data-testid="regex-rules">
      {#each regex as r, i (i)}
        <div class="grid grid-cols-[1fr_2fr_9rem_auto] items-end gap-2">
          <Input aria-label="Name der Regel {i + 1}" value={r.name} oninput={(e) => setRegex(regex.map((x, j) => (j === i ? { ...x, name: e.currentTarget.value } : x)))} placeholder="Name" />
          <Input aria-label="Muster der Regel {i + 1}" class="font-mono" value={r.pattern} oninput={(e) => setRegex(regex.map((x, j) => (j === i ? { ...x, pattern: e.currentTarget.value } : x)))} placeholder="(?i)\bpasswor[dt]\b" />
          <Select size="sm" label="Aktion der Regel {i + 1}" value={r.action ?? ''} onchange={(v) => setRegex(regex.map((x, j) => (j === i ? { ...x, action: v } : x)))} items={FILTER_ACTION} />
          <Button size="icon-sm" variant="ghost" icon="trash" aria-label="Regel {i + 1} entfernen" onclick={() => setRegex(regex.filter((_, j) => j !== i))} />
        </div>
      {/each}
      <div><Button size="sm" icon="plus" onclick={() => setRegex([...regex, { name: '', pattern: '' }])}>Regel hinzufügen</Button></div>
    </div>
    {@render section('Prompt-Injection')}
    {@render flag('Erkennung', ['injection', 'enabled'], 'Standard: an')}
    {@render text('Schwelle', ['injection', 'threshold'], { numeric: true, hint: 'Summe der Gewichte erkannter Signale' })}
    {@render choice('Aktion', ['injection', 'action'], FILTER_ACTION)}
    {@render section('Grenzen')}
    {@render text('Höchstzahl Nachrichten', ['limits', 'maxMessages'], { numeric: true })}
    {@render text('Höchstzahl Zeichen', ['limits', 'maxChars'], { numeric: true })}
    {@render text('Zeichen je Nachricht', ['limits', 'maxMessageChars'], { numeric: true })}
    {@render choice('Aktion der Grenzen', ['limits', 'action'], FILTER_ACTION)}
    {@render section('Unsichtbare Unicode-Zeichen')}
    {@render choice('Aktion', ['invisible', 'action'], INVISIBLE_ACTION, 'Tag-, Zero-Width-, Bidi- und Formatzeichen; Tag-Zeichen zählen zusätzlich als Injection-Signal')}
    {@render flag('Verbinder erlauben (ZWJ/ZWNJ)', ['invisible', 'allowJoiners'], 'Standard: an – Emoji-Sequenzen, Persisch, indische Schriften')}
    {@render text('Stream-Fenster (Bytes)', ['streamWindowBytes'], { numeric: true, hint: 'nur Ausgabe-Regeln, 16–4096' })}
  {:else if plugin === 'prompt-leak'}
    {@render choice('Aktion (Standard)', ['action'], LEAK_ACTION)}
    {@render section('Canary-Token im System-Prompt')}
    {@render flag('Canary', ['canary', 'enabled'], 'Standard: an (nur Anfragen mit System-Prompt)')}
    {@render choice('Aktion', ['canary', 'action'], LEAK_ACTION)}
    {@render choice('Erzeugung', ['canary', 'rotation'], [STD, { value: 'request', label: 'request – je Anfrage zufällig' }, { value: 'stable', label: 'stable – je Key und System-Prompt (Prompt-Caching)' }])}
    {@render section('Wörtliche Auszüge (N-Gramme)')}
    {@render flag('Erkennung', ['verbatim', 'enabled'], 'ohne Angaben aus; aktiv, sobald ein Feld gesetzt ist')}
    {@render text('Wörter je N-Gramm', ['verbatim', 'ngram'], { numeric: true, hint: '4–32, Standard 8' })}
    {@render text('Mindestzahl N-Gramme', ['verbatim', 'minNgrams'], { numeric: true, hint: 'Standard 12' })}
    {@render text('Anteil am System-Prompt', ['verbatim', 'ratio'], { numeric: true, hint: '0–1, Standard 0,5' })}
    {@render choice('Aktion', ['verbatim', 'action'], LEAK_ACTION)}
    {@render text('Stream-Fenster (Bytes)', ['streamWindowBytes'], { numeric: true, hint: '32–4096, Standard 64' })}
  {:else if plugin === 'content-safety'}
    {@render choice('Anbieter', ['provider'], [
      { value: 'azure', label: 'Azure AI Content Safety' },
      { value: 'openai', label: 'OpenAI Moderation' },
      { value: 'model', label: 'Modell des Gateways (z. B. Llama Guard)' },
    ])}
    {#if get(['provider']) === 'model'}
      {@render text('Modellgruppe', ['modelGroup'], { list: 'cs-model-groups', required: true })}
      <datalist id="cs-model-groups">{#each modelGroups as g (g)}<option value={g}></option>{/each}</datalist>
    {:else}
      {@render text('Endpunkt', ['endpoint'], { placeholder: 'https://…', hint: 'nur https, SSRF-Regeln wie Deployments' })}
      {#if !secretRefs}
        {@render refNote(csAuth ? `Zugang: ${CS_AUTH_LABEL[csAuth]} „${str([csAuth === 'secret' ? 'secret' : 'credential'])}“` : 'Zugang: ohne')}
      {:else}
      <Field label="Zugang" hint="nur als Verweis – nie Klartext (F149)">
        {#snippet children({ id })}
          <Select
            {id}
            value={csAuth}
            onchange={(v) => {
              set(['secret'], undefined);
              set(['credential'], undefined);
              set(['auth'], undefined);
              if (v === 'entra') {
                config.auth = 'entraId';
                config.credential = credentials[0] ?? '';
              } else if (v) config[v] = v === 'secret' ? (secrets[0] ?? '') : (credentials[0] ?? '');
            }}
            items={[
              { value: '', label: 'ohne' },
              { value: 'secret', label: 'Secret' },
              { value: 'credential', label: 'Anbieter-Zugangsdaten' },
              ...(get(['provider']) === 'azure' ? [{ value: 'entra', label: 'Entra ID (Zugangsdaten mit Entra-Anmeldung)' }] : []),
            ]}
          />
        {/snippet}
      </Field>
      {#if csAuth === 'secret'}{@render choice('Secret', ['secret'], secretItems)}{/if}
      {#if csAuth === 'credential'}{@render choice('Zugangsdaten', ['credential'], credItems)}{/if}
      {#if csAuth === 'entra'}{@render choice('Zugangsdaten (Entra)', ['credential'], credItems, 'Managed Identity, Workload Identity oder Service Principal (spec.entra); Endpunkt nur *.cognitiveservices.azure.com bzw. *.services.ai.azure.com')}{/if}
      {/if}
      {#if get(['provider']) === 'azure'}{@render text('API-Version', ['apiVersion'], { placeholder: '2024-09-01' })}{/if}
      {#if get(['provider']) === 'openai'}{@render text('Modell', ['model'], { placeholder: 'omni-moderation-latest' })}{/if}
    {/if}
    <Field label="Schwellen" hint="je Zeile kategorie=wert, z. B. hate=2" class="sm:col-span-2">
      {#snippet children({ id, describedby })}<textarea {id} class={[textarea, 'min-h-16 font-mono']} aria-describedby={describedby} value={thresholds} onchange={(e) => setThresholds(e.currentTarget.value)} spellcheck="false"></textarea>{/snippet}
    </Field>
    {@render choice('Richtung', ['direction'], DIRECTION, 'Standard: Eingabe')}
    {@render choice('Aktion', ['action'], FILTER_ACTION)}
    {@render choice('Ausgabeprüfung', ['outputMode'], [STD, { value: 'buffered', label: 'buffered – ganze Antwort' }, { value: 'sentence', label: 'sentence – satzweise im Stream' }])}
    {@render text('Höchstzahl Zeichen', ['maxChars'], { numeric: true })}
    {@render text('Höchstzahl Prüfungen je Anfrage', ['maxChecks'], { numeric: true, hint: 'Standard 100; darüber failMode bzw. im Stream sentenceOnError' })}
  {:else if plugin === 'webhook'}
    {@render text('URL', ['url'], { required: true, placeholder: 'https://policy.example.org/aigw', hint: 'nur https, keine privaten Ziele (SSRF)', wide: true })}
    <fieldset class="flex flex-col gap-1.5">
      <legend class="text-sm font-medium">Phasen</legend>
      <div class="flex gap-3 pt-1.5">
        {#each ['request', 'response'] as ph (ph)}
          {@const sel = (get(['phases']) as string[] | undefined) ?? ['request']}
          <label class="flex items-center gap-1.5 text-sm"
            ><input
              type="checkbox"
              class="accent-accent"
              checked={sel.includes(ph)}
              onchange={(e) => {
                const next = e.currentTarget.checked ? [...new Set([...sel, ph])] : sel.filter((x) => x !== ph);
                set(['phases'], next.length === 1 && next[0] === 'request' ? undefined : next);
              }}
            />
            {ph === 'request' ? 'Anfrage' : 'Antwort (puffert Streams)'}</label
          >
        {/each}
      </div>
    </fieldset>
    {#if !secretRefs}
      {@render refNote(whKind ? `Signatur: ${whKind} „${str(['secret', whKind])}“` : 'Signatur: unsigniert')}
    {:else}
    <Field label="Signatur (HMAC-Schlüssel)" hint="Verweis auf ein Geheimnis – nie Klartext in der Bindung">
      {#snippet children({ id })}
        <Select
          {id}
          value={whKind}
          onchange={setWhKind}
          items={[
            { value: '', label: 'unsigniert' },
            { value: 'secret', label: 'Secret' },
            { value: 'credential', label: 'Anbieter-Zugangsdaten' },
            { value: 'env', label: 'Umgebungsvariable' },
            { value: 'file', label: 'Datei' },
          ]}
        />
      {/snippet}
    </Field>
    {#if whKind === 'secret'}{@render choice('Secret', ['secret', 'secret'], secretItems)}{/if}
    {#if whKind === 'credential'}{@render choice('Zugangsdaten', ['secret', 'credential'], credItems)}{/if}
    {#if whKind === 'secret' || whKind === 'credential'}{@render text('Feld', ['secret', 'field'], { placeholder: 'webhookSecret' })}{/if}
    {#if whKind === 'env'}{@render text('Variable', ['secret', 'env'], { placeholder: 'AIGW_HOOK_SECRET' })}{/if}
    {#if whKind === 'file'}{@render text('Pfad', ['secret', 'file'], { placeholder: '/run/secrets/hook' })}{/if}
    {/if}
    {@render list('Client-Header im Payload', ['includeHeaders'], 'Allowlist, ein Header je Zeile; Auth-Header sind gesperrt', 'x-client-app')}
    {@render text('Max. Payload (Bytes)', ['maxPayloadBytes'], { numeric: true, hint: 'Standard 1 MiB' })}
    {@render text('Max. Antwort (Bytes)', ['maxResponseBytes'], { numeric: true, hint: 'Standard 1 MiB' })}
  {:else if plugin === 'wasm'}
    <Field label="Modul" required hint="hochgeladene WASM-Komponente (Reiter „WASM-Module“)">
      {#snippet children({ id })}<Select {id} value={str(['module'])} onchange={(v) => set(['module'], v || undefined)} items={wasmItems} />{/snippet}
    </Field>
    {@render text('SHA-256 (optional)', ['sha256'], { hint: 'nur genau diese Modulversion zulassen', placeholder: '64 Hex-Zeichen' })}
    <fieldset class="flex flex-col gap-1.5 sm:col-span-2">
      <legend class="text-sm font-medium">Phasen</legend>
      <div class="flex flex-wrap gap-3 pt-1.5">
        {#each [['request', 'Anfrage'], ['response', 'Antwort'], ['chunk', 'Stream']] as [ph, label] (ph)}
          {@const sel = get(['phases']) as string[] | undefined}
          <label class="flex items-center gap-1.5 text-sm"
            ><input
              type="checkbox"
              class="accent-accent"
              checked={!sel || sel.includes(ph)}
              onchange={(e) => {
                const cur = sel ?? ['request', 'response', 'chunk'];
                const next = e.currentTarget.checked ? [...new Set([...cur, ph])] : cur.filter((x) => x !== ph);
                set(['phases'], next.length === 3 ? undefined : next);
              }}
            />
            {label}</label
          >
        {/each}
      </div>
      <span class="text-xs text-fg-muted">Schnittmenge mit den Phasen, die das Modul meldet (alle angehakt = wie das Modul).</span>
    </fieldset>
    <Field label="Konfiguration für das Modul (JSON)" error={wasmCfgError ?? undefined} hint="geht als Text an describe/handle des Moduls" class="sm:col-span-2">
      {#snippet children({ id, describedby })}<textarea {id} class={[textarea, 'min-h-24 font-mono']} aria-describedby={describedby} value={wasmCfgText} oninput={(e) => setWasmCfg(e.currentTarget.value)} spellcheck="false" data-testid="wasm-config"></textarea>{/snippet}
    </Field>
    {@render text('Fuel je Aufruf', ['fuel'], { numeric: true, hint: 'CPU-Grenze; Standard wasm.default_fuel' })}
    {@render text('Speicher (MiB)', ['maxMemoryMb'], { numeric: true, hint: 'Standard wasm.default_memory_mb' })}
    {@render list('Client-Header für das Modul', ['includeHeaders'], 'Allowlist, ein Header je Zeile; Auth-Header sind gesperrt', 'x-client-app')}
  {/if}
</div>
