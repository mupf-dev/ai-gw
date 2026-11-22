<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Einstellungen: Anmeldung (Principal, Verfahren, Rollen, Abmelden), Nur-YAML-Modus, Verweis auf GitOps
  // (YAML-Import/-Export, /config), Datenquelle (Mock-Modus), Darstellung (Schema), Über.
  import { RadioGroup, Switch } from 'bits-ui';
  import { isMock, mockAvailable } from '../lib/mode.svelte';
  import { prefs, setMockMode, setTheme, THEMES, type Theme } from '../lib/prefs.svelte';
  import { auth, bindingLabel, identity, logout, METHOD_LABEL, requestLogin } from '../lib/session.svelte';
  import { dateTime } from '../lib/format';
  import Badge from '../lib/ui/Badge.svelte';
  import Button from '../lib/ui/Button.svelte';
  import Card from '../lib/ui/Card.svelte';
  import Icon, { type IconName } from '../lib/ui/Icon.svelte';
  import { switchRoot, switchThumb } from '../lib/ui/styles';
  import PageHeader from '../lib/ui/PageHeader.svelte';
  import { gateway, loadVersion } from '../lib/version.svelte';
  import { href } from '../lib/router.svelte';

  loadVersion();
  const THEME_ICON: Record<Theme, IconName> = { system: 'monitor', light: 'sun', dark: 'moon' };
  const option =
    'flex items-center gap-3 rounded-lg border border-line bg-surface p-3 text-left hover:border-line-strong data-[state=checked]:border-accent data-[state=checked]:ring-1 data-[state=checked]:ring-accent';
</script>

<PageHeader title="Einstellungen" description="Anmeldung, GitOps und Darstellung. Gateway-Einstellungen kommen aus der YAML-Startkonfiguration; Ressourcen lassen sich per GitOps verwalten." />

<div class="flex max-w-3xl flex-col gap-4">
  <Card title="Anmeldung" testid="session-card">
    {#if auth.session}
      {@const s = auth.session}
      <div class="flex flex-wrap items-start gap-4">
        <span class="grid size-10 place-items-center rounded-full bg-accent-soft text-sm font-semibold text-accent-soft-fg">{identity(s).initials}</span>
        <dl class="grid min-w-0 flex-1 grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
          <dt class="text-fg-muted">Angemeldet als</dt>
          <dd data-testid="session-name">{identity(s).name}{#if s.email}<span class="text-fg-muted"> · {s.email}</span>{/if}</dd>
          <dt class="text-fg-muted">Verfahren</dt>
          <dd>{METHOD_LABEL[s.method] ?? s.method} <span class="font-mono text-xs text-fg-muted">({s.subject})</span></dd>
          <dt class="text-fg-muted">Rollen</dt>
          <dd class="flex flex-wrap gap-1" data-testid="session-roles">
            {#each s.roles as r, i (i)}<Badge tone={r.role === 'admin' && !r.organization && !r.team ? 'accent' : 'neutral'}>{bindingLabel(r)}</Badge>{:else}<span class="text-fg-muted">keine</span>{/each}
          </dd>
          {#if s.expiresAt}<dt class="text-fg-muted">Gültig bis</dt><dd class="num">{dateTime(Date.parse(s.expiresAt))}</dd>{/if}
        </dl>
        <Button icon="logout" onclick={() => void logout()} data-testid="settings-logout">Abmelden</Button>
      </div>
      {#if s.method === 'bootstrap-token'}
        <p class="mt-4 flex gap-2 rounded-md bg-warning-bg px-3 py-2 text-sm text-warning-fg">
          <Icon name="alert" size={14} class="mt-0.5" />
          Bootstrap-Token: Notfall-/Ersteinrichtungszugang. Nach dem ersten globalen Admin per OIDC oder Admin-API-Key wird es abgeschaltet (Modus „auto“).
        </p>
      {/if}
    {:else}
      <div class="flex flex-wrap items-center gap-4">
        <p class="min-w-0 flex-1 text-base text-fg-muted">
          Nicht angemeldet{isMock() ? ' – die Seiten zeigen Mock-Daten' : ''}. {auth.oidc ? 'Anmeldung über den Identitätsanbieter (OIDC).' : 'Ohne OIDC: Bootstrap-Token oder Admin-API-Key.'}
        </p>
        <Button variant="primary" icon="lock" onclick={() => requestLogin()}>Anmelden</Button>
      </div>
    {/if}
  </Card>

  <Card title="Nur-YAML-Modus">
    <div class="flex items-start gap-3">
      <Icon name="file-text" size={18} class="mt-0.5 text-fg-muted" />
      <div class="min-w-0 flex-1 text-sm text-fg-muted">
        <p>
          Mit <code class="font-mono">gitops.read_only_admin: true</code> lehnt die Admin-API alle Änderungen ab (403 „read-only“) – außer GitOps-Apply. Die UI bleibt lesbar; Änderungen
          laufen dann ausschließlich über Manifeste im Repository (<code class="font-mono">aigw config apply</code> bzw. unten).
        </p>
        {#if auth.readOnly}
          <p class="mt-2"><Badge tone="warning" icon="lock">aktiv</Badge> Dieses Gateway hat eine Änderung mit „read-only“ abgelehnt.</p>
        {/if}
      </div>
    </div>
  </Card>

  <Card title="GitOps" description="Konfiguration als YAML-Manifeste (aigw/v1alpha1) exportieren, prüfen und anwenden." testid="gitops-card">
    <div class="flex flex-wrap items-center gap-3">
      <p class="min-w-0 flex-1 text-sm text-fg-muted">Export gesamt oder je Art, Organisation, Team und Ressource; Import mit Trockenlauf, Diff, Fehlern mit Zeilenbezug und Bestätigung.</p>
      <Button href={href('/config')} icon="file-text" data-testid="gitops-open">YAML-Import/-Export öffnen</Button>
    </div>
  </Card>

  {#if mockAvailable()}
  <Card title="Datenquelle">
    <div class="flex items-start gap-4">
      <div class="min-w-0 flex-1">
        <label for="mock-switch" class="text-base font-medium">Mock-Daten verwenden</label>
        <p class="mt-0.5 text-sm text-fg-muted">
          Standardmäßig aus. Eingeschaltet zeigen alle Seiten deterministische Beispieldaten (Demo, Entwicklung) und die UI ist deutlich als
          Mock gekennzeichnet; Änderungen bleiben im Browser. Ohne Mock zeigen alle Seiten die echten Daten der Admin-API. In Produktion
          lässt sich der Schalter mit <code class="font-mono text-xs">admin.ui.mock: false</code> ausblenden.
        </p>
      </div>
      <Switch.Root id="mock-switch" checked={prefs.mockMode === 'on'} onCheckedChange={(c) => setMockMode(c ? 'on' : 'off')} class={switchRoot} data-testid="mock-switch">
        <Switch.Thumb class={switchThumb} />
      </Switch.Root>
    </div>
  </Card>
  {/if}

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

  <Card title="Über">
    <dl class="grid grid-cols-[auto_1fr] gap-x-6 gap-y-2 text-base">
      <dt class="text-fg-muted">Gateway</dt>
      <dd>{gateway.info?.name ?? 'aigw'}</dd>
      <dt class="text-fg-muted">Version</dt>
      <dd class="num">{gateway.info?.version ?? (gateway.error ? 'nicht erreichbar' : '…')}</dd>
      <dt class="text-fg-muted">Lizenz</dt>
      <dd>FSL-1.1-ALv2</dd>
      <dt class="text-fg-muted">Admin-UI</dt>
      <dd class="flex items-center gap-2">Svelte 5 · Bits UI · Tailwind 4 <Badge tone="outline">CSP strikt</Badge></dd>
    </dl>
  </Card>
</div>
