// Near-direct port of PTP.jsx's PTPStatus.drawChart() (webui/src/PTP.jsx:128-207) - a hand-drawn
// canvas sparkline. Kept as plain JS interop rather than a charting library per the migration
// plan's "lightweight" goal - this is the one place in the app real JS interop is unavoidable
// (Blazor has no <canvas> 2D API of its own).
export function drawJitterChart(canvas, history) {
  if (!canvas || !history || history.length < 2) return;

  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth || 380, H = canvas.clientHeight || 90;
  canvas.width = W * dpr; canvas.height = H * dpr;
  const ctx = canvas.getContext('2d');
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.scale(dpr, dpr);

  const isDark = document.documentElement.getAttribute('data-theme') === 'dark' ||
    (!document.documentElement.getAttribute('data-theme') &&
     window.matchMedia('(prefers-color-scheme: dark)').matches);
  const p = isDark
    ? { bg: '#1a1f2e', text: '#9ca3af', grid: 'rgba(255,255,255,0.06)', axis: '#374151', line: '#60a5fa', fill: 'rgba(96,165,250,0.13)' }
    : { bg: '#f3f4f6', text: '#6b7280', grid: 'rgba(0,0,0,0.07)', axis: '#d1d5db', line: '#1967a8', fill: 'rgba(25,103,168,0.10)' };

  ctx.fillStyle = p.bg;
  ctx.fillRect(0, 0, W, H);

  const mL = 46, mR = 6, mT = 7, mB = 18;
  const cW = W - mL - mR, cH = H - mT - mB;

  const min = Math.min(...history), max = Math.max(...history);
  const span = max - min || 1;
  const toY = v => mT + (1 - (v - min) / span) * cH;
  const toX = i => mL + (i / (history.length - 1)) * cW;

  ctx.font = '9px monospace';
  const yTicks = [min, (min + max) / 2, max];
  yTicks.forEach(v => {
    const y = toY(v);
    ctx.strokeStyle = p.grid; ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(mL, y); ctx.lineTo(mL + cW, y); ctx.stroke();
    ctx.strokeStyle = p.axis;
    ctx.beginPath(); ctx.moveTo(mL - 3, y); ctx.lineTo(mL, y); ctx.stroke();
    ctx.fillStyle = p.text; ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
    ctx.fillText(Math.round(v), mL - 6, y);
  });

  const POLL_S = 5;
  const xIdxs = history.length > 4
    ? [0, Math.floor((history.length - 1) / 2), history.length - 1]
    : [0, history.length - 1];
  xIdxs.forEach((idx, i) => {
    const x = toX(idx);
    const ageSec = (history.length - 1 - idx) * POLL_S;
    const label = ageSec === 0 ? 'now' : `-${fmtAge(ageSec)}`;
    ctx.strokeStyle = p.axis; ctx.lineWidth = 1;
    ctx.beginPath(); ctx.moveTo(x, mT + cH); ctx.lineTo(x, mT + cH + 3); ctx.stroke();
    ctx.fillStyle = p.text; ctx.textBaseline = 'top';
    ctx.textAlign = i === 0 ? 'left' : (i === xIdxs.length - 1 ? 'right' : 'center');
    ctx.fillText(label, x, mT + cH + 4);
  });

  ctx.strokeStyle = p.axis; ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(mL, mT); ctx.lineTo(mL, mT + cH); ctx.lineTo(mL + cW, mT + cH);
  ctx.stroke();

  const pts = history.map((v, i) => ({ x: toX(i), y: toY(v) }));
  ctx.beginPath();
  ctx.moveTo(pts[0].x, mT + cH);
  pts.forEach(pt => ctx.lineTo(pt.x, pt.y));
  ctx.lineTo(pts[pts.length - 1].x, mT + cH);
  ctx.closePath();
  ctx.fillStyle = p.fill; ctx.fill();

  ctx.beginPath();
  pts.forEach((pt, i) => i ? ctx.lineTo(pt.x, pt.y) : ctx.moveTo(pt.x, pt.y));
  ctx.strokeStyle = p.line; ctx.lineWidth = 1.5; ctx.lineJoin = 'round';
  ctx.stroke();
}

function fmtAge(sec) {
  if (sec < 60) return `${sec}s`;
  return `${Math.round(sec / 60)}m`;
}
