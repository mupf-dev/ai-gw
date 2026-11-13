<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Limits & Budgets (AIGW-77): Budgetverbrauch aller Ebenen im laufenden Zeitraum (GET /usage/budgets) mit
  // Meter (80 %/95 %) und Hinweise zu ungenau wirkenden Limits (GET /usage/limit-warnings, F29).
  // Limits selbst werden an Organisation, Team, Key bzw. Modellgruppe gepflegt.
  import { ToggleGroup } from 'bits-ui';
  import type { S } from '../lib/api/resources';
  import BudgetMeters from '../lib/admin/BudgetMeters.svelte';
  import LimitWarnings from '../lib/admin/LimitWarnings.svelte';
  import Card from '../lib/ui/Card.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { segment, segmentItem } from '../lib/ui/styles';

  let level = $state<'' | S['BudgetLevelName']>('');
</script>

<PageHeader title="Limits & Budgets" description="Budgets je Organisation, Team und Key mit Verbrauch im laufenden Zeitraum. Ratenlimits werden am jeweiligen Objekt gepflegt (Detailseiten)." />

<div class="flex flex-col gap-4">
  <LimitWarnings />
  <Card flush title="Budgets">
    {#snippet actions()}
      <ToggleGroup.Root type="single" value={level} onValueChange={(v) => (level = (v ?? '') as typeof level)} class={segment} aria-label="Ebene">
        <ToggleGroup.Item value="" class={segmentItem}>Alle</ToggleGroup.Item>
        <ToggleGroup.Item value="organization" class={segmentItem}>Organisationen</ToggleGroup.Item>
        <ToggleGroup.Item value="team" class={segmentItem}>Teams</ToggleGroup.Item>
        <ToggleGroup.Item value="api-key" class={segmentItem}>Keys</ToggleGroup.Item>
      </ToggleGroup.Root>
    {/snippet}
    {#key level}<BudgetMeters level={level || undefined} testid="budget-list" />{/key}
  </Card>
</div>
