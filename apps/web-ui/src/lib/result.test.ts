import { describe, expect, it } from "vitest";
import { outputView, verificationView } from "./result";
import type { Run } from "./api";

describe("outputView", () => {
  it("renders the kernel finish shape as prose", () => {
    const v = outputView({ summary: "Wrote report.md with the findings." });
    expect(v.prose).toBe("Wrote report.md with the findings.");
    expect(v.detail).toBeNull();
  });

  it("keeps non-prose fields as detail (plan-mode shape)", () => {
    const v = outputView({ summary: "plan completed: 2 node(s)", nodes: [1, 2] });
    expect(v.prose).toBe("plan completed: 2 node(s)");
    expect(v.detail).toContain("nodes");
    expect(v.detail).not.toContain("plan completed");
  });

  it("uses a plain string output directly", () => {
    expect(outputView("all done").prose).toBe("all done");
  });

  it("degrades foreign prose keys", () => {
    expect(outputView({ message: "ok" }).prose).toBe("ok");
    expect(outputView({ text: "ok" }).prose).toBe("ok");
  });

  it("never dumps raw as prose for prose-less shapes", () => {
    for (const odd of [42, true, [1, 2, 3], { a: 1 }, { summary: 7 }]) {
      const v = outputView(odd);
      expect(v.prose).toBeNull();
      expect(v.detail).toBeTruthy();
    }
  });

  it("handles null/undefined/empty without throwing", () => {
    expect(outputView(null)).toEqual({ prose: null, detail: null });
    expect(outputView(undefined)).toEqual({ prose: null, detail: null });
    expect(outputView("   ")).toEqual({ prose: null, detail: null });
  });

  it("survives circular structures", () => {
    const a: Record<string, unknown> = {};
    a.self = a;
    expect(() => outputView(a)).not.toThrow();
  });
});

describe("verificationView (ADR 0037)", () => {
  const base: Run = {
    id: "r",
    task: "t",
    status: "completed",
    steps: [],
    output: null,
    error: null,
    policy: "p",
    created_at: 0,
    cost_usd: null,
    active_skills: [],
  };

  it("offers confirmation only for an unverified completed run", () => {
    const v = verificationView({ ...base, verification: "none", confirmation: null });
    expect(v.tone).toBe("unverified");
    expect(v.canConfirm).toBe(true);
    expect(v.canRevoke).toBe(false);
  });

  it("offers revoke once confirmed, and names the principal", () => {
    const v = verificationView({
      ...base,
      verification: "none",
      confirmation: { principal: "eryk", at_ms: 1 },
    });
    expect(v.tone).toBe("confirmed");
    expect(v.text).toContain("eryk");
    expect(v.canConfirm).toBe(false);
    expect(v.canRevoke).toBe(true);
  });

  it("never offers confirmation for contract-verified, legacy or incomplete runs", () => {
    for (const run of [
      { ...base, verification: "contract" as const },
      { ...base },
      { ...base, verification: null },
      { ...base, status: "incomplete" as const, verification: "none" as const },
    ]) {
      const v = verificationView(run);
      expect(v.canConfirm).toBe(false);
      expect(v.canRevoke).toBe(false);
    }
    expect(verificationView({ ...base, verification: "contract" }).tone).toBe("verified");
  });

  it("explains a failed contract on an incomplete run", () => {
    const v = verificationView({
      ...base,
      status: "incomplete",
      verification: "contract",
      contract_report: {
        satisfied: false,
        conditions: [
          { condition: { kind: "action_succeeded", action: "send_email" }, met: false, detail: "never executed" },
        ],
      },
    });
    expect(v.tone).toBe("contract_failed");
    expect(v.text).toContain("1 of 1");
  });
});
