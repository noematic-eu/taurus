const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const statusEl = () => document.querySelector("#status");

function showError(msg) {
  const el = statusEl();
  el.hidden = false;
  el.textContent = msg;
}

window.addEventListener("DOMContentLoaded", async () => {
  document.querySelector("#pick").addEventListener("click", () => {
    invoke("pick_pack").catch((e) => showError(String(e)));
  });

  await listen("taurus-error", (ev) => {
    showError(String(ev.payload));
  });
});
