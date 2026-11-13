<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts" generics="T">
  // Lade-, Fehler- und Datenzustand einer Abfrage (Loader). Fehler mit „Erneut versuchen“; 403 = keine Berechtigung.
  import type { Snippet } from 'svelte';
  import { ApiError } from '../api/client';
  import Button from '../ui/Button.svelte';
  import EmptyState from '../ui/EmptyState.svelte';
  import { problemText } from './errors.svelte';

  /** Loader oder Paged (gleiche Oberfläche). */
  interface Source<T> {
    data: T | null;
    error: unknown;
    loading: boolean;
    reload(): unknown;
  }
  let { loader, children, rows = 4 }: { loader: Source<T>; children: Snippet<[T]>; rows?: number } = $props();
  const forbidden = $derived(loader.error instanceof ApiError && loader.error.status === 403);
</script>

{#if loader.data !== null && !loader.error}
  {@render children(loader.data)}
{:else if loader.error}
  {#if forbidden}
    <EmptyState icon="lock" title="Keine Berechtigung" text="Ihre Rolle erlaubt diese Ansicht nicht. {problemText(loader.error)}" />
  {:else}
    <div role="alert">
      <EmptyState icon="alert" title="Laden fehlgeschlagen" text={problemText(loader.error)}>
        <Button icon="refresh" onclick={() => loader.reload()}>Erneut versuchen</Button>
      </EmptyState>
    </div>
  {/if}
{:else}
  <div class="flex flex-col gap-2 p-4" aria-busy="true" aria-label="Wird geladen">
    {#each Array.from({ length: rows }, (_, i) => i) as i (i)}
      <div class="h-8 animate-pulse rounded-md bg-skeleton"></div>
    {/each}
  </div>
{/if}
