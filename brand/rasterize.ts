import { COLORS, LOCKUP, MARK, markPath, WORDMARK } from "./geometry.ts";

/**
 * A page that draws the PNG and ICO files from the same geometry on a canvas and downloads them.
 * `window.rasterize()` returns them as base64 too, for scripts.
 */
export const renderRasterizer = () => {
  const data = JSON.stringify({
    mark: markPath(),
    word: WORDMARK.d,
    colors: COLORS,
    lockup: LOCKUP,
    stroke: MARK.stroke,
    small: MARK.smallStroke,
  });
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Norbelys · Rasterize</title>
<link rel="preconnect" href="https://fonts.googleapis.com"><link href="https://fonts.googleapis.com/css2?family=Outfit:wght@600&family=Inter:wght@400;500&display=swap" rel="stylesheet">
<style>body{background:#000;color:#f2f2f2;font:14px/22px Inter,sans-serif;padding:32px}button{font:600 13px Inter;padding:8px 14px;border-radius:3px;border:0;background:#f2f2f2;cursor:pointer}button:hover{background:#f472b6}#out{display:flex;flex-wrap:wrap;gap:16px;margin-top:24px}figure{margin:0;font-size:12px;color:#909192}canvas{display:block;background:repeating-conic-gradient(#1a1a1a 0 25%,#111 0 50%) 0 0/16px 16px;max-width:360px;height:auto}</style></head>
<body><h1>Rasterize the Norbelys files</h1><p>Every PNG and the favicon ICO, drawn from the same geometry as the SVGs.</p><button id="all">Download all</button><div id="out"></div>
<script>
const B = ${data};
const mark = new Path2D(B.mark), word = new Path2D(B.word);
const draw = {
  /** A mark of side s at (x, y), with the line scaled from the 64 grid. */
  mark(ctx, x, y, s, color, stroke) { ctx.save(); ctx.translate(x, y); ctx.scale(s / 64, s / 64); ctx.strokeStyle = color; ctx.lineWidth = stroke; ctx.lineCap = "round"; ctx.lineJoin = "round"; ctx.stroke(mark); ctx.restore(); },
  lockup(ctx, x, y, h, markColor, wordColor) { const k = h / B.lockup.height; draw.mark(ctx, x, y, 80 * k, markColor, B.stroke); ctx.save(); ctx.translate(x + B.lockup.wordX * k, y); ctx.scale(k, k); ctx.fillStyle = wordColor; ctx.fill(word); ctx.restore(); },
  glow(ctx, w, h, x, y, r, alpha) { const g = ctx.createRadialGradient(x, y, 0, x, y, r); g.addColorStop(0, "rgba(244,114,182," + alpha + ")"); g.addColorStop(1, "rgba(244,114,182,0)"); ctx.fillStyle = g; ctx.fillRect(0, 0, w, h); },
};
const icon = (size, stroke) => (ctx) => draw.mark(ctx, 0, 0, size, B.colors.pink, stroke);
const tile = (size, inset) => (ctx) => { ctx.fillStyle = "#000"; ctx.fillRect(0, 0, size, size); draw.mark(ctx, size * inset, size * inset, size * (1 - 2 * inset), B.colors.pink, B.stroke); };
const social = (w, h, headline, sub) => (ctx) => {
  ctx.fillStyle = "#000"; ctx.fillRect(0, 0, w, h);
  draw.glow(ctx, w, h, w * 0.86, h * 0.05, h * 1.1, 0.26);
  ctx.globalAlpha = 0.16; draw.mark(ctx, w - h * 0.78, -h * 0.06, h * 0.92, B.colors.pink, B.stroke); ctx.globalAlpha = 1;
  const pad = Math.round(h * 0.12);
  draw.lockup(ctx, pad, pad, Math.round(h * 0.1), B.colors.pink, "#fff");
  if (headline) { ctx.fillStyle = "#fff"; ctx.font = "600 " + Math.round(h * 0.1) + "px Outfit"; ctx.textBaseline = "alphabetic"; const lines = headline.split("\\n"); lines.forEach((t, i) => ctx.fillText(t, pad, h - pad - Math.round(h * 0.075) - (lines.length - 1 - i) * Math.round(h * 0.118))); }
  if (sub) { ctx.fillStyle = "#a1a1a1"; ctx.font = "400 " + Math.round(h * 0.034) + "px Inter"; ctx.fillText(sub, pad, h - pad + Math.round(h * 0.005)); }
};
const SUB = "Open-source outbound email · API-first";
const FILES = [
  ["icons/favicon-16.png", 16, 16, icon(16, B.small)], ["icons/favicon-32.png", 32, 32, icon(32, B.small)], ["icons/favicon-48.png", 48, 48, icon(48, B.small)],
  ["icons/apple-touch-icon.png", 180, 180, tile(180, 0.1875)], ["icons/icon-192.png", 192, 192, tile(192, 0.1875)], ["icons/icon-512.png", 512, 512, tile(512, 0.1875)], ["icons/maskable-512.png", 512, 512, tile(512, 0.25)],
  ["email-mark.png", 96, 96, (ctx) => draw.mark(ctx, 0, 0, 96, B.colors.deep, B.stroke)],
  ["social/og.png", 1200, 630, social(1200, 630, "Nothing sends until\\nyou approve it.", SUB)],
  ["social/github-preview.png", 1280, 640, social(1280, 640, "Open-source outbound\\nemail, API-first.", "github.com/eusoumaxi/norbelys")],
  ["social/banner.png", 1500, 500, social(1500, 500, "Nothing sends until you approve it.", SUB)],
  ["social/avatar.png", 400, 400, tile(400, 0.1875)],
];
const render = ([name, w, h, paint]) => { const c = document.createElement("canvas"); c.width = w; c.height = h; paint(c.getContext("2d")); return [name, c]; };
const ico = async (pngs) => { const n = pngs.length; const head = new DataView(new ArrayBuffer(6 + 16 * n)); head.setUint16(2, 1, true); head.setUint16(4, n, true); let offset = 6 + 16 * n; const bodies = []; for (let i = 0; i < n; i++) { const [size, blob] = pngs[i]; const bytes = new Uint8Array(await blob.arrayBuffer()); const e = 6 + 16 * i; head.setUint8(e, size % 256); head.setUint8(e + 1, size % 256); head.setUint16(e + 4, 1, true); head.setUint16(e + 6, 32, true); head.setUint32(e + 8, bytes.length, true); head.setUint32(e + 12, offset, true); offset += bytes.length; bodies.push(bytes); } return new Blob([head, ...bodies], { type: "image/x-icon" }); };
const blobOf = (canvas) => new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
const b64 = async (blob) => { const bytes = new Uint8Array(await blob.arrayBuffer()); let s = ""; for (const b of bytes) s += String.fromCharCode(b); return btoa(s); };
async function drawAll() {
  await document.fonts.load("600 64px Outfit"); await document.fonts.load("400 20px Inter");
  const out = {}; const canvases = FILES.map(render);
  for (const [name, canvas] of canvases) out[name] = await blobOf(canvas);
  out["icons/favicon.ico"] = await ico([[16, out["icons/favicon-16.png"]], [32, out["icons/favicon-32.png"]], [48, out["icons/favicon-48.png"]]]);
  const shown = document.getElementById("out"); shown.replaceChildren(...canvases.map(([name, canvas]) => { const f = document.createElement("figure"); f.append(canvas, Object.assign(document.createElement("figcaption"), { textContent: name + " · " + canvas.width + "×" + canvas.height })); return f; }));
  return out;
}
window.rasterize = async () => { const out = await drawAll(); const encoded = {}; for (const [name, blob] of Object.entries(out)) encoded[name] = await b64(blob); return encoded; };
document.getElementById("all").addEventListener("click", async () => { const out = await drawAll(); for (const [name, blob] of Object.entries(out)) { const a = document.createElement("a"); a.href = URL.createObjectURL(blob); a.download = name.replaceAll("/", "-"); a.click(); } });
drawAll();
</script></body></html>`;
};
