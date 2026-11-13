<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Budgetverbrauch im laufenden Zeitraum (GET /usage/budgets, AIGW-30/31) – je Zeile ein Meter (ab 80 % gelb,
  // ab 95 % rot), Betrag, Rest und Zeitraumende. Ohne `name`: alle Budgets der Ebene(n).
  import { amount, budgets, type BudgetStatus } from '../api/observe';
  import { dateTime } from '../format';
  import Badge from '../ui/Badge.svelte';
  import EmptyState from '../ui/EmptyState.svelte';
  import Meter from '../ui/Meter.svelte';
  import AsyncState from './AsyncState.svelte';
  import { Loader } from './errors.svelte';

  let { level, name, testid = 'budgets' }: { level?: BudgetStatus['level']; name?: string; testid?: string } = $props();

  const data = new Loader(async () => {
    const r = await budgets(level);
    return { ...r, items: r.items.filter((b) => !name || b.targetName === name) };
  });
  const PERIOD: Record<string, string> = { day: 'Tag', week: 'Woche', month: 'Monat', total: 'gesamt' };
  const LEVEL: Record<string, string> = { organization: 'Organisation', team: 'Team', 'api-key': 'Key' };
</script>

<div data-testid={testid}>
  <AsyncState loader={data} rows={2}>
    {#snippet children(r)}
      {#if r.items.length === 0}
        <EmptyState compact icon="coins" title="Kein Budget" text={name ? 'Für dieses Objekt ist kein Budget festgelegt.' : 'Es sind keine Budgets festgelegt.'} />
      {:else}
        <ul class="flex flex-col divide-y divide-line">
          {#each r.items as b (b.level + b.targetId)}
            <li class="grid grid-cols-[1fr_auto] items-center gap-x-4 gap-y-1.5 px-4 py-3" data-testid="budget-row">
              <span class="flex min-w-0 flex-wrap items-center gap-2">
                {#if !name}<Badge tone="outline">{LEVEL[b.level] ?? b.level}</Badge><span class="truncate font-mono text-sm font-medium">{b.targetName}</span>{/if}
                <span class="text-sm text-fg-muted">je {PERIOD[b.period] ?? b.period}{b.periodEnd ? ` · bis ${dateTime(Date.parse(b.periodEnd))}` : ''}</span>
                {#if b.exhausted}<Badge tone="danger" dot>{b.hardLimit ? 'ausgeschöpft – Anfragen abgelehnt' : 'ausgeschöpft (nur Warnung)'}</Badge>{/if}
              </span>
              <span class="num text-sm"><span class="text-fg">{amount(b.used)}</span> <span class="text-fg-muted">/ {amount(b.amount)} ({b.percent} %)</span></span>
              <Meter value={b.percent / 100} tone="auto" label="Budget {b.targetName}: {b.percent} % verbraucht" class="col-span-2" />
            </li>
          {/each}
        </ul>
        <p class="border-t border-line px-4 py-2 text-xs text-fg-subtle">Zeiträume in {r.timezone}; Stand {dateTime(Date.parse(r.at))}.</p>
      {/if}
    {/snippet}
  </AsyncState>
</div>
