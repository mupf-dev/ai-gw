<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Formularfehler der Admin-API (Problem Details: Detailtext), als Alarm vorgelesen.
  import Icon from '../ui/Icon.svelte';

  let { message }: { message: string | null | undefined } = $props();
</script>

{#if message}
  <div class="flex gap-2 rounded-lg bg-danger-bg p-3 text-sm text-danger-fg" role="alert" data-testid="form-error">
    <Icon name="alert" class="mt-0.5" />
    <p class="min-w-0 break-words">{message}</p>
  </div>
{/if}
