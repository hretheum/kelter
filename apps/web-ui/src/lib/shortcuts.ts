/* Global keymap (design/04 §1). The only place Enter submits is the composer
 * (handled locally there); this module wires the app-level chords. */

export interface ShortcutHandlers {
  newRun: () => void;
  openSettings: () => void;
  closeOverlay: () => boolean; // returns true if something closed (Esc chain)
  saveSettings: () => boolean; // ⌘S in settings; false = not applicable
  focusList: () => void; // ⌘K (design/07 D-5)
}

export function installShortcuts(h: ShortcutHandlers): () => void {
  const onKey = (e: KeyboardEvent) => {
    const mod = e.metaKey || e.ctrlKey;
    if (mod && e.key.toLowerCase() === "n") {
      e.preventDefault();
      h.newRun();
    } else if (mod && e.key === ",") {
      e.preventDefault();
      h.openSettings();
    } else if (mod && e.key.toLowerCase() === "s") {
      if (h.saveSettings()) e.preventDefault();
    } else if (mod && e.key.toLowerCase() === "k") {
      e.preventDefault();
      h.focusList();
    } else if (e.key === "Escape") {
      if (h.closeOverlay()) e.preventDefault();
    }
  };
  window.addEventListener("keydown", onKey);
  return () => window.removeEventListener("keydown", onKey);
}
