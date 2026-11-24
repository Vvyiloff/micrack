<script lang="ts">
  import { store, fmtDb } from '../stores.svelte';
  import Meter from './Meter.svelte';
  import Knob from './Knob.svelte';
  import PluginStrip from './PluginStrip.svelte';

  const t = $derived(store.telemetry);
</script>

<section class="flex min-w-0 flex-1 flex-col gap-2.5 overflow-y-auto p-4">
  <div class="panel flex items-center gap-3 px-4 py-2.5">
    <span class="label w-14 shrink-0">Вход</span>
    <div class="h-2.5 min-w-0 flex-1 overflow-hidden rounded-full border border-line/50 bg-black/40">
      <Meter peak={t?.input_peak ?? 0} />
    </div>
    <span class="w-16 shrink-0 text-right font-mono text-[11px] text-dim">{fmtDb(t?.input_peak ?? 0)}</span>
  </div>

  {#each store.chain as slot (`${slot.index}-${slot.name}`)}
    <PluginStrip {slot} peak={t?.slot_peaks[slot.index] ?? 0} count={store.chain.length} />
  {:else}
    <div class="grid flex-1 place-items-center rounded-xl border border-dashed border-line/80">
      <div class="flex flex-col items-center gap-3 py-14 text-center">
        <div class="grid size-12 place-items-center rounded-xl border border-line bg-raised">
          <svg viewBox="0 0 24 24" class="size-6 text-dim" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round">
            <path d="M9 3v5M15 3v5M7 8h10v4a5 5 0 0 1-10 0zM12 17v4" />
          </svg>
        </div>
        <div>
          <p class="text-sm font-medium">Цепочка пуста</p>
          <p class="mt-1 text-xs text-dim">Добавьте плагин из браузера, чтобы начать обработку микрофона.</p>
        </div>
      </div>
    </div>
  {/each}

  <div class="panel mt-auto flex items-center gap-4 border-accent/25 px-4 py-3 shadow-[0_0_24px_rgba(255,138,61,0.06)]">
    <Knob label="Мастер" size={62} value={store.masterDb} onchange={(v) => store.setMasterGain(v)} />
    <div class="min-w-0 flex-1">
      <div class="mb-1.5 flex items-baseline justify-between">
        <span class="label">Выход</span>
        <span class="font-mono text-[11px] text-dim">{fmtDb(t?.output_peak ?? 0)}</span>
      </div>
      <div class="h-4 overflow-hidden rounded-md border border-line/60 bg-black/40">
        <Meter peak={t?.output_peak ?? 0} />
      </div>
    </div>
    <button
      class={store.monitorOn ? 'btn shrink-0 border-ok/40 bg-ok/10 text-ok' : 'btn shrink-0'}
      onclick={() => store.toggleMonitor()}
      title="Прослушать обработанный голос в наушниках"
    >
      <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M4 14v-2a8 8 0 0 1 16 0v2" />
        <rect x="2" y="14" width="5" height="7" rx="1.5" />
        <rect x="17" y="14" width="5" height="7" rx="1.5" />
      </svg>
      {store.monitorOn ? 'Монитор вкл.' : 'Монитор'}
    </button>
  </div>
</section>