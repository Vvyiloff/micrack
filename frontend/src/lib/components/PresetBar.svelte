<script lang="ts">
  import { store } from '../stores.svelte';
</script>

<div class="panel flex items-center gap-2 p-2.5">
  <span class="label ml-1">Пресет</span>
  <button class="btn ml-auto" onclick={() => store.savePreset()}>
    <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M4 21h16" /></svg>
    Сохранить
  </button>
  <button class="btn" onclick={() => store.loadPreset()}>
    <svg viewBox="0 0 24 24" class="size-3.5" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 15V3M7 8l5-5 5 5M4 21h16" /></svg>
    Загрузить
  </button>
</div>