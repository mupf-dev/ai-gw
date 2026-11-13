<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Toast-Region unten rechts (aria-live polite; Fehler zusätzlich role=alert).
  import { dismiss, pauseToasts, toasts, type ToastKind } from '../toast.svelte';
  import Icon, { type IconName } from './Icon.svelte';
  import { btn } from './styles';

  const ICON: Record<ToastKind, IconName> = { success: 'check-circle', info: 'info', warning: 'alert', danger: 'x-circle' };
  const TONE: Record<ToastKind, string> = {
    success: 'text-success',
    info: 'text-info-fg',
    warning: 'text-warning',
    danger: 'text-danger',
  };
</script>

<div
  class="pointer-events-none fixed right-4 bottom-4 z-[60] flex w-[min(24rem,calc(100vw-2rem))] flex-col gap-2"
  aria-live="polite"
  role="region"
  aria-label="Benachrichtigungen"
  onmouseenter={() => pauseToasts(true)}
  onmouseleave={() => pauseToasts(false)}
  onfocusin={() => pauseToasts(true)}
  onfocusout={() => pauseToasts(false)}
>
  {#each toasts as t (t.id)}
    <div
      class="anim-toast pointer-events-auto flex items-start gap-3 rounded-lg border border-line bg-surface p-3 shadow-pop"
      role={t.kind === 'danger' ? 'alert' : 'status'}
      data-testid="toast"
    >
      <span class={['mt-0.5', TONE[t.kind]]}><Icon name={ICON[t.kind]} /></span>
      <div class="min-w-0 flex-1">
        <p class="text-base font-medium text-fg">{t.title}</p>
        {#if t.text}<p class="mt-0.5 text-sm text-fg-muted">{t.text}</p>{/if}
      </div>
      <button type="button" class={btn('ghost', 'icon-sm', '-m-1')} aria-label="Benachrichtigung schließen" onclick={() => dismiss(t.id)}>
        <Icon name="x" size={14} />
      </button>
    </div>
  {/each}
</div>
