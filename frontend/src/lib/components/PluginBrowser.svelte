<script lang="ts">
  import { store } from '../stores.svelte';

  let q = $state('');
  let showFolders = $state(false);
  const filtered = $derived(
    store.plugins.filter((p) =>
      `${p.name} ${p.vendor}`.toLowerCase().includes(q.trim().toLowerCase()),
    ),
  );
  const scan = $derived(store.telemetry?.scan);
</script>

<div class="panel flex min-h-0 flex-1 flex-col">
  <div class="flex items-center gap-2 border-b border-line p-3 pb-2.5">
    <span class="label">Плагины</span>
    <span class="font-mono text-[10px] text-dim">{store.plugins.length}</span>
    <button class="btn ml-auto px-2 py-1" onclick={() => (showFolders = !showFolders)} title="Папки плагинов">
      Папки
    </button>
    <button class="btn px-2 py-1" onclick={() => store.scan()} disabled={store.scanning}>
      {store.scanning ? 'Сканирование…' : 'Пересканировать'}
    </button>
  </div>

  <div class="p-3 pb-2">
    <input class="input" placeholder="Поиск по названию или разработчику…" bind:value={q} />
  </div>

  {#if showFolders}
    <div class="border-b border-line px-3 pb-2">
      <div class="mb-1 flex items-center justify-between">
        <span class="label">Папки сканирования</span>
        <button class="btn px-2 py-0.5 text-[10px]" onclick={() => store.addScanDir()}>+ Добавить</button>
      </div>
      <div class="flex flex-col gap-1">
        {#each store.scanDirs as d (d)}
          <div class="flex items-center gap-1 rounded bg-raised px-2 py-1">
            <span class="min-w-0 flex-1 truncate text-[10px] text-dim" title={d}>{d}</span>
            <button
              class="iconbtn size-4 text-clip/70 hover:text-clip"
              onclick={() => store.removeScanDir(d)}
              title="Удалить папку"
            >×</button>
          </div>
        {:else}
          <span class="text-[10px] text-dim">Только папки по умолчанию.</span>
        {/each}
      </div>
    </div>
  {/if}

  {#if scan}
    <div class="px-3 pb-2">
      <div class="h-1 overflow-hidden rounded-full bg-raised">
        <div
          class="h-full rounded-full bg-accent transition-all duration-200"
          style="width: {(100 * scan.done) / Math.max(1, scan.total)}%"
        ></div>
      </div>
      <p class="mt-1 truncate text-[10px] text-dim">{scan.current} ({scan.done}/{scan.total})</p>
    </div>
  {/if}

  <div class="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
    {#each filtered as p (p.id)}
      <div class="group flex items-center gap-2 rounded-lg px-2 py-2 transition hover:bg-raised">
        <div class="min-w-0 flex-1">
          <div class="truncate text-xs font-medium">{p.name}</div>
          <div class="truncate text-[10px] text-dim">{p.vendor}</div>
        </div>
        <span class="badge {p.format === 'vst3' ? 'bg-accent/15 text-accent' : 'bg-[#5b8def]/15 text-[#7fa7f5]'}">
          {p.format.toUpperCase()}
        </span>
        <button
          class="iconbtn border border-line text-accent opacity-60 transition group-hover:opacity-100 hover:border-accent/50 hover:bg-accent/10 hover:text-accent"
          onclick={() => store.add(p.id)}
          title="Добавить в цепочку"
        >
          <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
        </button>
      </div>
    {:else}
      <p class="p-3 text-xs text-dim">Нет плагинов по запросу "{q}".</p>
    {/each}
  </div>
</div>