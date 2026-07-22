/* The artifact panel (ADR 0029): files the run produced, surfaced beside the
 * result — open by default, markdown rendered, copy + download. Optional
 * context in the design/03 R-2 sense: it hides (never errors) when the daemon
 * has no files endpoint, and shows a quiet empty state when the run produced
 * nothing. */

import { useEffect, useState } from "react";
import ReactMarkdown from "react-markdown";
import { Button } from "@/components/ui/button";
import { announce } from "@/lib/announcer";
import { copyValue } from "@/lib/clipboard";
import type { RunFileInfo } from "@/lib/api";
import { fetchRunFile, fetchRunFiles } from "@/store/app";

function isMarkdown(name: string): boolean {
  return /\.(md|markdown)$/i.test(name);
}

function mimeOf(name: string): string {
  if (isMarkdown(name)) return "text/markdown";
  if (/\.json$/i.test(name)) return "application/json";
  return "text/plain";
}

function download(name: string, content: string) {
  const url = URL.createObjectURL(new Blob([content], { type: mimeOf(name) }));
  const a = document.createElement("a");
  a.href = url;
  /* The name is workspace-relative and may contain slashes — the last
   * segment is the file name to save under. */
  a.download = name.split("/").pop() ?? name;
  a.click();
  URL.revokeObjectURL(url);
}

/* Markdown → the app's type scale. react-markdown renders to React elements
 * (no innerHTML; raw HTML in the file is NOT executed), so a run's file can't
 * inject into the page. */
const MD_COMPONENTS: React.ComponentProps<typeof ReactMarkdown>["components"] = {
  h1: (p) => <h1 className="mt-3 mb-1.5 text-title3 font-semibold first:mt-0" {...p} />,
  h2: (p) => <h2 className="mt-3 mb-1 text-callout font-semibold first:mt-0" {...p} />,
  h3: (p) => <h3 className="mt-2 mb-1 text-body font-semibold first:mt-0" {...p} />,
  p: (p) => <p className="my-1.5 text-body" {...p} />,
  ul: (p) => <ul className="my-1.5 list-disc pl-5 text-body" {...p} />,
  ol: (p) => <ol className="my-1.5 list-decimal pl-5 text-body" {...p} />,
  li: (p) => <li className="my-0.5" {...p} />,
  a: (p) => (
    <a className="text-primary underline underline-offset-2 break-all" target="_blank" rel="noreferrer" {...p} />
  ),
  code: (p) => <code className="rounded-sm bg-card-raised px-1 font-mono text-caption" {...p} />,
  pre: (p) => (
    <pre className="my-1.5 overflow-x-auto rounded-md bg-card-raised p-2 font-mono text-caption" {...p} />
  ),
  blockquote: (p) => (
    <blockquote className="my-1.5 border-l-2 border-border pl-3 text-muted-foreground" {...p} />
  ),
  hr: () => <hr className="my-2 border-border" />,
};

export function ArtifactPanel({ runId }: { runId: string }) {
  const [files, setFiles] = useState<RunFileInfo[] | null>(null);
  const [open, setOpen] = useState(true); // opened by default
  const [selected, setSelected] = useState<string | null>(null);
  const [contents, setContents] = useState<Record<string, string>>({});
  const [loadError, setLoadError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let live = true;
    setFiles(null);
    setSelected(null);
    setContents({});
    setLoadError(null);
    fetchRunFiles(runId)
      .then((list) => {
        if (!live) return;
        setFiles(list);
        if (list.length > 0) setSelected(list[0].name);
      })
      .catch(() => {
        /* unsupported daemon or vanished run — the panel simply hides */
      });
    return () => {
      live = false;
    };
  }, [runId]);

  useEffect(() => {
    if (!selected || contents[selected] !== undefined) return;
    let live = true;
    fetchRunFile(runId, selected)
      .then((file) => {
        if (live) setContents((c) => ({ ...c, [file.name]: file.content }));
      })
      .catch((e) => {
        if (live) setLoadError((e as Error).message);
      });
    return () => {
      live = false;
    };
  }, [runId, selected, contents]);

  if (files === null) return null; // loading or unsupported: no chrome yet

  if (!open) {
    return (
      <div className="mt-4 md:mt-0 md:shrink-0">
        <Button variant="secondary" size="sm" onClick={() => setOpen(true)} aria-expanded={false}>
          Files ({files.length})
        </Button>
      </div>
    );
  }

  const content = selected != null ? contents[selected] : undefined;

  return (
    <aside
      aria-label="Produced files"
      className="mt-4 w-full min-w-0 rounded-lg border border-border bg-card md:mt-0 md:w-96 md:shrink-0"
    >
      <div className="flex items-center justify-between border-b border-border px-3 py-2">
        <h3 className="text-callout font-medium">
          Files{files.length > 0 && <span className="tnum text-muted-foreground"> ({files.length})</span>}
        </h3>
        <Button
          variant="ghost"
          size="sm"
          aria-label="Hide files panel"
          aria-expanded={true}
          onClick={() => setOpen(false)}
        >
          Hide
        </Button>
      </div>

      {files.length === 0 ? (
        <p className="px-3 py-4 text-caption text-muted-foreground">
          This run produced no files.
        </p>
      ) : (
        <>
          {files.length > 1 && (
            <div className="flex flex-wrap gap-1 border-b border-border px-2 py-1.5" role="tablist">
              {files.map((f) => (
                <button
                  key={f.name}
                  role="tab"
                  aria-selected={f.name === selected}
                  onClick={() => setSelected(f.name)}
                  className={`rounded-sm px-2 py-0.5 font-mono text-caption focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring ${
                    f.name === selected
                      ? "bg-card-raised text-foreground"
                      : "text-muted-foreground hover:text-foreground"
                  }`}
                >
                  {f.name}
                </button>
              ))}
            </div>
          )}
          {selected != null && (
            <div className="flex items-center justify-between gap-2 px-3 pt-2">
              <span className="truncate font-mono text-caption text-muted-foreground" title={selected}>
                {selected}
              </span>
              <span className="flex shrink-0 items-center">
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={`Copy ${selected}`}
                  disabled={content === undefined}
                  onClick={async () => {
                    if (content === undefined) return;
                    const ok = await copyValue(content);
                    if (ok) {
                      announce("Copied");
                      setCopied(true);
                      window.setTimeout(() => setCopied(false), 1500);
                    }
                  }}
                >
                  {copied ? "Copied ✓" : "Copy"}
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  aria-label={`Download ${selected}`}
                  disabled={content === undefined}
                  onClick={() => content !== undefined && download(selected, content)}
                >
                  Download
                </Button>
              </span>
            </div>
          )}
          <div className="max-h-[32rem] overflow-y-auto px-3 pb-3 pt-1">
            {loadError ? (
              <p className="text-caption text-danger">Couldn't load this file. {loadError}</p>
            ) : content === undefined ? (
              <p className="text-caption text-muted-foreground">Loading…</p>
            ) : selected != null && isMarkdown(selected) ? (
              <ReactMarkdown components={MD_COMPONENTS}>{content}</ReactMarkdown>
            ) : (
              <pre className="font-mono text-caption whitespace-pre-wrap break-words">{content}</pre>
            )}
          </div>
        </>
      )}
    </aside>
  );
}
