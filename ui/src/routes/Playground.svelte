<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Playground (AIGW-54): Modellgruppen im Namen eines virtuellen Keys ausprobieren – Chat (SSE-Streaming oder
  // JSON), Systemprompt, Parameter (temperature, max_tokens, n), Vergleich bis zu drei Modelle nebeneinander,
  // Kennzahlen je Antwort (TTFT, Latenz, Tokens, Kosten, Deployment, Cache, Guardrails, Request-ID mit Link zum
  // Log) und Code-Beispiele. Die Anfragen laufen über POST /admin/api/v1/playground/chat durch die Pipeline der
  // Datenebene: Freigaben, Limits, Budgets und Guardrails des Keys gelten, jede Anfrage steht im Audit-Log.
  import { Switch } from 'bits-ui';
  import { playgroundModels, type Schemas } from '../lib/api/client';
  import { apiKeys, type ApiKey } from '../lib/api/resources';
  import { handleError, problemText } from '../lib/admin/errors.svelte';
  import { canWriteKey } from '../lib/rbac.svelte';
  import { isMock } from '../lib/mode.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Field from '../lib/ui/Field.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Input from '../lib/ui/Input.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Select from '../lib/ui/Select.svelte';
  import { route } from '../lib/router.svelte';
  import { switchRoot, switchThumb, textarea } from '../lib/ui/styles';
  import { buildBody, run, type Message, type Params } from './playground/chat';
  import CodeSnippets from './playground/CodeSnippets.svelte';
  import Pane, { type Turn } from './playground/Pane.svelte';

  type Model = Schemas['PlaygroundModel'];

  // ------------------------------------------------------------ Kontext: Key und Modelle
  // Vorbelegung per URL (?key=&model=), z. B. aus Tests oder Links.
  const initial = new URLSearchParams(route.search);
  let keys = $state<ApiKey[]>([]);
  let keysLoaded = $state(false);
  let key = $state(initial.get('key') ?? '');
  let models = $state<Model[]>([]);
  let modelsError = $state<string | null>(null);

  async function loadKeys() {
    try {
      const all = await apiKeys.list({ state: 'active' });
      keys = all.filter((k) => canWriteKey(k.spec.team, k.spec.organization));
      if (!keys.some((k) => k.metadata.name === key)) key = keys[0]?.metadata.name ?? '';
    } catch (e) {
      handleError(e, { action: 'Keys laden' });
    } finally {
      keysLoaded = true;
    }
  }
  loadKeys();

  let modelSeq = 0;
  $effect(() => {
    const k = key;
    const mine = ++modelSeq;
    models = [];
    modelsError = null;
    if (!k) return;
    playgroundModels(k)
      .then((r) => {
        if (mine !== modelSeq) return;
        models = r.models;
        const wanted = initial.get('model');
        for (const p of panes) if (!models.some((m) => m.name === p.model)) p.model = (wanted && models.some((m) => m.name === wanted) ? wanted : models[0]?.name) ?? '';
      })
      .catch((e) => {
        if (mine === modelSeq) modelsError = problemText(e);
      });
  });

  // ------------------------------------------------------------ Parameter
  let system = $state('');
  let temperature = $state('');
  let maxTokens = $state('');
  let n = $state('1');
  let stream = $state(true);
  let showCode = $state(false);

  // ------------------------------------------------------------ Spalten (Vergleich)
  interface PaneState {
    id: number;
    model: string;
    turns: Turn[];
  }
  let nextId = 1;
  let panes = $state<PaneState[]>([{ id: nextId++, model: '', turns: [] }]);
  const modelItems = $derived(models.map((m) => ({ value: m.name, label: m.name, hint: m.aliases.length ? m.aliases.join(', ') : undefined })));

  function addPane() {
    if (panes.length >= 3) return;
    const used = new Set(panes.map((p) => p.model));
    panes.push({ id: nextId++, model: models.find((m) => !used.has(m.name))?.name ?? models[0]?.name ?? '', turns: [] });
  }
  function removePane(id: number) {
    if (panes.length > 1) panes = panes.filter((p) => p.id !== id);
  }
  function clearAll() {
    for (const p of panes) p.turns = [];
  }

  // ------------------------------------------------------------ Senden
  let prompt = $state('');
  let controller: AbortController | null = null;
  let busy = $state(false);
  const params = (model: string): Params => ({ model, system, temperature, maxTokens, n: Number(n) || 1, stream });

  function history(p: PaneState, next: string): Message[] {
    const msgs: Message[] = [];
    for (const t of p.turns) {
      msgs.push({ role: 'user', content: t.prompt });
      const a = t.outcome?.choices[0]?.content;
      if (a) msgs.push({ role: 'assistant', content: a });
    }
    msgs.push({ role: 'user', content: next });
    return msgs;
  }

  const preview = $derived(buildBody(params(panes[0]?.model || 'modellgruppe'), history(panes[0] ?? { id: 0, model: '', turns: [] }, prompt || '…')));

  async function send() {
    const text = prompt.trim();
    if (!text || !key || busy) return;
    if (panes.some((p) => !p.model)) return;
    prompt = '';
    busy = true;
    controller = new AbortController();
    const signal = controller.signal;
    await Promise.all(
      panes.map(async (p) => {
        const body = buildBody(params(p.model), history(p, text));
        const turn: Turn = { prompt: text, model: p.model, outcome: null, adminError: null };
        p.turns.push(turn);
        const idx = p.turns.length - 1;
        try {
          await run(
            key,
            body,
            (o) => {
              // Neues Objekt je Stand, damit Svelte die Änderung sieht.
              p.turns[idx] = { ...p.turns[idx], outcome: { ...o, choices: o.choices.map((c) => ({ ...c })) } };
            },
            signal,
          );
        } catch (e) {
          const aborted = e instanceof DOMException && e.name === 'AbortError';
          const prev = p.turns[idx].outcome;
          p.turns[idx] = {
            ...p.turns[idx],
            adminError: aborted ? null : problemText(e),
            outcome: prev ? { ...prev, done: true, error: aborted ? { message: 'Abgebrochen', code: 'aborted', type: null } : prev.error } : null,
          };
        }
      }),
    );
    controller = null;
    busy = false;
  }

  function stop() {
    controller?.abort();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      void send();
    }
  }

  const keyItems = $derived(keys.map((k) => ({ value: k.metadata.name, label: k.metadata.name, hint: [k.spec.organization, k.spec.team].filter(Boolean).join(' / ') })));
  const keyObj = $derived(keys.find((k) => k.metadata.name === key));
</script>

<PageHeader
  title="Playground"
  description="Modellgruppen ausprobieren – im Namen eines virtuellen Keys: dessen Modellfreigaben, Limits, Budgets und Guardrails gelten, jede Anfrage steht im Request-Log und im Audit-Log."
>
  {#snippet actions()}
    <Button icon="terminal" onclick={() => (showCode = true)} disabled={!panes[0]?.model} data-testid="pg-code">Code</Button>
    <Button icon="refresh" onclick={clearAll} disabled={busy} data-testid="pg-clear">Neu beginnen</Button>
  {/snippet}
</PageHeader>

{#if keysLoaded && !keys.length}
  <Card>
    <EmptyState
      icon="key"
      title="Kein nutzbarer Key"
      text={isMock()
        ? 'Im Mock-Modus gibt es keine aktiven Keys mit Schreibrecht.'
        : 'Der Playground läuft im Namen eines aktiven virtuellen Keys, den du verwalten darfst (admin, org-admin oder team-admin). Lege unter „Keys“ einen an.'}
    />
  </Card>
{:else}
  <div class="grid gap-4 xl:grid-cols-[18rem_minmax(0,1fr)]">
    <Card title="Kontext & Parameter" testid="pg-params">
      <div class="flex flex-col gap-4">
        <Field label="Key" hint={keyObj ? `Organisation ${keyObj.spec.organization}${keyObj.spec.team ? `, Team ${keyObj.spec.team}` : ''}` : 'Anfragen laufen im Namen dieses Keys.'}>
          {#snippet children({ id, describedby })}
            <Select {id} {describedby} bind:value={key} items={keyItems} placeholder="Key wählen …" icon="key" />
          {/snippet}
        </Field>
        {#if modelsError}<p class="text-sm text-danger-fg" data-testid="pg-models-error">{modelsError}</p>{/if}
        {#if key && !modelsError && models.length === 0}<p class="text-sm text-fg-muted">Keine freigegebenen Modellgruppen für diesen Key.</p>{/if}
        <Field label="Systemprompt" optional>
          {#snippet children({ id, describedby })}
            <textarea {id} aria-describedby={describedby} class={[textarea, 'min-h-24 font-sans text-base']} bind:value={system} placeholder="Du bist ein hilfreicher Assistent." data-testid="pg-system"></textarea>
          {/snippet}
        </Field>
        <div class="grid grid-cols-2 gap-3">
          <Field label="temperature" optional>
            {#snippet children({ id, describedby })}
              <Input {id} aria-describedby={describedby} bind:value={temperature} inputmode="decimal" placeholder="Standard" data-testid="pg-temperature" />
            {/snippet}
          </Field>
          <Field label="max_tokens" optional>
            {#snippet children({ id, describedby })}
              <Input {id} aria-describedby={describedby} bind:value={maxTokens} inputmode="numeric" placeholder="Standard" data-testid="pg-max-tokens" />
            {/snippet}
          </Field>
        </div>
        <Field label="n (Varianten)">
          {#snippet children({ id, describedby })}
            <Select {id} {describedby} bind:value={n} items={['1', '2', '3', '4'].map((v) => ({ value: v, label: v }))} size="sm" />
          {/snippet}
        </Field>
        <label class="flex items-center justify-between gap-3 text-base">
          <span>Streaming (SSE)</span>
          <Switch.Root bind:checked={stream} class={switchRoot} data-testid="pg-stream"><Switch.Thumb class={switchThumb} /></Switch.Root>
        </label>
        <div class="flex items-center justify-between gap-2 border-t border-line pt-3">
          <span class="text-sm text-fg-muted">Vergleich: <span class="num">{panes.length}</span> von 3 Spalten</span>
          <Button size="sm" icon="plus" onclick={addPane} disabled={panes.length >= 3 || busy} data-testid="pg-add-pane">Spalte</Button>
        </div>
      </div>
    </Card>

    <div class="flex min-w-0 flex-col gap-4">
      <div class={['grid gap-4', panes.length === 2 && 'lg:grid-cols-2', panes.length === 3 && 'lg:grid-cols-3']}>
        {#each panes as p (p.id)}
          <Pane bind:model={p.model} turns={p.turns} {modelItems} {models} removable={panes.length > 1} onremove={() => removePane(p.id)} disabled={busy} />
        {/each}
      </div>

      <Card>
        <div class="flex flex-col gap-2">
          <label for="pg-prompt" class="sr-only">Nachricht</label>
          <textarea
            id="pg-prompt"
            class={[textarea, 'min-h-20 font-sans text-base']}
            bind:value={prompt}
            onkeydown={onKey}
            placeholder="Nachricht … (Strg+Enter sendet)"
            disabled={!key || !models.length}
            data-testid="pg-prompt"
          ></textarea>
          <div class="flex flex-wrap items-center gap-2">
            <span class="text-xs text-fg-muted"><Icon name="info" size={12} class="mr-1 align-[-1px]" />Verlauf je Spalte; jede Anfrage wird mit Key und Request-ID auditiert.</span>
            <div class="ml-auto flex gap-2">
              {#if busy}<Button icon="x" onclick={stop} data-testid="pg-stop">Stopp</Button>{/if}
              <Button variant="primary" icon="play" onclick={send} disabled={busy || !prompt.trim() || !key || panes.some((x) => !x.model)} loading={busy} data-testid="pg-send">Senden</Button>
            </div>
          </div>
        </div>
      </Card>
    </div>
  </div>
{/if}

<CodeSnippets bind:open={showCode} body={preview} />
