<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Kettenvorschau (AIGW-52): wirksame Plugin-Kette für einen Kontext (Organisation, Team, Key, Modellgruppe)
  // nach den Regeln aus 12-plugins.md §4 (chain.ts). Anfrage-Haken laufen von außen nach innen, Antwort- und
  // Stream-Haken in umgekehrter Reihenfolge (Zwiebelprinzip, F167b) – gezeigt als Schichten.
  import type { PluginBinding } from '../../api/resources';
  import { catalog, orgOfTeam } from '../../context.svelte';
  import Badge from '../../ui/Badge.svelte';
  import EmptyState from '../../ui/EmptyState.svelte';
  import Field from '../../ui/Field.svelte';
  import Icon from '../../ui/Icon.svelte';
  import Input from '../../ui/Input.svelte';
  import { compose, type ChainContext } from './chain';

  let { bindings, onedit }: { bindings: PluginBinding[]; onedit?: (name: string) => void } = $props();

  let c = $state<ChainContext>({ organization: '', team: '', apiKey: '', modelGroup: '' });
  // Team gewählt → Organisation des Teams vorbelegen.
  $effect(() => {
    const o = orgOfTeam(c.team);
    if (o && !c.organization) c.organization = o;
  });
  const chain = $derived(compose(bindings, c));
  // Einrückung je Schicht (Klassen statt Inline-Stil, CSP).
  const INDENT = ['', 'pl-3', 'pl-6', 'pl-9', 'pl-12', 'pl-15', 'pl-18'];
  const LEVEL: Record<string, string> = { global: 'global', organization: 'Organisation', team: 'Team', 'api-key': 'Key', 'model-group': 'Modellgruppe' };
  const where = (b: PluginBinding) => `${LEVEL[b.spec.scope.level]}${b.spec.scope.target ? ` · ${b.spec.scope.target}` : ''}`;
  const targetsOf = (level: string) => [...new Set(bindings.filter((b) => b.spec.scope.level === level).map((b) => b.spec.scope.target ?? ''))].filter(Boolean);
</script>

<div class="flex flex-col gap-4" data-testid="chain-view">
  <div class="grid gap-3 sm:grid-cols-4">
    <Field label="Organisation">
      {#snippet children({ id })}<Input {id} bind:value={c.organization} list="cv-orgs" autocomplete="off" placeholder="–" />{/snippet}
    </Field>
    <Field label="Team">
      {#snippet children({ id })}<Input {id} bind:value={c.team} list="cv-teams" autocomplete="off" placeholder="–" />{/snippet}
    </Field>
    <Field label="API-Key">
      {#snippet children({ id })}<Input {id} bind:value={c.apiKey} list="cv-keys" autocomplete="off" placeholder="–" />{/snippet}
    </Field>
    <Field label="Modellgruppe">
      {#snippet children({ id })}<Input {id} bind:value={c.modelGroup} list="cv-groups" autocomplete="off" placeholder="–" />{/snippet}
    </Field>
  </div>
  <datalist id="cv-orgs">{#each catalog.orgs as o (o.metadata.name)}<option value={o.metadata.name}></option>{/each}</datalist>
  <datalist id="cv-teams">{#each catalog.teams.filter((t) => !c.organization || t.spec.organization === c.organization) as t (t.metadata.name)}<option value={t.metadata.name}></option>{/each}</datalist>
  <datalist id="cv-keys">{#each targetsOf('api-key') as k (k)}<option value={k}></option>{/each}</datalist>
  <datalist id="cv-groups">{#each targetsOf('model-group') as g (g)}<option value={g}></option>{/each}</datalist>

  {#if chain.steps.length === 0}
    <EmptyState compact icon="layers" title="Keine wirksamen Plugins" text="Für diesen Kontext greift keine Bindung (global, Organisation, Team, Key, Modellgruppe)." />
  {:else}
    <div class="grid gap-4 lg:grid-cols-[1fr_auto]">
      <ol class="flex flex-col" aria-label="Wirksame Kette, Anfrage-Reihenfolge" data-testid="chain-steps">
        {#each chain.steps as s, i (s.binding.metadata.name)}
          <li class={INDENT[Math.min(i, INDENT.length - 1)]}>
            <div class="flex flex-wrap items-center gap-2 rounded-md border border-line bg-surface px-3 py-2" data-testid="chain-step">
              <span class="num w-6 text-sm text-fg-muted">{i + 1}.</span>
              <Badge mono>{s.binding.spec.plugin}</Badge>
              {#if onedit}
                <button type="button" class="font-mono text-sm underline-offset-2 hover:underline" onclick={() => onedit?.(s.binding.metadata.name)}>{s.binding.metadata.name}</button>
              {:else}<span class="font-mono text-sm">{s.binding.metadata.name}</span>{/if}
              <span class="text-xs text-fg-subtle">{where(s.binding)} · Reihenfolge {s.binding.spec.order ?? 0}</span>
              {#if s.binding.spec.locked}<Badge tone="accent" icon="lock">gesperrt</Badge>{/if}
              {#if s.binding.effective}<Badge tone={s.binding.effective.failMode === 'closed' ? 'warning' : 'info'} dot>{s.binding.effective.failMode}</Badge>{/if}
            </div>
          </li>
        {/each}
      </ol>
      <div class="flex flex-col gap-1 text-sm text-fg-muted lg:w-56">
        <span class="flex items-center gap-1.5"><Icon name="arrow-down" size={14} /> Anfrage: 1 → {chain.steps.length}</span>
        <span class="flex items-center gap-1.5"><Icon name="arrow-up" size={14} /> Antwort/Stream: {chain.steps.length} → 1</span>
        <span class="text-xs">Zwiebelprinzip: die äußerste Schicht sieht die Antwort zuletzt (z. B. ein Webhook hinter pii nur Pseudonyme).</span>
      </div>
    </div>
  {/if}
  {#if chain.removed.length || chain.ineffective.length}
    <ul class="flex flex-col gap-1 text-sm text-fg-muted" data-testid="chain-removed">
      {#each chain.removed as r (r.binding.metadata.name + r.by)}
        <li><span class="font-mono">{r.binding.metadata.name}</span> ({where(r.binding)}) entfällt – ersetzt/abgeschaltet durch <span class="font-mono">{r.by}</span></li>
      {/each}
      {#each chain.ineffective as r (r.binding.metadata.name)}
        <li><span class="font-mono">{r.binding.metadata.name}</span> ({r.binding.spec.inherit}) ist wirkungslos – <span class="font-mono">{r.by}</span> ist gesperrt</li>
      {/each}
    </ul>
  {/if}
  <p class="text-xs text-fg-subtle">Vorschau nach den Regeln der Plugin-Kette (12-plugins.md §4) aus den sichtbaren Bindungen; ungültige Bindungen erscheinen hier, werden im Betrieb aber übersprungen bzw. sperren die Kette.</p>
</div>
