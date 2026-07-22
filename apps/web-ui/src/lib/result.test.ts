import { describe, expect, it } from "vitest";
import { outputView } from "./result";

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
