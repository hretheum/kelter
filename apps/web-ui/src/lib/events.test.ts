/* Reducer tests for the execution-axis engine (src/lib/events.ts).
 *
 * Focused on the forward-only ingest contract (audit finding 3-b/#12): the
 * dead `pending`/`rest` reorder buffer was removed, and these tests pin the
 * behaviour that replaces it — dedupe by seq, apply anything newer, and
 * (crucially) never drop or block on a forward seq gap, which is the
 * legitimate post-reconnect case a blocking buffer would have deadlocked on. */

import { beforeEach, describe, expect, it } from "vitest";
import {
  __resetAxesForTest,
  axisFor,
  axisSteps,
  ingestEnvelope,
  ingestReplay,
} from "./events";
import type { RunEvent, RunEventEnvelope } from "./api";

const RUN = "run-1";

function env(seq: number, event: RunEvent): RunEventEnvelope {
  return { run_id: RUN, seq, event };
}

function proposed(step: number, action: string): RunEvent {
  return { type: "proposed", step, action, arguments: {}, rationale: null, branch: null };
}

function executed(step: number, action: string, ok = true): RunEvent {
  return { type: "executed", step, action, ok, result: {}, branch: null };
}

beforeEach(() => __resetAxesForTest());

describe("ingestEnvelope — forward-only ordered stream", () => {
  it("applies a normal in-order burst", () => {
    ingestEnvelope(env(1, proposed(0, "a")));
    ingestEnvelope(env(2, executed(0, "a")));
    ingestEnvelope(env(3, proposed(1, "b")));
    const steps = axisSteps(axisFor(RUN));
    expect(steps.map((s) => s.action)).toEqual(["a", "b"]);
    expect(steps[0].executed?.ok).toBe(true);
  });

  it("dedupes a re-delivered seq (resume re-emits from 1) — no double apply", () => {
    ingestEnvelope(env(1, proposed(0, "a")));
    ingestEnvelope(env(2, executed(0, "a", false)));
    // Reconnect re-emits seq 1 (a duplicate). It must be ignored, NOT
    // re-applied over the newer executed state.
    ingestEnvelope(env(1, proposed(0, "a")));
    const step = axisSteps(axisFor(RUN))[0];
    expect(step.executed).not.toBeNull();
    expect(step.executed?.ok).toBe(false);
    expect(axisFor(RUN).lastSeq).toBe(2);
  });

  it("does NOT drop or block on a forward seq gap (the post-reconnect case)", () => {
    // A client that missed seqs 2..9 while disconnected: the next LIVE event
    // legitimately jumps to seq 10 (the missed ones were journal-replayed, not
    // re-sent live). It must be applied, never buffered forever.
    ingestEnvelope(env(1, proposed(0, "a")));
    ingestEnvelope(env(10, executed(5, "f")));
    const steps = axisSteps(axisFor(RUN));
    // Both the seq-1 step and the seq-10 step are present.
    expect(steps.find((s) => s.step === 0)?.action).toBe("a");
    expect(steps.find((s) => s.step === 5)?.action).toBe("f");
    expect(axisFor(RUN).lastSeq).toBe(10);
  });

  it("does not advance lastSeq for advisory progress (seq 0), and keeps it off the gate", () => {
    ingestEnvelope(env(5, proposed(0, "delegate_to_x")));
    // Progress rides seq 0 — must apply and NOT reset lastSeq below 5.
    ingestEnvelope(
      env(0, {
        type: "agent_delegation_progress",
        step: 0,
        agent: "x",
        task_id: "t1",
        state: "TASK_STATE_WORKING",
        note: "thinking",
        ordinal: 0,
        branch: null,
      })
    );
    expect(axisFor(RUN).lastSeq).toBe(5);
    const step = axisSteps(axisFor(RUN)).find((s) => s.step === 0);
    expect(step?.progress.map((p) => p.note)).toEqual(["thinking"]);
  });
});

/* ADR 0033: a run that finished without any successful action has its own
 * terminal — it must never fold into the axis as "completed". */
describe("incomplete terminal (ADR 0033)", () => {
  it("live event sets an incomplete terminal with the model's output and the kernel's reason", () => {
    ingestEnvelope(
      env(1, { type: "incomplete", output: { summary: "e-mail sent" }, reason: "no action" })
    );
    expect(axisFor(RUN).terminal).toEqual({
      kind: "incomplete",
      output: { summary: "e-mail sent" },
      reason: "no action",
    });
  });

  it("journal replay of an incomplete terminal is not read as completed", () => {
    ingestReplay(RUN, { task: "t", policy: "p" }, [
      { kind: "terminal", status: "incomplete", output: { summary: "x" }, error: "no action" },
    ]);
    expect(axisFor(RUN).terminal?.kind).toBe("incomplete");
  });
});
