/* The shell (design/01): top bar + two destinations (Runs master–detail,
 * Settings round-trip) + the connection sheet as chrome. Narrow (<720px):
 * list and detail become a back-navigable stack (design/01 §4). */

import { useEffect, useRef } from "react";
import { Button } from "@/components/ui/button";
import { Banner } from "@/components/ui/badge";
import { installShortcuts } from "@/lib/shortcuts";
import { getState, openSheet, selectRun, showSettings } from "@/store/app";
import { useAppState } from "@/store/hooks";
import { ConnectionSheet } from "@/features/connection-sheet/ConnectionSheet";
import { MasterList } from "@/features/runs/MasterList";
import { RunDetail } from "@/features/runs/RunDetail";
import { ModelConfig } from "@/features/settings/ModelConfig";
import { TopBar } from "@/features/topbar/TopBar";

export function App() {
  const s = useAppState();
  const listRef = useRef<HTMLUListElement>(null);

  // Global keymap (design/04 §1). Esc chain: sheet → settings → nothing.
  useEffect(() => {
    return installShortcuts({
      newRun: () => selectRun(null),
      openSettings: () => showSettings(true),
      closeOverlay: () => {
        const st = getState();
        if (st.sheetOpen) {
          openSheet(false);
          return true;
        }
        if (st.view === "settings") {
          showSettings(false);
          return true;
        }
        return false;
      },
      saveSettings: () => {
        if (getState().view !== "settings") return false;
        window.dispatchEvent(new Event("kelter:save-settings"));
        return true;
      },
      focusList: () => listRef.current?.focus(),
    });
  }, []);

  const detailOpen = s.selectedRunId !== null;

  return (
    <div className="flex h-dvh flex-col">
      <TopBar />

      {s.conn === "disconnected" && s.runsLoaded && (
        <Banner tone="warning" className="mx-4 mt-2">
          Connection lost — your runs are safe on the kernel. Reconnecting…
        </Banner>
      )}

      <main id="main" tabIndex={-1} className="flex min-h-0 flex-1 outline-none">
        {s.view === "settings" ? (
          <div className="flex-1 overflow-y-auto">
            <ModelConfig />
          </div>
        ) : (
          <>
            {/* Master list: always on desktop; on narrow, only when no run
                is selected (stack navigation). */}
            <div className={detailOpen ? "hidden md:flex" : "flex w-full md:w-auto"}>
              <MasterList onNewRun={() => selectRun(null)} listRef={listRef} />
            </div>
            <section
              aria-label="Run detail"
              className={`min-w-0 flex-1 overflow-y-auto ${detailOpen ? "" : "hidden md:block"}`}
            >
              {detailOpen && (
                <div className="border-b border-border px-4 py-1.5 md:hidden">
                  <Button variant="ghost" size="sm" onClick={() => selectRun(null)}>
                    ← Runs
                  </Button>
                </div>
              )}
              <RunDetail />
            </section>
          </>
        )}
      </main>

      <ConnectionSheet />
    </div>
  );
}
