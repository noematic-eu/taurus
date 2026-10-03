const { invoke } = window.__TAURI__.core;

const packId = () => {
  const label = window.__TAURI_INTERNALS__?.metadata?.currentWindow?.label || "";
  const id = Number(label.slice("ask-".length));
  return Number.isInteger(id) ? id : null;
};

const currentEl = () => document.querySelector("#current");
const noteEl = () => document.querySelector("#note");
const batchEl = () => document.querySelector("#batch");
const errorEl = () => document.querySelector("#error");
const pendingEl = () => document.querySelector("#pending");
const answerEl = () => document.querySelector("#answer");
const filesEl = () => document.querySelector("#files");

function showError(msg) {
  const el = errorEl();
  el.hidden = !msg;
  el.textContent = msg || "";
}

function render(view) {
  const current = view.current || "";
  currentEl().textContent = current || "Aucune page courante.";
  noteEl().hidden = view.batch.length > 0;
  const list = batchEl();
  list.replaceChildren();
  for (const rel of view.batch) {
    const li = document.createElement("li");
    const name = document.createElement("span");
    name.textContent = rel;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "alt";
    button.textContent = "Retirer";
    button.addEventListener("click", () => remove(rel));
    li.append(name, button);
    list.append(li);
  }
}

async function refresh() {
  const id = packId();
  if (id == null) {
    return;
  }
  try {
    render(await invoke("ask_state", { id }));
  } catch (err) {
    showError(String(err));
  }
}

async function remove(rel) {
  const id = packId();
  if (id == null) {
    return;
  }
  try {
    render(await invoke("ask_remove", { id, rel }));
    showError("");
  } catch (err) {
    showError(String(err));
  }
}

window.addEventListener("DOMContentLoaded", () => {
  document.querySelector("#add").addEventListener("click", async () => {
    const id = packId();
    if (id == null) {
      return;
    }
    try {
      render(await invoke("ask_add", { id }));
      showError("");
    } catch (err) {
      showError(String(err));
    }
  });

  document.querySelector("#go").addEventListener("click", async () => {
    const id = packId();
    if (id == null) {
      return;
    }
    const button = document.querySelector("#go");
    button.disabled = true;
    pendingEl().hidden = false;
    showError("");
    answerEl().textContent = "";
    filesEl().replaceChildren();
    try {
      const reply = await invoke("ask_question", {
        id,
        question: document.querySelector("#q").value,
      });
      answerEl().textContent = reply.answer || "";
      for (const rel of reply.files || []) {
        const li = document.createElement("li");
        li.textContent = rel;
        filesEl().append(li);
      }
    } catch (err) {
      showError(String(err));
    } finally {
      button.disabled = false;
      pendingEl().hidden = true;
    }
  });

  refresh();
  setInterval(refresh, 800);
});
