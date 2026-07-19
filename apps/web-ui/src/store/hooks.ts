import { useSyncExternalStore } from "react";
import { getState, subscribe, type AppState } from "./app";
import { axisFor, subscribeAxis, type AxisState } from "@/lib/events";

export function useAppState(): AppState {
  return useSyncExternalStore(subscribe, getState);
}

/* Subscribe to one run's axis. The axis object is mutated in place by the
 * ingest layer; a version counter makes useSyncExternalStore see changes. */
const versions = new Map<string, number>();

export function useAxis(runId: string): { axis: AxisState; version: number } {
  const subscribeFn = (cb: () => void) =>
    subscribeAxis(runId, () => {
      versions.set(runId, (versions.get(runId) ?? 0) + 1);
      cb();
    });
  const version = useSyncExternalStore(
    subscribeFn,
    () => versions.get(runId) ?? 0
  );
  return { axis: axisFor(runId), version };
}
