<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" module>
  export type Tone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger' | 'info' | 'outline';
</script>

<script lang="ts">
  // Badge/Status-Chip. Status nie nur über Farbe: Text ist Pflicht, `dot` ergänzt nur.
  import type { Snippet } from 'svelte';
  import Icon, { type IconName } from './Icon.svelte';

  let {
    tone = 'neutral',
    dot = false,
    icon,
    mono = false,
    class: cls = '',
    children,
  }: { tone?: Tone; dot?: boolean; icon?: IconName; mono?: boolean; class?: string; children: Snippet } = $props();

  const TONES: Record<Tone, string> = {
    neutral: 'bg-neutral-bg text-neutral-fg',
    accent: 'bg-accent-soft text-accent-soft-fg',
    success: 'bg-success-bg text-success-fg',
    warning: 'bg-warning-bg text-warning-fg',
    danger: 'bg-danger-bg text-danger-fg',
    info: 'bg-info-bg text-info-fg',
    outline: 'border border-line text-fg-muted',
  };
</script>

<span
  class={[
    'inline-flex h-5 max-w-full items-center gap-1 rounded-sm px-1.5 text-xs font-medium whitespace-nowrap',
    mono && 'font-mono',
    TONES[tone],
    cls,
  ]}
>
  {#if dot}<span class="size-1.5 shrink-0 rounded-full bg-current" aria-hidden="true"></span>{/if}
  {#if icon}<Icon name={icon} size={12} />{/if}
  <span class="truncate">{@render children()}</span>
</span>
