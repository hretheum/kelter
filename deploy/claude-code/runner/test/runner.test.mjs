import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import {
  BadRequest, authorized, claudeArgs, parseCheck, parseRun, runResult, taskId, taskPaths, validateConfig, workerEnv,
} from "../lib.mjs";
import { createHttpServer, createRunner, exec } from "../server.mjs";

const here = fileURLToPath(new URL(".", import.meta.url));
const fake = join(here, "fake-claude.sh");
chmodSync(fake, 0o755);

function fixture({ checks } = {}) {
  const root = mkdtempSync(join(tmpdir(), "ccr-"));
  const src = join(root, "src");
  const ident = { GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t" };
  const g = (args, cwd) => execFileSync("git", args, { cwd, stdio: "pipe", env: { ...process.env, ...ident } });
  g(["init", "-q", "-b", "main", src]);
  writeFileSync(join(src, "README"), "x\n");
  g(["add", "."], src);
  g(["commit", "-q", "-m", "init"], src);
  g(["clone", "-q", "--bare", src, join(root, "repos", "demo.git")]);
  const cfg = validateConfig({
    repos_dir: join(root, "repos"),
    work_dir: join(root, "work"),
    state_dir: join(root, "state"),
    out_dir: join(root, "out"),
    worker: null,
    repos: { demo: {} },
    checks: checks ?? {
      "plik-jest": { repo: "demo", cmd: ["test", "-f", "wynik.txt"] },
      "zawsze-zle": { repo: "demo", cmd: ["sh", "-c", "echo boom; exit 2"] },
      "bez-sekretow": { repo: "demo", cmd: ["sh", "-c", "test -z \"$CLAUDE_CODE_OAUTH_TOKEN\" && test -z \"$SEKRET_RUNNERA\""] },
    },
  });
  const log = join(root, "fake.log");
  // A variable that exists only in the runner's own environment.
  process.env.SEKRET_RUNNERA = "nie-dla-workera";
  const runner = createRunner(cfg, { claudeBin: fake, oauthToken: "oauth-test", passEnv: { FAKE_LOG: log } });
  return { root, cfg, log, runner };
}

const req = (over = {}) => ({ idempotency_key: "run1:0:a", repo: "demo", model: "sonnet", zlecenie: "zrób", ...over });
const lines = (p) => (existsSync(p) ? readFileSync(p, "utf8").trim().split("\n") : []);

test("config refuses a check on an unknown repo and a non-array cmd", () => {
  assert.throws(() => validateConfig({ repos: {}, checks: { x: { repo: "nope", cmd: ["true"] } } }), /unknown repo/);
  assert.throws(() => validateConfig({ repos: {}, checks: { x: { repo: "constructor", cmd: ["true"] } } }), /unknown repo/);
  assert.throws(() => validateConfig({ repos: { r: {} }, checks: { x: { repo: "r", cmd: "true" } } }), /cmd/);
});

test("parseRun validates model, effort, time, base, repo and body shape", () => {
  const cfg = validateConfig({ repos: { demo: {} } });
  assert.equal(parseRun(req(), cfg).time, 1800);
  for (const bad of [{ model: "gpt" }, { effort: "max" }, { limit_czasu_s: 10 }, { limit_czasu_s: 99999 }, { baza: "../x" },
    { baza: "--no-checkout" }, { baza: "-f" }, { repo: "inne" }, { repo: "constructor" }, { zlecenie: " " }]) {
    assert.throws(() => parseRun(req(bad), cfg), BadRequest, JSON.stringify(bad));
  }
  assert.throws(() => parseRun(null, cfg), BadRequest);
  assert.throws(() => parseRun([], cfg), BadRequest);
});

test("parseCheck accepts only configured checks and runner-shaped branches", () => {
  const cfg = validateConfig({ repos: { demo: {} }, checks: { t: { repo: "demo", cmd: ["true"] } } });
  assert.equal(parseCheck({ sprawdzenie: "t", galaz: "kelter/0123456789ab" }, cfg).name, "t");
  assert.throws(() => parseCheck({ sprawdzenie: "toString", galaz: "kelter/0123456789ab" }, cfg), BadRequest);
  assert.throws(() => parseCheck({ sprawdzenie: "t", galaz: "main" }, cfg), BadRequest);
});

test("claude: headless, per-model budget, prompt never on the command line", () => {
  const cfg = validateConfig({ repos: { demo: {} } });
  const run = parseRun(req({ model: "opus", effort: "high", zlecenie: "- tajny brief" }), cfg);
  const args = claudeArgs(run, cfg, "kelter/abc");
  const at = (flag) => args[args.indexOf(flag) + 1];
  assert.equal(at("--model"), "opus");
  assert.equal(at("--effort"), "high");
  assert.equal(at("--max-budget-usd"), "10");
  assert.match(at("--append-system-prompt"), /Do not push/);
  assert.ok(!args.includes("- tajny brief"));
});

test("worker env: explicit, token only for claude, subagents pinned off Opus", () => {
  const cfg = validateConfig({ repos: { demo: {} } });
  const p = taskPaths(cfg, "0123456789ab");
  process.env.SEKRET_RUNNERA = "x";
  const check = workerEnv(cfg, p);
  assert.equal(check.CLAUDE_CODE_OAUTH_TOKEN, undefined);
  assert.equal(check.SEKRET_RUNNERA, undefined);
  assert.equal(check.HOME, `${cfg.work_dir}/0123456789ab/home`);
  const sonnet = workerEnv(cfg, p, { model: "sonnet", oauthToken: "t" });
  assert.equal(sonnet.CLAUDE_CODE_OAUTH_TOKEN, "t");
  assert.equal(sonnet.CLAUDE_CODE_SUBAGENT_MODEL, "claude-sonnet-5-5");
  assert.equal(sonnet.ANTHROPIC_DEFAULT_OPUS_MODEL, "claude-sonnet-5-5");
  const opus = workerEnv(cfg, p, { model: "opus", oauthToken: "t" });
  assert.equal(opus.CLAUDE_CODE_SUBAGENT_MODEL, undefined);
});

test("runResult: ok only for a clean success; a non-Opus run that used Opus fails", () => {
  const base = { id: "x", run: { model: "sonnet", time: 60 }, branch: "kelter/x", stderr: "", seconds: 1, commit: null, diffStat: null, bundle: null };
  const res = (o) => JSON.stringify({ type: "result", subtype: "success", is_error: false, result: "ok", modelUsage: { "claude-sonnet-5-5": {} }, ...o });
  assert.equal(runResult({ ...base, exit: 0, stdout: res(), timedOut: false }).ok, true);
  assert.equal(runResult({ ...base, exit: 0, stdout: res(), timedOut: true }).ok, false);
  const opus = runResult({ ...base, exit: 0, stdout: res({ modelUsage: { "claude-sonnet-5-5": {}, "claude-opus-5-5": {} } }), timedOut: false });
  assert.equal(opus.ok, false);
  assert.match(opus.blad, /naruszenie.*Opusa bez zgody/);
  const approved = runResult({ ...base, run: { model: "opus", time: 60 }, exit: 0, stdout: res({ modelUsage: { "claude-opus-5-5": {} } }), timedOut: false });
  assert.equal(approved.ok, true);
  const r = runResult({ ...base, exit: 1, stdout: JSON.stringify({ type: "result", subtype: "error_max_budget_usd", is_error: true }), timedOut: false });
  assert.equal(r.blad, "error_max_budget_usd");
  assert.equal(runResult({ ...base, exit: 0, stdout: "not json", timedOut: false }).ok, false);
});

test("a run clones, works on its own branch, exports a bundle; the same key never runs twice", async () => {
  const { runner, log, cfg } = fixture();
  const r1 = await runner.run(req());
  assert.equal(r1.ok, true, JSON.stringify(r1));
  assert.equal(r1.galaz, `kelter/${taskId("run1:0:a")}`);
  assert.match(r1.zmiany, /wynik\.txt/);
  const bundle = join(cfg.out_dir, r1.bundle.plik);
  assert.equal(createHash("sha256").update(readFileSync(bundle)).digest("hex"), r1.bundle.sha256);
  const heads = execFileSync("git", ["bundle", "list-heads", bundle]).toString();
  assert.match(heads, new RegExp(`refs/heads/${r1.galaz}`));
  // The base repo is never written to.
  assert.throws(() => execFileSync("git", ["-C", join(cfg.repos_dir, "demo.git"), "rev-parse", "--verify", r1.galaz], { stdio: "pipe" }));
  assert.deepEqual(await runner.run(req()), r1);
  assert.equal(lines(log).length, 1, "claude ran exactly once");
  assert.match(lines(log)[0], /sub=claude-sonnet-5-5 opus_alias=claude-sonnet-5-5 token=set runner_token_file= /);
  assert.ok(!lines(log)[0].includes("zrób"), "prompt not on the command line");
  await assert.rejects(runner.run(req({ model: "haiku" })), /different parameters/);
});

test("concurrent calls with one key share the in-flight execution", async () => {
  const { runner, log } = fixture();
  const [a, b] = await Promise.all([runner.run(req()), runner.run(req())]);
  assert.deepEqual(a, b);
  assert.equal(lines(log).length, 1);
});

test("a brief starting with '-' reaches claude as text", async () => {
  const { runner } = fixture();
  const r = await runner.run(req({ idempotency_key: "dash", zlecenie: "- popraw X\n- dodaj test" }));
  assert.equal(r.ok, true, JSON.stringify(r));
  assert.equal(r.wynik, "brief z myślnikiem dotarł jako tekst");
});

test("leftovers are committed; claude failure and Opus-in-Sonnet are ok:false", async () => {
  const { runner } = fixture();
  const left = await runner.run(req({ idempotency_key: "k2", zlecenie: "nocommit" }));
  assert.equal(left.ok, true, JSON.stringify(left));
  assert.match(left.zmiany, /niezacommitowane\.txt/);
  const failed = await runner.run(req({ idempotency_key: "k3", zlecenie: "fail" }));
  assert.equal(failed.ok, false);
  assert.equal(failed.blad, "error_max_budget_usd");
  const sneaky = await runner.run(req({ idempotency_key: "k4", zlecenie: "opus-sub" }));
  assert.equal(sneaky.ok, false);
  assert.match(sneaky.blad, /Opusa bez zgody/);
});

test("checks: run in the task's clone without secrets; only after a clean run with changes", async () => {
  const { runner } = fixture();
  const r = await runner.run(req());
  assert.equal((await runner.check({ sprawdzenie: "plik-jest", galaz: r.galaz })).ok, true);
  assert.equal((await runner.check({ sprawdzenie: "bez-sekretow", galaz: r.galaz })).ok, true);
  const bad = await runner.check({ sprawdzenie: "zawsze-zle", galaz: r.galaz });
  assert.equal(bad.ok, false);
  assert.equal(bad.kod_wyjscia, 2);
  assert.match(bad.wyjscie, /boom/);
  await assert.rejects(runner.check({ sprawdzenie: "plik-jest", galaz: "kelter/000000000000" }), /no task/);
  // Not evidence: a run that changed nothing, and a failed run.
  const nochange = await runner.run(req({ idempotency_key: "nc", zlecenie: "nochange" }));
  assert.equal(nochange.bundle, null);
  const c1 = await runner.check({ sprawdzenie: "plik-jest", galaz: nochange.galaz });
  assert.equal(c1.ok, false);
  assert.match(c1.blad, /nie byłoby dowodem/);
  const failed = await runner.run(req({ idempotency_key: "f", zlecenie: "fail" }));
  assert.equal((await runner.check({ sprawdzenie: "plik-jest", galaz: failed.galaz })).ok, false);
});

test("a planted symlink at the bundle path is replaced, never written through", async () => {
  const { runner, cfg, root } = fixture();
  const p = taskPaths(cfg, taskId("sym"));
  writeFileSync(join(root, "secret"), "s");
  mkdirSync(p.dir, { recursive: true });
  symlinkSync(join(root, "secret"), p.bundle);
  const r = await runner.run(req({ idempotency_key: "sym" }));
  assert.equal(readFileSync(join(root, "secret"), "utf8"), "s");
  assert.ok(r.bundle, JSON.stringify(r));
});

test("a task left running by a restart is reported failed, not re-run", async () => {
  const { cfg, log } = fixture();
  const id = taskId("k9");
  mkdirSync(join(cfg.state_dir, "runs"), { recursive: true });
  const fp = createHash("sha256").update(JSON.stringify(["demo", "sonnet", "medium", 1800, "main", "zrób"])).digest("hex");
  writeFileSync(join(cfg.state_dir, "runs", `${id}.json`), JSON.stringify({ status: "running", fingerprint: fp }));
  const restarted = createRunner(cfg, { claudeBin: fake, passEnv: { FAKE_LOG: log } });
  const res = await restarted.run(req({ idempotency_key: "k9" }));
  assert.equal(res.ok, false);
  assert.match(res.blad, /przerwane restartem/);
  assert.equal(existsSync(log), false, "claude never started");
});

test("exec: timeout kills the whole group; a leftover grandchild does not hang it", async () => {
  const r = await exec("sh", ["-c", "sleep 30 & sleep 30"], { timeoutS: 1 });
  assert.equal(r.timedOut, true);
  const t0 = Date.now();
  const r2 = await exec("sh", ["-c", "sleep 30 & echo done"], { timeoutS: 20 });
  assert.equal(r2.timedOut, false);
  assert.equal(r2.exit, 0);
  assert.ok(Date.now() - t0 < 10_000, "resolved on exit, not on the grandchild");
});

test("HTTP: bearer required, 400 on bad input incl. null body, 200 with the result", async () => {
  const { runner } = fixture();
  const token = "t".repeat(40);
  const server = createHttpServer(runner, token).listen(0);
  const url = `http://127.0.0.1:${server.address().port}`;
  const post = (path, body, auth = `Bearer ${token}`) =>
    fetch(url + path, { method: "POST", headers: { authorization: auth, "content-type": "application/json" }, body });
  try {
    assert.equal((await fetch(`${url}/healthz`)).status, 200);
    assert.equal((await post("/v1/run", JSON.stringify(req()), "Bearer nope")).status, 401);
    assert.equal((await post("/v1/run", JSON.stringify(req({ model: "x" })))).status, 400);
    assert.equal((await post("/v1/run", "null")).status, 400);
    const ok = await post("/v1/run", JSON.stringify(req()));
    assert.equal(ok.status, 200);
    assert.equal((await ok.json()).ok, true);
  } finally {
    server.close();
  }
});

test("authorized is exact", () => {
  assert.equal(authorized("Bearer abc", "abc"), true);
  assert.equal(authorized("Bearer abcd", "abc"), false);
  assert.equal(authorized(undefined, "abc"), false);
});

// ---- KRW.2: path recommendation (CLM shadow) --------------------------------

import { createServer } from "node:http";
import { PATH_QUESTION, clmRequest, parseRecommend, recommendResult } from "../lib.mjs";

function fakeClm(reply) {
  const seen = [];
  const server = createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      seen.push({ url: req.url, auth: req.headers.authorization, body: JSON.parse(raw) });
      const [code, body] = typeof reply === "function" ? reply() : reply;
      res.writeHead(code, { "content-type": "application/json" });
      res.end(typeof body === "string" ? body : JSON.stringify(body));
    });
  }).listen(0);
  return { server, seen, url: () => `http://127.0.0.1:${server.address().port}` };
}

const clmAnswer = { answers: { sciezka: { choice: "sonnet", probabilities: { bez: 0.02, haiku: 0.1, sonnet: 0.81234, opus: 0.06766 }, confidence: 0.75 } } };

test("recommend: the TypeSafe-shaped question, options mapped 1:1 to the skill's paths", () => {
  const cfg = validateConfig({ repos: {}, clm: { url: "http://clm:8700" } });
  const body = clmRequest(parseRecommend({ zlecenie: "popraw literówkę", repo: "kelter" }), cfg);
  assert.equal(body.model, "clm-latest");
  assert.deepEqual(body.state, { task: "popraw literówkę", repository: "kelter" });
  assert.equal(body.questions.sciezka.type, "choice");
  assert.deepEqual(Object.keys(PATH_QUESTION.criteria), ["bez", "haiku", "sonnet", "opus"]);
  assert.throws(() => parseRecommend({}), BadRequest);
  assert.throws(() => validateConfig({ repos: {}, clm: { url: "clm:8700" } }), /clm.url/);
});

test("recommend: a usable answer is recorded with rounded probabilities; anything else is ok:false", () => {
  const r = recommendResult(clmAnswer, 42);
  assert.deepEqual(r, { ok: true, rekomendacja: "sonnet", prawdopodobienstwa: { bez: 0.02, haiku: 0.1, sonnet: 0.812, opus: 0.068 }, pewnosc: 0.75, opoznienie_ms: 42, tryb: "cien" });
  for (const bad of [null, {}, { answers: { sciezka: { choice: "gpt", probabilities: {} } } }, { answers: { sciezka: { choice: "opus" } } }]) {
    assert.equal(recommendResult(bad, 1).ok, false);
  }
});

test("recommend end to end: calls CLM with the key, logs the answer, never queues behind a task", async () => {
  const clm = fakeClm([200, clmAnswer]);
  try {
    const { cfg } = fixture();
    const runner = createRunner({ ...cfg, clm: { url: clm.url(), model: "clm-latest", timeout_s: 5 } }, { claudeBin: fake, clmKey: "k-123" });
    const r = await runner.recommend({ zlecenie: "dodaj test", repo: "demo" });
    assert.equal(r.ok, true, JSON.stringify(r));
    assert.equal(r.rekomendacja, "sonnet");
    assert.equal(clm.seen[0].url, "/v1/systemone");
    assert.equal(clm.seen[0].auth, "Bearer k-123");
    const log = readFileSync(join(cfg.state_dir, "recommendations.jsonl"), "utf8").trim().split("\n");
    assert.equal(log.length, 1);
    assert.equal(JSON.parse(log[0]).rekomendacja, "sonnet");
    assert.ok(!log[0].includes("dodaj test"), "the task text is not stored, only its hash");
  } finally {
    clm.server.close();
  }
});

test("recommend degrades: no CLM configured, CLM down, CLM error — all ok:false, nothing thrown", async () => {
  const { cfg } = fixture();
  const none = createRunner({ ...cfg, clm: null }, { claudeBin: fake });
  assert.match((await none.recommend({ zlecenie: "x" })).blad, /not configured/);
  const down = createRunner({ ...cfg, clm: { url: "http://127.0.0.1:9", model: "clm-latest", timeout_s: 2 } }, { claudeBin: fake });
  assert.match((await down.recommend({ zlecenie: "x" })).blad, /unreachable/);
  const clm = fakeClm([502, { error: "embedder unreachable" }]);
  try {
    const err = createRunner({ ...cfg, clm: { url: clm.url(), model: "clm-latest", timeout_s: 5 } }, { claudeBin: fake });
    assert.match((await err.recommend({ zlecenie: "x" })).blad, /CLM HTTP 502/);
  } finally {
    clm.server.close();
  }
});
