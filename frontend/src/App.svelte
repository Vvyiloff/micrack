<script lang="ts">
  import { onMount } from 'svelte';
  import { fly } from 'svelte/transition';
  import { store } from './lib/stores.svelte';
  import TransportBar from './lib/components/TransportBar.svelte';
  import ChainRack from './lib/components/ChainRack.svelte';
  import DevicePanel from './lib/components/DevicePanel.svelte';
  import PluginBrowser from './lib/components/PluginBrowser.svelte';
  import PresetBar from './lib/components/PresetBar.svelte';

  onMount(() => {
    store.init();
  });

  const tone = (kind: string) =>
    kind === 'error'
      ? 'border-clip/40 text-clip'
      : kind === 'warn'
        ? 'border-warn/40 text-warn'
        : 'border-line text-txt';
</script>

<div class="flex h-screen min-w-[1000px] flex-col">
  <TransportBar />
  <main class="flex min-h-0 flex-1">
    <ChainRack />
    <aside class="flex w-[340px] shrink-0 flex-col gap-3 border-l border-line bg-panel/40 p-3">
      <PresetBar />
      <div class="panel flex items-center gap-2 p-2.5">
        <span class="label ml-1">Автозапуск</span>
        <button
          class={store.autostart ? 'btn ml-auto border-ok/40 bg-ok/10 text-ok' : 'btn ml-auto'}
          onclick={() => store.toggleAutostart()}
          title="Запускать micrack при входе в Windows"
        >
          {store.autostart ? 'Включён' : 'Выключен'}
        </button>
      </div>
      <DevicePanel />
      <PluginBrowser />
    </aside>
  </main>

  <div class="pointer-events-none fixed bottom-4 left-4 z-50 flex flex-col gap-2">
    {#each store.notices as n (n.id)}
      <div
        class="panel pointer-events-auto max-w-md px-3 py-2 text-xs shadow-lg backdrop-blur {tone(n.kind)}"
        transition:fly={{ x: -16, duration: 180 }}
      >
        {n.message}
      </div>
    {/each}
  </div>
</div>
