<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Formular im Dialog: Inhalt (Felder), Formularfehler der API, Abbrechen/Speichern; Absenden per Enter.
  import type { Snippet } from 'svelte';
  import Button from '../ui/Button.svelte';
  import Dialog from '../ui/Dialog.svelte';
  import FormError from './FormError.svelte';

  let {
    open = $bindable(false),
    title,
    description,
    size = 'md',
    busy = false,
    loading = false,
    error = null,
    submitLabel = 'Speichern',
    onsubmit,
    children,
    testid,
  }: {
    open?: boolean;
    title: string;
    description?: string;
    size?: 'sm' | 'md' | 'lg';
    busy?: boolean;
    loading?: boolean;
    error?: string | null;
    submitLabel?: string;
    onsubmit: () => void;
    children: Snippet;
    testid: string;
  } = $props();
  const uid = $props.id();
</script>

<Dialog bind:open {title} {description} {size} {testid}>
  {#if loading}
    <div class="h-32 animate-pulse rounded-md bg-skeleton" aria-busy="true"></div>
  {:else}
    <form
      id="form-{uid}"
      class="grid gap-5 sm:grid-cols-2"
      novalidate
      onsubmit={(e) => {
        e.preventDefault();
        onsubmit();
      }}
    >
      {@render children()}
      <div class="sm:col-span-2"><FormError message={error} /></div>
    </form>
  {/if}
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (open = false)}>Abbrechen</Button>
    <Button variant="primary" type="submit" form="form-{uid}" icon="check" loading={busy} disabled={busy || loading} data-testid="{testid}-submit">{submitLabel}</Button>
  {/snippet}
</Dialog>
