<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Textfeld mit optionalem Icon links und Einheit/Suffix rechts.
  import type { HTMLInputAttributes } from 'svelte/elements';
  import Icon, { type IconName } from './Icon.svelte';
  import { input } from './styles';

  let {
    value = $bindable(),
    icon,
    suffix,
    class: cls = '',
    ...rest
  }: HTMLInputAttributes & { icon?: IconName; suffix?: string } = $props();
</script>

<div class={['relative flex items-center', cls]}>
  {#if icon}
    <span class="pointer-events-none absolute left-2.5 text-fg-subtle"><Icon name={icon} size={15} /></span>
  {/if}
  <input bind:value class={[input, icon && 'pl-8', suffix && 'pr-12']} {...rest} />
  {#if suffix}
    <span class="pointer-events-none absolute right-3 text-sm text-fg-subtle">{suffix}</span>
  {/if}
</div>
