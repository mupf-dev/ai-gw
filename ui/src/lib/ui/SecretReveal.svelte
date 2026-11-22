<!-- SPDX-FileCopyrightText: 2026 mupf-dev -->
<!-- SPDX-License-Identifier: FSL-1.1-ALv2 -->
<script lang="ts">
  // Einmalige Klartext-Anzeige eines Secrets (API-Key): Warnhinweis, Kopieren, Verbergen, Bestätigung.
  // Der Klartext lebt nur in dieser Komponente; nach dem Schließen ist er weg (Server speichert nur den Hash).
  import { Checkbox } from 'bits-ui';
  import CopyButton from './CopyButton.svelte';
  import Icon from './Icon.svelte';
  import { btn } from './styles';

  let { secret, confirmed = $bindable(false) }: { secret: string; confirmed?: boolean } = $props();
  let visible = $state(true);
  let copied = $state(false);
  const masked = $derived(`${secret.slice(0, 6)}${'•'.repeat(Math.max(0, secret.length - 10))}${secret.slice(-4)}`);
</script>

<div class="flex flex-col gap-3" data-testid="secret-reveal">
  <div class="flex gap-2.5 rounded-lg bg-warning-bg p-3 text-sm text-warning-fg" role="note">
    <Icon name="alert" class="mt-0.5" />
    <p>
      <strong class="font-semibold">Nur jetzt sichtbar.</strong> Der Key wird nicht im Klartext gespeichert und kann später nicht erneut
      angezeigt werden. Bei Verlust: Key rotieren.
    </p>
  </div>

  <div class="scheme-dark flex items-center gap-1 rounded-lg border border-code-line bg-code py-1 pr-1 pl-3">
    <Icon name="key" size={14} class="text-code-muted" />
    <code class="min-w-0 flex-1 truncate font-mono text-sm text-code-fg" data-testid="secret-value">{visible ? secret : masked}</code>
    <button type="button" class={btn('ghost', 'icon-sm')} aria-label={visible ? 'Key verbergen' : 'Key anzeigen'} onclick={() => (visible = !visible)}>
      <Icon name={visible ? 'eye-off' : 'eye'} size={14} />
    </button>
    <CopyButton text={secret} label="Key kopieren" variant="primary" showLabel oncopy={() => (copied = true)} />
  </div>

  <label class="flex items-center gap-2 text-sm text-fg">
    <Checkbox.Root
      bind:checked={confirmed}
      class="grid size-4 shrink-0 place-items-center rounded-sm border border-line-strong bg-surface data-[state=checked]:border-accent data-[state=checked]:bg-accent data-[state=checked]:text-on-accent"
    >
      {#snippet children({ checked })}
        {#if checked}<Icon name="check" size={12} />{/if}
      {/snippet}
    </Checkbox.Root>
    Ich habe den Key sicher gespeichert{copied ? '' : ' (z. B. im Secret-Manager)'}.
  </label>
</div>
