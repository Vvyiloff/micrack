<script lang="ts">
  let { running = false }: { running?: boolean } = $props();
</script>

<span class="relative flex size-2.5">
  {#if running}
    <span class="absolute inline-flex h-full w-full animate-ping rounded-full bg-ok opacity-50"></span>
  {/if}
  <span
    class="relative inline-flex size-2.5 rounded-full {running
      ? 'bg-ok shadow-[0_0_8px_#3ddc84]'
      : 'bg-dim/40'}"
  ></span>
</span>