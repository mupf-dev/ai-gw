<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Formularmuster: Label oben, Hilfetext darunter, Fehler ersetzt den Hilfetext (aria-describedby).
  // Das Steuerelement bekommt id/describedby/invalid über das children-Snippet.
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';

  let {
    label,
    hint,
    error,
    required = false,
    optional = false,
    id: idProp,
    class: cls = '',
    children,
  }: {
    label: string;
    hint?: string;
    error?: string | null;
    required?: boolean;
    optional?: boolean;
    id?: string;
    class?: string;
    children: Snippet<[{ id: string; describedby: string | undefined; invalid: boolean }]>;
  } = $props();
  const uid = $props.id();
  const id = $derived(idProp ?? `f-${uid}`);
  const descId = $derived(error || hint ? `${id}-desc` : undefined);
</script>

<div class={['flex flex-col gap-1.5', cls]}>
  <label for={id} class="flex items-baseline gap-1 text-sm font-medium text-fg">
    {label}
    {#if required}<span class="text-danger" aria-hidden="true">*</span>{/if}
    {#if optional}<span class="text-xs font-normal text-fg-subtle">(optional)</span>{/if}
  </label>
  {@render children({ id, describedby: descId, invalid: !!error })}
  {#if error}
    <p id={descId} class="flex items-center gap-1 text-xs text-danger-fg"><Icon name="alert" size={12} />{error}</p>
  {:else if hint}
    <p id={descId} class="text-xs text-fg-muted">{hint}</p>
  {/if}
</div>
