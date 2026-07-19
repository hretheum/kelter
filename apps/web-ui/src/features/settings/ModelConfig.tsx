/* S-1 (design/03): the only Settings screen. Editable (200) with a REAL
 * Test connection (3 outcomes incl. the SSRF-allowlist refusal), save order
 * key → model; read-only (501) renders disabled + the deploy-time note —
 * never a live-looking form the backend would reject. */

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Banner, Skeleton } from "@/components/ui/badge";
import { Input, Label, Select } from "@/components/ui/input";
import { api, type ApiError, type ModelConfigView, type ModelTestResult } from "@/lib/api";
import { loadConfig, showSettings } from "@/store/app";
import { useAppState } from "@/store/hooks";

/* Presets pre-fill the form (ported from the vanilla CFG_PRESETS). */
const PRESETS = {
  llamacpp: { base_url: "http://127.0.0.1:8080", model: "qwen2.5-0.5b-instruct", api: false },
  ollama: { base_url: "http://127.0.0.1:11434/v1", model: "qwen2.5:0.5b", api: false },
  openrouter: { base_url: "https://openrouter.ai/api/v1", model: "anthropic/claude-sonnet-5", api: true },
} as const;
type PresetKey = keyof typeof PRESETS | "custom";

function presetOf(view: ModelConfigView): PresetKey {
  return (
    ({ local: "llamacpp", ollama: "ollama", openrouter: "openrouter" } as const)[
      view.provider as "local" | "ollama" | "openrouter"
    ] ?? "custom"
  );
}

export function ModelConfig() {
  const s = useAppState();
  const readonly = s.config.status === "readonly";
  const [provider, setProvider] = useState<"local" | "api">("local");
  const [preset, setPreset] = useState<PresetKey>("llamacpp");
  const [baseUrl, setBaseUrl] = useState("");
  const [model, setModel] = useState("");
  const [key, setKey] = useState("");
  const [keySet, setKeySet] = useState(false);
  const [result, setResult] = useState<{ text: string; kind: "ok" | "err" | "" }>({ text: "", kind: "" });
  const [busy, setBusy] = useState<"" | "test" | "save">("");

  // Prefill from GET /config/model (flow 02 step 1).
  useEffect(() => {
    const v = s.config.view;
    if (!v) return;
    setPreset(presetOf(v));
    setProvider(v.provider === "openrouter" ? "api" : "local");
    setBaseUrl(v.base_url ?? "");
    setModel(v.model);
    setKeySet(v.api_key_set);
  }, [s.config.view]);

  const applyPreset = (p: PresetKey) => {
    setPreset(p);
    if (p !== "custom") {
      const c = PRESETS[p];
      setBaseUrl(c.base_url);
      setModel(c.model);
      setProvider(c.api ? "api" : "local");
    }
  };

  // Which provider the /test endpoint names: local | ollama | openrouter.
  const testKind = () =>
    preset === "custom"
      ? provider === "api"
        ? "openrouter"
        : "local"
      : preset === "llamacpp"
        ? "local"
        : preset;

  const doTest = async () => {
    if (!baseUrl.trim()) {
      setResult({ text: "Enter a base URL first.", kind: "err" });
      return;
    }
    setBusy("test");
    setResult({ text: "Testing…", kind: "" });
    try {
      const res = await api<ModelTestResult>("/api/v1/config/model/test", {
        method: "POST",
        body: JSON.stringify({ provider: testKind(), base_url: baseUrl.trim() }),
      });
      // Three distinct outcomes (flow 02): reachable / bad / SSRF-refused.
      if (res.ok) {
        setResult({ text: `✓ Reachable — ${res.detail}`, kind: "ok" });
      } else if (res.detail.includes("allowlist")) {
        setResult({
          text: "✕ Refused — this address isn't on the connection allowlist (presets + your configured base URL).",
          kind: "err",
        });
      } else {
        setResult({ text: `✕ ${res.detail}`, kind: "err" });
      }
    } catch (e) {
      setResult({ text: `✕ ${(e as Error).message}`, kind: "err" });
    } finally {
      setBusy("");
    }
  };

  const doSave = async () => {
    setBusy("save");
    setResult({ text: "Saving…", kind: "" });
    try {
      if (key) {
        // Key first (flow 02): stored server-side 0600, never echoed back.
        await api("/api/v1/config/model/key", {
          method: "POST",
          body: JSON.stringify({ api_key: key }),
        });
        setKey("");
        setKeySet(true);
      }
      const scheme = testKind() === "openrouter" ? "openrouter" : "local";
      await api<ModelConfigView>("/api/v1/config/model", {
        method: "POST",
        body: JSON.stringify({
          engine: `${scheme}:${model.trim()}`,
          base_url: baseUrl.trim(),
        }),
      });
      await loadConfig();
      setResult({ text: "Saved. Model configured.", kind: "ok" });
      window.setTimeout(() => showSettings(false), 600); // return to Runs
    } catch (e) {
      const err = e as ApiError;
      setResult({ text: `✕ ${err.message}`, kind: "err" }); // 422 reason inline
    } finally {
      setBusy("");
    }
  };

  // ⌘S (design/04 §1) while Settings is open.
  useEffect(() => {
    const onSave = () => {
      if (!readonly && busy === "") void doSave();
    };
    window.addEventListener("kelter:save-settings", onSave);
    return () => window.removeEventListener("kelter:save-settings", onSave);
  });

  if (s.config.status === "loading") {
    return (
      <div className="mx-auto w-full max-w-md space-y-3 p-6">
        {[0, 1, 2, 3].map((i) => (
          <Skeleton key={i} className="h-8 w-full" />
        ))}
      </div>
    );
  }

  return (
    <div className="mx-auto w-full max-w-md p-6">
      <div className="flex items-center justify-between">
        <h2 className="text-title3 font-semibold">Settings — Model</h2>
        <Button variant="secondary" size="sm" onClick={() => showSettings(false)}>
          Done <kbd className="text-caption2 opacity-70" aria-hidden="true">⎋</kbd>
        </Button>
      </div>

      {readonly && (
        <Banner tone="info" className="mt-3">
          This deployment's model is fixed at deploy time — not editable here.
        </Banner>
      )}

      <div className="mt-4 space-y-3">
        <fieldset disabled={readonly} className="space-y-3 disabled:opacity-60">
          <div>
            <legend className="sr-only">Provider</legend>
            <Label>Provider</Label>
            <div role="radiogroup" aria-label="Provider" className="mt-1 flex gap-1 rounded-md border border-border bg-input p-0.5">
              {(["local", "api"] as const).map((p) => (
                <button
                  key={p}
                  role="radio"
                  aria-checked={provider === p}
                  onClick={() => setProvider(p)}
                  className={`flex-1 h-7 rounded-sm text-caption font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background ${
                    provider === p
                      ? "bg-card-raised text-foreground"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  {p === "local" ? "Local" : "API"}
                </button>
              ))}
            </div>
          </div>

          <div className="space-y-1">
            <Label htmlFor="cfg-preset">Preset</Label>
            <Select
              id="cfg-preset"
              value={preset}
              onChange={(e) => applyPreset(e.target.value as PresetKey)}
            >
              <option value="llamacpp">llama.cpp (local)</option>
              <option value="ollama">Ollama (local)</option>
              <option value="openrouter">OpenRouter (API)</option>
              <option value="custom">Custom</option>
            </Select>
          </div>

          <div className="space-y-1">
            <Label htmlFor="cfg-base-url">Base URL</Label>
            <Input
              id="cfg-base-url"
              placeholder="http://127.0.0.1:8080"
              autoComplete="off"
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
            />
          </div>

          <div className="space-y-1">
            <Label htmlFor="cfg-model">Model</Label>
            <Input
              id="cfg-model"
              placeholder="qwen2.5-0.5b-instruct"
              autoComplete="off"
              value={model}
              onChange={(e) => setModel(e.target.value)}
            />
          </div>

          {provider === "api" && (
            <div className="space-y-1">
              <Label htmlFor="cfg-key">API key</Label>
              <Input
                id="cfg-key"
                type="password"
                placeholder="sk-..."
                autoComplete="off"
                value={key}
                onChange={(e) => setKey(e.target.value)}
                aria-describedby="cfg-key-note"
              />
              <p id="cfg-key-note" className="text-caption2 text-muted-foreground">
                {keySet
                  ? "A key is already stored server-side. Leave blank to keep it."
                  : "Stored server-side, never committed or logged; or set OPENROUTER_API_KEY."}
              </p>
            </div>
          )}

          <div className="flex gap-2 pt-1">
            <Button variant="secondary" onClick={() => void doTest()} loading={busy === "test"}>
              {busy === "test" ? "Testing…" : "Test connection"}
            </Button>
            <Button onClick={() => void doSave()} loading={busy === "save"} aria-keyshortcuts="Meta+S">
              {busy === "save" ? "Saving…" : "Save"}
            </Button>
          </div>
        </fieldset>

        <p
          role="status"
          aria-live="polite"
          className={`min-h-4 text-caption ${
            result.kind === "ok"
              ? "text-success"
              : result.kind === "err"
                ? "text-danger"
                : "text-muted-foreground"
          }`}
        >
          {result.text}
        </p>
      </div>
    </div>
  );
}
