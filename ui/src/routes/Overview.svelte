<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  import { api, type VersionInfo } from '../lib/api/client';

  let info = $state<VersionInfo | null>(null);
  let error = $state<string | null>(null);

  $effect(() => {
    api
      .GET('/admin/api/v1/version')
      .then(({ data, response }) => {
        if (data) info = data;
        else error = `HTTP ${response.status}`;
      })
      .catch((e: unknown) => {
        error = e instanceof Error ? e.message : String(e);
      });
  });
</script>

<h1 class="mb-4 text-2xl font-semibold">Übersicht</h1>

<section class="max-w-md rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
  <h2 class="mb-2 text-sm font-medium text-zinc-500">Gateway</h2>
  {#if info}
    <dl class="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
      <dt class="text-zinc-500">Name</dt>
      <dd>{info.name}</dd>
      <dt class="text-zinc-500">Version</dt>
      <dd data-testid="version">{info.version}</dd>
    </dl>
  {:else if error}
    <p class="text-sm text-red-600">Admin-API nicht erreichbar: {error}</p>
  {:else}
    <p class="text-sm text-zinc-500">Lade …</p>
  {/if}
</section>
