<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Echte Daten brauchen eine Anmeldung: ohne Session (und ohne Mock-Modus) Hinweis + Anmelden statt Fehlerflut.
  import type { Snippet } from 'svelte';
  import { isMock } from '../mode.svelte';
  import { setMockMode } from '../prefs.svelte';
  import { auth, requestLogin } from '../session.svelte';
  import Button from '../ui/Button.svelte';
  import Card from '../ui/Card.svelte';
  import EmptyState from '../ui/EmptyState.svelte';

  let { children }: { children: Snippet } = $props();
</script>

{#if isMock() || auth.session}
  {@render children()}
{:else}
  <Card>
    <EmptyState
      icon="lock"
      title="Anmeldung erforderlich"
      text={auth.oidc ? 'Melden Sie sich über den Identitätsanbieter an.' : 'Melden Sie sich mit dem Bootstrap-Token oder einem Admin-API-Key an.'}
    >
      <Button variant="primary" icon="lock" onclick={() => requestLogin()} data-testid="gate-login">Anmelden</Button>
      <Button onclick={() => setMockMode('on')}>Mock-Daten ansehen</Button>
    </EmptyState>
  </Card>
{/if}
