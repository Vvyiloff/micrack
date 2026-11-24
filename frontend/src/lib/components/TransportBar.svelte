<script lang="ts">
  import { store } from '../stores.svelte';
  import StatusDot from './StatusDot.svelte';

  const s = $derived(store.status);
  const t = $derived(store.telemetry);
  const running = $derived(s?.running ?? false);
  const xruns = $derived((t?.underruns ?? 0) + (t?.overflows ?? 0));

  const readouts = $derived([
    { label: 'Задержка', value: `${(t?.latency_ms ?? s?.latency_ms ?? 0).toFixed(1)} ms`, tone: 'text-accent' },
    { label: 'Частота', value: `${((s?.sample_rate ?? 48000) / 1000).toFixed(1)} kHz`, tone: 'text-txt' },
    { label: 'Буфер', value: `${s?.block_size ?? 128} smp`, tone: 'text-txt' },
    { label: 'Нагрузка DSP', value: `${(t?.callback_load_pct ?? 0).toFixed(0)} %`, tone: 'text-txt' },
    { label: 'Сбои', value: `${xruns}`, tone: xruns > 0 ? 'text-clip' : 'text-dim' },
  ]);
</script>

<header class="flex items-center gap-4 border-b border-line bg-panel/80 px-4 py-2.5 backdrop-blur">
  <div class="flex items-center gap-2.5">
    <div class="grid size-8 place-items-center rounded-lg border border-accent/30 bg-accent/15 shadow-[0_0_14px_rgba(255,138,61,0.25)]">
      <svg viewBox="0 0 24 24" class="size-4.5" fill="none" stroke="#ff8a3d" stroke-width="2" stroke-linecap="round">
        <rect x="9" y="3" width="6" height="11" rx="3" />
        <path d="M5 11a7 7 0 0 0 14 0M12 18v3" />
      </svg>
    </div>
    <div>
      <div class="text-base font-semibold leading-none tracking-tight">mic<span class="text-accent">rack</span></div>
      <div class="label mt-1">живая обработка микрофона</div>
    </div>
  </div>

  <button
    class={!running
      ? 'btn ml-2 border-accent bg-accent font-semibold text-[#1a0e02] shadow-[0_0_18px_rgba(255,138,61,0.35)] hover:bg-[#ffa25f]'
      : store.processingOff
        ? 'btn ml-2 border-accent/50 text-accent hover:bg-accent/10'
        : 'btn ml-2 border-clip/40 text-clip hover:bg-clip/10'}
    onclick={() => store.toggleProcessing()}
    title="Микрофон продолжает работать: выключается только обработка, звук в Discord/OBS не пропадает"
  >
    {#if !running}
      <svg viewBox="0 0 24 24" class="size-3" fill="currentColor"><path d="M7 4.5v15l13-7.5z" /></svg>
      Запустить
    {:else if store.processingOff}
      <svg viewBox="0 0 24 24" class="size-3" fill="currentColor"><path d="M7 4.5v15l13-7.5z" /></svg>
      Включить обработку
    {:else}
      <svg viewBox="0 0 24 24" class="size-3" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M12 3v9" /><path d="M6.5 6.5a8 8 0 1 0 11 0" /></svg>
      Выключить обработку
    {/if}
  </button>

  <div class="ml-auto flex items-center gap-5">
    {#each readouts as r (r.label)}
      <div class="flex flex-col items-end gap-0.5">
        <span class="label">{r.label}</span>
        <span class="font-mono text-xs leading-none {r.tone}">{r.value}</span>
      </div>
    {/each}
    <div class="flex items-center gap-2 rounded-lg border border-line bg-raised px-3 py-1.5">
      <StatusDot {running} />
      <span class="text-[11px] font-semibold tracking-widest {running ? (store.processingOff ? 'text-warn' : 'text-ok') : 'text-dim'}">
        {running ? (store.processingOff ? 'ОБХОД' : 'РАБОТАЕТ') : 'ОЖИДАНИЕ'}
      </span>
    </div>
  </div>
</header>