<script lang="ts">
  import type { SlotState } from '../types';
  import { store } from '../stores.svelte';
  import Meter from './Meter.svelte';

  let { slot, peak, count }: { slot: SlotState; peak: number; count: number } = $props();
</script>

<div
  class="panel group relative flex items-center gap-3 px-3 py-2.5 transition duration-200 {slot.bypass ? 'opacity-55' : 'hover:border-dim/40'}"
>
  {#if !slot.bypass}
    <span class="absolute bottom-2 left-0 top-2 w-[3px] rounded-full bg-accent shadow-[0_0_10px_rgba(255,138,61,0.5)]"></span>
  {/if}

  <svg viewBox="0 0 10 16" class="size-3 shrink-0 cursor-grab text-dim/40" fill="currentColor" aria-hidden="true">
    <circle cx="2.5" cy="2.5" r="1.4" /><circle cx="7.5" cy="2.5" r="1.4" />
    <circle cx="2.5" cy="8" r="1.4" /><circle cx="7.5" cy="8" r="1.4" />
    <circle cx="2.5" cy="13.5" r="1.4" /><circle cx="7.5" cy="13.5" r="1.4" />
  </svg>

  <span class="w-5 shrink-0 text-center font-mono text-[11px] text-dim">{slot.index + 1}</span>

  <div class="min-w-0 flex-1">
    <div class="flex items-center gap-2">
      <span class="truncate text-sm font-medium">{slot.name}</span>
      <span class="badge {slot.format === 'vst3' ? 'bg-accent/15 text-accent' : 'bg-[#5b8def]/15 text-[#7fa7f5]'}">
        {slot.format.toUpperCase()}
      </span>
      {#if slot.bypass}<span class="badge bg-warn/15 text-warn">ОБХОД</span>{/if}
    </div>
    <div class="mt-1.5 h-1.5 overflow-hidden rounded-full border border-line/50 bg-black/40">
      <Meter {peak} />
    </div>
  </div>

  <div class="flex shrink-0 items-center gap-0.5">
    <button class="iconbtn" disabled={slot.index === 0} onclick={() => store.move(slot.index, slot.index - 1)} title="Вверх">
      <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M6 14l6-6 6 6" /></svg>
    </button>
    <button class="iconbtn" disabled={slot.index === count - 1} onclick={() => store.move(slot.index, slot.index + 1)} title="Вниз">
      <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M6 10l6 6 6-6" /></svg>
    </button>
    <button
      class="iconbtn {slot.bypass ? 'bg-warn/10 text-warn' : ''}"
      onclick={() => store.bypass(slot.index, !slot.bypass)}
      title={slot.bypass ? 'Включить' : 'Обход'}
    >
      <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M12 3v8" /><path d="M6.3 6.3a8 8 0 1 0 11.4 0" /></svg>
    </button>
    <button
      class="iconbtn {slot.editor_open ? 'bg-accent/10 text-accent' : ''}"
      onclick={() => store.editor(slot.index, !slot.editor_open)}
      title={slot.editor_open ? 'Закрыть редактор' : 'Открыть редактор'}
    >
      <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4 8h9M19 8h1M4 16h3M13 16h7" /><circle cx="16" cy="8" r="2.2" /><circle cx="10" cy="16" r="2.2" /></svg>
    </button>
    <button class="iconbtn hover:text-clip" onclick={() => store.remove(slot.index)} title="Удалить">
      <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18" /></svg>
    </button>
  </div>
</div>