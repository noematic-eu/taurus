const { invoke } = window.__TAURI__.core;

function showError(msg) {
  const el = document.querySelector("#error");
  el.hidden = !msg;
  el.textContent = msg || "";
}

window.addEventListener("DOMContentLoaded", () => {
  const doc = document.querySelector("#doc");
  doc.addEventListener("click", (event) => {
    if (event.target.closest("a")) {
      event.preventDefault();
    }
  });

  invoke("link_report")
    .then((html) => {
      doc.innerHTML = html || "";
      showError("");
    })
    .catch((err) => {
      doc.innerHTML = "";
      showError(String(err));
    });
});
