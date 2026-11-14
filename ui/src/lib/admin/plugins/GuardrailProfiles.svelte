<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // AIGW-86: Standard-Guardrail-Profile (Basis, Streng, Offen) als Karten mit Erklärung und Vorschau der Bindungen;
  // „Anwenden auf …“ mit Scope-Auswahl, Dry-Run-Diff und anschließendem Anwenden (POST …/{name}:apply). Rechte prüft
  // der Server wie bei Plugin-Bindungen (F183, AIGW-58: Secret-Verweise nur globaler admin).
  import { applyGuardrailProfile, guardrailProfiles, type GuardrailProfile, type GuardrailProfileApplyResult, type S } from '../../api/resources';
  import { catalog } from '../../context.svelte';
  import { toast } from '../../toast.svelte';
  import Badge from '../../ui/Badge.svelte';
  import Button from '../../ui/Button.svelte';
  import Card from '../../ui/Card.svelte';
  import Field from '../../ui/Field.svelte';
  import Input from '../../ui/Input.svelte';
  import Select from '../../ui/Select.svelte';
  import AsyncState from '../AsyncState.svelte';
  import { formErrorOf, Loader } from '../errors.svelte';
  import FormDialog from '../FormDialog.svelte';

  let { onapplied }: { onapplied?: () => void } = $props();

  const profiles = new Loader(() => guardrailProfiles());

  const LEVELS: { value: S['ScopeLevel']; label: string }[] = [
    { value: 'global', label: 'global' },
    { value: 'organization', label: 'Organisation' },
    { value: 'team', label: 'Team' },
    { value: 'api-key', label: 'Key' },
    { value: 'model-group', label: 'Modellgruppe' },
  ];
  const ACTION: Record<string, { label: string; tone: 'success' | 'warning' | 'danger' | 'neutral' }> = {
    create: { label: 'neu', tone: 'success' },
    update: { label: 'ändern', tone: 'warning' },
    delete: { label: 'entfernen', tone: 'danger' },
    unchanged: { label: 'unverändert', tone: 'neutral' },
  };

  // ------------------------------------------------------------ Anwenden
  let open = $state(false);
  let profile = $state<GuardrailProfile | null>(null);
  let level = $state<S['ScopeLevel']>('team');
  let target = $state('');
  let logging = $state(true);
  // AIGW-89: Azure wahlweise mit Entra ID (auth: entraId + ProviderCredential mit spec.entra) statt API-Key-Secret.
  const CS_EMPTY = { provider: '', endpoint: '', secret: '', modelGroup: '', auth: 'apiKey', credential: '' };
  let cs = $state({ ...CS_EMPTY });
  let preview = $state<GuardrailProfileApplyResult | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);
  const needsCs = $derived(!!profile?.bindings.some((b) => b.requires === 'contentSafety'));
  const targets = $derived(level === 'organization' ? catalog.orgs.map((o) => o.metadata.name) : level === 'team' ? catalog.teams.map((t) => t.metadata.name) : []);

  function start(p: GuardrailProfile) {
    profile = p;
    level = 'team';
    target = '';
    logging = true;
    cs = { ...CS_EMPTY };
    preview = null;
    error = null;
    open = true;
  }
  // Jede Eingabe verwirft die Vorschau – angewendet wird nur, was zuvor als Diff zu sehen war.
  $effect(() => {
    void [level, target, logging, cs.provider, cs.endpoint, cs.secret, cs.modelGroup, cs.auth, cs.credential];
    preview = null;
  });

  function body(dryRun: boolean): S['GuardrailProfileApply'] {
    const b: S['GuardrailProfileApply'] = { scope: { level, target: level === 'global' ? null : target.trim() }, dryRun, logging };
    if (needsCs && cs.provider) {
      const c: Record<string, string> = { provider: cs.provider };
      if (cs.provider === 'model') c.modelGroup = cs.modelGroup.trim();
      else {
        if (cs.endpoint.trim()) c.endpoint = cs.endpoint.trim();
        if (cs.provider === 'azure' && cs.auth === 'entraId') {
          c.auth = 'entraId';
          if (cs.credential.trim()) c.credential = cs.credential.trim();
        } else if (cs.secret.trim()) c.secret = cs.secret.trim();
      }
      b.contentSafety = c as unknown as S['GuardrailProfileApply']['contentSafety'];
    }
    return b;
  }

  async function submit() {
    if (!profile) return;
    if (level !== 'global' && !target.trim()) {
      error = 'Ziel angeben.';
      return;
    }
    busy = true;
    error = null;
    const name = profile.name;
    try {
      if (!preview) {
        preview = await applyGuardrailProfile(name, body(true));
        return;
      }
      const r = await applyGuardrailProfile(name, body(false));
      const n = r.changes.filter((c) => c.action !== 'unchanged').length;
      toast('success', `Profil „${profile.title}“ angewendet`, n ? `${n} Änderung(en)` : 'Alles war schon aktuell.');
      open = false;
      onapplied?.();
    } catch (e) {
      error = formErrorOf(e, 'Profil anwenden');
    } finally {
      busy = false;
    }
  }
  const pending = $derived(preview ? preview.changes.filter((c) => c.action !== 'unchanged').length : 0);
</script>

<AsyncState loader={profiles}>
  {#snippet children(items)}
    <div class="grid gap-4 lg:grid-cols-3" data-testid="guardrail-profiles">
      {#each items as p (p.name)}
        <Card title={p.title} description={p.summary} testid={`profile-${p.name}`}>
          {#snippet actions()}<Badge tone="outline" mono>v{p.version}</Badge>{/snippet}
          <p class="text-sm whitespace-pre-line text-fg-muted">{p.description.trim()}</p>
          <h4 class="mt-4 mb-2 text-sm font-medium">Bindungen</h4>
          <ul class="space-y-2 text-sm">
            {#each p.bindings as b (b.spec.plugin)}
              <li class="flex flex-col gap-1">
                <span class="flex flex-wrap items-center gap-1.5">
                  <Badge mono>{b.spec.plugin}</Badge>
                  <Badge tone="outline">Reihenfolge {b.spec.order ?? 0}</Badge>
                  {#if b.spec.locked}<Badge tone="accent" icon="lock">gesperrt</Badge>{/if}
                  {#if b.requires}<Badge tone="info">optional</Badge>{/if}
                </span>
                <span class="text-fg-muted">{b.note}</span>
              </li>
            {/each}
            {#if p.logging}
              <li class="flex flex-wrap items-center gap-1.5">
                <Badge tone="warning">Payload-Logging ≤ {p.logging.payloadModeMax}</Badge>
              </li>
            {/if}
          </ul>
          <details class="mt-3 text-sm" data-testid={`profile-preview-${p.name}`}>
            <summary class="cursor-pointer text-fg-muted hover:text-fg">Vorschau der Bindungen (Specs)</summary>
            <pre class="scrollbar-thin mt-2 max-h-72 overflow-auto rounded-md bg-surface-2 p-2 font-mono text-xs">{JSON.stringify(
                p.bindings.map((b) => b.spec),
                null,
                2,
              )}</pre>
          </details>
          <div class="mt-4">
            <Button variant="primary" size="sm" icon="shield" onclick={() => start(p)} data-testid={`apply-${p.name}`}>Anwenden auf …</Button>
          </div>
        </Card>
      {/each}
    </div>
  {/snippet}
</AsyncState>

<FormDialog
  bind:open
  title={profile ? `Profil „${profile.title}“ anwenden` : 'Profil anwenden'}
  description="Erst wird der Diff angezeigt, dann angewendet. Bindungen heißen <profil>-<plugin>-<ebene>-<ziel>; Bindungen eines anderen Profils auf demselben Ziel werden ersetzt, eigene Bindungen bleiben."
  size="lg"
  {busy}
  {error}
  submitLabel={preview ? (pending ? `Anwenden (${pending})` : 'Anwenden') : 'Diff anzeigen'}
  onsubmit={submit}
  testid="profile-apply"
>
  <Field label="Ebene">
    {#snippet children({ id })}<Select {id} bind:value={level} items={LEVELS} />{/snippet}
  </Field>
  {#if level !== 'global'}
    <Field label="Ziel" required hint="Name der Organisation, des Teams, Keys bzw. der Modellgruppe.">
      {#snippet children({ id, describedby })}<Input {id} bind:value={target} aria-describedby={describedby} list="profile-targets" autocomplete="off" />{/snippet}
    </Field>
    <datalist id="profile-targets">{#each targets as t (t)}<option value={t}></option>{/each}</datalist>
  {/if}
  {#if profile?.logging}
    <Field label="Payload-Logging begrenzen" hint={`payloadModeMax ≤ ${profile.logging.payloadModeMax} an Organisation bzw. Team (nur absenken)`}>
      {#snippet children({ id })}<Select {id} value={logging ? 'true' : 'false'} onchange={(v) => (logging = v === 'true')} items={[{ value: 'true', label: 'ja' }, { value: 'false', label: 'nein' }]} />{/snippet}
    </Field>
  {/if}
  {#if needsCs}
    <Field label="Externe Inhaltsprüfung" hint="content-safety; ohne Angabe wird die Bindung ausgelassen">
      {#snippet children({ id })}<Select
          {id}
          bind:value={cs.provider}
          items={[
            { value: '', label: 'ohne' },
            { value: 'azure', label: 'Azure AI Content Safety' },
            { value: 'openai', label: 'OpenAI Moderation' },
            { value: 'model', label: 'Prüfmodell (Modellgruppe)' },
          ]}
        />{/snippet}
    </Field>
    {#if cs.provider === 'model'}
      <Field label="Modellgruppe" hint="z. B. ein Llama-Guard-Modell; auch über Foundry mit Managed Identity (ohne API-Key)">
        {#snippet children({ id })}<Input {id} bind:value={cs.modelGroup} autocomplete="off" />{/snippet}
      </Field>
    {:else if cs.provider}
      <Field label="Endpunkt" hint="https://…">
        {#snippet children({ id })}<Input {id} bind:value={cs.endpoint} autocomplete="off" placeholder="https://acme.cognitiveservices.azure.com" />{/snippet}
      </Field>
      {#if cs.provider === 'azure'}
        <Field label="Zugang" hint="Entra ID: Managed Identity, Workload Identity oder Service Principal statt API-Key">
          {#snippet children({ id })}<Select
              {id}
              bind:value={cs.auth}
              items={[
                { value: 'apiKey', label: 'Secret (API-Key)' },
                { value: 'entraId', label: 'Entra ID (Anbieter-Zugangsdaten)' },
              ]}
            />{/snippet}
        </Field>
      {/if}
      {#if cs.provider === 'azure' && cs.auth === 'entraId'}
        <Field label="Zugangsdaten (Entra)" hint="Name einer Anbieter-Zugangsdaten mit Entra-Anmeldung (spec.entra; nur globaler admin)">
          {#snippet children({ id })}<Input {id} bind:value={cs.credential} autocomplete="off" />{/snippet}
        </Field>
      {:else}
        <Field label="Secret" hint="Name eines Secret mit dem API-Key (nur globaler admin)">
          {#snippet children({ id })}<Input {id} bind:value={cs.secret} autocomplete="off" />{/snippet}
        </Field>
      {/if}
    {/if}
  {/if}
  {#if preview}
    <div class="sm:col-span-2" data-testid="profile-diff">
      <h3 class="mb-2 text-sm font-medium">Diff</h3>
      <ul class="divide-y divide-line rounded-md border border-line text-sm">
        {#each preview.changes as c (c.kind + c.name)}
          <li class="flex flex-wrap items-center gap-2 px-3 py-2">
            <Badge tone={ACTION[c.action].tone}>{ACTION[c.action].label}</Badge>
            <span class="font-mono">{c.name}</span>
            {#if c.plugin}<Badge mono tone="outline">{c.plugin}</Badge>{:else}<Badge tone="outline">{c.kind === 'team' ? 'Team' : 'Organisation'} · Payload-Logging</Badge>{/if}
            {#if c.kind !== 'plugin-binding' && c.after}<span class="text-xs text-fg-muted">→ {JSON.stringify(c.after)}</span>{/if}
          </li>
        {/each}
      </ul>
      {#each preview.warnings as w (w)}
        <p class="mt-2 rounded-md border border-warning-fg/30 bg-warning-bg px-3 py-2 text-sm text-warning-fg" role="status">{w}</p>
      {/each}
    </div>
  {/if}
</FormDialog>
