/* Thin stateless client: the kernel owns all state; this page only renders it.
 * Works same-origin (served by the daemon) or against a remote kernel URL. */

const $ = (id) => document.getElementById(id);

const state = {
  base: localStorage.getItem("ak.base") || "",
  token: localStorage.getItem("ak.token") || "",
  ws: null,
};

// The desktop shell (and pairing links) pass the token in the URL fragment:
// http://127.0.0.1:PORT/#token=... — the fragment never reaches the server.
const hashToken = location.hash.match(/token=([^&]+)/);
if (hashToken) {
  state.token = decodeURIComponent(hashToken[1]);
  state.base = "";
}

$("server-url").value = state.base;
$("token").value = state.token;

function apiUrl(path) {
  return (state.base || "") + path;
}

async function api(path, opts = {}) {
  const res = await fetch(apiUrl(path), {
    ...opts,
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${state.token}`,
      ...(opts.headers || {}),
    },
  });
  if (!res.ok) {
    const err = new Error(`${res.status} ${await res.text()}`);
    err.status = res.status; // callers can branch on it (e.g. 501 = not mutable)
    throw err;
  }
  if (res.status === 204) return null; // no body (e.g. key stored)
  return res.json();
}

function setConn(ok) {
  const el = $("conn");
  el.textContent = ok ? "connected" : "disconnected";
  el.className = `pill ${ok ? "online" : "offline"}`;
  $("submit").disabled = !ok;
}

function logEvent(env) {
  const line = document.createElement("span");
  line.className = `ev-${env.event.type}`;
  const short = env.run_id.slice(0, 8);
  line.textContent = `[${short}] ${env.event.type} ${formatEvent(env.event)}\n`;
  const log = $("event-log");
  log.appendChild(line);
  log.scrollTop = log.scrollHeight;
}

function formatEvent(ev) {
  switch (ev.type) {
    case "created": return `task="${ev.task}" policy=${ev.policy}`;
    case "proposed": return `#${ev.step} ${ev.action} ${JSON.stringify(ev.arguments)}`;
    case "rejected": return `#${ev.step} attempt=${ev.attempt} ${ev.reason}`;
    case "executed": return `#${ev.step} ${ev.action} ok=${ev.ok}`;
    case "completed": return JSON.stringify(ev.output);
    case "failed": return ev.error;
    case "reflection_written": return `episode=${ev.episode_id.slice(0, 8)}`;
    default: return "";
  }
}

async function refreshRuns() {
  try {
    const runs = await api("/api/v1/runs");
    const list = $("run-list");
    list.innerHTML = "";
    for (const run of runs.reverse()) {
      const li = document.createElement("li");
      for (const [cls, text] of [
        [`status ${run.status}`, run.status],
        ["task", run.task],
        ["policy", run.policy],
      ]) {
        const span = document.createElement("span");
        span.className = cls;
        span.textContent = text;
        li.appendChild(span);
      }
      list.appendChild(li);
    }
  } catch (e) {
    console.warn("refreshRuns:", e);
  }
}

function connectWs() {
  if (state.ws) state.ws.close();
  const wsBase = (state.base || location.origin).replace(/^http/, "ws");
  // Browsers cannot set Authorization headers on WebSocket — token goes in the query.
  const ws = new WebSocket(`${wsBase}/api/v1/events?token=${encodeURIComponent(state.token)}`);
  ws.onopen = () => setConn(true);
  ws.onclose = () => setConn(false);
  ws.onmessage = (msg) => {
    const env = JSON.parse(msg.data);
    logEvent(env);
    if (["created", "completed", "failed"].includes(env.event.type)) refreshRuns();
  };
  state.ws = ws;
}

async function connect() {
  state.base = $("server-url").value.trim().replace(/\/$/, "");
  state.token = $("token").value.trim();
  localStorage.setItem("ak.base", state.base);
  localStorage.setItem("ak.token", state.token);
  try {
    await api("/api/v1/runs");
    connectWs();
    refreshRuns();
    routeByModelHealth(); // first-run routing: config screen vs run view
  } catch (e) {
    setConn(false);
    alert(`Cannot connect: ${e.message}`);
  }
}

$("connect").onclick = connect;

$("submit").onclick = async () => {
  const task = $("task").value.trim();
  if (!task) return;
  try {
    await api("/api/v1/runs", { method: "POST", body: JSON.stringify({ task }) });
    $("task").value = "";
  } catch (e) {
    alert(`Submit failed: ${e.message}`);
  }
};

// ---- Model configuration (Phase 8.5) ----------------------------------------
// Presets pre-fill the form. `scheme` builds the engine string (`scheme:model`);
// `test` is the provider name sent to the /test health-check endpoint.
const CFG_PRESETS = {
  llamacpp:   { base_url: "http://127.0.0.1:8080",       model: "qwen2.5-0.5b-instruct",     api: false },
  ollama:     { base_url: "http://127.0.0.1:11434/v1",   model: "qwen2.5:0.5b",              api: false },
  openrouter: { base_url: "https://openrouter.ai/api/v1", model: "anthropic/claude-sonnet-5", api: true },
};

// Which provider (as the /test endpoint names them: local | ollama | openrouter)
// the current form describes. For "Custom", derive it from the Local/API picker.
function currentKind() {
  const preset = $("cfg-preset").value;
  if (preset === "custom") return $("cfg-provider").value === "api" ? "openrouter" : "local";
  if (preset === "llamacpp") return "local";
  return preset; // "ollama" | "openrouter"
}

// engine = `${scheme}:${model}` — "local" for llama.cpp/ollama, "openrouter" for the API.
const schemeOf = (kind) => (kind === "openrouter" ? "openrouter" : "local");

// Show the API-key input only for API providers.
function applyProviderVisibility() {
  $("cfg-key-row").style.display = $("cfg-provider").value === "api" ? "block" : "none";
}

function applyPreset() {
  const p = $("cfg-preset").value;
  if (p !== "custom") {
    const c = CFG_PRESETS[p];
    $("cfg-base-url").value = c.base_url;
    $("cfg-model").value = c.model;
    $("cfg-provider").value = c.api ? "api" : "local";
  }
  applyProviderVisibility();
}

// Fill the form from GET /api/v1/config/model. `provider` is local | ollama | openrouter.
function prefillConfig(cfg) {
  const kind = cfg.provider || "local";
  $("cfg-preset").value = { local: "llamacpp", ollama: "ollama", openrouter: "openrouter" }[kind] || "custom";
  $("cfg-provider").value = kind === "openrouter" ? "api" : "local";
  $("cfg-base-url").value = cfg.base_url || "";
  $("cfg-model").value = cfg.model || "";
  $("cfg-key-note").textContent = cfg.api_key_set
    ? "A key is already stored server-side. Leave blank to keep it."
    : "stored server-side, never committed or logged; or set OPENROUTER_API_KEY env";
  applyProviderVisibility();
}

function setCfgResult(text, kind) {
  const el = $("cfg-result");
  el.textContent = text;
  el.className = "cfg-result" + (kind ? " " + kind : "");
}

// Toggle between the config screen and the normal run/submit view.
function showConfig(show) {
  $("config").hidden = !show;
  for (const id of ["submit-box", "runs", "events"]) $(id).hidden = show;
}

// Read-only mode: some deployments (daemon without a runtime config controller)
// return 501 for /config/model. Rather than show an editable form the backend
// would reject on Save, disable the fields and explain the model is fixed at
// deploy time. Standalone/daemon-with-controller stays fully editable.
function setConfigEditable(editable, note) {
  for (const id of ["cfg-provider", "cfg-preset", "cfg-base-url", "cfg-model", "cfg-key", "cfg-test", "cfg-save"]) {
    const el = $(id);
    if (el) el.disabled = !editable;
  }
  const res = $("cfg-result");
  if (res) {
    res.textContent = note || "";
    res.className = editable ? "cfg-result" : "cfg-result note";
  }
}

// First-run routing: after connecting, check the configured model is reachable.
// If GET fails or the /test says not-ok, land on the config screen instead of a
// dead run view. Never fakes the check — it hits the real /test endpoint.
async function routeByModelHealth() {
  let cfg;
  try {
    cfg = await api("/api/v1/config/model");
  } catch (e) {
    if (e.status === 501) {
      // Model is configured at deploy time and is NOT runtime-mutable here.
      // Honest: read-only config, run view stays (the model is already set).
      setConfigEditable(false, "Model is configured at deploy time in this deployment — not editable here.");
      showConfig(false);
      $("cfg-toggle").hidden = false;
      return;
    }
    console.warn("model config:", e);
    setConfigEditable(true);
    showConfig(true); // other error → let the user configure (first-run)
    $("cfg-toggle").hidden = false;
    return;
  }
  // 200: runtime-mutable. Editable form + real reachability routing.
  setConfigEditable(true);
  prefillConfig(cfg);
  let reachable = false;
  try {
    const res = await api("/api/v1/config/model/test", {
      method: "POST",
      body: JSON.stringify({ provider: cfg.provider || "local", base_url: cfg.base_url }),
    });
    reachable = !!res.ok;
  } catch (e) {
    console.warn("model health:", e);
  }
  showConfig(!reachable);          // unreachable → show config, hide run view
  $("cfg-toggle").hidden = false;  // reveal the header toggle once connected
}

$("cfg-provider").onchange = applyProviderVisibility;
$("cfg-preset").onchange = applyPreset;
$("cfg-toggle").onclick = () => showConfig($("config").hidden);

// Test connection: real health check against the selected provider + base_url.
$("cfg-test").onclick = async () => {
  const provider = currentKind();
  const base_url = $("cfg-base-url").value.trim();
  setCfgResult("Testing…", "");
  try {
    const res = await api("/api/v1/config/model/test", {
      method: "POST",
      body: JSON.stringify({ provider, base_url }),
    });
    setCfgResult(res.detail || (res.ok ? "ok" : "failed"), res.ok ? "ok" : "err");
  } catch (e) {
    setCfgResult(e.message, "err");
  }
};

// Save: store the key first (if entered), then set the model. 422 → show the error.
$("cfg-save").onclick = async () => {
  const base_url = $("cfg-base-url").value.trim();
  const model = $("cfg-model").value.trim();
  const engine = `${schemeOf(currentKind())}:${model}`;
  const key = $("cfg-key").value;
  setCfgResult("Saving…", "");
  try {
    if (key) {
      // Stored securely server-side; the response never echoes it back.
      await api("/api/v1/config/model/key", { method: "POST", body: JSON.stringify({ api_key: key }) });
      $("cfg-key").value = "";
    }
    const view = await api("/api/v1/config/model", {
      method: "POST",
      body: JSON.stringify({ engine, base_url }),
    });
    prefillConfig(view);
    setCfgResult("Saved. Model configured.", "ok");
    showConfig(false); // reveal the normal run view
  } catch (e) {
    setCfgResult(e.message, "err");
  }
};

applyPreset(); // sensible defaults before any GET prefill

if ("serviceWorker" in navigator && location.protocol.startsWith("http")) {
  navigator.serviceWorker.register("sw.js").catch(() => {});
}

if (state.token) connect();
