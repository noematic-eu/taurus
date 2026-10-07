const { invoke } = window.__TAURI__.core;

function packId() {
  const label = window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label || "";
  const prefix = "ask-reply-";
  if (!label.startsWith(prefix)) {
    return null;
  }
  const id = Number(label.slice(prefix.length));
  return Number.isInteger(id) ? id : null;
}

function showError(msg) {
  const el = document.querySelector("#error");
  el.hidden = !msg;
  el.textContent = msg || "";
}

async function load() {
  const id = packId();
  const doc = document.querySelector("#doc");
  if (id == null) {
    showError("Cette page n’est pas liée à un pack.");
    return false;
  }
  try {
    const page = await invoke("ask_rendered", { id });
    doc.innerHTML = page.html || "";
    showError("");
    return true;
  } catch (err) {
    doc.innerHTML = "";
    showError(String(err));
    return false;
  }
}

window.addEventListener("DOMContentLoaded", () => {
  const doc = document.querySelector("#doc");
  doc.addEventListener("click", (event) => {
    if (event.target.closest("a")) {
      event.preventDefault();
    }
  });

  const button = document.querySelector("#save");
  button.disabled = true;
  button.addEventListener("click", async () => {
    const id = packId();
    if (id == null) {
      return;
    }
    button.disabled = true;
    try {
      const path = await invoke("ask_save", { id });
      if (path) {
        const saved = document.querySelector("#saved");
        saved.hidden = false;
        saved.textContent = `Enregistré : ${path}`;
        showError("");
      }
    } catch (err) {
      showError(String(err));
    } finally {
      button.disabled = false;
    }
  });

  load().then((ready) => {
    button.disabled = !ready;
  });
});
