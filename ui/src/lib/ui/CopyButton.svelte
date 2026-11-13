<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Kopieren in die Zwischenablage mit sichtbarer Bestätigung (Icon + Text für Screenreader).
  import Icon from './Icon.svelte';
  import { btn, type ButtonVariant } from './styles';

  let {
    text,
    label = 'Kopieren',
    variant = 'ghost',
    showLabel = false,
    oncopy,
    class: cls = '',
  }: { text: string; label?: string; variant?: ButtonVariant; showLabel?: boolean; oncopy?: () => void; class?: string } = $props();
  let copied = $state(false);
  let timer: ReturnType<typeof setTimeout> | undefined;

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      /* Zwischenablage gesperrt (http, Berechtigung) – Nutzer kann markieren und kopieren */
      return;
    }
    copied = true;
    oncopy?.();
    clearTimeout(timer);
    timer = setTimeout(() => (copied = false), 1600);
  }
</script>

<button type="button" class={btn(variant, showLabel ? 'sm' : 'icon-sm', cls)} aria-label={showLabel ? undefined : label} onclick={copy}>
  <Icon name={copied ? 'check' : 'copy'} size={14} class={copied ? 'text-success' : ''} />
  {#if showLabel}{copied ? 'Kopiert' : label}{/if}
  <span class="sr-only" aria-live="polite">{copied ? 'In die Zwischenablage kopiert' : ''}</span>
</button>
