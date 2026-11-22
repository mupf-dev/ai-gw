<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Anzeige von Limits (LimitSpec) samt Budget: „600 Anfragen je Minute · gpt-4o“.
  import type { S } from '../api/resources';
  import { num } from '../format';
  import { metricLabel, PERIOD, windowLabel } from './limits';

  let { limits, budget = null }: { limits: S['LimitSpec'][]; budget?: S['Budget'] | null } = $props();
</script>

<div class="flex flex-col gap-3 text-sm" data-testid="limit-list">
  {#if limits.length}
    <ul class="flex flex-col gap-1">
      {#each limits as l, i (i)}<li class="num">{num(l.limit)} {metricLabel(l.metric)} je {windowLabel(l.window)}{l.modelGroup ? ` · ${l.modelGroup}` : ''}</li>{/each}
    </ul>
  {:else}
    <p class="text-fg-muted">Keine eigenen Limits (übergeordnete gelten).</p>
  {/if}
  {#if budget}
    <p class="num">Budget {budget.amount} je {PERIOD[budget.period]}{budget.hardLimit === false ? ' (nur Warnung)' : ''}{budget.enforcement === 'soft' ? ' · soft' : ''}</p>
  {/if}
</div>
