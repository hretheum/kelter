// Pure pieces of the runner: config + request validation, ids, the claude
// command line and environment, result shaping. No I/O here except
// `loadConfig`, so the rules are testable without git or a model.
import { createHash, timingSafeEqual } from "node:crypto";
import { readFileSync } from "node:fs";

export const MODELS = ["haiku", "sonnet", "opus"];
export const EFFORTS = ["low", "medium", "high"];
const NAME = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const BRANCH = /^kelter\/[0-9a-f]{12}$/;
// A plain branch name: no leading '-' (it would reach git as an option), no '..'.
const BASE = /^(?!-)[A-Za-z0-9._/-]{1,100}$/;

export class BadRequest extends Error {}

export function loadConfig(path) {
  return validateConfig(JSON.parse(readFileSync(path, "utf8")));
}

export function validateConfig(cfg) {
  const out = {
    repos_dir: cfg.repos_dir ?? "/repos",
    work_dir: cfg.work_dir ?? "/work",
    state_dir: cfg.state_dir ?? "/state",
    out_dir: cfg.out_dir ?? "/out",
    max_time_s: cfg.max_time_s ?? 7200,
    default_time_s: cfg.default_time_s ?? 1800,
    // Task directories whose caches (cargo, target, pnpm) are kept; older
    // tasks keep only their clone and bundle.
    keep_caches: cfg.keep_caches ?? 10,
    // The unprivileged uid/gid that runs claude and the checks; null = run
    // as the runner itself (tests). The runner's secrets are not readable by it.
    worker: cfg.worker === undefined ? { uid: 1001, gid: 1001 } : cfg.worker,
    budget_usd: { haiku: 1, sonnet: 3, opus: 10, ...(cfg.budget_usd ?? {}) },
    // Full model ids, used to pin subagents and the `opus` alias inside a
    // non-Opus session to the run's own model.
    model_ids: {
      haiku: "claude-haiku-5-5", sonnet: "claude-sonnet-5-5", opus: "claude-opus-5-5",
      ...(cfg.model_ids ?? {}),
    },
    repos: cfg.repos ?? {},
    checks: cfg.checks ?? {},
    // KRW.2: the CLM service for path recommendations (shadow mode). Absent =
    // /v1/recommend answers ok:false and the run goes on by the skill's rules.
    // Where CLM runs (GPU, CPU, another host) is only this URL.
    clm: cfg.clm ? { url: cfg.clm.url, model: cfg.clm.model ?? "clm-latest", timeout_s: cfg.clm.timeout_s ?? 60, api_key: cfg.clm.api_key ?? null } : null,
  };
  if (out.clm && (typeof out.clm.url !== "string" || !/^https?:\/\//.test(out.clm.url))) {
    throw new Error("config: clm.url must be an http(s) URL");
  }
  for (const name of Object.keys(out.repos)) {
    if (!NAME.test(name)) throw new Error(`config: bad repo name '${name}'`);
  }
  for (const [name, c] of Object.entries(out.checks)) {
    if (!NAME.test(name)) throw new Error(`config: bad check name '${name}'`);
    if (!Object.hasOwn(out.repos, c.repo)) throw new Error(`config: check '${name}' names unknown repo '${c.repo}'`);
    if (!Array.isArray(c.cmd) || c.cmd.length === 0 || !c.cmd.every((s) => typeof s === "string")) {
      throw new Error(`config: check '${name}' needs cmd as a non-empty array of strings`);
    }
  }
  return out;
}

/// One id per idempotency key: the same key always maps to the same task,
/// branch and stored result (exactly-once on a re-dispatch, ADR 0031).
export function taskId(idempotencyKey) {
  return createHash("sha256").update(idempotencyKey).digest("hex").slice(0, 12);
}

export function branchFor(id) {
  return `kelter/${id}`;
}

function str(body, field, { max = 20000 } = {}) {
  const v = body[field];
  if (typeof v !== "string" || v.trim() === "") throw new BadRequest(`'${field}' is required`);
  if (v.length > max) throw new BadRequest(`'${field}' is longer than ${max} characters`);
  return v;
}

function object(body) {
  if (body === null || typeof body !== "object" || Array.isArray(body)) throw new BadRequest("body must be a JSON object");
  return body;
}

export function parseRun(body, cfg) {
  object(body);
  const key = str(body, "idempotency_key", { max: 200 });
  const repo = str(body, "repo", { max: 64 });
  if (!Object.hasOwn(cfg.repos, repo)) throw new BadRequest(`unknown repo '${repo}' (known: ${Object.keys(cfg.repos).join(", ")})`);
  const model = str(body, "model", { max: 16 });
  if (!MODELS.includes(model)) throw new BadRequest(`model must be one of ${MODELS.join(", ")}`);
  const effort = body.effort ?? "medium";
  if (!EFFORTS.includes(effort)) throw new BadRequest(`effort must be one of ${EFFORTS.join(", ")}`);
  const time = body.limit_czasu_s ?? cfg.default_time_s;
  if (!Number.isInteger(time) || time < 60 || time > cfg.max_time_s) {
    throw new BadRequest(`limit_czasu_s must be an integer in 60..${cfg.max_time_s}`);
  }
  const base = body.baza ?? cfg.repos[repo].base ?? "main";
  if (typeof base !== "string" || !BASE.test(base) || base.includes("..")) {
    throw new BadRequest("'baza' must be a plain branch name");
  }
  return { key, repo, model, effort, time, base, prompt: str(body, "zlecenie") };
}

export function parseCheck(body, cfg) {
  object(body);
  const name = str(body, "sprawdzenie", { max: 64 });
  if (!Object.hasOwn(cfg.checks, name)) throw new BadRequest(`unknown check '${name}' (known: ${Object.keys(cfg.checks).join(", ")})`);
  const branch = str(body, "galaz", { max: 64 });
  if (!BRANCH.test(branch)) throw new BadRequest("'galaz' must be a branch returned by a run (kelter/<12 hex>)");
  return { name, check: cfg.checks[name], branch };
}

export function systemNote(branch) {
  return [
    `You are working in a git clone on branch ${branch}, started for an orchestrator (Kelter).`,
    "Do not push, do not switch branches, do not change git remotes or hooks.",
    "Do not start other Claude sessions or subagents on a different model.",
    "When done, commit your changes on this branch with a short message.",
    "End with a brief summary: what changed and how you verified it.",
  ].join(" ");
}

/// The prompt goes in on stdin (a brief starting with '-' must never be
/// parsed as an option).
export function claudeArgs(run, cfg, branch) {
  return [
    "-p",
    "--output-format", "json",
    "--model", run.model,
    "--effort", run.effort,
    "--max-budget-usd", String(cfg.budget_usd[run.model]),
    "--permission-mode", "bypassPermissions",
    "--no-session-persistence",
    // A nested `claude` would be a separate session, invisible in this
    // one's modelUsage. Not airtight (see README), but closes the obvious path.
    "--disallowedTools", "Bash(claude:*)", "Bash(*claude -p*)",
    "--append-system-prompt", systemNote(branch),
  ];
}

/// Per-task directories: everything a session or a check can write that a
/// later task could read (HOME dotfiles, cargo config and registry, build
/// output, pnpm store) is private to the task.
export function taskPaths(cfg, id) {
  const dir = `${cfg.work_dir}/${id}`;
  return {
    dir, repo: `${dir}/repo`, home: `${dir}/home`, bundle: `${dir}/wynik.bundle`,
    cargo: `${dir}/cargo`, target: `${dir}/target`, pnpm: `${dir}/pnpm`, tmp: `${dir}/tmp`,
  };
}

/// The whole environment of a session or a check — nothing inherited from
/// the runner, so the runner token never reaches it. Only a claude session
/// gets the subscription token; checks (which run code the model wrote) get
/// none. A non-Opus session has its subagents and the `opus` alias pinned to
/// its own model, so it cannot reach Opus by accident.
export function workerEnv(cfg, paths, { model, oauthToken } = {}) {
  const env = {
    PATH: "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin",
    HOME: paths.home,
    LANG: "C.UTF-8",
    RUSTUP_HOME: "/usr/local/rustup",
    CARGO_HOME: paths.cargo,
    CARGO_TARGET_DIR: paths.target,
    npm_config_store_dir: paths.pnpm,
    TMPDIR: paths.tmp,
    // pnpm versions are baked into the image (read-only); never download.
    COREPACK_HOME: "/opt/corepack",
    COREPACK_ENABLE_DOWNLOAD_PROMPT: "0",
    COREPACK_ENABLE_NETWORK: "0",
    DISABLE_AUTOUPDATER: "1",
    GIT_AUTHOR_NAME: "Kelter (Claude Code)", GIT_AUTHOR_EMAIL: "kelter@localhost",
    GIT_COMMITTER_NAME: "Kelter (Claude Code)", GIT_COMMITTER_EMAIL: "kelter@localhost",
  };
  if (model !== undefined) {
    env.CLAUDE_CODE_OAUTH_TOKEN = oauthToken;
    if (model !== "opus") {
      env.CLAUDE_CODE_SUBAGENT_MODEL = cfg.model_ids[model];
      env.ANTHROPIC_DEFAULT_OPUS_MODEL = cfg.model_ids[model];
    }
  }
  return env;
}

const clip = (s, n) => (s.length > n ? `…${s.slice(-n)}` : s);

/// Models a session actually used, from claude's `modelUsage`.
export function modelsUsed(parsed) {
  return parsed?.modelUsage && typeof parsed.modelUsage === "object" ? Object.keys(parsed.modelUsage) : [];
}

/// Shape what goes back to the kernel. `ok` is the runner's verdict on the
/// *execution* only; whether the work is right is the contract's job
/// (ADR 0037), via a separate `sprawdz` call. A non-Opus run that used Opus
/// anyway is a failure: the human never approved it.
export function runResult({ id, run, branch, exit, stdout, stderr, timedOut, seconds, commit, diffStat, bundle }) {
  let parsed = null;
  try { parsed = JSON.parse(stdout); } catch { /* not JSON: claude failed before answering */ }
  const used = modelsUsed(parsed);
  const opusWithoutApproval = run.model !== "opus" && used.some((m) => /opus/i.test(m));
  const clean = !timedOut && exit === 0 && parsed?.type === "result" && parsed.is_error === false && parsed.subtype === "success";
  const ok = clean && !opusWithoutApproval;
  let blad = null;
  if (opusWithoutApproval) blad = `naruszenie: sesja ${run.model} użyła Opusa bez zgody (${used.join(", ")})`;
  else if (timedOut) blad = `przekroczony limit czasu (${run.time} s)`;
  else if (!clean) blad = clip(parsed?.subtype ?? (stderr || stdout || `exit ${exit}`), 2000);
  return {
    ok,
    zadanie: id,
    galaz: branch,
    model: run.model,
    modele_uzyte: used,
    wynik: parsed?.result ? clip(String(parsed.result), 4000) : null,
    blad,
    koszt_usd: typeof parsed?.total_cost_usd === "number" ? parsed.total_cost_usd : null,
    tury: parsed?.num_turns ?? null,
    czas_s: seconds,
    commit,
    zmiany: diffStat,
    bundle,
  };
}

export function authorized(header, token) {
  const want = Buffer.from(`Bearer ${token}`);
  const got = Buffer.from(header ?? "");
  return got.length === want.length && timingSafeEqual(got, want);
}

// ---- KRW.2: path recommendation (CLM, shadow mode) --------------------------

/* The one question CLM answers per task. Options are described in plain
 * English prose — what the heads were trained on — and map 1:1 onto the
 * skill's paths. Keys are what Kelter's journal records. */
export const PATH_QUESTION = {
  type: "choice",
  instructions: "Which execution path should handle this task at the lowest cost that will still succeed?",
  criteria: {
    bez: "No coding agent is needed: the task is reading, summarizing, listing or writing notes, with no code change in a repository.",
    haiku: "A small, mechanical code change: a typo, a configuration entry, a version bump, or one clearly described function.",
    sonnet: "A typical coding task: fix a bug, add a test, or build a small feature across a few files.",
    opus: "A large or risky coding task: core runtime logic, concurrency or exactly-once semantics, migrations, security, or a task a smaller model already failed.",
  },
};

export function parseRecommend(body) {
  object(body);
  const task = str(body, "zlecenie", { max: 8000 });
  const repo = body.repo === undefined ? null : str(body, "repo", { max: 64 });
  return { task, repo };
}

export function clmRequest(req, cfg) {
  const state = req.repo ? { task: req.task, repository: req.repo } : { task: req.task };
  return { model: cfg.clm.model, state, questions: { sciezka: PATH_QUESTION } };
}

/* CLM's answer → what the kernel records. A malformed answer is a failure,
 * never a guessed recommendation. */
export function recommendResult(answer, latencyMs) {
  const a = answer?.answers?.sciezka;
  const probs = a?.probabilities;
  if (!a || typeof a.choice !== "string" || !Object.hasOwn(PATH_QUESTION.criteria, a.choice) || !probs || typeof probs !== "object") {
    return { ok: false, blad: "CLM returned no usable answer for 'sciezka'" };
  }
  const p = Object.fromEntries(Object.keys(PATH_QUESTION.criteria).map((k) => [k, typeof probs[k] === "number" ? Math.round(probs[k] * 1000) / 1000 : null]));
  return {
    ok: true,
    rekomendacja: a.choice,
    prawdopodobienstwa: p,
    pewnosc: typeof a.confidence === "number" ? Math.round(a.confidence * 1000) / 1000 : null,
    opoznienie_ms: latencyMs,
    tryb: "cien",
  };
}
