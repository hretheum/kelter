/* First-run launcher: pick standalone vs remote, then hand off to the same
 * web UI the daemon serves. All state lives behind the shell commands. */

const invoke = window.__TAURI__.core.invoke;
const status = (msg) => { document.getElementById("status").textContent = msg; };

async function goStandalone() {
  status("Starting embedded kernel…");
  try {
    const [url, token] = await invoke("start_standalone");
    await invoke("save_settings", { settings: { mode: "standalone" } });
    await invoke("open_ui", { url, token });
  } catch (e) {
    status(`Failed to start standalone kernel:\n${e}`);
  }
}

async function goRemote(url, token) {
  try {
    await invoke("save_settings", { settings: { mode: "remote", url, token } });
    await invoke("open_ui", { url, token });
  } catch (e) {
    status(`Failed to open remote UI:\n${e}`);
  }
}

document.getElementById("standalone").onclick = goStandalone;
document.getElementById("remote").onclick = () => {
  document.getElementById("remote-form").style.display = "block";
};
document.getElementById("connect").onclick = () => {
  const url = document.getElementById("url").value.trim();
  const token = document.getElementById("token").value.trim();
  if (!url || !token) { status("URL and token are both required."); return; }
  goRemote(url, token);
};

/* Not first run? Jump straight to the remembered topology. */
(async () => {
  const settings = await invoke("get_settings");
  if (settings.mode === "standalone") goStandalone();
  else if (settings.mode === "remote" && settings.url && settings.token) {
    goRemote(settings.url, settings.token);
  }
})();
