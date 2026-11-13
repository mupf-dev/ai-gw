<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Modelle & Routen: Modellgruppen (Alias) als Master/Detail mit Prioritätsgruppen, Gewichten, Zustand und
  // Fallback-Kette; Routing-Regeln (Eingangsname/Muster → Modellgruppe) als Tabelle.
  import { Switch, Tabs } from 'bits-ui';
  import { models } from '../lib/data.svelte';
  import { ms, num, percent, short } from '../lib/format';
  import { PROVIDERS, teamName } from '../lib/mock/data';
  import type { Deployment, Health, ModelGroup, RouteRule, Strategy } from '../lib/mock/types';
  import { route } from '../lib/router.svelte';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import DataTable, { type Column } from '../lib/ui/DataTable.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Meter from '../lib/ui/Meter.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import Tooltip from '../lib/ui/Tooltip.svelte';
  import { tabsList, tabsTrigger } from '../lib/ui/styles';

  const res = $derived(models());
  const groups = $derived(res.state === 'ok' ? res.data.groups : []);
  let routes = $state<RouteRule[]>([]);
  $effect(() => {
    routes = res.state === 'ok' ? res.data.routes.map((r) => ({ ...r })) : [];
  });

  let tab = $state('groups');
  let selected = $state(new URLSearchParams(route.search).get('group') ?? 'gpt-4o');
  const group = $derived(groups.find((g) => g.alias === selected) ?? groups[0]);

  const STRATEGY: Record<Strategy, { label: string; text: string }> = {
    priority: { label: 'Priorität', text: 'Höchste verfügbare Prioritätsgruppe; innerhalb der Gruppe nach Gewicht verteilt.' },
    weighted: { label: 'Gewichtet', text: 'Last anteilig nach Gewicht über alle gesunden Deployments.' },
    latency: { label: 'Geringste Latenz', text: 'Deployment mit der niedrigsten gleitenden p95-Latenz zuerst.' },
    cost: { label: 'Geringste Kosten', text: 'Günstigstes gesundes Deployment zuerst, Gewicht bei Gleichstand.' },
  };
  const HEALTH: Record<Health, { tone: 'success' | 'warning' | 'danger'; label: string }> = {
    healthy: { tone: 'success', label: 'Gesund' },
    degraded: { tone: 'warning', label: 'Eingeschränkt' },
    down: { tone: 'danger', label: 'Ausgefallen' },
  };
  const DOT: Record<Health, string> = { healthy: 'bg-success', degraded: 'bg-warning', down: 'bg-danger' };

  function byPriority(g: ModelGroup): [number, Deployment[]][] {
    const m = new Map<number, Deployment[]>();
    for (const d of g.deployments) m.set(d.priority, [...(m.get(d.priority) ?? []), d]);
    return [...m.entries()].sort((a, b) => a[0] - b[0]);
  }
  const share = (d: Deployment, list: Deployment[]) => d.weight / (list.reduce((a, x) => a + x.weight, 0) || 1);

  const routeColumns: Column<RouteRule>[] = [
    { id: 'order', label: '#', cell: orderCell, class: 'w-10' },
    { id: 'match', label: 'Eingangsmodell (Muster)', cell: matchCell, sort: (r) => r.match },
    { id: 'target', label: 'Modellgruppe', cell: targetCell, sort: (r) => r.target },
    { id: 'teams', label: 'Teams', cell: teamsCell, from: 'md' },
    { id: 'note', label: 'Notiz', cell: noteCell, from: 'lg' },
    { id: 'enabled', label: 'Aktiv', cell: enabledCell, align: 'right', class: 'w-16' },
  ];
</script>

{#snippet orderCell(r: RouteRule)}<span class="num text-fg-subtle">{routes.findIndex((x) => x.id === r.id) + 1}</span>{/snippet}
{#snippet matchCell(r: RouteRule)}<code class="rounded-sm bg-surface-2 px-1.5 py-0.5 text-sm">{r.match}</code>{/snippet}
{#snippet targetCell(r: RouteRule)}
  <span class="inline-flex items-center gap-1.5 text-fg-muted"><Icon name="arrow-left" size={12} class="rotate-180" /><span class="font-mono text-sm text-fg">{r.target}</span></span>
{/snippet}
{#snippet teamsCell(r: RouteRule)}
  {#if r.teams === 'all'}<span class="text-sm text-fg-muted">alle</span>{:else}<span class="flex flex-wrap gap-1"
      >{#each r.teams as t (t)}<Badge>{teamName(t)}</Badge>{/each}</span
    >{/if}
{/snippet}
{#snippet noteCell(r: RouteRule)}<span class="text-sm text-fg-muted">{r.note}</span>{/snippet}
{#snippet enabledCell(r: RouteRule)}
  <Switch.Root
    checked={r.enabled}
    onCheckedChange={(c) => {
      routes = routes.map((x) => (x.id === r.id ? { ...x, enabled: c } : x));
      toast('info', c ? 'Regel aktiviert' : 'Regel deaktiviert', r.match);
    }}
    aria-label="Regel {r.match} aktiv"
    class="inline-flex h-5 w-9 shrink-0 items-center rounded-full bg-line-strong p-0.5 transition-colors data-[state=checked]:bg-accent"
  >
    <Switch.Thumb class="size-4 rounded-full bg-surface shadow-card transition-transform data-[state=checked]:translate-x-4" />
  </Switch.Root>
{/snippet}

<PageHeader title="Modelle & Routen" description="Modellgruppen bündeln Deployments verschiedener Anbieter unter einem Alias – mit Prioritäten, Gewichten und Fallbacks.">
  {#snippet actions()}
    <Button variant="secondary" icon="file-text">Als YAML</Button>
    <Button variant="primary" icon="plus">Modellgruppe</Button>
  {/snippet}
</PageHeader>

{#if res.state === 'unavailable'}
  <Card><EmptyState icon="route" title="Modelle noch nicht verfügbar" text={res.reason} /></Card>
{:else}
  <Tabs.Root bind:value={tab}>
    <Tabs.List class={tabsList} aria-label="Ansicht">
      <Tabs.Trigger value="groups" class={tabsTrigger}>Modellgruppen <Badge>{groups.length}</Badge></Tabs.Trigger>
      <Tabs.Trigger value="routes" class={tabsTrigger}>Routing-Regeln <Badge>{routes.length}</Badge></Tabs.Trigger>
    </Tabs.List>

    <Tabs.Content value="groups" class="mt-4 grid gap-4 lg:grid-cols-[18rem_1fr]">
      <nav aria-label="Modellgruppen" class="flex flex-col gap-1.5" data-testid="group-list">
        {#each groups as g (g.alias)}
          <button
            type="button"
            onclick={() => (selected = g.alias)}
            aria-current={g.alias === group?.alias ? 'true' : undefined}
            class="flex flex-col gap-1.5 rounded-lg border border-line bg-surface px-3 py-2.5 text-left shadow-card transition-colors hover:border-line-strong aria-[current=true]:border-accent aria-[current=true]:ring-1 aria-[current=true]:ring-accent"
          >
            <span class="flex items-center gap-2">
              <span class="flex-1 truncate font-mono text-sm font-medium text-fg">{g.alias}</span>
              <span class="flex gap-0.5" aria-hidden="true">{#each g.deployments as d (d.id)}<span class={['size-1.5 rounded-full', DOT[d.health]]}></span>{/each}</span>
            </span>
            <span class="flex flex-wrap items-center gap-x-2 text-xs text-fg-muted">
              <span>{STRATEGY[g.strategy].label}</span><span>{g.deployments.length} Deployments</span>
            </span>
          </button>
        {/each}
      </nav>

      {#if group}
        <div class="flex min-w-0 flex-col gap-4" data-testid="group-detail">
          <Card>
            <div class="flex flex-wrap items-start gap-4">
              <div class="min-w-0 flex-1">
                <div class="flex flex-wrap items-center gap-2">
                  <h2 class="font-mono text-lg font-semibold text-fg">{group.alias}</h2>
                  <Badge tone="accent">{group.modality === 'chat' ? 'Chat' : 'Embedding'}</Badge>
                  <Badge tone="outline">{STRATEGY[group.strategy].label}</Badge>
                </div>
                <p class="mt-1 text-base text-fg-muted">{group.description}</p>
              </div>
              <Button size="sm" variant="secondary" icon="play">Im Playground testen</Button>
            </div>
            <dl class="mt-4 grid grid-cols-2 gap-3 border-t border-line pt-4 text-sm sm:grid-cols-4">
              <div><dt class="text-fg-muted">Kontextfenster</dt><dd class="num mt-0.5 text-base text-fg">{num(group.contextWindow)}</dd></div>
              <div><dt class="text-fg-muted">Preis Eingabe / 1 Mio.</dt><dd class="num mt-0.5 text-base text-fg">{group.priceIn.toFixed(2)} €</dd></div>
              <div><dt class="text-fg-muted">Preis Ausgabe / 1 Mio.</dt><dd class="num mt-0.5 text-base text-fg">{group.priceOut.toFixed(2)} €</dd></div>
              <div><dt class="text-fg-muted">Requests 24 h</dt><dd class="num mt-0.5 text-base text-fg">{short(group.requests24h)}</dd></div>
            </dl>
          </Card>

          <Card title="Deployments" description={STRATEGY[group.strategy].text} flush>
            {#each byPriority(group) as [prio, list] (prio)}
              <div class="border-b border-line last:border-0">
                <div class="flex items-center gap-2 bg-surface-2 px-4 py-1.5">
                  <span class="label-caps text-fg-muted">Priorität {prio}</span>
                  <span class="text-xs text-fg-subtle">{prio === 1 ? 'primär' : 'nur wenn Priorität ' + (prio - 1) + ' nicht verfügbar'}</span>
                </div>
                <ul>
                  {#each list as d (d.id)}
                    <li class="grid grid-cols-[1fr_auto] items-center gap-x-4 gap-y-2 border-t border-line px-4 py-3 first:border-t-0 md:grid-cols-[minmax(0,1.6fr)_7.5rem_minmax(0,1fr)_4.5rem_4.5rem]">
                      <div class="min-w-0">
                        <div class="flex items-center gap-2">
                          <span class="grid h-5 min-w-8 place-items-center rounded-sm bg-surface-2 px-1 font-mono text-2xs font-semibold text-fg-muted">{PROVIDERS[d.provider].short}</span>
                          <span class="truncate text-base font-medium text-fg">{d.id}</span>
                        </div>
                        <div class="mt-0.5 truncate font-mono text-xs text-fg-muted">{d.upstreamModel} · {d.region}</div>
                      </div>
                      <span class="justify-self-end md:justify-self-start"><Badge tone={HEALTH[d.health].tone} dot>{HEALTH[d.health].label}</Badge></span>
                      <div class="col-span-2 flex flex-col gap-1 md:col-span-1">
                        <span class="flex justify-between text-xs text-fg-muted"><span>Gewicht</span><span class="num">{percent(share(d, list))}</span></span>
                        <Meter value={share(d, list)} label="Gewicht {d.id}" />
                      </div>
                      <div class="hidden text-right md:block">
                        <div class="num text-sm text-fg">{d.health === 'down' ? '–' : ms(d.p95)}</div>
                        <div class="text-xs text-fg-subtle">p95</div>
                      </div>
                      <div class="hidden text-right md:block">
                        <Tooltip text="{num(d.rpm)} von {num(d.rpmLimit)} RPM Kontingent">
                          {#snippet trigger({ props })}
                            <button {...props} type="button" class="num cursor-help text-sm text-fg">{d.health === 'down' ? '–' : percent(d.successRate)}</button>
                          {/snippet}
                        </Tooltip>
                        <div class="text-xs text-fg-subtle">Erfolg</div>
                      </div>
                    </li>
                  {/each}
                </ul>
              </div>
            {/each}
          </Card>

          <Card title="Fallback-Kette" description="Bei 5xx, 429 oder Zeitüberschreitung nach 2 Wiederholungen mit Backoff; höchstens 2 Fallbacks pro Request.">
            {#if group.fallbacks.length}
              <ol class="flex flex-wrap items-center gap-2" data-testid="fallbacks">
                <li><span class="inline-flex h-8 items-center rounded-md border border-accent bg-accent-soft px-3 font-mono text-sm text-accent-soft-fg">{group.alias}</span></li>
                {#each group.fallbacks as fb, i (fb)}
                  <li class="flex items-center gap-2">
                    <Icon name="chevron-right" size={14} class="text-fg-subtle" />
                    <button
                      type="button"
                      class="inline-flex h-8 items-center gap-2 rounded-md border border-line bg-surface px-3 font-mono text-sm text-fg hover:border-line-strong"
                      onclick={() => (selected = fb)}
                    >
                      <span class="num text-2xs text-fg-subtle">{i + 1}</span>{fb}
                    </button>
                  </li>
                {/each}
              </ol>
            {:else}
              <p class="text-base text-fg-muted">Keine Fallbacks – Fehler werden direkt an den Client gemeldet.</p>
            {/if}
          </Card>
        </div>
      {/if}
    </Tabs.Content>

    <Tabs.Content value="routes" class="mt-4">
      <Card flush>
        <DataTable rows={routes} columns={routeColumns} key={(r) => r.id} caption="Routing-Regeln" search={(r) => `${r.match} ${r.target} ${r.note}`} searchPlaceholder="Muster oder Ziel …" testid="routes-table" />
      </Card>
      <p class="mt-2 flex items-center gap-1.5 text-sm text-fg-muted">
        <Icon name="info" size={14} /> Regeln werden von oben nach unten geprüft; die erste passende gewinnt. Unbekannte Modelle → 404.
      </p>
    </Tabs.Content>
  </Tabs.Root>
{/if}
