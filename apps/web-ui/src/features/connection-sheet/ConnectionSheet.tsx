/* G-2 (design/03): the connection sheet — the bare-browser remedy and
 * reconnect surface. Native <dialog> gives the focus trap + Esc for free;
 * focus restores to the pill on close (design/04 §3). */

import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input, Label } from "@/components/ui/input";
import { connection, hasProvisionedToken } from "@/lib/api";
import { openSheet, reconnectWith } from "@/store/app";
import { useAppState } from "@/store/hooks";

export function ConnectionSheet() {
  const s = useAppState();
  const ref = useRef<HTMLDialogElement>(null);
  const [base, setBase] = useState(connection().base);
  const [token, setToken] = useState(connection().token);
  const busy = s.conn === "connecting";

  useEffect(() => {
    const dlg = ref.current;
    if (!dlg) return;
    if (s.sheetOpen && !dlg.open) dlg.showModal();
    if (!s.sheetOpen && dlg.open) dlg.close();
  }, [s.sheetOpen]);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!token.trim()) return;
    reconnectWith(base, token);
  };

  return (
    <dialog
      ref={ref}
      aria-labelledby="conn-title"
      onClose={() => openSheet(false)}
      className="m-auto w-[min(92vw,380px)] rounded-lg border border-border bg-card-raised p-4 text-foreground backdrop:bg-black/50"
    >
      <form onSubmit={submit} className="space-y-3">
        <div className="flex items-center justify-between">
          <h2 id="conn-title" className="text-callout font-semibold">
            Connection
          </h2>
          <Button
            type="button"
            variant="ghost"
            size="icon"
            aria-label="Close"
            onClick={() => openSheet(false)}
          >
            <span aria-hidden="true">✕</span>
          </Button>
        </div>

        <div className="space-y-1">
          <Label htmlFor="conn-server">Server</Label>
          <Input
            id="conn-server"
            placeholder="(same origin)"
            autoComplete="off"
            value={base}
            onChange={(e) => setBase(e.target.value)}
          />
        </div>
        <div className="space-y-1">
          <Label htmlFor="conn-token">API token</Label>
          <Input
            id="conn-token"
            type="password"
            placeholder="bearer token"
            autoComplete="off"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            aria-describedby={s.connError ? "conn-error" : undefined}
          />
        </div>

        <Button type="submit" loading={busy} disabled={!token.trim()}>
          {busy ? "Connecting…" : "Connect"}
        </Button>

        {s.connError && (
          <p id="conn-error" role="alert" className="text-caption text-danger">
            {s.connError}
          </p>
        )}
        {hasProvisionedToken && (
          <p className="text-caption2 text-muted-foreground">
            This session was provisioned by the launcher — you rarely need this
            form.
          </p>
        )}
      </form>
    </dialog>
  );
}
