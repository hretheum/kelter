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
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
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

if ("serviceWorker" in navigator && location.protocol.startsWith("http")) {
  navigator.serviceWorker.register("sw.js").catch(() => {});
}

if (state.token) connect();
