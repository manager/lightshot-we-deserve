const { invoke } = window.__TAURI__.core;
const listen = window.__TAURI__.event && window.__TAURI__.event.listen;

const bar = document.getElementById("bar");
const timeEl = document.getElementById("time");
const stopBtn = document.getElementById("stopBtn");

const started = Date.now();
const tick = setInterval(() => {
  const s = Math.floor((Date.now() - started) / 1000);
  const m = Math.floor(s / 60);
  timeEl.textContent = m + ":" + String(s % 60).padStart(2, "0");
}, 250);

let stopping = false;
function stop() {
  if (stopping) return;
  stopping = true;
  clearInterval(tick);
  invoke("stop_recording").catch(() => {});
}

stopBtn.addEventListener("click", stop);
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") { e.preventDefault(); stop(); }
});

// Backend says the encoder fell over mid-recording — freeze the timer and turn
// the indicator into a dismissible error so it never sits there spinning.
if (listen) {
  listen("recording-error", (e) => {
    clearInterval(tick);
    bar.classList.add("error");
    timeEl.textContent = (e && e.payload) || "Recording failed";
    // Backend already cleared the recording slot; the button just dismisses the
    // indicator now (stop_recording is a harmless no-op in this state).
    stopBtn.textContent = "Close";
  });
}
