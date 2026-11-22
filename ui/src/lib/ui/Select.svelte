<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts" module>
  export interface SelectItem {
    value: string;
    label: string;
    hint?: string;
    disabled?: boolean;
  }
</script>

<script lang="ts">
  // Einfachauswahl auf Bits UI Select (Tastatur, Typeahead, ARIA). Trigger im Input-Look.
  import { Select } from 'bits-ui';
  import Icon, { type IconName } from './Icon.svelte';
  import { cx, popover } from './styles';

  let {
    value = $bindable(''),
    items,
    placeholder = 'Auswählen …',
    label,
    id,
    icon,
    size = 'md',
    describedby,
    invalid = false,
    disabled = false,
    class: cls = '',
    onchange,
  }: {
    value?: string;
    items: SelectItem[];
    placeholder?: string;
    /** Zugänglicher Name, falls kein <label for> existiert. */
    label?: string;
    id?: string;
    icon?: IconName;
    size?: 'sm' | 'md';
    describedby?: string;
    invalid?: boolean;
    /** Ganze Auswahl gesperrt (z. B. durch eine übergeordnete Richtlinie). */
    disabled?: boolean;
    class?: string;
    onchange?: (v: string) => void;
  } = $props();
  const selected = $derived(items.find((i) => i.value === value));
</script>

<Select.Root type="single" bind:value {items} {disabled} onValueChange={(v) => onchange?.(v)}>
  <Select.Trigger
    {id}
    aria-label={label}
    aria-describedby={describedby}
    aria-invalid={invalid || undefined}
    class={cx(
      'inline-flex w-full min-w-0 items-center gap-2 rounded-md border border-line bg-surface text-left text-fg transition-colors hover:border-line-strong data-[state=open]:border-accent aria-invalid:border-danger data-disabled:cursor-not-allowed data-disabled:opacity-60 data-disabled:hover:border-line',
      size === 'sm' ? 'h-7 px-2 text-sm' : 'h-9 px-3 text-base',
      cls,
    )}
  >
    {#if icon}<Icon name={icon} size={14} class="text-fg-subtle" />{/if}
    <span class={['flex-1 truncate', !selected && 'text-fg-subtle']}>{selected?.label ?? placeholder}</span>
    <Icon name="chevron-down" size={14} class="text-fg-subtle" />
  </Select.Trigger>
  <Select.Portal>
    <Select.Content class={cx(popover, 'max-h-80 min-w-(--bits-select-anchor-width)')} sideOffset={4}>
      <Select.Viewport class="p-0.5">
        {#each items as item (item.value)}
          <Select.Item
            value={item.value}
            label={item.label}
            disabled={item.disabled}
            class="flex min-h-8 cursor-default items-center gap-2 rounded-md px-2 py-1 text-base outline-none data-disabled:opacity-50 data-highlighted:bg-surface-hover"
          >
            {#snippet children({ selected })}
              <span class="flex min-w-0 flex-1 flex-col">
                <span class="truncate">{item.label}</span>
                {#if item.hint}<span class="truncate text-xs text-fg-muted">{item.hint}</span>{/if}
              </span>
              {#if selected}<Icon name="check" size={14} class="text-accent-text" />{/if}
            {/snippet}
          </Select.Item>
        {/each}
      </Select.Viewport>
    </Select.Content>
  </Select.Portal>
</Select.Root>
