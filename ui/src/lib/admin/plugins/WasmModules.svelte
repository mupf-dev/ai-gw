<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // WASM-Module (AIGW-63): Komponenten für die Plugin-Art `wasm`. Liste mit SHA-256, Größe und Verwendung in
  // Bindungen; Hochladen (SHA-256 wird im Browser berechnet und vom Server geprüft) und Löschen (409, solange
  // Bindungen darauf verweisen). Schreiben nur globaler Admin (Katalog).
  import { wasmModules, NAME_HINT, NAME_RE, type WasmModule } from '../../api/resources';
  import AsyncState from '../AsyncState.svelte';
  import ConfirmDialog from '../ConfirmDialog.svelte';
  import { formErrorOf, handleError, Loader } from '../errors.svelte';
  import FormDialog from '../FormDialog.svelte';
  import { ago } from '../../format';
  import { canWrite } from '../../rbac.svelte';
  import { toast } from '../../toast.svelte';
  import Badge from '../../ui/Badge.svelte';
  import Button from '../../ui/Button.svelte';
  import Card from '../../ui/Card.svelte';
  import EmptyState from '../../ui/EmptyState.svelte';
  import Field from '../../ui/Field.svelte';
  import Input from '../../ui/Input.svelte';

  const list = new Loader(() => wasmModules.list());
  const writable = $derived(canWrite({ kind: 'catalog' }));
  const size = (n: number) => (n >= 1024 * 1024 ? `${(n / 1024 / 1024).toFixed(1)} MiB` : `${Math.max(1, Math.round(n / 1024))} KiB`);

  let dlgOpen = $state(false);
  let replace = $state<string | null>(null);
  let f = $state({ name: '', description: '', sha256: '' });
  let file = $state<File | null>(null);
  let busy = $state(false);
  let formError = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});

  function openUpload(name: string | null) {
    replace = name;
    f = { name: name ?? '', description: list.data?.find((m) => m.name === name)?.description ?? '', sha256: '' };
    file = null;
    formError = null;
    errors = {};
    dlgOpen = true;
  }
  async function submit() {
    const e: Record<string, string> = {};
    if (!replace && !NAME_RE.test(f.name)) e.name = 'Ungültiger Name.';
    if (!file) e.file = 'Datei (.wasm) wählen.';
    if (f.sha256.trim() && !/^[0-9a-fA-F]{64}$/.test(f.sha256.trim())) e.sha256 = '64 Hex-Zeichen.';
    errors = e;
    formError = null;
    if (Object.keys(e).length || !file) return;
    busy = true;
    try {
      const m = await wasmModules.upload(f.name.trim(), await file.arrayBuffer(), { sha256: f.sha256, description: f.description, exists: !!replace });
      dlgOpen = false;
      toast('success', replace ? 'WASM-Modul ersetzt' : 'WASM-Modul hochgeladen', `${m.name} · ${m.sha256.slice(0, 12)}…`);
      void list.reload();
    } catch (err) {
      handleError(err, { action: 'Modul hochladen', form: (m) => (formError = m) });
    } finally {
      busy = false;
    }
  }

  let del = $state<WasmModule | null>(null);
  let delOpen = $state(false);
  async function runDelete(): Promise<string | null> {
    if (!del) return null;
    try {
      await wasmModules.remove(del.name);
      toast('success', 'WASM-Modul gelöscht', del.name);
      void list.reload();
      return null;
    } catch (e) {
      return formErrorOf(e, 'Modul löschen');
    }
  }
</script>

<Card flush>
  <div class="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
    <p class="text-sm text-fg-muted">WebAssembly-Komponenten (WIT-Welt <span class="font-mono">aigw:plugin/plugin</span>) für Bindungen der Art <Badge mono>wasm</Badge>. Freigabe nur durch globale Admins.</p>
    {#if writable}<Button class="ml-auto" size="sm" variant="primary" icon="plus" onclick={() => openUpload(null)} data-testid="wasm-upload">Modul hochladen</Button>{/if}
  </div>
  <AsyncState loader={list} rows={2}>
    {#snippet children(items)}
      {#if items.length === 0}
        <EmptyState compact icon="shield" title="Noch keine WASM-Module" text="Beispiel: plugins/keyword-guard (Stichwort-Blocker, Header-Setzer)." />
      {:else}
        <div class="overflow-x-auto">
          <table class="w-full text-left text-base" data-testid="wasm-table">
            <caption class="sr-only">WASM-Module</caption>
            <thead>
              <tr class="border-b border-line bg-surface-2 text-sm text-fg-muted">
                <th class="h-9 px-3 font-medium">Modul</th>
                <th class="h-9 px-3 font-medium">SHA-256</th>
                <th class="hidden h-9 px-3 font-medium md:table-cell">Größe</th>
                <th class="h-9 px-3 font-medium">Verwendet von</th>
                <th class="hidden h-9 px-3 font-medium lg:table-cell">Geändert</th>
                <th class="h-9 px-3 font-medium"><span class="sr-only">Aktionen</span></th>
              </tr>
            </thead>
            <tbody>
              {#each items as m (m.name)}
                <tr class="border-b border-line last:border-0" data-testid="wasm-row">
                  <td class="px-3 py-2">
                    <span class="font-mono text-sm">{m.name}</span>
                    {#if m.description}<span class="block text-xs text-fg-muted">{m.description}</span>{/if}
                  </td>
                  <td class="px-3 py-2 font-mono text-xs" title={m.sha256}>{m.sha256.slice(0, 16)}…</td>
                  <td class="num hidden px-3 py-2 text-sm md:table-cell">{size(m.size)}</td>
                  <td class="px-3 py-2 text-sm">
                    {#if m.usedBy.length}{#each m.usedBy as b (b)}<Badge mono>{b}</Badge> {/each}{:else}<span class="text-fg-muted">–</span>{/if}
                  </td>
                  <td class="hidden px-3 py-2 text-sm text-fg-muted lg:table-cell">{ago(Date.parse(m.updatedAt))} · {m.createdBy}</td>
                  <td class="px-3 py-2 text-right whitespace-nowrap">
                    {#if writable}
                      <Button size="sm" variant="ghost" onclick={() => openUpload(m.name)} aria-label="{m.name} ersetzen">Ersetzen</Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        icon="trash"
                        aria-label="{m.name} löschen"
                        data-testid="wasm-delete-{m.name}"
                        onclick={() => {
                          del = m;
                          delOpen = true;
                        }}>Löschen</Button
                      >
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    {/snippet}
  </AsyncState>
</Card>

<FormDialog bind:open={dlgOpen} title={replace ? `Modul „${replace}“ ersetzen` : 'WASM-Modul hochladen'} {busy} error={formError} submitLabel={replace ? 'Ersetzen' : 'Hochladen'} onsubmit={submit} testid="wasm-form">
  {#if !replace}
    <Field label="Name" required hint={NAME_HINT} error={errors.name}>
      {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.name} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="keyword-guard" />{/snippet}
    </Field>
  {/if}
  <Field label="Datei (.wasm)" required error={errors.file} hint="WebAssembly-Komponente, z. B. aus cargo build --target wasm32-wasip2">
    {#snippet children({ id, describedby })}
      <input
        {id}
        type="file"
        accept=".wasm,application/wasm"
        aria-describedby={describedby}
        class="text-sm"
        data-testid="wasm-file"
        onchange={(e) => {
          file = e.currentTarget.files?.[0] ?? null;
          if (file && !replace && !f.name) f.name = file.name.replace(/\.wasm$/i, '').replaceAll('_', '-').toLowerCase();
        }}
      />
    {/snippet}
  </Field>
  <Field label="SHA-256" optional error={errors.sha256} hint="leer = im Browser berechnen; der Server prüft den Inhalt dagegen">
    {#snippet children({ id, describedby, invalid })}<Input {id} bind:value={f.sha256} aria-describedby={describedby} aria-invalid={invalid || undefined} autocomplete="off" placeholder="64 Hex-Zeichen" />{/snippet}
  </Field>
  <Field label="Beschreibung" optional>
    {#snippet children({ id })}<Input {id} bind:value={f.description} autocomplete="off" />{/snippet}
  </Field>
</FormDialog>

<ConfirmDialog bind:open={delOpen} title="WASM-Modul löschen?" confirmLabel="Löschen" onconfirm={runDelete} testid="wasm-delete-dialog">
  <p>„<span class="font-mono text-fg">{del?.name}</span>“ wird gelöscht. Solange Plugin-Bindungen darauf verweisen, lehnt der Server ab.</p>
</ConfirmDialog>
