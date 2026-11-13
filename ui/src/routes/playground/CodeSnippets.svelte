<!-- SPDX-FileCopyrightText: 2026 ai-gw contributors -->
<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->
<script lang="ts">
  // Code-Beispiele zur aktuellen Playground-Anfrage (curl, OpenAI-SDK, Anthropic-SDK) gegen die Datenebene. Der
  // Key-Klartext ist der UI nie bekannt – Platzhalter $AIGW_URL / $AIGW_API_KEY.
  import { Tabs } from 'bits-ui';
  import CodeBlock from '../../lib/ui/CodeBlock.svelte';
  import Sheet from '../../lib/ui/Sheet.svelte';
  import { tabsList, tabsTrigger } from '../../lib/ui/styles';
  import { snippet, type SnippetLang } from './chat';

  let { open = $bindable(false), body }: { open?: boolean; body: Record<string, unknown> } = $props();
  let lang = $state<SnippetLang>('curl');
  const LANGS: { id: SnippetLang; label: string; code: string }[] = [
    { id: 'curl', label: 'curl', code: 'bash' },
    { id: 'openai', label: 'OpenAI (Python)', code: 'python' },
    { id: 'anthropic', label: 'Anthropic (Python)', code: 'python' },
  ];
</script>

<Sheet bind:open title="Code-Beispiele" description="Dieselbe Anfrage direkt an die Datenebene – mit dem Klartext des Keys in AIGW_API_KEY." size="lg" testid="pg-snippets">
  <div class="flex flex-col gap-3 px-5 py-4">
    <Tabs.Root bind:value={lang}>
      <Tabs.List class={tabsList}>
        {#each LANGS as l (l.id)}<Tabs.Trigger value={l.id} class={tabsTrigger} data-testid="pg-snippet-{l.id}">{l.label}</Tabs.Trigger>{/each}
      </Tabs.List>
      {#each LANGS as l (l.id)}
        <Tabs.Content value={l.id} class="mt-3">
          <CodeBlock code={snippet(l.id, body)} lang={l.code} title={l.label} maxHeight="max-h-[60vh]" />
        </Tabs.Content>
      {/each}
    </Tabs.Root>
    <p class="text-xs text-fg-muted">Anthropic-Format: ai-gw nimmt <code class="font-mono">/v1/messages</code> ebenso an und übersetzt über die IR.</p>
  </div>
</Sheet>
