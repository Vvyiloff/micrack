<script lang="ts">
  import { onMount } from 'svelte';
  import { fly } from 'svelte/transition';
  import { store } from './lib/stores.svelte';
  import * as ipc from './lib/ipc';
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
  {#if store.cable && !store.cable.cable_input && !store.cable.voicemeeter && store.cfg.input.host === 'wasapi'}
    <div class="flex items-center gap-3 border-b border-warn/40 bg-warn/10 px-4 py-2 text-xs text-warn">
      <span>Чтобы обработанный голос дошёл до Discord/OBS, нужен виртуальный кабель. Установите бесплатный VB-CABLE — micrack заметит его сам и переключит вывод.</span>
      <button class="btn ml-auto shrink-0 border-warn/40" onclick={() => ipc.openCableDownload()}>Скачать VB-CABLE</button>
      <button class="btn shrink-0" onclick={() => store.checkCable()}>Проверить снова</button>
    </div>
  {/if}
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
