const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const statusEl = () => document.querySelector("#status");
const loadingEl = () => document.querySelector("#loading");
const loadingLabel = () => document.querySelector("#loading-label");
const loadingDetail = () => document.querySelector("#loading-detail");
const loadingBar = () => document.querySelector("#loading-bar");
const loadingTrack = () => document.querySelector("#loading-track");

let shownGen = 0;
let pending = null;
let reveal = 0;
const inflight = new Map();

function showError(msg) {
  const el = statusEl();
  el.hidden = false;
  el.textContent = msg;
}

function renderProgress(progress) {
  loadingLabel().textContent = progress.label || "Ouverture…";
  loadingDetail().textContent = progress.detail || "";
  const bar = loadingBar();
  const track = loadingTrack();
  if (typeof progress.ratio === "number") {
    const pct = Math.max(0, Math.min(100, Math.round(progress.ratio * 100)));
    bar.classList.remove("indeterminate");
    bar.style.width = pct + "%";
    track.setAttribute("aria-valuenow", String(pct));
    track.setAttribute("aria-valuetext", progress.detail || progress.label || "");
  } else {
    bar.classList.add("indeterminate");
    bar.style.width = "";
    track.removeAttribute("aria-valuenow");
    track.setAttribute("aria-valuetext", progress.detail || progress.label || "");
  }
}

function hideProgress() {
  loadingEl().hidden = true;
  pending = null;
  shownGen = 0;
  if (reveal) {
    clearTimeout(reveal);
    reveal = 0;
  }
}

function showLatest() {
  const last = Array.from(inflight.values()).at(-1);
  if (!last) {
    hideProgress();
    return;
  }
  shownGen = last.gen;
  pending = last;
  if (!loadingEl().hidden) {
    renderProgress(last);
    return;
  }
  if (!reveal) {
    reveal = setTimeout(() => {
      reveal = 0;
      if (pending && inflight.has(pending.gen)) {
        loadingEl().hidden = false;
        renderProgress(pending);
      }
    }, 150);
  }
}

function onProgress(progress) {
  if (progress.done) {
    inflight.delete(progress.gen);
    if (inflight.size === 0) {
      hideProgress();
    } else if (progress.gen === shownGen) {
      showLatest();
    }
    return;
  }
  inflight.set(progress.gen, progress);
  shownGen = progress.gen;
  pending = progress;
  if (!loadingEl().hidden) {
    renderProgress(progress);
    return;
  }
  if (!reveal) {
    reveal = setTimeout(() => {
      reveal = 0;
      if (pending && inflight.has(pending.gen)) {
        loadingEl().hidden = false;
        renderProgress(pending);
      }
    }, 150);
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  document.querySelector("#pick-folder").addEventListener("click", () => {
    invoke("pick_folder").catch((e) => showError(String(e)));
  });

  document.querySelector("#pick").addEventListener("click", () => {
    invoke("pick_pack").catch((e) => showError(String(e)));
  });

  document.querySelector("#check").addEventListener("click", () => {
    invoke("pick_link_check").catch((e) => showError(String(e)));
  });

  await listen("taurus-error", (ev) => {
    showError(String(ev.payload));
  });

  await listen("taurus-progress", (ev) => {
    onProgress(ev.payload);
  });
});
