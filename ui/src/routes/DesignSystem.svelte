<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Komponentenübersicht (Entwicklerseite /admin/design): alle Basiskomponenten und Tokens in einem Blick,
  // dient auch dem CSP-Test (Popover, Dialog, Tooltip, Sheet).
  import { DropdownMenu, Tabs } from 'bits-ui';
  import { toast } from '../lib/toast.svelte';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import CodeBlock from '../lib/ui/CodeBlock.svelte';
  import Dialog from '../lib/ui/Dialog.svelte';
  import EmptyState from '../lib/ui/EmptyState.svelte';
  import Field from '../lib/ui/Field.svelte';
  import Icon from '../lib/ui/Icon.svelte';
  import Input from '../lib/ui/Input.svelte';
  import KpiTile from '../lib/ui/KpiTile.svelte';
  import Meter from '../lib/ui/Meter.svelte';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import SecretReveal from '../lib/ui/SecretReveal.svelte';
  import Select from '../lib/ui/Select.svelte';
  import Sheet from '../lib/ui/Sheet.svelte';
  import Tooltip from '../lib/ui/Tooltip.svelte';
  import { btn, cx, menuItem, menuSeparator, popover, tabsList, tabsTrigger } from '../lib/ui/styles';

  let dialogOpen = $state(false);
  let sheetOpen = $state(false);
  let sel = $state('gpt-4o');
  let name = $state('');
  let tab = $state('a');

  const SWATCHES = [
    ['bg', 'bg-bg'],
    ['surface', 'bg-surface'],
    ['surface-2', 'bg-surface-2'],
    ['surface-hover', 'bg-surface-hover'],
    ['line', 'bg-line'],
    ['fg-muted', 'bg-fg-muted'],
    ['fg', 'bg-fg'],
    ['accent', 'bg-accent'],
    ['accent-soft', 'bg-accent-soft'],
    ['success', 'bg-success'],
    ['warning', 'bg-warning'],
    ['danger', 'bg-danger'],
    ['chart-1', 'bg-chart-1'],
    ['chart-2', 'bg-chart-2'],
    ['chart-3', 'bg-chart-3'],
    ['chart-4', 'bg-chart-4'],
    ['chart-5', 'bg-chart-5'],
    ['chart-6', 'bg-chart-6'],
    ['sidebar', 'bg-sidebar'],
    ['code', 'bg-code'],
  ] as const;
</script>

<PageHeader title="Design-System" description="Bausteine der Admin-UI (AIGW-48). Alle Farben über Tokens, alle interaktiven Teile auf Bits UI." />

<div class="grid gap-4 xl:grid-cols-2">
  <Card title="Farb-Tokens" class="xl:col-span-2">
    <div class="grid grid-cols-4 gap-3 sm:grid-cols-5 lg:grid-cols-10">
      {#each SWATCHES as [label, cls] (label)}
        <div class="flex flex-col gap-1">
          <span class={['h-10 rounded-md border border-line', cls]}></span>
          <span class="truncate font-mono text-2xs text-fg-muted">{label}</span>
        </div>
      {/each}
    </div>
  </Card>

  <Card title="Buttons">
    <div class="flex flex-wrap items-center gap-2">
      <Button variant="primary" icon="plus">Primär</Button>
      <Button variant="secondary">Sekundär</Button>
      <Button variant="ghost">Ghost</Button>
      <Button variant="danger" icon="trash">Löschen</Button>
      <Button variant="link">Link</Button>
      <Button variant="secondary" loading>Speichert</Button>
      <Button variant="secondary" disabled>Deaktiviert</Button>
      <Button variant="secondary" size="icon" aria-label="Einstellungen"><Icon name="settings" /></Button>
      <Button variant="primary" size="sm">Klein</Button>
    </div>
  </Card>

  <Card title="Badges & Status">
    <div class="flex flex-wrap gap-2">
      <Badge>Neutral</Badge><Badge tone="accent">Akzent</Badge><Badge tone="success" dot>Gesund</Badge><Badge tone="warning" dot>Eingeschränkt</Badge>
      <Badge tone="danger" dot>Ausgefallen</Badge><Badge tone="info">Info</Badge><Badge tone="outline">Outline</Badge><Badge mono tone="danger">502</Badge>
    </div>
    <div class="mt-4 flex flex-col gap-2">
      <Meter value={0.42} label="42 %" tone="auto" /><Meter value={0.86} label="86 %" tone="auto" /><Meter value={0.97} label="97 %" tone="auto" />
    </div>
  </Card>

  <Card title="Formular">
    <div class="grid gap-4 sm:grid-cols-2">
      <Field label="Name" required hint="Kleinbuchstaben und Bindestriche">
        {#snippet children({ id, describedby })}<Input {id} bind:value={name} aria-describedby={describedby} placeholder="support-bot" />{/snippet}
      </Field>
      <Field label="Modellgruppe">
        {#snippet children({ id })}
          <Select {id} bind:value={sel} items={[{ value: 'gpt-4o', label: 'gpt-4o', hint: 'Priorität' }, { value: 'claude-sonnet', label: 'claude-sonnet' }, { value: 'llama-3.3-70b', label: 'llama-3.3-70b', hint: 'on-prem' }]} />
        {/snippet}
      </Field>
      <Field label="Budget" error="Budget muss eine positive Zahl sein.">
        {#snippet children({ id, describedby, invalid })}<Input {id} value="-5" suffix="EUR" aria-describedby={describedby} aria-invalid={invalid} />{/snippet}
      </Field>
      <Field label="Suche">
        {#snippet children({ id })}<Input {id} icon="search" placeholder="Suchen …" />{/snippet}
      </Field>
    </div>
  </Card>

  <Card title="Overlays">
    <div class="flex flex-wrap items-center gap-2">
      <Button variant="secondary" onclick={() => (dialogOpen = true)} data-testid="open-dialog">Dialog</Button>
      <Button variant="secondary" onclick={() => (sheetOpen = true)} data-testid="open-sheet">Sheet</Button>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger class={btn('secondary')} data-testid="open-dropdown">Dropdown <Icon name="chevron-down" size={14} /></DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content class={cx(popover, 'w-48')} sideOffset={4} data-testid="dropdown-content">
            <DropdownMenu.Item class={menuItem}><Icon name="pencil" size={14} /> Bearbeiten</DropdownMenu.Item>
            <DropdownMenu.Item class={menuItem}><Icon name="copy" size={14} /> Duplizieren</DropdownMenu.Item>
            <DropdownMenu.Separator class={menuSeparator} />
            <DropdownMenu.Item class={cx(menuItem, 'text-danger-fg')}><Icon name="trash" size={14} /> Löschen</DropdownMenu.Item>
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
      <Tooltip text="Tooltips ergänzen, sie ersetzen keine Beschriftung.">
        {#snippet trigger({ props })}<button {...props} type="button" class={btn('secondary')} data-testid="tooltip-trigger">Tooltip</button>{/snippet}
      </Tooltip>
      <Button variant="secondary" onclick={() => toast('success', 'Gespeichert', 'Änderungen sind aktiv.')} data-testid="open-toast">Toast</Button>
    </div>
    <Tabs.Root bind:value={tab} class="mt-4">
      <Tabs.List class={tabsList}>
        <Tabs.Trigger value="a" class={tabsTrigger}>Erster</Tabs.Trigger>
        <Tabs.Trigger value="b" class={tabsTrigger}>Zweiter</Tabs.Trigger>
      </Tabs.List>
      <Tabs.Content value="a" class="pt-3 text-base text-fg-muted">Inhalt des ersten Tabs.</Tabs.Content>
      <Tabs.Content value="b" class="pt-3 text-base text-fg-muted">Inhalt des zweiten Tabs.</Tabs.Content>
    </Tabs.Root>
  </Card>

  <KpiTile label="Requests" icon="activity" value="1,2 Mio." delta={0.12} trend={[3, 4, 3, 5, 6, 5, 7, 8, 7, 9]} />
  <Card title="Leerer Zustand" flush>
    <EmptyState compact icon="key" title="Noch keine Keys" text="Erstelle den ersten Key.">
      <Button variant="primary" size="sm" icon="plus">Key erstellen</Button>
    </EmptyState>
  </Card>
  <Card title="Code">
    <CodeBlock code={{ model: 'gpt-4o', messages: [{ role: 'user', content: 'Hallo!' }], stream: true, temperature: 0.2 }} />
  </Card>
  <Card title="Einmalige Key-Anzeige">
    <SecretReveal secret="sk-gw-7mQ2xVb9KpL4tR8nW3cY6hJ5dF2s" />
  </Card>
</div>

<Dialog bind:open={dialogOpen} title="Beispieldialog" description="Fokus bleibt im Dialog, Esc schließt." testid="demo-dialog">
  <p class="text-base text-fg-muted">Dialoge für Bestätigungen und kurze Formulare. Längere Abläufe gehören auf eigene Seiten.</p>
  {#snippet footer()}
    <Button variant="secondary" onclick={() => (dialogOpen = false)}>Abbrechen</Button>
    <Button variant="primary" onclick={() => (dialogOpen = false)}>Bestätigen</Button>
  {/snippet}
</Dialog>
<Sheet bind:open={sheetOpen} title="Beispiel-Sheet" description="Detailansichten ohne Seitenwechsel." testid="demo-sheet">
  <div class="p-5 text-base text-fg-muted">Inhalt des Sheets.</div>
</Sheet>
