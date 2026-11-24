<script lang="ts">
  import { store } from '../stores.svelte';
  import Knob from './Knob.svelte';

  const rates = [44100, 48000, 96000];
  const blocks = [64, 128, 256, 512];
</script>

<div class="panel flex flex-col gap-2.5 p-3">
  <div class="flex items-center justify-between">
    <span class="label">Аудио вход/выход</span>
    {#if store.status?.running}
      <span class="text-[10px] text-dim">изменения перезапускают движок</span>
    {/if}
  </div>

  <label class="flex flex-col gap-1">
    <span class="label">Хост</span>
    <select class="select" value={store.cfg.input.host} onchange={(e) => store.setHost(e.currentTarget.value)}>
      {#each store.hosts as h (h.id)}
        <option value={h.id}>{h.name}</option>
      {/each}
    </select>
  </label>

  <label class="flex flex-col gap-1">
    <span class="label">Входное устройство</span>
    <select class="select" bind:value={store.cfg.input.device} onchange={() => store.applyConfig()}>
      {#each store.inputDevices as d (d.id)}
        <option value={d.id}>{d.name}{d.is_default ? ' •' : ''}</option>
      {/each}
    </select>
  </label>

  <label class="flex flex-col gap-1">
    <span class="label">Выходное устройство</span>
    <select class="select" bind:value={store.cfg.output.device} onchange={() => store.applyConfig()}>
      {#each store.outputDevices as d (d.id)}
        <option value={d.id}>{d.name}{d.is_default ? ' •' : ''}</option>
      {/each}
    </select>
  </label>

  <div class="grid grid-cols-2 gap-2">
    <label class="flex flex-col gap-1">
      <span class="label">Частота дискретизации</span>
      <select class="select" bind:value={store.cfg.sample_rate} onchange={() => store.applyConfig()}>
        {#each rates as r (r)}
          <option value={r}>{(r / 1000).toFixed(1)} kHz</option>
        {/each}
      </select>
    </label>
    <label class="flex flex-col gap-1">
      <span class="label">Буфер</span>
      <select class="select" bind:value={store.cfg.block_size} onchange={() => store.applyConfig()}>
        {#each blocks as b (b)}
          <option value={b}>{b} smp</option>
        {/each}
      </select>
    </label>
  </div>

  <div class="mt-1 flex items-center gap-3 border-t border-line/60 pt-2.5">
    <Knob label="Вход" size={54} value={store.inputDb} onchange={(v) => store.setInputGain(v)} />
    <p class="text-[11px] leading-relaxed text-dim">
      Усиление входа перед цепочкой.<br />Тяните для настройки, Shift — точно,<br />двойной клик — сброс.
    </p>
  </div>
</div>