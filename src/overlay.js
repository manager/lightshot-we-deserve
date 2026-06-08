const { invoke } = window.__TAURI__.core;
const listen = window.__TAURI__.event && window.__TAURI__.event.listen;

const canvas = document.getElementById("c");
const ctx = canvas.getContext("2d");
const hint = document.getElementById("hint");
const sizeBadge = document.getElementById("sizebadge");
const toolbar = document.getElementById("toolbar");
const actionbar = document.getElementById("actionbar");
const swatchesEl = document.getElementById("swatches");

const COLORS = ["#e7402e", "#e67e22", "#f1c40f", "#2ecc71", "#3498db", "#9b59b6", "#000000", "#ffffff"];

const frozenImg = new Image();
let frozenNatW = 0, frozenNatH = 0;
let cssW = 0, cssH = 0, dpr = 1, sx = 1, sy = 1;

let sel = null;            // {x,y,w,h} in css px
let tool = null;           // null=select, or pen/line/arrow/rect/marker/text/blur
let color = COLORS[0];
const sizes = { pen: 4, line: 4, arrow: 5, rect: 4, marker: 18, text: 24, blur: 12 };

let committed = [];        // permanent (beyond 5-step undo memory)
let undoable = [];         // up to 5 undoable actions
let current = null;        // in-progress annotation

let selecting = false, drawing = false, selStart = null;
let textInput = null, textPos = null;
let badgeTimer = null;

// ---------- frozen image ----------

async function loadFrozen() {
  let shot;
  try { shot = await invoke("get_frozen"); } catch (_) { return; }
  if (!shot) return;
  await new Promise((res) => {
    frozenImg.onload = res;
    frozenImg.onerror = res;
    frozenImg.src = shot.data_url;
  });
  frozenNatW = shot.width;
  frozenNatH = shot.height;
  resetState();
  resize();
}

function resetState() {
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
  positionUI();
  render();
}

function normRect(ax, ay, bx, by) {
  return { x: Math.min(ax, bx), y: Math.min(ay, by), w: Math.abs(bx - ax), h: Math.abs(by - ay) };
}

// ---------- rendering ----------

function render() {
  ctx.clearRect(0, 0, cssW, cssH);
  if (frozenImg.complete && frozenNatW) {
    ctx.drawImage(frozenImg, 0, 0, cssW, cssH);
  }
  ctx.fillStyle = "rgba(0,0,0,0.45)";
  ctx.fillRect(0, 0, cssW, cssH);

  if (sel) {
    ctx.drawImage(frozenImg, sel.x * sx, sel.y * sy, sel.w * sx, sel.h * sy, sel.x, sel.y, sel.w, sel.h);
    ctx.save();
    ctx.beginPath();
    ctx.rect(sel.x, sel.y, sel.w, sel.h);
    ctx.clip();
    drawAnnotations(ctx, committed.concat(undoable));
    if (current) drawOne(ctx, current);
    ctx.restore();

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
  } else if (a.type === "line" || a.type === "arrow") {
    g.strokeStyle = a.color;
    g.fillStyle = a.color;
    g.lineWidth = a.size;
    g.lineCap = "round";
    g.beginPath();
    g.moveTo(a.x1, a.y1);
    g.lineTo(a.x2, a.y2);
    g.stroke();
    if (a.type === "arrow") drawArrowHead(g, a);
  } else if (a.type === "rect") {
    g.strokeStyle = a.color;
    g.lineWidth = a.size;
    g.strokeRect(a.x, a.y, a.w, a.h);
  } else if (a.type === "blur") {
    g.beginPath();
    g.rect(a.x, a.y, a.w, a.h);
    g.clip();
    g.filter = `blur(${Math.max(2, a.size)}px)`;
    g.drawImage(frozenImg, a.x * sx, a.y * sy, a.w * sx, a.h * sy, a.x, a.y, a.w, a.h);
  } else if (a.type === "text") {
    g.fillStyle = a.color;
    g.textBaseline = "top";
    g.font = `${a.size}px system-ui, "Segoe UI", sans-serif`;
    a.text.split("\n").forEach((ln, i) => g.fillText(ln, a.x, a.y + i * a.size * 1.2));
  }
  g.restore();
}

function drawArrowHead(g, a) {
  const ang = Math.atan2(a.y2 - a.y1, a.x2 - a.x1);
  const len = Math.max(12, a.size * 3.2);
  g.beginPath();
  g.moveTo(a.x2, a.y2);
  g.lineTo(a.x2 - len * Math.cos(ang - Math.PI / 6), a.y2 - len * Math.sin(ang - Math.PI / 6));
  g.lineTo(a.x2 - len * Math.cos(ang + Math.PI / 6), a.y2 - len * Math.sin(ang + Math.PI / 6));
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
}

function positionUI() {
  if (!sel) {
    toolbar.classList.remove("show");
    actionbar.classList.remove("show");
    return;
  }
  toolbar.classList.add("show");
  actionbar.classList.add("show");

  const tb = toolbar.getBoundingClientRect();
  const tw = tb.width || 40, th = tb.height || 280;
  let tx = sel.x + sel.w + 8;
  if (tx + tw > cssW) tx = sel.x - tw - 8;
  if (tx < 4) tx = Math.max(4, Math.min(cssW - tw - 4, sel.x + sel.w + 8));
  let ty = sel.y;
  if (ty + th > cssH) ty = cssH - th - 4;
  if (ty < 4) ty = 4;
  toolbar.style.left = tx + "px";
  toolbar.style.top = ty + "px";

  const ab = actionbar.getBoundingClientRect();
  const aw = ab.width || 180, ah = ab.height || 36;
  let ax = sel.x + sel.w - aw;
  if (ax < 4) ax = 4;
  if (ax + aw > cssW) ax = cssW - aw - 4;
  let ay = sel.y + sel.h + 8;
  if (ay + ah > cssH) ay = sel.y - ah - 8;
  if (ay < 4) ay = 4;
  actionbar.style.left = ax + "px";
  actionbar.style.top = ay + "px";
}

function showBadge(e) {
  if (!tool) return;
  sizeBadge.textContent = `${tool}: ${sizes[tool]}`;
  sizeBadge.style.left = (e.clientX + 14) + "px";
  sizeBadge.style.top = (e.clientY + 14) + "px";
  sizeBadge.style.display = "block";
  if (badgeTimer) clearTimeout(badgeTimer);
  badgeTimer = setTimeout(hideBadge, 900);
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
  if (tool === "pen" || tool === "marker") {
    current = { type: tool, color, size: sizes[tool], points: [pt] };
  } else if (tool === "line" || tool === "arrow") {
    current = { type: tool, color, size: sizes[tool], x1: pt.x, y1: pt.y, x2: pt.x, y2: pt.y };
  } else if (tool === "rect" || tool === "blur") {
    current = { type: tool, color, size: sizes[tool], ox: pt.x, oy: pt.y, x: pt.x, y: pt.y, w: 0, h: 0 };
  }
  render();
});

window.addEventListener("mousemove", (e) => {
  const pt = { x: e.clientX, y: e.clientY };
  if (selecting) {
    sel = normRect(selStart.x, selStart.y, pt.x, pt.y);
    render();
    return;
  }
  if (drawing && current) {
    if (current.type === "pen" || current.type === "marker") {
      current.points.push(pt);
    } else if (current.type === "line" || current.type === "arrow") {
      current.x2 = pt.x; current.y2 = pt.y;
    } else if (current.type === "rect" || current.type === "blur") {
      const r = normRect(current.ox, current.oy, pt.x, pt.y);
      current.x = r.x; current.y = r.y; current.w = r.w; current.h = r.h;
    }
    render();
  }
});

window.addEventListener("mouseup", () => {
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
    const big = (a.type === "rect" || a.type === "blur") ? (a.w > 3 && a.h > 3) : true;
    const longEnough = (a.type === "line" || a.type === "arrow")
      ? Math.hypot(a.x2 - a.x1, a.y2 - a.y1) > 3 : true;
    if (big && longEnough) pushAnnotation(a);
    render();
  }
});

window.addEventListener("wheel", (e) => {
  if (!e.ctrlKey || !tool) return;
  e.preventDefault();
  const step = tool === "text" ? 2 : tool === "marker" || tool === "blur" ? 2 : 1;
  const min = tool === "text" ? 8 : 1;
  const max = tool === "text" ? 120 : tool === "marker" ? 80 : 60;
  let v = sizes[tool] + (e.deltaY < 0 ? step : -step);
  sizes[tool] = Math.max(min, Math.min(max, v));
  if (textInput && tool === "text") {
    textInput.style.fontSize = sizes.text + "px";
    autosize(textInput);
  }
  showBadge(e);
}, { passive: false });

// ---------- toolbar / actions ----------

toolbar.querySelectorAll(".tool").forEach((b) => {
  if (b.id === "undoBtn") {
    b.addEventListener("mousedown", (e) => { e.preventDefault(); e.stopPropagation(); undo(); });
    return;
  }
  b.addEventListener("mousedown", (e) => {
    e.preventDefault();
    e.stopPropagation();
    setTool(tool === b.dataset.tool ? null : b.dataset.tool);
  });
});

document.getElementById("undoBtn"); // ensured above

document.getElementById("saveBtn").addEventListener("click", doSave);
document.getElementById("copyBtn").addEventListener("click", doCopy);
document.getElementById("closeBtn").addEventListener("click", cancel);
[toolbar, actionbar].forEach((p) =>
  p.addEventListener("mousedown", (e) => e.stopPropagation())
);

function exportPNG() {
  const w = Math.max(1, Math.round(sel.w * sx));
  const h = Math.max(1, Math.round(sel.h * sy));
  const ec = document.createElement("canvas");
  ec.width = w;
  ec.height = h;
  const ex = ec.getContext("2d");
  ex.drawImage(frozenImg, sel.x * sx, sel.y * sy, sel.w * sx, sel.h * sy, 0, 0, w, h);
  ex.save();
  ex.scale(sx, sy);
  ex.translate(-sel.x, -sel.y);
  drawAnnotations(ex, committed.concat(undoable));
  ex.restore();
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

async function doCopy() {
  if (!sel) return;
  if (textInput) commitText();
  try {
    const url = exportPNG();
    await invoke("copy_capture", { pngDataUrl: url });
    resetState();
  } catch (_) { /* keep editor open on failure */ }
}

function cancel() {
  cancelText();
  invoke("cancel_area").catch(() => {});
}

// ---------- keyboard ----------

window.addEventListener("keydown", (e) => {
  if (textInput) return; // textarea handles its own keys
  if (e.key === "Escape") { e.preventDefault(); cancel(); return; }
  if (e.ctrlKey || e.metaKey) {
    const k = e.key.toLowerCase();
    if (k === "z") { e.preventDefault(); undo(); }
    else if (k === "s") { e.preventDefault(); doSave(); }
    else if (k === "c") { e.preventDefault(); doCopy(); }
  }
});

window.addEventListener("contextmenu", (e) => e.preventDefault());
window.addEventListener("resize", resize);

// ---------- init ----------

buildSwatches();
if (listen) listen("frozen-ready", () => loadFrozen());
loadFrozen();
