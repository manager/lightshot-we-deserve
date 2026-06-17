const { invoke } = window.__TAURI__.core;
const listen = window.__TAURI__.event && window.__TAURI__.event.listen;

const canvas = document.getElementById("c");
const ctx = canvas.getContext("2d");
const hint = document.getElementById("hint");
const sizeBadge = document.getElementById("sizebadge");
const toolbar = document.getElementById("toolbar");
const actionbar = document.getElementById("actionbar");
const swatchesEl = document.getElementById("swatches");
const sizeVal = document.getElementById("sizeVal");

const COLORS = ["#e7402e", "#e67e22", "#f1c40f", "#2ecc71", "#3498db", "#9b59b6", "#000000", "#ffffff"];

const frozenImg = new Image();
let frozenNatW = 0, frozenNatH = 0;
let cssW = 0, cssH = 0, dpr = 1, sx = 1, sy = 1;

// Pre-rendered static background (frozen image + dim). Built once per resize,
// then blitted each frame instead of rescaling the huge full-desktop image.
const bgCanvas = document.createElement("canvas");
const bgCtx = bgCanvas.getContext("2d");
let bgReady = false;

let sel = null;            // {x,y,w,h} in css px
let tool = null;           // null=select, or pen/line/arrow/rect/marker/text/blur
let color = COLORS[0];
const sizes = { pen: 4, line: 4, arrow: 5, rect: 4, marker: 18, text: 24, blur: 12 };

let committed = [];        // permanent (beyond 5-step undo memory)
let undoable = [];         // up to 5 undoable actions
let current = null;        // in-progress annotation

let selecting = false, drawing = false, selStart = null;
let moving = false, moveStart = null;  // drag the finished selection frame
let lastMouse = { x: 0, y: 0 };  // for placing the size badge on keyboard resize
let textInput = null, textPos = null;
let badgeTimer = null;
let ready = false;

// ---------- frozen image ----------

function clearCanvasHard() {
  ctx.save();
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.restore();
}

async function loadFrozen() {
  ready = false;
  clearCanvasHard();           // wipe any leftover frame from a previous capture
  let shot;
  try { shot = await invoke("get_frozen"); } catch (_) { return; }
  if (!shot) return;
  await new Promise((res) => {
    frozenImg.onload = res;
    frozenImg.onerror = res;
    frozenImg.crossOrigin = "anonymous"; // keep the canvas untainted for export
    frozenImg.src = shot.url;
  });
  frozenNatW = shot.width;
  frozenNatH = shot.height;
  resetState();
  ready = true;
  resize();
  // Frame is painted — now reveal the window so dim + crosshair show together.
  invoke("overlay_ready").catch(() => {});
}

function resetState() {
  ready = false;
  clearCanvasHard();
  sel = null;
  tool = null;
  committed = [];
  undoable = [];
  current = null;
  selecting = false;
  drawing = false;
  cancelText();
  setTool(null);
  hint.style.display = "";
  hideBadge();
  if (nameModal) nameModal.classList.add("hidden");
  positionUI();
}

// ---------- geometry ----------

function resize() {
  cssW = window.innerWidth;
  cssH = window.innerHeight;
  dpr = window.devicePixelRatio || 1;
  sx = frozenNatW ? frozenNatW / cssW : dpr;
  sy = frozenNatH ? frozenNatH / cssH : dpr;
  canvas.width = Math.round(cssW * dpr);
  canvas.height = Math.round(cssH * dpr);
  canvas.style.width = cssW + "px";
  canvas.style.height = cssH + "px";
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  buildBackground();
  positionUI();
  render();
}

// Paint the frozen image + dim layer once into an offscreen canvas at device
// resolution. render() then just copies this in one fast blit per frame.
function buildBackground() {
  bgReady = false;
  if (!ready || !frozenImg.complete || !frozenNatW) return;
  bgCanvas.width = Math.round(cssW * dpr);
  bgCanvas.height = Math.round(cssH * dpr);
  bgCtx.setTransform(dpr, 0, 0, dpr, 0, 0);
  bgCtx.clearRect(0, 0, cssW, cssH);
  bgCtx.drawImage(frozenImg, 0, 0, cssW, cssH);
  bgCtx.fillStyle = "rgba(0,0,0,0.45)";
  bgCtx.fillRect(0, 0, cssW, cssH);
  bgReady = true;
}

function normRect(ax, ay, bx, by) {
  return { x: Math.min(ax, bx), y: Math.min(ay, by), w: Math.abs(bx - ax), h: Math.abs(by - ay) };
}

function clamp(v, lo, hi) { return Math.max(lo, Math.min(hi, v)); }

function insideSel(pt) {
  return pt.x >= sel.x && pt.x <= sel.x + sel.w && pt.y >= sel.y && pt.y <= sel.y + sel.h;
}

// ---------- rendering ----------

function render() {
  ctx.clearRect(0, 0, cssW, cssH);
  if (!ready || !frozenImg.complete || !frozenNatW) return;
  if (!bgReady) buildBackground();
  ctx.drawImage(bgCanvas, 0, 0, cssW, cssH);

  if (sel) {
    ctx.drawImage(frozenImg, sel.x * sx, sel.y * sy, sel.w * sx, sel.h * sy, sel.x, sel.y, sel.w, sel.h);
    // Annotations are NOT clipped to the selection — the user can draw outside
    // it (those strokes just won't end up in the cropped screenshot).
    drawAnnotations(ctx, committed.concat(undoable));
    if (current) drawOne(ctx, current);

    ctx.strokeStyle = "#2ec8c8";
    ctx.lineWidth = 1;
    ctx.strokeRect(sel.x + 0.5, sel.y + 0.5, sel.w, sel.h);

    ctx.fillStyle = "#2ec8c8";
    ctx.font = "12px system-ui, sans-serif";
    ctx.fillText(`${Math.round(sel.w * sx)} × ${Math.round(sel.h * sy)}`, sel.x, Math.max(12, sel.y - 4));
  }
}

function drawAnnotations(g, list) {
  for (const a of list) drawOne(g, a);
}

function drawOne(g, a) {
  g.save();
  if (a.type === "pen" || a.type === "marker") {
    g.strokeStyle = a.color;
    g.lineWidth = a.size;
    g.lineJoin = "round";
    g.lineCap = "round";
    if (a.type === "marker") {
      g.globalCompositeOperation = "multiply";
      g.globalAlpha = 0.85;
    }
    const p = a.points;
    g.beginPath();
    g.moveTo(p[0].x, p[0].y);
    for (let i = 1; i < p.length; i++) g.lineTo(p[i].x, p[i].y);
    if (p.length === 1) g.lineTo(p[0].x + 0.1, p[0].y);
    g.stroke();
  } else if (a.type === "line") {
    g.strokeStyle = a.color;
    g.lineWidth = a.size;
    g.lineCap = "round";
    g.beginPath();
    g.moveTo(a.x1, a.y1);
    g.lineTo(a.x2, a.y2);
    g.stroke();
  } else if (a.type === "arrow") {
    drawArrow(g, a);
  } else if (a.type === "rect") {
    g.strokeStyle = a.color;
    g.lineWidth = a.size;
    g.strokeRect(a.x, a.y, a.w, a.h);
  } else if (a.type === "blur") {
    g.beginPath();
    const r = Math.max(4, a.size);
    for (const p of a.points) {
      g.moveTo(p.x + r, p.y);
      g.arc(p.x, p.y, r, 0, Math.PI * 2);
    }
    g.clip();
    g.filter = `blur(${Math.max(4, r * 0.8)}px)`;
    g.drawImage(g.canvas, 0, 0, cssW, cssH);
  } else if (a.type === "text") {
    g.fillStyle = a.color;
    g.textBaseline = "top";
    g.font = `${a.size}px system-ui, "Segoe UI", sans-serif`;
    a.text.split("\n").forEach((ln, i) => g.fillText(ln, a.x, a.y + i * a.size * 1.2));
  }
  g.restore();
}

function drawArrow(g, a) {
  const dx = a.x2 - a.x1, dy = a.y2 - a.y1;
  const len = Math.hypot(dx, dy);
  if (len < 0.5) return;
  const ux = dx / len, uy = dy / len;            // unit along the arrow
  const px = -uy, py = ux;                        // unit perpendicular
  // One filled outline: a shaft that widens into a proper arrowhead. Every
  // part scales from the line width, so it stays a clean arrow at any size
  // instead of a blob + detached triangle.
  const shaft = a.size / 2;                        // shaft half-thickness
  const headLen = Math.min(len, Math.max(14, a.size * 3.2));
  const headW = Math.max(a.size * 1.9, headLen * 0.6); // barb half-width
  const bx = a.x2 - ux * headLen, by = a.y2 - uy * headLen; // head base center

  g.fillStyle = a.color;
  g.lineJoin = "round";
  g.beginPath();
  g.moveTo(a.x1 + px * shaft, a.y1 + py * shaft);
  g.lineTo(bx + px * shaft, by + py * shaft);
  g.lineTo(bx + px * headW, by + py * headW);
  g.lineTo(a.x2, a.y2);
  g.lineTo(bx - px * headW, by - py * headW);
  g.lineTo(bx - px * shaft, by - py * shaft);
  g.lineTo(a.x1 - px * shaft, a.y1 - py * shaft);
  g.closePath();
  g.fill();
}

// ---------- annotation history ----------

function pushAnnotation(a) {
  undoable.push(a);
  while (undoable.length > 5) committed.push(undoable.shift());
}

function undo() {
  if (undoable.length) {
    undoable.pop();
    render();
  }
}

// ---------- tools / ui ----------

function buildSwatches() {
  COLORS.forEach((c, i) => {
    const b = document.createElement("div");
    b.className = "sw" + (i === 0 ? " active" : "");
    b.style.background = c;
    b.dataset.color = c;
    b.addEventListener("mousedown", (e) => {
      e.preventDefault();
      e.stopPropagation();
      color = c;
      [...swatchesEl.children].forEach((s) => s.classList.toggle("active", s === b));
      if (textInput) textInput.style.color = c;
    });
    swatchesEl.appendChild(b);
  });
}

function setTool(t) {
  tool = t;
  [...toolbar.querySelectorAll(".tool")].forEach((b) =>
    b.classList.toggle("active", b.dataset.tool === t)
  );
  updateSizeVal();
}

function updateSizeVal() {
  sizeVal.textContent = tool ? sizes[tool] : "–";
}

function positionUI() {
  if (!sel || selecting) {
    toolbar.classList.remove("show");
    actionbar.classList.remove("show");
    return;
  }
  toolbar.classList.add("show");
  actionbar.classList.add("show");

  const tb = toolbar.getBoundingClientRect();
  const tw = tb.width || 40, th = tb.height || 280;
  let tx = sel.x + sel.w + 8;
  if (tx + tw > cssW - 4) tx = sel.x - tw - 8;
  tx = Math.max(4, Math.min(tx, cssW - tw - 4));
  let ty = sel.y;
  ty = Math.max(4, Math.min(ty, cssH - th - 4));
  toolbar.style.left = tx + "px";
  toolbar.style.top = ty + "px";

  const ab = actionbar.getBoundingClientRect();
  const aw = ab.width || 180, ah = ab.height || 36;
  let ay = sel.y + sel.h + 8;
  if (ay + ah > cssH - 4) ay = sel.y - ah - 8;
  ay = Math.max(4, Math.min(ay, cssH - ah - 4));
  let ax = Math.max(4, Math.min(sel.x + sel.w - aw, cssW - aw - 4));
  actionbar.style.left = ax + "px";
  actionbar.style.top = ay + "px";
}

function showBadge(x, y) {
  if (!tool) return;
  sizeBadge.textContent = `${tool}: ${sizes[tool]}`;
  sizeBadge.style.left = (x + 14) + "px";
  sizeBadge.style.top = (y + 14) + "px";
  sizeBadge.style.display = "block";
  if (badgeTimer) clearTimeout(badgeTimer);
  badgeTimer = setTimeout(hideBadge, 900);
}

// Add `delta` px to the active tool size, clamped to its range.
function nudgeSize(delta) {
  if (!tool) return;
  const min = tool === "text" ? 8 : 1;
  const max = tool === "text" ? 120 : tool === "marker" ? 80 : 60;
  sizes[tool] = Math.max(min, Math.min(max, sizes[tool] + delta));
  if (textInput && tool === "text") {
    textInput.style.fontSize = sizes.text + "px";
    autosize(textInput);
  }
  updateSizeVal();
}

// Resize by one natural step in `dir` (+1 bigger, -1 smaller) — used by the
// scroll wheel and the −/+ toolbar buttons.
function resizeTool(dir) {
  if (!tool) return;
  const step = tool === "text" ? 2 : tool === "marker" || tool === "blur" ? 2 : 1;
  nudgeSize(dir * step);
}
function hideBadge() {
  sizeBadge.style.display = "none";
}

// ---------- text tool ----------

function startText(pt) {
  textPos = { x: pt.x, y: pt.y };
  const ta = document.createElement("textarea");
  ta.id = "textinput";
  ta.style.left = pt.x + "px";
  ta.style.top = pt.y + "px";
  ta.style.color = color;
  ta.style.fontSize = sizes.text + "px";
  ta.rows = 1;
  document.body.appendChild(ta);
  textInput = ta;
  autosize(ta);
  setTimeout(() => ta.focus(), 0);
  ta.addEventListener("input", () => autosize(ta));
  ta.addEventListener("blur", commitText);
  ta.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") { e.preventDefault(); cancelText(); }
    else if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); commitText(); }
  });
}

function autosize(ta) {
  ta.style.width = "20px";
  ta.style.height = "auto";
  ta.style.width = (ta.scrollWidth + 6) + "px";
  ta.style.height = (ta.scrollHeight + 2) + "px";
}

function commitText() {
  if (!textInput) return;
  const ta = textInput;
  textInput = null;
  const val = ta.value;
  ta.remove();
  if (val.trim().length) {
    pushAnnotation({ type: "text", color, size: sizes.text, x: textPos.x, y: textPos.y, text: val });
  }
  render();
}

function cancelText() {
  if (!textInput) return;
  const ta = textInput;
  textInput = null;
  ta.remove();
  render();
}

// ---------- pointer ----------

canvas.addEventListener("mousedown", (e) => {
  if (e.button !== 0) return;
  if (textInput) { commitText(); return; }
  const pt = { x: e.clientX, y: e.clientY };

  // No tool active + click inside the existing frame -> drag it instead of
  // starting a new selection.
  if (tool === null && sel && insideSel(pt)) {
    moving = true;
    moveStart = { x: pt.x, y: pt.y, sx: sel.x, sy: sel.y };
    return;
  }

  if (!sel || tool === null) {
    selecting = true;
    selStart = pt;
    sel = { x: pt.x, y: pt.y, w: 0, h: 0 };
    hint.style.display = "none";
    positionUI();
    render();
    return;
  }

  if (tool === "text") { startText(pt); return; }

  drawing = true;
  if (tool === "pen" || tool === "marker" || tool === "blur") {
    current = { type: tool, color, size: sizes[tool], points: [pt] };
  } else if (tool === "line" || tool === "arrow") {
    current = { type: tool, color, size: sizes[tool], x1: pt.x, y1: pt.y, x2: pt.x, y2: pt.y };
  } else if (tool === "rect") {
    current = { type: tool, color, size: sizes[tool], ox: pt.x, oy: pt.y, x: pt.x, y: pt.y, w: 0, h: 0 };
  }
  render();
});

window.addEventListener("mousemove", (e) => {
  const pt = { x: e.clientX, y: e.clientY };
  lastMouse = pt;
  if (moving) {
    const nx = clamp(moveStart.sx + (pt.x - moveStart.x), 0, cssW - sel.w);
    const ny = clamp(moveStart.sy + (pt.y - moveStart.y), 0, cssH - sel.h);
    sel.x = nx; sel.y = ny;
    positionUI();
    render();
    return;
  }
  if (!selecting && !drawing) {
    // "" falls back to the high-contrast crosshair defined in overlay.css.
    canvas.style.cursor = (tool === null && sel && insideSel(pt)) ? "move" : "";
  }
  if (selecting) {
    sel = normRect(selStart.x, selStart.y, pt.x, pt.y);
    render();
    return;
  }
  if (drawing && current) {
    if (current.type === "pen" || current.type === "marker" || current.type === "blur") {
      current.points.push(pt);
    } else if (current.type === "line" || current.type === "arrow") {
      let nx = pt.x, ny = pt.y;
      if (e.shiftKey) {
        // Snap to 45° increments — gives clean horizontal / vertical lines.
        const dx = nx - current.x1, dy = ny - current.y1;
        const len = Math.hypot(dx, dy);
        const step = Math.PI / 4;
        const ang = Math.round(Math.atan2(dy, dx) / step) * step;
        nx = current.x1 + len * Math.cos(ang);
        ny = current.y1 + len * Math.sin(ang);
      }
      current.x2 = nx; current.y2 = ny;
    } else if (current.type === "rect") {
      const r = normRect(current.ox, current.oy, pt.x, pt.y);
      current.x = r.x; current.y = r.y; current.w = r.w; current.h = r.h;
    }
    render();
  }
});

window.addEventListener("mouseup", () => {
  if (moving) { moving = false; return; }
  if (selecting) {
    selecting = false;
    if (sel.w < 4 || sel.h < 4) {
      sel = null;
      hint.style.display = "";
    }
    positionUI();
    render();
    return;
  }
  if (drawing && current) {
    drawing = false;
    const a = current;
    current = null;
    const big = (a.type === "rect") ? (a.w > 3 && a.h > 3) : true;
    const longEnough = (a.type === "line" || a.type === "arrow")
      ? Math.hypot(a.x2 - a.x1, a.y2 - a.y1) > 3 : true;
    if (big && longEnough) pushAnnotation(a);
    render();
  }
});

// Touchpads fire many tiny wheel events per swipe (and mice fire a few big
// ones), so accumulate scroll distance and emit exactly one size step per
// fixed chunk — same predictable feel on both, no random jumps.
let wheelAccum = 0;
const WHEEL_STEP_PX = 100;
window.addEventListener("wheel", (e) => {
  if (!tool || e.deltaY === 0) return;
  e.preventDefault();
  let d = e.deltaY;
  if (e.deltaMode === 1) d *= 16;        // lines -> ~px
  else if (e.deltaMode === 2) d *= 100;  // pages -> ~px
  wheelAccum += d;
  while (Math.abs(wheelAccum) >= WHEEL_STEP_PX) {
    resizeTool(wheelAccum < 0 ? 1 : -1);
    wheelAccum -= Math.sign(wheelAccum) * WHEEL_STEP_PX;
  }
  showBadge(e.clientX, e.clientY);
}, { passive: false });

// ---------- toolbar / actions ----------

toolbar.querySelectorAll(".tool").forEach((b) => {
  if (b.id === "undoBtn") {
    b.addEventListener("mousedown", (e) => { e.preventDefault(); e.stopPropagation(); undo(); });
    return;
  }
  if (b.id === "sizeDown" || b.id === "sizeUp") return; // bound below
  b.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    setTool(tool === b.dataset.tool ? null : b.dataset.tool);
  });
});

// Size +/- buttons: click changes one step; press-and-hold auto-repeats.
// Clickable controls are the bulletproof path when touchpad scroll fails.
function bindSizeButton(id, dir) {
  const btn = document.getElementById(id);
  let hold, repeat;
  const bump = () => { resizeTool(dir); showBadge(lastMouse.x, lastMouse.y); };
  const stop = () => { clearTimeout(hold); clearInterval(repeat); };
  btn.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    bump();
    hold = setTimeout(() => { repeat = setInterval(bump, 70); }, 350);
  });
  btn.addEventListener("mouseup", stop);
  btn.addEventListener("mouseleave", stop);
}
bindSizeButton("sizeDown", -1);
bindSizeButton("sizeUp", 1);

document.getElementById("saveBtn").addEventListener("click", doSave);
document.getElementById("copyBtn").addEventListener("click", doCopy);
document.getElementById("saveAsBtn").addEventListener("click", openNameModal);
document.getElementById("recordBtn").addEventListener("click", doRecord);
document.getElementById("closeBtn").addEventListener("click", cancel);

const nameModal = document.getElementById("nameModal");
const nameInput = document.getElementById("nameInput");
document.getElementById("nameOk").addEventListener("click", confirmNameSave);
document.getElementById("nameCancel").addEventListener("click", closeNameModal);
nameModal.addEventListener("mousedown", (e) => {
  e.stopPropagation();
  if (e.target === nameModal) closeNameModal();
});
nameInput.addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Enter") { e.preventDefault(); confirmNameSave(); }
  else if (e.key === "Escape") { e.preventDefault(); closeNameModal(); }
});
[toolbar, actionbar].forEach((p) =>
  p.addEventListener("mousedown", (e) => e.stopPropagation())
);

function exportPNG() {
  // Render the WHOLE screen at physical resolution first, then crop the
  // selection. The blur tool samples its own canvas (drawImage(canvas,...)),
  // so it needs the full frozen image present to sample from — a pre-cropped
  // canvas would blur from empty pixels.
  const full = document.createElement("canvas");
  full.width = frozenNatW;
  full.height = frozenNatH;
  const fx = full.getContext("2d");
  fx.drawImage(frozenImg, 0, 0, frozenNatW, frozenNatH);
  fx.save();
  fx.scale(sx, sy);            // annotations are stored in css px
  drawAnnotations(fx, committed.concat(undoable));
  fx.restore();

  const w = Math.max(1, Math.round(sel.w * sx));
  const h = Math.max(1, Math.round(sel.h * sy));
  const ec = document.createElement("canvas");
  ec.width = w;
  ec.height = h;
  const ex = ec.getContext("2d");
  ex.drawImage(full, Math.round(sel.x * sx), Math.round(sel.y * sy), w, h, 0, 0, w, h);
  return ec.toDataURL("image/png");
}

async function doSave() {
  if (!sel) return;
  if (textInput) commitText();
  try {
    const url = exportPNG();
    await invoke("save_capture", { pngDataUrl: url });
    resetState();
  } catch (_) { /* keep editor open on failure */ }
}

function openNameModal() {
  if (!sel) return;
  if (textInput) commitText();
  nameInput.value = "";
  nameModal.classList.remove("hidden");
  nameInput.focus();
}

function closeNameModal() {
  nameModal.classList.add("hidden");
}

async function confirmNameSave() {
  const name = nameInput.value;
  closeNameModal();
  try {
    const url = exportPNG();
    await invoke("save_capture", { pngDataUrl: url, name });
    resetState();
  } catch (_) { /* keep editor open on failure */ }
}

async function doCopy() {
  if (!sel) return;
  if (textInput) commitText();
  try {
    const url = exportPNG();
    await invoke("copy_capture", { pngDataUrl: url });
    resetState();
  } catch (_) { /* keep editor open on failure */ }
}

// Hand the selection to the recorder in physical px relative to the frozen
// frame's origin (sx/sy map css -> native). The backend dismisses this overlay
// before grabbing the first frame, so the dim layer never lands in the video.
async function doRecord() {
  if (!sel) return;
  if (textInput) commitText();
  const x = Math.round(sel.x * sx);
  const y = Math.round(sel.y * sy);
  const w = Math.round(sel.w * sx);
  const h = Math.round(sel.h * sy);
  if (w < 8 || h < 8) return;
  try {
    await invoke("start_recording", { x, y, w, h });
    resetState();
  } catch (_) {
    // Surface the failure (most likely the bundled video component is missing)
    // instead of silently doing nothing, then leave the editor open.
    hint.textContent = "Couldn't start recording — try saving a screenshot instead.";
    hint.style.display = "";
  }
}

function cancel() {
  cancelText();
  resetState();
  invoke("cancel_area").catch(() => {});
}

// ---------- keyboard ----------

window.addEventListener("keydown", (e) => {
  if (textInput) return; // textarea handles its own keys
  if (e.key === "Escape" || e.code === "Escape") { e.preventDefault(); cancel(); return; }
  // Resize the active tool from the keyboard (works on any laptop, no scroll
  // needed): +/= grows, -/_ shrinks, and [ ] do the same.
  if (tool && !e.ctrlKey && !e.metaKey) {
    if (e.key === "+" || e.key === "=" || e.code === "BracketRight") {
      e.preventDefault(); resizeTool(1); showBadge(lastMouse.x, lastMouse.y); return;
    }
    if (e.key === "-" || e.key === "_" || e.code === "BracketLeft") {
      e.preventDefault(); resizeTool(-1); showBadge(lastMouse.x, lastMouse.y); return;
    }
  }
  if (e.ctrlKey || e.metaKey) {
    // Use physical key codes so shortcuts work regardless of layout (e.g. RU).
    // Ctrl+= / Ctrl+- resize the active tool by ±3; holding the key auto-repeats
    // (the browser fires repeated keydowns), so the size changes smoothly.
    if (tool && (e.code === "Equal" || e.code === "NumpadAdd")) {
      e.preventDefault(); nudgeSize(3); showBadge(lastMouse.x, lastMouse.y); return;
    }
    if (tool && (e.code === "Minus" || e.code === "NumpadSubtract")) {
      e.preventDefault(); nudgeSize(-3); showBadge(lastMouse.x, lastMouse.y); return;
    }
    if (e.code === "KeyZ") { e.preventDefault(); undo(); }
    else if (e.code === "KeyS") { e.preventDefault(); doSave(); }
    else if (e.code === "KeyC") { e.preventDefault(); doCopy(); }
  }
});

window.addEventListener("contextmenu", (e) => e.preventDefault());
window.addEventListener("resize", resize);

// ---------- init ----------

buildSwatches();
if (listen) listen("frozen-ready", () => loadFrozen());
loadFrozen();
