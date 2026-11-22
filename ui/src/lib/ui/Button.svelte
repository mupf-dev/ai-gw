<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Button bzw. Link im Button-Look (mit href). Varianten/Größen siehe styles.ts.
  import type { Snippet } from 'svelte';
  import type { HTMLButtonAttributes } from 'svelte/elements';
  import Icon, { type IconName } from './Icon.svelte';
  import { btn, type ButtonSize, type ButtonVariant } from './styles';

  let {
    variant = 'secondary',
    size = 'md',
    icon,
    iconRight,
    href,
    loading = false,
    class: cls = '',
    children,
    type = 'button',
    ...rest
  }: Omit<HTMLButtonAttributes, 'class'> & {
    class?: string;
    variant?: ButtonVariant;
    size?: ButtonSize;
    icon?: IconName;
    iconRight?: IconName;
    href?: string;
    loading?: boolean;
    children?: Snippet;
  } = $props();
  const iconSize = $derived(size === 'sm' || size === 'icon-sm' ? 14 : 16);
</script>

{#if href}
  <a {href} class={btn(variant, size, cls)}>
    {#if icon}<Icon name={icon} size={iconSize} />{/if}
    {@render children?.()}
    {#if iconRight}<Icon name={iconRight} size={iconSize} />{/if}
  </a>
{:else}
  <button {type} class={btn(variant, size, cls)} aria-busy={loading || undefined} {...rest}>
    {#if loading}
      <Icon name="refresh" size={iconSize} class="animate-spin" />
    {:else if icon}
      <Icon name={icon} size={iconSize} />
    {/if}
    {@render children?.()}
    {#if iconRight}<Icon name={iconRight} size={iconSize} />{/if}
  </button>
{/if}
