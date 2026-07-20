/* ADR 0023 Part 3: agent trust — register/remove and the delegation policy
 * (trusted/all/ask). Stacked below SkillsSettings in the single Settings
 * view (design/07 D-9, design/05 §5.0 — no Section/Card primitive exists, so
 * a heading + divider is the whole layout change), mirroring its shape and
 * conventions exactly. Hidden entirely — not shown disabled — on a daemon
 * without the /api/v1/agents routes (config-501 convention). Unlike a skill,
 * a registered agent is a REAL delegation target — "policy" here gates
 * whether the model may send it real work, not what it may read. No
 * "install a local agent" concept: a network agent only ever registers by
 * card URL, so there is no local-folder/bundle form the way skills have one. */

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge, Banner, Skeleton } from "@/components/ui/badge";
import { Input, Label, Select } from "@/components/ui/input";
import {
  fetchAgents,
  fetchAgentsPolicy,
  registerAgent,
  removeAgent,
  setAgentsPolicy,
} from "@/store/app";
import { useAppState } from "@/store/hooks";
import type { AgentView, AgentPolicyView, ApiError } from "@/lib/api";

export function AgentsSettings() {
  const s = useAppState();
  const [loaded, setLoaded] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [agents, setAgents] = useState<AgentView[]>([]);
  const [policyView, setPolicyView] = useState<AgentPolicyView | null>(null);

  const [policy, setPolicy] = useState("trusted");
  const [askFallback, setAskFallback] = useState("deny");
  const [askTimeoutSecs, setAskTimeoutSecs] = useState(300);

  const [cardUrl, setCardUrl] = useState("");
  const [tokenEnv, setTokenEnv] = useState("");
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const [busyKey, setBusyKey] = useState("");
  const [result, setResult] = useState<{ text: string; kind: "ok" | "err" | "" }>({ text: "", kind: "" });

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [list, pol] = await Promise.all([fetchAgents(), fetchAgentsPolicy()]);
        if (cancelled) return;
        setAgents(list);
        setPolicyView(pol);
        setLoaded(true);
      } catch (e) {
        if (cancelled) return;
        const err = e as ApiError;
        if (err.status !== 404 && err.status !== 405) setLoadError(err.message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Sync the policy form whenever the server view (re)loads.
  useEffect(() => {
    if (!policyView) return;
    setPolicy(policyView.policy);
    setAskFallback(policyView.ask_fallback);
    setAskTimeoutSecs(policyView.ask_timeout_secs);
  }, [policyView]);

  const refreshAgents = async () => setAgents(await fetchAgents());

  const doRegister = async () => {
    const url = cardUrl.trim();
    if (!url) {
      setResult({ text: "Enter a card URL first.", kind: "err" });
      return;
    }
    setBusyKey("register");
    setResult({ text: "Registering…", kind: "" });
    try {
      const view = await registerAgent({
        card_url: url,
        token_env: tokenEnv.trim() || undefined,
      });
      setCardUrl("");
      setTokenEnv("");
      await refreshAgents();
      setResult({ text: `✓ Registered ${view.name}`, kind: "ok" });
    } catch (e) {
      setResult({ text: `✕ ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusyKey("");
    }
  };

  const doRemove = async (name: string) => {
    setBusyKey(`remove:${name}`);
    setResult({ text: "", kind: "" });
    try {
      await removeAgent(name);
      setConfirmRemove(null);
      await refreshAgents();
      setResult({ text: `✓ Removed ${name}`, kind: "ok" });
    } catch (e) {
      setResult({ text: `✕ Couldn't remove ${name} — ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusyKey("");
    }
  };

  const doSavePolicy = async () => {
    setBusyKey("policy");
    setResult({ text: "Saving…", kind: "" });
    try {
      const updated = await setAgentsPolicy({
        policy,
        ask_fallback: askFallback,
        ask_timeout_secs: askTimeoutSecs,
      });
      setPolicyView(updated);
      setResult({ text: "✓ Policy saved", kind: "ok" });
    } catch (e) {
      setResult({ text: `✕ ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusyKey("");
    }
  };

  if (!s.agentsManagementSupported) return null;

  if (loadError) {
    return (
      <div className="mx-auto w-full max-w-md p-6">
        <h2 className="text-title3 font-semibold">Agents</h2>
        <Banner tone="warning" className="mt-3">
          Couldn't load agents — {loadError}
        </Banner>
      </div>
    );
  }

  if (!loaded || !policyView) {
    return (
      <div className="mx-auto w-full max-w-md space-y-3 p-6">
        {[0, 1, 2].map((i) => (
          <Skeleton key={i} className="h-8 w-full" />
        ))}
      </div>
    );
  }

  const busy = busyKey !== "";

  return (
    <div className="mx-auto w-full max-w-md p-6">
      <h2 className="text-title3 font-semibold">Agents</h2>

      <div className="mt-4 space-y-3">
        <div className="space-y-1">
          <Label htmlFor="agents-policy">When the model wants to send work to an untrusted agent</Label>
          <Select id="agents-policy" value={policy} onChange={(e) => setPolicy(e.target.value)}>
            <option value="trusted">Only send work to already-trusted agents</option>
            <option value="all">Allow sending work to any registered agent</option>
            <option value="ask">Ask before sending work to an untrusted agent</option>
          </Select>
        </div>

        {policy === "ask" && (
          <>
            <div className="space-y-1">
              <Label htmlFor="agents-ask-fallback">If no one is watching</Label>
              <Select
                id="agents-ask-fallback"
                value={askFallback}
                onChange={(e) => setAskFallback(e.target.value)}
              >
                <option value="deny">Skip the delegation automatically</option>
                <option value="queue">Pause the run and wait</option>
              </Select>
            </div>
            {askFallback === "queue" && (
              <div className="space-y-1">
                <Label htmlFor="agents-ask-timeout">Give up waiting after (seconds)</Label>
                <Input
                  id="agents-ask-timeout"
                  type="number"
                  min={1}
                  value={askTimeoutSecs}
                  onChange={(e) => setAskTimeoutSecs(Number(e.target.value))}
                />
              </div>
            )}
          </>
        )}

        <Button size="sm" onClick={() => void doSavePolicy()} loading={busyKey === "policy"} disabled={busy && busyKey !== "policy"}>
          {busyKey === "policy" ? "Saving…" : "Save policy"}
        </Button>
      </div>

      <hr className="my-4 border-border" />

      <div className="space-y-1">
        <Label htmlFor="agents-register-url">Register by card URL</Label>
        <div className="flex gap-2">
          <Input
            id="agents-register-url"
            placeholder="https://…/.well-known/agent-card.json"
            autoComplete="off"
            value={cardUrl}
            onChange={(e) => setCardUrl(e.target.value)}
            className="flex-1"
          />
          <Button size="sm" onClick={() => void doRegister()} loading={busyKey === "register"} disabled={busy && busyKey !== "register"}>
            {busyKey === "register" ? "Registering…" : "Register"}
          </Button>
        </div>
        <div className="space-y-1">
          <Label htmlFor="agents-register-token-env">Bearer token env var (optional)</Label>
          <Input
            id="agents-register-token-env"
            placeholder="MY_AGENT_TOKEN"
            autoComplete="off"
            value={tokenEnv}
            onChange={(e) => setTokenEnv(e.target.value)}
          />
        </div>
        <p className="text-caption2 text-muted-foreground">
          Newly registered agents start untrusted — the model can only send them
          work under "Allow any" or after you authorize a specific run.
        </p>
      </div>

      <p role="status" aria-live="polite" className={`mt-2 min-h-4 text-caption ${
        result.kind === "ok" ? "text-success" : result.kind === "err" ? "text-danger" : "text-muted-foreground"
      }`}>
        {result.text}
      </p>

      <hr className="my-4 border-border" />

      <h3 className="text-callout font-medium">Registered</h3>
      {agents.length === 0 ? (
        <p className="mt-1 text-body text-muted-foreground">No agents registered yet.</p>
      ) : (
        <ul className="mt-2 space-y-2">
          {agents.map((a) => (
            <li key={a.name} className="rounded-md border border-border bg-card px-3 py-2">
              <div className="flex items-start gap-2">
                <div className="min-w-0 flex-1">
                  <p className="truncate font-mono text-body">{a.name}</p>
                  <p className="text-caption text-muted-foreground">{a.description}</p>
                </div>
                <Badge tone={a.trust === "trusted" ? "success" : "quiet"}>
                  {a.trust === "trusted" ? "✓ trusted" : "○ untrusted"}
                </Badge>
              </div>
              <p className="mt-1 text-caption2 text-muted-foreground">
                {a.source_kind}: {a.source}
              </p>
              <div className="mt-2 flex flex-wrap items-center gap-1.5">
                {confirmRemove === a.name ? (
                  <span className="flex items-center gap-1.5 text-caption">
                    Remove this agent?
                    <Button
                      variant="danger"
                      size="sm"
                      loading={busyKey === `remove:${a.name}`}
                      onClick={() => void doRemove(a.name)}
                    >
                      Confirm
                    </Button>
                    <Button variant="ghost" size="sm" onClick={() => setConfirmRemove(null)}>
                      Cancel
                    </Button>
                  </span>
                ) : (
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy}
                    onClick={() => setConfirmRemove(a.name)}
                  >
                    Remove
                  </Button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
