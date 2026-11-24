<script lang="ts">
  import { ui } from '../uiActivity.svelte';

  let {
    peak = 0,
    vertical = false,
    class: klass = '',
  }: { peak?: number; vertical?: boolean; class?: string } = $props();

  let canvas!: HTMLCanvasElement;

  const FLOOR = -60;
  const toDb = (p: number) => (p > 0 ? Math.max(FLOOR, 20 * Math.log10(p)) : FLOOR);
  const norm = (db: number) => (db - FLOOR) / -FLOOR;

  $effect(() => {
    // Re-runs on visibility change: minimized window cancels the rAF loop entirely.
    if (!ui.active) return;
    const ctx = canvas.getContext('2d')!;
    let raf = 0;
    let level = FLOOR;
    let hold = FLOOR;
    let holdT = 0;
    let last = performance.now();

    const resize = () => {
      const r = canvas.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      canvas.width = Math.max(1, Math.round(r.width * dpr));
      canvas.height = Math.max(1, Math.round(r.height * dpr));
    };
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    resize();

    const draw = (now: number) => {
      // Cap at ~60 fps: ballistics use real elapsed time, so a 180 Hz display
      // no longer triples the canvas work.
      if (now - last < 15) {
        raf = requestAnimationFrame(draw);
        return;
      }
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      // fast attack, slower release ballistics
      const target = toDb(peak);
      const k = 1 - Math.exp(-dt * (target > level ? 34 : 9));
      level += (target - level) * k;
      if (level >= hold) {
        hold = level;
        holdT = now;
      } else if (now - holdT > 900) {
        hold = Math.max(level, hold - dt * 26);
      }

      const w = canvas.width;
      const h = canvas.height;
      ctx.clearRect(0, 0, w, h);
      ctx.fillStyle = '#0b0d11';
      ctx.fillRect(0, 0, w, h);

      const grad = vertical
        ? ctx.createLinearGradient(0, h, 0, 0)
        : ctx.createLinearGradient(0, 0, w, 0);
      const a = norm(-12);
      const b = norm(-3);
      grad.addColorStop(0, '#27b56a');
      grad.addColorStop(a, '#3ddc84');
      grad.addColorStop(Math.min(1, a + 0.001), '#ffb020');
      grad.addColorStop(b, '#ffb020');
      grad.addColorStop(Math.min(1, b + 0.001), '#ff5555');
      grad.addColorStop(1, '#ff5555');
      ctx.fillStyle = grad;
      const n = norm(level);
      if (vertical) ctx.fillRect(0, h * (1 - n), w, h * n);
      else ctx.fillRect(0, 0, w * n, h);

      // peak-hold line
      const hn = norm(hold);
      const t = Math.max(1.5, (window.devicePixelRatio || 1) * 1.2);
      ctx.fillStyle = hold > -3 ? '#ff7a7a' : hold > -12 ? '#ffc95e' : '#b9f6d6';
      if (vertical) ctx.fillRect(0, Math.max(0, h * (1 - hn) - t), w, t);
      else ctx.fillRect(Math.min(w - t, w * hn - t), 0, t, h);

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  });
</script>

<canvas bind:this={canvas} class="block h-full w-full {klass}"></canvas>