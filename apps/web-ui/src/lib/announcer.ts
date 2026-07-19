/* Screen-reader announcements (design/04 §6): milestone-throttled — never one
 * per proposed/executed pair on a fast run. Polite by default; assertive only
 * for a run failure (a result the user must notice). Live regions never steal
 * focus. */

let politeEl: HTMLElement | null = null;
let assertiveEl: HTMLElement | null = null;

function ensure(): void {
  if (politeEl) return;
  for (const [ariaLive, assign] of [
    ["polite", (el: HTMLElement) => (politeEl = el)],
    ["assertive", (el: HTMLElement) => (assertiveEl = el)],
  ] as const) {
    const el = document.createElement("div");
    el.setAttribute("role", "status");
    el.setAttribute("aria-live", ariaLive);
    el.setAttribute("aria-atomic", "true");
    el.className = "sr-only";
    el.style.cssText =
      "position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0)";
    document.body.appendChild(el);
    assign(el);
  }
}

export function announce(text: string, priority: "polite" | "assertive" = "polite") {
  ensure();
  const el = priority === "assertive" ? assertiveEl! : politeEl!;
  el.textContent = ""; // clear first so repeats re-announce
  window.setTimeout(() => {
    el.textContent = text;
  }, 50);
}
