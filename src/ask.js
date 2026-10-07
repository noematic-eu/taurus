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
const actionsEl = () => document.querySelector("#actions");
const savedEl = () => document.querySelector("#saved");
const filesEl = () => document.querySelector("#files");
const modelEl = () => document.querySelector("#model");
const ctxEl = () => document.querySelector("#ctx");
const meterEl = () => document.querySelector("#meter");

let fillingModels = false;
let modelChanging = false;
let lastWindow = null;
let modelRetry = 0;
let modelRetryTimer = 0;

function fmtTokens(n) {
  return Number(n).toLocaleString("fr-FR");
}

function showContext(used, windowTokens) {
  const usedOk = typeof used === "number" && Number.isFinite(used);
  const windowOk = typeof windowTokens === "number" && windowTokens > 0;
  const el = ctxEl();
  const meter = meterEl();
  let text = "";
  if (usedOk && windowOk) {
    text = `Contexte : ${fmtTokens(used)} / ${fmtTokens(windowTokens)} jetons`;
    if (used >= windowTokens) {
      text += " La fenêtre est pleine.";
    }
  } else if (windowOk) {
    text = `Fenêtre : ${fmtTokens(windowTokens)} jetons`;
  } else if (usedOk) {
    text = `Contexte envoyé : ${fmtTokens(used)} jetons`;
  } else {
    return;
  }
  if (windowOk) {
    lastWindow = windowTokens;
  }
  const full = usedOk && windowOk && used >= windowTokens;
  el.textContent = text;
  el.classList.toggle("full", full);
  if (usedOk && windowOk) {
    const pct = Math.max(0, Math.min(100, (used / windowTokens) * 100));
    meter.hidden = false;
    meter.classList.toggle("full", full);
    meter.querySelector("span").style.width = `${pct}%`;
  } else {
    meter.hidden = true;
    meter.classList.remove("full");
  }
}

function fillModels(view) {
  const select = modelEl();
  const chosen = view.model || "";
  fillingModels = true;
  select.replaceChildren();
  for (const name of view.models || []) {
    const option = document.createElement("option");
    option.value = name;
    option.textContent = name;
    if (name === chosen) {
      option.selected = true;
    }
    select.append(option);
  }
  fillingModels = false;
  if (typeof view.context_tokens === "number") {
    showContext(null, view.context_tokens);
  } else {
    lastWindow = null;
    ctxEl().textContent = "";
    ctxEl().classList.remove("full");
    meterEl().hidden = true;
  }
}

function showError(msg) {
  const el = errorEl();
  el.hidden = !msg;
  el.textContent = msg || "";
}

function clearAnswer() {
  answerEl().innerHTML = "";
  actionsEl().hidden = true;
  const saved = savedEl();
  saved.hidden = true;
  saved.textContent = "";
  filesEl().replaceChildren();
}

function showAnswer(reply) {
  answerEl().innerHTML = reply.html || "";
  actionsEl().hidden = !reply.html;
}

function keepLinksInPage(article) {
  article.addEventListener("click", (event) => {
    if (event.target.closest("a")) {
      event.preventDefault();
    }
  });
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
  if (
    !modelChanging &&
    (view.model || view.context_tokens != null || view.prompt_tokens != null)
  ) {
    showContext(view.prompt_tokens, view.context_tokens);
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

function scheduleModelRetry() {
  if (modelRetryTimer || modelEl().options.length > 0) {
    return;
  }
  const wait = Math.min(8000, 1000 * 2 ** modelRetry);
  modelRetry += 1;
  modelRetryTimer = window.setTimeout(() => {
    modelRetryTimer = 0;
    if (modelEl().options.length === 0) {
      loadModels();
    }
  }, wait);
}

async function loadModels() {
  const id = packId();
  if (id == null) {
    return;
  }
  try {
    fillModels(await invoke("ask_models", { id }));
    modelRetry = 0;
    if (modelRetryTimer) {
      window.clearTimeout(modelRetryTimer);
      modelRetryTimer = 0;
    }
  } catch (err) {
    const msg = String(err);
    if (modelEl().options.length === 0) {
      ctxEl().textContent = msg;
    }
    showError(msg);
    if (modelEl().options.length === 0 && !msg.includes("fermé")) {
      scheduleModelRetry();
    }
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
    const askedModel = modelEl().value;
    button.disabled = true;
    pendingEl().hidden = false;
    showError("");
    clearAnswer();
    try {
      const reply = await invoke("ask_question", {
        id,
        question: document.querySelector("#q").value,
      });
      showAnswer(reply);
      for (const rel of reply.files || []) {
        const li = document.createElement("li");
        li.textContent = rel;
        filesEl().append(li);
      }
      if (modelEl().value === askedModel) {
        showContext(reply.prompt_tokens, reply.context_tokens);
      }
    } catch (err) {
      showError(String(err));
    } finally {
      button.disabled = false;
      pendingEl().hidden = true;
    }
  });

  modelEl().addEventListener("change", async () => {
    if (fillingModels) {
      return;
    }
    const id = packId();
    if (id == null) {
      return;
    }
    const select = modelEl();
    modelChanging = true;
    showContext(null, lastWindow);
    try {
      const view = await invoke("ask_set_model", { id, model: select.value });
      fillModels(view);
      showError("");
    } catch (err) {
      showError(String(err));
      await loadModels();
    } finally {
      modelChanging = false;
    }
  });

  document.querySelector("#open").addEventListener("click", async () => {
    const id = packId();
    if (id == null) {
      return;
    }
    try {
      await invoke("ask_open", { id });
      showError("");
    } catch (err) {
      showError(String(err));
    }
  });

  document.querySelector("#save").addEventListener("click", async () => {
    const id = packId();
    if (id == null) {
      return;
    }
    const button = document.querySelector("#save");
    button.disabled = true;
    try {
      const path = await invoke("ask_save", { id });
      if (path) {
        const saved = savedEl();
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

  keepLinksInPage(answerEl());
  refresh();
  loadModels();
  setInterval(refresh, 800);
});
