<script lang="ts">
  let {
    value = $bindable(0),
    min = -24,
    max = 12,
    reset = 0,
    label = '',
    size = 64,
    onchange,
  }: {
    value?: number;
    min?: number;
    max?: number;
    reset?: number;
    label?: string;
    size?: number;
    onchange?: (v: number) => void;
  } = $props();

  let dragging = $state(false);
  let startY = 0;
  let startV = 0;

  const clamp = (v: number) => Math.min(max, Math.max(min, v));
  const set = (v: number) => {
    value = Math.round(clamp(v) * 10) / 10;
    onchange?.(value);
  };

  function pointerdown(e: PointerEvent) {
    (e.currentTarget as Element).setPointerCapture(e.pointerId);
    dragging = true;
    startY = e.clientY;
    startV = value;
  }
  function pointermove(e: PointerEvent) {
    if (!dragging) return;
    const fine = e.shiftKey ? 0.15 : 1;
    set(startV + ((startY - e.clientY) * (max - min) * fine) / 150);
  }
  function pointerup() {
    dragging = false;
  }
  function keydown(e: KeyboardEvent) {
    const step = e.shiftKey ? 0.1 : 0.5;
    if (e.key === 'ArrowUp' || e.key === 'ArrowRight') set(value + step);
    else if (e.key === 'ArrowDown' || e.key === 'ArrowLeft') set(value - step);
    else if (e.key === 'Home' || e.key === '0') set(reset);
    else return;
    e.preventDefault();
  }

  const A0 = -135;
  const SWEEP = 270;
  const frac = $derived((value - min) / (max - min));
  const ang = $derived(A0 + SWEEP * frac);
  const pt = (a: number, r: number) => {
    const rad = (a * Math.PI) / 180;
    return [50 + r * Math.sin(rad), 50 - r * Math.cos(rad)];
  };
  const arc = (from: number, to: number, r: number) => {
    const [sx, sy] = pt(from, r);
    const [ex, ey] = pt(to, r);
    return `M ${sx.toFixed(2)} ${sy.toFixed(2)} A ${r} ${r} 0 ${to - from > 180 ? 1 : 0} 1 ${ex.toFixed(2)} ${ey.toFixed(2)}`;
  };
  const track = arc(A0, A0 + SWEEP, 40);
  const valueArc = $derived(frac > 0.004 ? arc(A0, ang, 40) : '');
  const needle = $derived({ a: pt(ang, 22), b: pt(ang, 31) });
  const text = $derived(`${value > 0 ? '+' : ''}${value.toFixed(1)}`);
</script>

<div class="flex flex-col items-center gap-1" style="width: {size}px">
  <svg
    viewBox="0 0 100 100"
    width={size}
    height={size}
    role="slider"
    tabindex="0"
    aria-label={label || 'усиление'}
    aria-valuemin={min}
    aria-valuemax={max}
    aria-valuenow={value}
    aria-orientation="vertical"
    class="cursor-ns-resize touch-none rounded-full outline-none transition focus-visible:ring-2 focus-visible:ring-accent/50 {dragging ? 'brightness-110' : ''}"
    onpointerdown={pointerdown}
    onpointermove={pointermove}
    onpointerup={pointerup}
    onpointercancel={pointerup}
    ondblclick={() => set(reset)}
    onkeydown={keydown}
  >
    <circle cx="50" cy="50" r="34" fill="#1e2128" stroke="#2a2e37" stroke-width="1" />
    <path d={track} fill="none" stroke="#2a2e37" stroke-width="6" stroke-linecap="round" />
    {#if valueArc}
      <path
        d={valueArc}
        fill="none"
        stroke="#ff8a3d"
        stroke-width="6"
        stroke-linecap="round"
        style="filter: drop-shadow(0 0 4px rgba(255,138,61,0.55))"
      />
    {/if}
    <line
      x1={needle.a[0]}
      y1={needle.a[1]}
      x2={needle.b[0]}
      y2={needle.b[1]}
      stroke="#d6d9e0"
      stroke-width="3"
      stroke-linecap="round"
    />
    <text x="50" y="52" text-anchor="middle" fill="#d6d9e0" font-size="14" font-family="var(--font-mono)">{text}</text>
    <text x="50" y="65" text-anchor="middle" fill="#8a90a0" font-size="9">dB</text>
  </svg>
  {#if label}<span class="label">{label}</span>{/if}
</div>