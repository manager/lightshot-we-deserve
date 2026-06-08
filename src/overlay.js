const { invoke } = window.__TAURI__.core;

const canvas = document.getElementById("c");
const ctx = canvas.getContext("2d");
const hint = document.getElementById("hint");

let dpr = window.devicePixelRatio || 1;
let start = null;
let cur = null;
let dragging = false;
let done = false;

function resize() {
  dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(window.innerWidth * dpr);
  canvas.height = Math.round(window.innerHeight * dpr);
  canvas.style.width = window.innerWidth + "px";
  canvas.style.height = window.innerHeight + "px";
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  draw();
}

function rect() {
  const x = Math.min(start.x, cur.x);
  const y = Math.min(start.y, cur.y);
  return { x, y, w: Math.abs(cur.x - start.x), h: Math.abs(cur.y - start.y) };
}

function phys(r) {
  return {
    x: Math.round(r.x * dpr),
    y: Math.round(r.y * dpr),
    w: Math.round(r.w * dpr),
    h: Math.round(r.h * dpr),
  };
}

function draw() {
  ctx.clearRect(0, 0, window.innerWidth, window.innerHeight);
  ctx.fillStyle = "rgba(0,0,0,0.35)";
  ctx.fillRect(0, 0, window.innerWidth, window.innerHeight);
  if (start && cur) {
    const r = rect();
    ctx.clearRect(r.x, r.y, r.w, r.h);
    ctx.strokeStyle = "#2ec8c8";
    ctx.lineWidth = 1.5;
    ctx.strokeRect(r.x + 0.5, r.y + 0.5, r.w, r.h);
    const p = phys(r);
    ctx.fillStyle = "#2ec8c8";
    ctx.font = "12px system-ui, sans-serif";
    ctx.fillText(`${p.w} × ${p.h}`, r.x, Math.max(12, r.y - 4));
  }
}

function cancel() {
  if (done) return;
  done = true;
  invoke("cancel_area");
}

window.addEventListener("mousedown", (e) => {
  if (done) return;
  dragging = true;
  start = { x: e.clientX, y: e.clientY };
  cur = { ...start };
  hint.style.display = "none";
  draw();
});

window.addEventListener("mousemove", (e) => {
  if (!dragging) return;
  cur = { x: e.clientX, y: e.clientY };
  draw();
});

window.addEventListener("mouseup", (e) => {
  if (!dragging || done) return;
  dragging = false;
  cur = { x: e.clientX, y: e.clientY };
  const r = rect();
  if (r.w < 3 || r.h < 3) {
    cancel();
    return;
  }
  const p = phys(r);
  done = true;
  invoke("area_selected", { x: p.x, y: p.y, width: p.w, height: p.h }).catch(() => {});
});

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") cancel();
});

window.addEventListener("resize", resize);
resize();
