// Phase 1 walking-skeleton chrome. Pure UI: it only sends worker commands and
// renders worker events. It never touches window lifecycle or native handles
// (CONTEXT D-02/D-09).
//
// The real IPC wiring (invoke/listen) lands with the worker in a later plan;
// this skeleton page contains only the chrome the layout contract requires.

const logPane = document.getElementById("log");

function appendEvent(level: string, message: string): void {
  if (!logPane) return;
  const line = document.createElement("div");
  line.className = `event ${level}`;
  line.textContent = `[${level}] ${message}`;
  logPane.appendChild(line);
  logPane.scrollTop = logPane.scrollHeight;
}

for (const id of ["import", "preview", "export"] as const) {
  document.getElementById(id)?.addEventListener("click", () => {
    appendEvent("info", `${id}: not wired yet (worker lands in a later plan)`);
  });
}
