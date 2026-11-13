<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Tooltip (Bits UI): nur ergänzende Information, nie die einzige Quelle. Der Trigger wird über das
  // Snippet `trigger({ props })` gerendert, damit vorhandene Buttons/Links ihre Semantik behalten.
  import { Tooltip } from 'bits-ui';
  import type { Snippet } from 'svelte';
  import { tooltip } from './styles';

  let {
    text,
    side = 'top',
    trigger,
  }: {
    text: string;
    side?: 'top' | 'right' | 'bottom' | 'left';
    trigger: Snippet<[{ props: Record<string, unknown> }]>;
  } = $props();
</script>

<Tooltip.Root delayDuration={250}>
  <Tooltip.Trigger>
    {#snippet child({ props })}{@render trigger({ props })}{/snippet}
  </Tooltip.Trigger>
  <Tooltip.Portal>
    <Tooltip.Content {side} sideOffset={6} class={tooltip} role="tooltip" data-testid="tooltip">{text}</Tooltip.Content>
  </Tooltip.Portal>
</Tooltip.Root>
