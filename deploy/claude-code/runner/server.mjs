// Claude Code runner: a small HTTP service that runs `claude -p` in a fresh
// git clone for Kelter, one task at a time. Kelter reaches it through the
// `claude-code` extension (a stdio MCP bridge); nothing else should.
//
//   POST /v1/run    {idempotency_key, repo, model, zlecenie, effort?, limit_czasu_s?, baza?}
//   POST /v1/check  {sprawdzenie, galaz}
//   POST /v1/recommend {zlecenie, repo?}   — KRW.2, CLM path recommendation (shadow)
//   GET  /healthz
//
// Exactly-once per idempotency key: a repeated key returns the stored result
// (or waits for the one in flight) and never starts a second execution. A
// task interrupted by a runner restart is reported as failed, never re-run.
//
// Isolation: the runner (container root = the host user, rootless) holds
// the secrets; claude and the checks run as an unprivileged worker uid with
// an explicit environment and per-task HOME/caches. Base repos are mounted
// read-only; results leave as a git bundle in out_dir — the host never runs
// git inside anything the model could write.
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import {
  appendFileSync, chownSync, closeSync, constants, existsSync, fstatSync, mkdirSync, openSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";
import {
  BadRequest, authorized, branchFor, claudeArgs, clmRequest, loadConfig, parseCheck, parseRecommend, parseRun, recommendResult, runResult,
  taskId, taskPaths, workerEnv,
} from "./lib.mjs";

const OUTPUT_CAP = 4 * 1024 * 1024;

export function exec(cmd, args, { cwd, env, timeoutS = 600, input, user } = {}) {
  return new Promise((resolve) => {
    const child = spawn(cmd, args, {
      cwd, env: env ?? process.env, detached: true, stdio: ["pipe", "pipe", "pipe"], ...(user ?? {}),
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let done = false;
    const keep = (acc, chunk) => (acc.length < OUTPUT_CAP ? acc + chunk : acc);
    child.stdout.on("data", (c) => { stdout = keep(stdout, c); });
    child.stderr.on("data", (c) => { stderr = keep(stderr, c); });
    child.stdin.on("error", () => {});
    child.stdin.end(input ?? "");
    const killGroup = () => { try { process.kill(-child.pid, "SIGKILL"); } catch { /* gone */ } };
    const timer = setTimeout(() => { timedOut = true; killGroup(); }, timeoutS * 1000);
    const finish = (code) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      killGroup(); // a leftover grandchild must not keep the lane busy
      resolve({ exit: code ?? -1, stdout, stderr, timedOut });
    };
    child.on("error", (e) => { stderr += String(e); finish(-1); });
    // Decide on the process exit; give the pipes a moment to drain, but a
    // descendant holding them open must not hang the lane.
    child.on("exit", (code) => setTimeout(() => finish(code), 2000));
    child.on("close", (code) => finish(code));
  });
}

/// SIGKILL every process owned by `uid` (the runner is root in its user
/// namespace, so it may). Called after each worker exec: one task at a time,
/// so nothing of the worker may outlive its task — not even a `setsid`
/// escapee that left the process group.
export function killUid(uid) {
  for (let round = 0; round < 5; round++) {
    let found = 0;
    for (const pid of readdirSync("/proc").filter((d) => /^\d+$/.test(d))) {
      let status;
      try { status = readFileSync(`/proc/${pid}/status`, "utf8"); } catch { continue; }
      const m = status.match(/^Uid:\s+(\d+)/m);
      if (m && Number(m[1]) === uid) {
        found++;
        try { process.kill(Number(pid), "SIGKILL"); } catch { /* gone */ }
      }
    }
    if (found === 0) return;
  }
}

export function createRunner(cfg, { claudeBin = "claude", oauthToken = "", passEnv = {}, clmKey = null } = {}) {
  const runsDir = join(cfg.state_dir, "runs");
  mkdirSync(runsDir, { recursive: true });
  mkdirSync(cfg.work_dir, { recursive: true });
  mkdirSync(cfg.out_dir, { recursive: true });
  const user = cfg.worker ? { uid: cfg.worker.uid, gid: cfg.worker.gid } : undefined;
  const inflight = new Map();
  let lane = Promise.resolve();
  const enqueue = (fn) => {
    const p = lane.then(fn, fn);
    lane = p.catch(() => {});
    return p;
  };

  const statePath = (id) => join(runsDir, `${id}.json`);
  const readState = (id) => (existsSync(statePath(id)) ? JSON.parse(readFileSync(statePath(id), "utf8")) : null);
  const writeState = (id, s) => {
    writeFileSync(`${statePath(id)}.tmp`, JSON.stringify(s, null, 2));
    renameSync(`${statePath(id)}.tmp`, statePath(id));
  };

  // A task still "running" at startup was cut off by a restart: report it,
  // do not resume it (its clone may hold half-done work worth a look).
  for (const f of readdirSync(runsDir)) {
    if (!f.endsWith(".json")) continue;
    const id = f.slice(0, -5);
    const s = readState(id);
    if (s?.status === "running") {
      writeState(id, { ...s, status: "done", result: { ok: false, zadanie: id, galaz: branchFor(id), blad: "przerwane restartem runnera — nie wznowione; zmiany (jeśli są) zostały w klonie zadania" } });
    }
  }

  const fingerprint = (run) => createHash("sha256").update(JSON.stringify([run.repo, run.model, run.effort, run.time, run.base, run.prompt])).digest("hex");
  const repoPath = (repo) => cfg.repos[repo].path ?? join(cfg.repos_dir, `${repo}.git`);
  const envFor = (paths, opts) => ({ ...workerEnv(cfg, paths, opts), ...passEnv });
  // Every worker exec goes through here: afterwards no worker process survives.
  const workerExec = async (cmd, args, opts) => {
    try {
      return await exec(cmd, args, { ...opts, user });
    } finally {
      if (user) killUid(user.uid);
    }
  };

  async function git(args, cwd, paths) {
    const r = await workerExec("git", args, { cwd, env: envFor(paths), timeoutS: 300 });
    if (r.exit !== 0) throw new Error(`git ${args[0]}: ${r.stderr.trim() || r.stdout.trim()}`);
    return r.stdout.trim();
  }

  function prepare(paths) {
    for (const d of [paths.dir, paths.home, paths.cargo, paths.target, paths.pnpm, paths.tmp]) mkdirSync(d, { recursive: true });
    // The clone is the worker's, not the runner's: let its git read it.
    writeFileSync(join(paths.home, ".gitconfig"), "[safe]\n\tdirectory = *\n");
    if (user) {
      for (const d of [paths.dir, paths.home, paths.cargo, paths.target, paths.pnpm, paths.tmp]) chownSync(d, user.uid, user.gid);
      chownSync(join(paths.home, ".gitconfig"), user.uid, user.gid);
    }
  }

  /// Move the worker's bundle out, refusing anything but a regular file
  /// (the worker could have planted a symlink to a runner secret).
  function exportBundle(paths, id) {
    let fd;
    try {
      fd = openSync(paths.bundle, constants.O_RDONLY | constants.O_NOFOLLOW);
    } catch (e) {
      if (e.code === "ENOENT") return null;
      throw new Error(`wynik.bundle refused: ${e.code}`);
    }
    try {
      const st = fstatSync(fd);
      if (!st.isFile() || st.nlink !== 1) throw new Error("wynik.bundle is not a regular file — refused");
      const data = readFileSync(fd);
      const dest = join(cfg.out_dir, `${id}.bundle`);
      writeFileSync(dest, data);
      return { plik: `${id}.bundle`, sha256: createHash("sha256").update(data).digest("hex") };
    } finally {
      closeSync(fd);
    }
  }

  function pruneCaches() {
    const tasks = readdirSync(cfg.work_dir)
      .filter((d) => /^[0-9a-f]{12}$/.test(d))
      .map((d) => ({ d, t: statSync(join(cfg.work_dir, d)).mtimeMs }))
      .sort((a, b) => b.t - a.t);
    for (const { d } of tasks.slice(cfg.keep_caches)) {
      const p = taskPaths(cfg, d);
      for (const c of [p.cargo, p.target, p.pnpm]) rmSync(c, { recursive: true, force: true });
    }
  }

  async function execute(id, run) {
    const branch = branchFor(id);
    const paths = taskPaths(cfg, id);
    const started = Date.now();
    prepare(paths);
    await git(["clone", "-q", "--no-local", "--no-hardlinks", "--branch", run.base, "--", repoPath(run.repo), paths.repo], paths.dir, paths);
    await git(["checkout", "-q", "-b", branch], paths.repo, paths);
    const r = await workerExec(claudeBin, claudeArgs(run, cfg, branch), {
      cwd: paths.repo, env: envFor(paths, { model: run.model, oauthToken }), timeoutS: run.time, input: run.prompt,
    });
    let commit = null;
    let diffStat = null;
    let bundle = null;
    try {
      await git(["add", "-A"], paths.repo, paths);
      if ((await git(["status", "--porcelain"], paths.repo, paths)) !== "") {
        await git(["commit", "-q", "-m", `kelter ${id}: zmiany pozostawione bez commita`], paths.repo, paths);
      }
      commit = await git(["rev-parse", "HEAD"], paths.repo, paths);
      diffStat = (await git(["diff", "--stat", `${run.base}...HEAD`], paths.repo, paths)).split("\n").slice(-20).join("\n") || null;
      if (diffStat) {
        rmSync(paths.bundle, { force: true }); // whatever the session left there
        await git(["bundle", "create", "-q", paths.bundle, branch, `^${run.base}`], paths.repo, paths);
        bundle = exportBundle(paths, id);
      }
    } catch (e) {
      diffStat = `nie udało się odczytać zmian: ${e.message}`;
    }
    return runResult({ id, run, branch, ...r, seconds: Math.round((Date.now() - started) / 1000), commit, diffStat, bundle });
  }

  async function run(body) {
    const req = parseRun(body, cfg);
    const id = taskId(req.key);
    const fp = fingerprint(req);
    const prior = readState(id);
    if (prior && prior.fingerprint !== fp) {
      throw new BadRequest(`idempotency_key reused with different parameters (task ${id})`);
    }
    if (prior?.status === "done") return prior.result;
    if (inflight.has(id)) return inflight.get(id);
    const request = { ...req, prompt: undefined };
    writeState(id, { status: "running", fingerprint: fp, request, started_at: new Date().toISOString() });
    const p = enqueue(async () => {
      let result;
      try {
        result = await execute(id, req);
      } catch (e) {
        result = { ok: false, zadanie: id, galaz: branchFor(id), blad: String(e.message ?? e) };
      }
      writeState(id, { status: "done", fingerprint: fp, request, result });
      try { pruneCaches(); } catch { /* best effort */ }
      return result;
    }).finally(() => inflight.delete(id));
    inflight.set(id, p);
    return p;
  }

  async function check(body) {
    const { name, check: c, branch } = parseCheck(body, cfg);
    const id = branch.slice("kelter/".length);
    const paths = taskPaths(cfg, id);
    const state = readState(id);
    if (!state || !existsSync(paths.repo)) throw new BadRequest(`no task for ${branch} on this runner`);
    if (state.request?.repo !== c.repo) throw new BadRequest(`check '${name}' is for repo '${c.repo}', branch ${branch} is not`);
    // A check is evidence only for a task that ran cleanly and changed
    // something: tests passing on an untouched base prove nothing.
    const r0 = state.result;
    if (!r0?.ok || !r0.bundle) {
      return { ok: false, sprawdzenie: name, galaz: branch, blad: `zadanie ${id} nie zakończyło się sukcesem albo nie zmieniło nic względem bazy — sprawdzenie nie byłoby dowodem` };
    }
    return enqueue(async () => {
      const started = Date.now();
      const r = await workerExec(c.cmd[0], c.cmd.slice(1), { cwd: paths.repo, env: envFor(paths), timeoutS: c.timeout_s ?? 1800 });
      const out = `${r.stdout}\n${r.stderr}`.trim();
      return {
        ok: !r.timedOut && r.exit === 0,
        sprawdzenie: name,
        galaz: branch,
        commit: r0.commit,
        kod_wyjscia: r.timedOut ? null : r.exit,
        blad: r.timedOut ? `przekroczony limit czasu (${c.timeout_s ?? 1800} s)` : null,
        czas_s: Math.round((Date.now() - started) / 1000),
        wyjscie: out.length > 3000 ? `…${out.slice(-3000)}` : out,
      };
    });
  }

  /// KRW.2: ask CLM which path fits the task. Shadow mode: the answer is
  /// recorded (here and in Kelter's journal), never acted on by the runner.
  /// Not queued behind tasks — it runs no worker code.
  async function recommend(body) {
    const req = parseRecommend(body);
    if (!cfg.clm) return { ok: false, blad: "CLM not configured on this runner (clm.url)" };
    const started = Date.now();
    let result;
    try {
      const res = await fetch(`${cfg.clm.url.replace(/\/$/, "")}/v1/systemone`, {
        method: "POST",
        headers: { "content-type": "application/json", ...(clmKey ? { authorization: `Bearer ${clmKey}` } : {}) },
        body: JSON.stringify(clmRequest(req, cfg)),
        signal: AbortSignal.timeout(cfg.clm.timeout_s * 1000),
      });
      const text = await res.text();
      result = res.ok
        ? recommendResult(JSON.parse(text), Date.now() - started)
        : { ok: false, blad: `CLM HTTP ${res.status}: ${text.slice(0, 300)}` };
    } catch (e) {
      result = { ok: false, blad: `CLM unreachable: ${e.message ?? e}` };
    }
    try {
      appendFileSync(join(cfg.state_dir, "recommendations.jsonl"),
        `${JSON.stringify({ at: new Date().toISOString(), repo: req.repo, task_sha256: createHash("sha256").update(req.task).digest("hex").slice(0, 16), ...result })}\n`);
    } catch { /* the journal in Kelter is the record that matters */ }
    return result;
  }

  return { run, check, recommend };
}

export function createHttpServer(runner, token) {
  const server = createServer((req, res) => {
    const send = (code, obj) => {
      res.writeHead(code, { "content-type": "application/json" });
      res.end(JSON.stringify(obj));
    };
    if (req.method === "GET" && req.url === "/healthz") return send(200, { ok: true });
    if (!authorized(req.headers.authorization, token)) return send(401, { error: "unauthorized" });
    const handler = req.method === "POST" && { "/v1/run": runner.run, "/v1/check": runner.check, "/v1/recommend": runner.recommend }[req.url];
    if (!handler) return send(404, { error: "not found" });
    let raw = "";
    req.setEncoding("utf8");
    req.on("data", (c) => {
      raw += c;
      if (raw.length > 100_000) req.destroy();
    });
    req.on("end", async () => {
      try {
        send(200, await handler(JSON.parse(raw || "{}")));
      } catch (e) {
        if (e instanceof BadRequest || e instanceof SyntaxError) send(400, { error: e.message });
        else send(500, { error: String(e.message ?? e) });
      }
    });
  });
  server.requestTimeout = 0; // a task may run for hours; limits are per task
  return server;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const read = (p) => readFileSync(p, "utf8").trim();
  const conf = process.env.RUNNER_CONFIG_DIR ?? "/etc/claude-code-runner";
  const cfg = loadConfig(join(conf, "config.json"));
  const token = read(join(conf, "runner-token"));
  if (token.length < 32) throw new Error("runner token must be at least 32 characters");
  // Test hook only: names of variables copied into the worker environment.
  const passEnv = Object.fromEntries((process.env.RUNNER_PASS_ENV ?? "").split(",").filter(Boolean).map((k) => [k, process.env[k] ?? ""]));
  const runner = createRunner(cfg, {
    claudeBin: process.env.RUNNER_CLAUDE_BIN ?? "claude",
    oauthToken: read(join(conf, "claude-oauth-token")),
    passEnv,
    clmKey: cfg.clm?.api_key ? read(join(conf, cfg.clm.api_key)) : null,
  });
  const port = Number(process.env.PORT ?? 8790);
  createHttpServer(runner, token).listen(port, "0.0.0.0", () => {
    console.log(`claude-code-runner on :${port}, repos: ${Object.keys(cfg.repos).join(", ")}, checks: ${Object.keys(cfg.checks).join(", ")}, worker: ${cfg.worker ? cfg.worker.uid : "self"}`);
  });
}
