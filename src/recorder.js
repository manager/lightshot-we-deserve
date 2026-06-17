const { invoke } = window.__TAURI__.core;
const listen = window.__TAURI__.event && window.__TAURI__.event.listen;

const bar = document.getElementById("bar");
const timeEl = document.getElementById("time");
const stopBtn = document.getElementById("stopBtn");

// This window is pre-created at startup and REUSED for every recording, so the
// timer and Stop state must be reset on each fresh start rather than relying on
// the module loading again (it loads only once).
let started = Date.now();
let tick = null;
let stopping = false;

function renderTime() {
  const s = Math.floor((Date.now() - started) / 1000);
  const m = Math.floor(s / 60);
  timeEl.textContent = m + ":" + String(s % 60).padStart(2, "0");
}

function startTimer() {
  if (tick) clearInterval(tick);
  started = Date.now();
  renderTime();
  tick = setInterval(renderTime, 250);
}

function reset() {
  stopping = false;
  bar.classList.remove("error");
  stopBtn.textContent = "Stop";
  startTimer();
}

function stop() {
  if (stopping) return;
  stopping = true;
  if (tick) clearInterval(tick);
  invoke("stop_recording").catch(() => {});
}

stopBtn.addEventListener("click", stop);
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") { e.preventDefault(); stop(); }
});

if (listen) {
  // Each new recording reuses this window — reset the timer and Stop button.
  listen("recording-started", () => reset());

  // Backend says the encoder fell over mid-recording — freeze the timer and turn
  // the indicator into a dismissible error so it never sits there spinning.
  listen("recording-error", (e) => {
    if (tick) clearInterval(tick);
    stopping = true;
    bar.classList.add("error");
    timeEl.textContent = (e && e.payload) || "Recording failed";
    // Backend already cleared the recording slot; the button just dismisses the
    // indicator now (stop_recording is a harmless no-op in this state).
    stopBtn.textContent = "Close";
  });
}

startTimer();
