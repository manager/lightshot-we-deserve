const { invoke } = window.__TAURI__.core;

const areaEl = document.getElementById("hk-area");
const fullEl = document.getElementById("hk-full");
const dirEl = document.getElementById("save-dir");
const autostartEl = document.getElementById("autostart");
const qualityEl = document.getElementById("video-quality");
const statusEl = document.getElementById("status");

function setStatus(msg, isError) {
  statusEl.textContent = msg;
  statusEl.className = isError ? "error" : "";
}

function comboFromEvent(e) {
  const parts = [];
  if (e.ctrlKey || e.metaKey) parts.push("CmdOrCtrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  let key = null;
  const c = e.code;
  if (/^Key[A-Z]$/.test(c)) key = c.slice(3);
  else if (/^Digit[0-9]$/.test(c)) key = c.slice(5);
  else if (/^F[0-9]{1,2}$/.test(c)) key = c;
  else if (c === "Space") key = "Space";
  if (!key) return null;
  parts.push(key);
  return parts.join("+");
}

function bindRecorder(el) {
  el.addEventListener("focus", () => el.classList.add("recording"));
  el.addEventListener("blur", () => el.classList.remove("recording"));
  el.addEventListener("keydown", (e) => {
    e.preventDefault();
    const combo = comboFromEvent(e);
    if (combo) {
      el.value = combo;
      el.blur();
    }
  });
}

bindRecorder(areaEl);
bindRecorder(fullEl);

document.getElementById("save").addEventListener("click", async () => {
  if (!areaEl.value || !fullEl.value) {
    setStatus("Please set both hotkeys.", true);
    return;
  }
  try {
    await invoke("save_settings", {
      settings: {
        hotkey_area: areaEl.value,
        hotkey_full: fullEl.value,
        save_dir: dirEl.value.trim(),
        autostart: autostartEl.checked,
        video_quality: qualityEl.value,
      },
    });
    setStatus("Saved ✓", false);
  } catch (err) {
    setStatus("Save failed: " + err, true);
  }
});

document.getElementById("test").addEventListener("click", async () => {
  try {
    const path = await invoke("capture_full_now");
    setStatus("Saved: " + path, false);
  } catch (err) {
    setStatus("Capture failed: " + err, true);
  }
});

document.getElementById("close").addEventListener("click", () => {
  invoke("close_settings");
});

(async () => {
  try {
    const s = await invoke("get_settings");
    areaEl.value = s.hotkey_area || "";
    fullEl.value = s.hotkey_full || "";
    dirEl.value = s.save_dir || "";
    autostartEl.checked = s.autostart !== false;
    qualityEl.value = s.video_quality === "low" ? "low" : "high";
  } catch (err) {
    setStatus("Could not load settings: " + err, true);
  }
})();
