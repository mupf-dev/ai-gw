<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Einstellungen: Darstellung (Schema, Akzent zum Vergleich), Datenquelle (Mock-Modus), Über.
  import { RadioGroup, Switch } from 'bits-ui';
  import { ACCENTS, prefs, setAccent, setMock, setTheme, THEMES, type Accent, type Theme } from '../lib/prefs.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Card from '../lib/ui/Card.svelte';
  import Icon, { type IconName } from '../lib/ui/Icon.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { gateway, loadVersion } from '../lib/version.svelte';

  loadVersion();
  const THEME_ICON: Record<Theme, IconName> = { system: 'monitor', light: 'sun', dark: 'moon' };
  const SWATCH: Record<Accent, string> = { violett: 'bg-[#5b45d6]', petrol: 'bg-[#0f6f68]', bernstein: 'bg-[#b45309]' };
  const option =
    'flex items-center gap-3 rounded-lg border border-line bg-surface p-3 text-left hover:border-line-strong data-[state=checked]:border-accent data-[state=checked]:ring-1 data-[state=checked]:ring-accent';
</script>

<PageHeader title="Einstellungen" description="Darstellung gilt pro Browser; Gateway-Einstellungen kommen aus der YAML-Konfiguration (GitOps)." />

<div class="flex max-w-3xl flex-col gap-4">
  <Card title="Farbschema" description="System folgt der Einstellung des Betriebssystems.">
    <RadioGroup.Root value={prefs.theme} onValueChange={(v) => setTheme(v as Theme)} class="grid gap-2 sm:grid-cols-3" aria-label="Farbschema">
      {#each THEMES as t (t.value)}
        <RadioGroup.Item value={t.value} class={option}>
          <span class="grid size-8 place-items-center rounded-md bg-surface-2 text-fg-muted"><Icon name={THEME_ICON[t.value]} /></span>
          <span class="text-base font-medium">{t.label}</span>
        </RadioGroup.Item>
      {/each}
    </RadioGroup.Root>
  </Card>

  <Card title="Akzentfarbe" description="Zum Vergleich umschaltbar, bis die Entscheidung gefallen ist (offene Designfrage).">
    <RadioGroup.Root value={prefs.accent} onValueChange={(v) => setAccent(v as Accent)} class="grid gap-2 sm:grid-cols-3" aria-label="Akzentfarbe" data-testid="accent-picker">
      {#each ACCENTS as a (a.value)}
        <RadioGroup.Item value={a.value} class={option}>
          <span class={['size-8 shrink-0 rounded-md', SWATCH[a.value]]} aria-hidden="true"></span>
          <span class="min-w-0">
            <span class="block text-base font-medium">{a.label}</span>
            <span class="block truncate text-xs text-fg-muted">{a.note}</span>
          </span>
        </RadioGroup.Item>
      {/each}
    </RadioGroup.Root>
  </Card>

  <Card title="Datenquelle">
    <div class="flex items-start gap-4">
      <div class="min-w-0 flex-1">
        <label for="mock-switch" class="text-base font-medium">Mock-Daten verwenden</label>
        <p class="mt-0.5 text-sm text-fg-muted">
          Zeigt deterministische Beispieldaten, solange die Admin-API die Endpunkte noch nicht bereitstellt. Ausgeschaltet zeigen die Seiten nur echte
          Daten (bzw. einen Hinweis).
        </p>
      </div>
      <Switch.Root
        id="mock-switch"
        checked={prefs.mock}
        onCheckedChange={setMock}
        class="mt-0.5 inline-flex h-5 w-9 shrink-0 items-center rounded-full bg-line-strong p-0.5 transition-colors data-[state=checked]:bg-accent"
        data-testid="mock-switch"
      >
        <Switch.Thumb class="size-4 rounded-full bg-surface shadow-card transition-transform data-[state=checked]:translate-x-4" />
      </Switch.Root>
    </div>
  </Card>

  <Card title="Über">
    <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
      <dt class="text-fg-muted">Gateway</dt>
      <dd>{gateway.info?.name ?? 'aigw'}</dd>
      <dt class="text-fg-muted">Version</dt>
      <dd class="num">{gateway.info?.version ?? (gateway.error ? 'nicht erreichbar' : '…')}</dd>
      <dt class="text-fg-muted">Lizenz</dt>
      <dd>AGPL-3.0-or-later</dd>
      <dt class="text-fg-muted">Admin-UI</dt>
      <dd class="flex items-center gap-2">Svelte 5 · Bits UI · Tailwind 4 <Badge tone="outline">CSP strikt</Badge></dd>
    </dl>
  </Card>
</div>
