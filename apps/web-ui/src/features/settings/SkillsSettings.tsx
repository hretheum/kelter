/* ADR 0021 Part 6: skills management — install/activate/deactivate/remove
 * and the model-activation policy (trusted/all/ask). Stacked below
 * ModelConfig in the single Settings view (design/07 D-9, design/05 §5.0 —
 * no Section/Card primitive exists, so a heading + divider is the whole
 * layout change). Hidden entirely — not shown disabled — on a daemon
 * without the /api/v1/skills routes (config-501 convention). */

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Badge, Banner, Skeleton } from "@/components/ui/badge";
import { Input, Label, Select } from "@/components/ui/input";
import {
  activateSkill,
  deactivateSkill,
  fetchSkills,
  fetchSkillsPolicy,
  installSkill,
  removeSkill,
  setSkillsPolicy,
} from "@/store/app";
import { useAppState } from "@/store/hooks";
import type { ApiError, SkillsPolicyView, SkillView } from "@/lib/api";

export function SkillsSettings() {
  const s = useAppState();
  const [loaded, setLoaded] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [skills, setSkills] = useState<SkillView[]>([]);
  const [policyView, setPolicyView] = useState<SkillsPolicyView | null>(null);

  const [modelActivation, setModelActivation] = useState("trusted");
  const [askFallback, setAskFallback] = useState("deny");
  const [askTimeoutSecs, setAskTimeoutSecs] = useState(300);

  const [installUrl, setInstallUrl] = useState("");
  const [confirmRemove, setConfirmRemove] = useState<string | null>(null);
  const [busyKey, setBusyKey] = useState("");
  const [result, setResult] = useState<{ text: string; kind: "ok" | "err" | "" }>({ text: "", kind: "" });

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [list, pol] = await Promise.all([fetchSkills(), fetchSkillsPolicy()]);
        if (cancelled) return;
        setSkills(list);
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
    setModelActivation(policyView.model_activation);
    setAskFallback(policyView.ask_fallback);
    setAskTimeoutSecs(policyView.ask_timeout_secs);
  }, [policyView]);

  const refreshSkills = async () => setSkills(await fetchSkills());

  const doInstall = async () => {
    const url = installUrl.trim();
    if (!url) {
      setResult({ text: "Enter a URL first.", kind: "err" });
      return;
    }
    setBusyKey("install");
    setResult({ text: "Installing…", kind: "" });
    try {
      const report = await installSkill(url);
      setInstallUrl("");
      await refreshSkills();
      if (report.installed.length > 0) {
        setResult({ text: `✓ Installed ${report.installed.join(", ")}`, kind: "ok" });
      } else if (report.skipped.length > 0) {
        setResult({
          text: `✕ Skipped — ${report.skipped.map(([name, reason]) => `${name}: ${reason}`).join("; ")}`,
          kind: "err",
        });
      } else {
        setResult({ text: "Nothing installed.", kind: "err" });
      }
    } catch (e) {
      setResult({ text: `✕ ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusyKey("");
    }
  };

  const doActivate = async (name: string) => {
    setBusyKey(`activate:${name}`);
    setResult({ text: "", kind: "" });
    try {
      await activateSkill(name);
      await refreshSkills();
      setResult({ text: `✓ ${name} active`, kind: "ok" });
    } catch (e) {
      setResult({ text: `✕ Couldn't activate ${name} — ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusyKey("");
    }
  };

  const doDeactivate = async (name: string) => {
    setBusyKey(`deactivate:${name}`);
    setResult({ text: "", kind: "" });
    try {
      await deactivateSkill(name);
      await refreshSkills();
      setResult({ text: `${name} deactivated`, kind: "ok" });
    } catch (e) {
      setResult({ text: `✕ Couldn't deactivate ${name} — ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusyKey("");
    }
  };

  const doRemove = async (name: string) => {
    setBusyKey(`remove:${name}`);
    setResult({ text: "", kind: "" });
    try {
      await removeSkill(name);
      setConfirmRemove(null);
      await refreshSkills();
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
      const updated = await setSkillsPolicy({
        model_activation: modelActivation,
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

  if (!s.skillsManagementSupported) return null;

  if (loadError) {
    return (
      <div className="mx-auto w-full max-w-md p-6">
        <h2 className="text-title3 font-semibold">Skills</h2>
        <Banner tone="warning" className="mt-3">
          Couldn't load skills — {loadError}
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
      <h2 className="text-title3 font-semibold">Skills</h2>

      <div className="mt-4 space-y-3">
        <div className="space-y-1">
          <Label htmlFor="skills-policy">When the model wants an untrusted skill</Label>
          <Select
            id="skills-policy"
            value={modelActivation}
            onChange={(e) => setModelActivation(e.target.value)}
          >
            <option value="trusted">Only use already-trusted skills</option>
            <option value="all">Allow any installed skill</option>
            <option value="ask">Ask before an untrusted skill</option>
          </Select>
        </div>

        {modelActivation === "ask" && (
          <>
            <div className="space-y-1">
              <Label htmlFor="skills-ask-fallback">If no one is watching</Label>
              <Select
                id="skills-ask-fallback"
                value={askFallback}
                onChange={(e) => setAskFallback(e.target.value)}
              >
                <option value="deny">Skip the skill automatically</option>
                <option value="queue">Pause the run and wait</option>
              </Select>
            </div>
            {askFallback === "queue" && (
              <div className="space-y-1">
                <Label htmlFor="skills-ask-timeout">Give up waiting after (seconds)</Label>
                <Input
                  id="skills-ask-timeout"
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
        <Label htmlFor="skills-install-url">Install from a URL</Label>
        <div className="flex gap-2">
          <Input
            id="skills-install-url"
            placeholder="https://…"
            autoComplete="off"
            value={installUrl}
            onChange={(e) => setInstallUrl(e.target.value)}
            className="flex-1"
          />
          <Button size="sm" onClick={() => void doInstall()} loading={busyKey === "install"} disabled={busy && busyKey !== "install"}>
            {busyKey === "install" ? "Installing…" : "Install"}
          </Button>
        </div>
        <p className="text-caption2 text-muted-foreground">
          A local folder or bundle installs from the command line
          (<code className="font-mono">kelter skill add</code>) — this form only takes a URL.
        </p>
      </div>

      <p role="status" aria-live="polite" className={`mt-2 min-h-4 text-caption ${
        result.kind === "ok" ? "text-success" : result.kind === "err" ? "text-danger" : "text-muted-foreground"
      }`}>
        {result.text}
      </p>

      <hr className="my-4 border-border" />

      <h3 className="text-callout font-medium">Installed</h3>
      {skills.length === 0 ? (
        <p className="mt-1 text-body text-muted-foreground">No skills installed yet.</p>
      ) : (
        <ul className="mt-2 space-y-2">
          {skills.map((sk) => (
            <li key={sk.name} className="rounded-md border border-border bg-card px-3 py-2">
              <div className="flex items-start gap-2">
                <div className="min-w-0 flex-1">
                  <p className="truncate font-mono text-body">{sk.name}</p>
                  <p className="text-caption text-muted-foreground">{sk.description}</p>
                </div>
                <div className="flex shrink-0 flex-col items-end gap-1">
                  <Badge tone={sk.trust === "trusted" ? "success" : "quiet"}>
                    {sk.trust === "trusted" ? "✓ trusted" : "○ untrusted"}
                  </Badge>
                  <Badge tone={sk.active ? "accent" : "quiet"}>
                    {sk.active ? "◉ active" : "○ inactive"}
                  </Badge>
                </div>
              </div>
              <p className="mt-1 text-caption2 text-muted-foreground">
                {sk.provenance_source_kind}: {sk.provenance_source}
              </p>
              <div className="mt-2 flex flex-wrap items-center gap-1.5">
                {sk.active ? (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={busy && busyKey !== `deactivate:${sk.name}`}
                    loading={busyKey === `deactivate:${sk.name}`}
                    onClick={() => void doDeactivate(sk.name)}
                  >
                    Deactivate
                  </Button>
                ) : (
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={busy && busyKey !== `activate:${sk.name}`}
                    loading={busyKey === `activate:${sk.name}`}
                    onClick={() => void doActivate(sk.name)}
                  >
                    Activate
                  </Button>
                )}
                {confirmRemove === sk.name ? (
                  <span className="flex items-center gap-1.5 text-caption">
                    Remove this skill?
                    <Button
                      variant="danger"
                      size="sm"
                      loading={busyKey === `remove:${sk.name}`}
                      onClick={() => void doRemove(sk.name)}
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
                    onClick={() => setConfirmRemove(sk.name)}
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
