const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);

function render(snapshot) {
  const status = document.querySelector("#status");
  status.textContent = snapshot.running
    ? `Gateway running at ${snapshot.listen}`
    : `Gateway stopped. Configured listen ${snapshot.listen}`;
  document.querySelector("#notice").textContent = snapshot.message || "";
  const body = document.querySelector("#agents");
  body.replaceChildren();
  for (const agent of snapshot.agents) {
    const row = document.createElement("tr");
    const name = document.createElement("td");
    name.textContent = agent.display_name;
    const installed = document.createElement("td");
    installed.textContent = agent.installed ? "yes" : "no";
    const action = document.createElement("td");
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = agent.connected ? "Disconnect" : "Connect";
    button.addEventListener("click", () => {
      invoke("set_agent", { id: agent.id, connect: !agent.connected })
        .then(render)
        .catch(showError);
    });
    action.append(button);
    row.append(name, installed, action);
    body.append(row);
  }
}

function showError(error) {
  document.querySelector("#notice").textContent = String(error);
}

async function refresh() {
  try {
    render(await invoke("snapshot"));
    document.querySelector("#usage").textContent = await invoke("usage_summary");
  } catch (error) {
    showError(error);
  }
}

window.addEventListener("DOMContentLoaded", () => {
  document.querySelector("#start").addEventListener("click", () => {
    invoke("start_gateway").then(render).catch(showError);
  });
  document.querySelector("#stop").addEventListener("click", () => {
    invoke("stop_gateway").then(render).catch(showError);
  });
  document.querySelector("#config").addEventListener("click", () => {
    invoke("open_config").then(refresh).catch(showError);
  });
  document.querySelector("#refresh").addEventListener("click", () => {
    refresh();
  });
  if (window.__TAURI__ && window.__TAURI__.event) {
    window.__TAURI__.event.listen("snapshot", (event) => {
      render(event.payload);
    });
  }
  refresh();
});
