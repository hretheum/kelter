import { describe, expect, it } from "vitest";
import {
  CODING_CONTRACT, budgetStopText, executorAvailable, executorCostUsd, executorView, runRequest,
} from "./executor";
import type { BudgetStop } from "./api";

const runOk = {
  ok: true, zadanie: "6b231618c7b3", galaz: "kelter/6b231618c7b3", model: "sonnet",
  wynik: "Dodałem TEST.md", blad: null, koszt_usd: 0.0305588, tury: 2, czas_s: 6,
  zmiany: "TEST.md | 0\n 1 file changed", bundle: { plik: "6b231618c7b3.bundle", sha256: "077e" },
};

describe("runRequest", () => {
  it("adds the coding contract only for a coding task", () => {
    expect(runRequest("x", false)).toEqual({ task: "x" });
    expect(runRequest("x", true)).toEqual({ task: "x", success_contract: CODING_CONTRACT });
  });
  it("a contract on sprawdz alone covers the Opus path too", () => {
    expect(CODING_CONTRACT.require).toEqual([{ kind: "action_succeeded", action: "sprawdz" }]);
  });
  it("turns minutes into a wall-clock budget; ignores blanks and junk", () => {
    expect(runRequest("x", true, "30").budget).toEqual({ max_wall_secs: 1800 });
    for (const bad of ["", "0", "-5", "1.5", "abc"]) expect(runRequest("x", true, bad).budget).toBeUndefined();
  });
});

describe("executorAvailable", () => {
  it("needs a run tool and the check", () => {
    expect(executorAvailable([{ name: "claude_code_run" }, { name: "sprawdz" }])).toBe(true);
    expect(executorAvailable([{ name: "claude_code_opus" }, { name: "sprawdz" }])).toBe(true);
    expect(executorAvailable([{ name: "claude_code_run" }])).toBe(false);
    expect(executorAvailable([{ name: "zapisz_plik" }])).toBe(false);
  });
});

describe("executorView", () => {
  it("summarizes a run: model, branch, time, cost, changes", () => {
    const v = executorView("claude_code_run", runOk)!;
    expect(v.kind).toBe("run");
    expect(v.ok).toBe(true);
    expect(v.headline).toBe("sonnet · kelter/6b231618c7b3 · 6s · ~$0.03");
    expect(v.changes).toContain("TEST.md");
    expect(v.error).toBeNull();
  });
  it("surfaces `blad` and flags an Opus violation", () => {
    const v = executorView("claude_code_run", { ...runOk, ok: false, blad: "naruszenie: sesja sonnet użyła Opusa bez zgody (claude-opus-5-5)" })!;
    expect(v.ok).toBe(false);
    expect(v.violation).toBe(true);
    expect(v.error).toMatch(/Opusa bez zgody/);
  });
  it("opus tool without a model field still reads as opus", () => {
    expect(executorView("claude_code_opus", { ok: true })!.headline).toBe("opus");
  });
  it("a failed check shows its exit code and the tail of its output", () => {
    const long = "x".repeat(2000) + "FAIL src/a.test.ts";
    const v = executorView("sprawdz", { ok: false, sprawdzenie: "glassbox-testy", galaz: "kelter/abc", kod_wyjscia: 1, wyjscie: long })!;
    expect(v.headline).toBe("glassbox-testy · kelter/abc · exit 1");
    expect(v.error).toBe("check exited with 1");
    expect(v.outputTail!.endsWith("FAIL src/a.test.ts")).toBe(true);
    expect(v.outputTail!.length).toBeLessThan(1300);
  });
  it("a refused check carries the runner's reason; a passed one hides output", () => {
    const refused = executorView("sprawdz", { ok: false, sprawdzenie: "t", blad: "zadanie x nie zakończyło się sukcesem" })!;
    expect(refused.error).toMatch(/nie zakończyło/);
    const passed = executorView("sprawdz", { ok: true, sprawdzenie: "t", kod_wyjscia: 0, wyjscie: "ok" })!;
    expect(passed.outputTail).toBeNull();
    expect(passed.headline).toBe("t · passed");
  });
  it("ignores other tools and odd results", () => {
    expect(executorView("zapisz_plik", runOk)).toBeNull();
    expect(executorView("claude_code_run", "text")).toBeNull();
    expect(executorView("claude_code_run", null)).toBeNull();
  });
});

describe("executorCostUsd", () => {
  it("sums notional cost over run steps only", () => {
    const steps = [
      { action: "claude_code_run", result: runOk },
      { action: "sprawdz", result: { ok: true, koszt_usd: 99 } },
      { action: "claude_code_opus", result: { ok: true, koszt_usd: 1.5 } },
      { action: "claude_code_run", result: null },
    ];
    expect(executorCostUsd(steps)).toBeCloseTo(1.5305588);
  });
  it("is null when no run reported a cost", () => {
    expect(executorCostUsd([{ action: "zapisz_plik", result: {} }])).toBeNull();
  });
});

describe("budgetStopText", () => {
  const base: BudgetStop = { limit: "wall_time", max: 14400, used: 14410, requested: 0, checkpoint: "action", step: 3 };
  it("says which limit, how much, where — and that it was not crossed", () => {
    expect(budgetStopText(base)).toBe(
      "Stopped by the time limit (240 min): 240 min used, stopped before the next action at step 4 — the limit was never crossed."
    );
    expect(budgetStopText({ ...base, limit: "tokens", max: 200000, used: 199000, checkpoint: "model_call" })).toMatch(
      /tokens limit \(200,000 tokens\).*before the next model call/
    );
    expect(budgetStopText({ ...base, limit: "daily_usd", max: 5, used: 4.9 })).toMatch(/daily spending limit \(\$5.00 per day\)/);
  });
  it("null when there is no stop", () => {
    expect(budgetStopText(null)).toBeNull();
    expect(budgetStopText(undefined)).toBeNull();
  });
});
