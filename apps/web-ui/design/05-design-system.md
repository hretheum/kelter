<!-- provenance: ai_authored | agent: claude-opus-4-8 | session_date: 2026-07-19 | human_validated: false | context: Kelter web-ui redesign — Stage 5 design system: HIG-flavored, dark-first tokens as a Tailwind v4 @theme + shadcn component variants with all states. Values are deliberate, not shadcn defaults. -->

# 05 — Design system (HIG-flavored, dark-first)

Tokens expressed as a **Tailwind v4 `@theme`** block + **shadcn/ui component
variants (CVA)**. Deliberately tuned to Apple HIG values on the web — SF Pro
system stack, ~13px body, macOS-dense spacing, vibrancy — **not** stock shadcn.
Dark-first; a light theme is provided but secondary (the app ships dark).

---

## 1. Foundations

### 1.1 Typography — SF Pro, macOS scale

Native system stack (no web-font payload — keeps the ~2–2.5MB bundle budget):

```
--font-sans: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", system-ui, sans-serif;
--font-mono: "SF Mono", ui-monospace, "JetBrains Mono", "Menlo", monospace;
```

Body is **13px** (macOS control size), not the 14–16px web default. Tight,
information-dense, HIG-correct.

| Token | px / line-height | weight | Use |
|---|---|---|---|
| `text-caption2` | 11 / 13 | 400 | timestamps, meta, chips |
| `text-caption` | 12 / 16 | 400/500 | labels, secondary meta |
| `text-body` | **13 / 18** | 400 | **default body**, inputs, rows |
| `text-callout` | 14 / 19 | 500 | section titles, buttons |
| `text-title3` | 16 / 21 | 600 | run title, screen header |
| `text-title2` | 20 / 25 | 600 | rare — empty-state hero |
| `text-mono` | 12 / 18 | 400 | JSON args/results, tokens |

- Letter-spacing: 0 at these sizes (SF optical); `-0.01em` at title sizes.
- Numeric: `font-variant-numeric: tabular-nums` for timers, cost, seq/step.

### 1.2 Color — OKLCH, dark-first

Perceptually-tuned OKLCH. Dark is the ground truth; the chrome uses a faint blue
undertone (hue ≈ 250) matching Kelter's existing `#101418` lineage but cleaner.

**Dark (default) — `@theme`:**

| Semantic | OKLCH | ~hex | Role |
|---|---|---|---|
| `background` | `oklch(20% 0.012 250)` | #16191d | app canvas |
| `card` / panel | `oklch(24% 0.014 250)` | #1d2127 | surfaces, rows, cards |
| `card-raised` | `oklch(27% 0.015 250)` | #232830 | popovers, sheets, hover |
| `foreground` | `oklch(93% 0.006 250)` | #e6e9ec | primary text |
| `muted-foreground` | `oklch(68% 0.012 250)` | #97a0ab | secondary text |
| `border` | `oklch(32% 0.014 250)` | #2b313a | hairlines, dividers |
| `input` | `oklch(28% 0.014 250)` | #242a32 | field fills |
| `primary` | `oklch(62% 0.17 255)` | #4ea1ff | accent (macOS system blue), CTAs |
| `primary-foreground` | `oklch(18% 0.03 255)` | #06121f | text on accent |
| `ring` | `oklch(70% 0.15 255)` | #6cb6ff | focus ring |
| `success` | `oklch(72% 0.17 150)` | #3fb950 | completed, ok, reachable |
| `danger` | `oklch(64% 0.20 25)` | #f85149 | failed, rejected, error |
| `warning` | `oklch(78% 0.15 75)` | #e3a008 | connecting, canceled, lagged |

**Semantic event/status colors** (drive R-1 glyphs + R-3 axis; always paired with
a glyph/text, never color-only — see a11y):

| Token | maps to | events / states |
|---|---|---|
| `--color-ev-proposed` | primary | `proposed` |
| `--color-ev-executed` | foreground | `executed ok:true` |
| `--color-ev-rejected` | danger | `rejected`, `executed ok:false` |
| `--color-ev-completed` | success | `completed`, status completed |
| `--color-ev-failed` | danger | `failed`, status failed |
| `--color-ev-quiet` | muted-foreground | `created`, `reflection_written`, canceled |

**Light theme** (secondary, `@custom-variant`/`.light`): invert L — background
`oklch(98% 0.003 250)`, card `oklch(100% 0 0)`, foreground `oklch(22% 0.01 250)`,
border `oklch(90% 0.006 250)`; primary hue held, L→`55%`. Contrast re-verified in
`08` for both.

### 1.3 Spacing — macOS-dense (4px base)

```
--space-0.5:2px  --space-1:4px  --space-1.5:6px  --space-2:8px
--space-3:12px   --space-4:16px --space-5:20px   --space-6:24px  --space-8:32px
```

Default control padding is tight: buttons `6px 12px`, inputs `6px 10px`, rows
`6px 10px`, section padding `12–16px`. This is HIG density, not web roominess.

### 1.4 Radius

```
--radius-sm:5px  --radius-md:7px  --radius-lg:10px  --radius-full:9999px
```

Fields/buttons `md` (7px), cards/sheets `lg` (10px), pills/chips `full`.

### 1.5 Elevation & vibrancy

Dark HIG uses light borders + soft shadow + backdrop blur, not heavy drop
shadows:

```
--elevation-0: none;                                   /* flush surfaces */
--elevation-1: 0 1px 0 0 oklch(100% 0 0 / 4%) inset,   /* top hairline */
               0 1px 2px oklch(0% 0 0 / 30%);          /* rows, cards */
--elevation-2: 0 8px 24px oklch(0% 0 0 / 40%);         /* popover, sheet */
--vibrancy: saturate(180%) blur(20px);                 /* top bar, sheets */
```

Top bar and G-2 sheet use `backdrop-filter: var(--vibrancy)` over a translucent
`card / 72%` fill — the macOS material look, degrading to solid where blur is
unsupported.

### 1.6 Motion

```
--motion-fast: 120ms;  --motion-base: 150ms;  --motion-slow: 220ms;
--ease-out: cubic-bezier(0.2, 0, 0, 1);   /* macOS ease-out */
--ease-in-out: cubic-bezier(0.4, 0, 0.2, 1);
```

All motion gated by `prefers-reduced-motion` (see `04 §8`). Keyframes: `fade-in`,
`slide-in` (≤8px), `pulse` (live badge). Nothing bounces.

---

## 2. The `@theme` block (implementation-ready, Tailwind v4)

```css
@import "tailwindcss";
@custom-variant light (&:where(.light, .light *));   /* dark is default */

@theme {
  /* type */
  --font-sans: -apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif;
  --font-mono: "SF Mono", ui-monospace, "JetBrains Mono", Menlo, monospace;
  --text-caption2: 0.6875rem;  --text-caption2--line-height: 0.8125rem;
  --text-caption:  0.75rem;    --text-caption--line-height: 1rem;
  --text-body:     0.8125rem;  --text-body--line-height: 1.125rem;   /* 13px */
  --text-callout:  0.875rem;   --text-callout--line-height: 1.1875rem;
  --text-title3:   1rem;       --text-title3--line-height: 1.3125rem;
  --text-title2:   1.25rem;    --text-title2--line-height: 1.5625rem;

  /* color — dark ground truth */
  --color-background: oklch(20% 0.012 250);
  --color-card: oklch(24% 0.014 250);
  --color-card-raised: oklch(27% 0.015 250);
  --color-foreground: oklch(93% 0.006 250);
  --color-muted-foreground: oklch(68% 0.012 250);
  --color-border: oklch(32% 0.014 250);
  --color-input: oklch(28% 0.014 250);
  --color-primary: oklch(62% 0.17 255);
  --color-primary-foreground: oklch(18% 0.03 255);
  --color-ring: oklch(70% 0.15 255);
  --color-success: oklch(72% 0.17 150);
  --color-danger: oklch(64% 0.20 25);
  --color-warning: oklch(78% 0.15 75);
  /* event/status aliases */
  --color-ev-proposed: var(--color-primary);
  --color-ev-executed: var(--color-foreground);
  --color-ev-rejected: var(--color-danger);
  --color-ev-completed: var(--color-success);
  --color-ev-failed: var(--color-danger);
  --color-ev-quiet: var(--color-muted-foreground);

  /* radius / spacing extras / motion */
  --radius-sm: 5px; --radius-md: 7px; --radius-lg: 10px;
  --animate-slide-in: slide-in 150ms cubic-bezier(0.2,0,0,1);
  --animate-fade-in: fade-in 120ms ease-out;
  --animate-pulse-live: pulse-live 1.6s ease-in-out infinite;

  @keyframes fade-in { from{opacity:0} to{opacity:1} }
  @keyframes slide-in { from{opacity:0;transform:translateY(6px)} to{opacity:1;transform:translateY(0)} }
  @keyframes pulse-live { 0%,100%{opacity:1} 50%{opacity:.45} }
}

.light {
  --color-background: oklch(98% 0.003 250);
  --color-card: oklch(100% 0 0);
  --color-card-raised: oklch(97% 0.004 250);
  --color-foreground: oklch(22% 0.01 250);
  --color-muted-foreground: oklch(48% 0.012 250);
  --color-border: oklch(90% 0.006 250);
  --color-input: oklch(96% 0.004 250);
  --color-primary: oklch(55% 0.18 255);
  --color-primary-foreground: oklch(99% 0.005 255);
  --color-ring: oklch(55% 0.18 255);
}

@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after { animation-duration: .001ms !important; transition-duration: .001ms !important; }
}
```

---

## 3. Component inventory → shadcn, with every state

Each row: the component, which **shadcn/ui** primitive it maps to, the variants we
add, and the states (default · hover · focus-visible · active · disabled ·
loading · error · empty) that must be built.

| Kelter component | shadcn base | Variants | States to build |
|---|---|---|---|
| **Button** | `button` (CVA) | `primary` (Run/Save/Connect), `secondary` (Test), `ghost` (chips), `link` (top-bar) | default · hover (`/90`) · focus-ring · active · disabled (`opacity-50`) · **loading** (spinner + label swap: "Running…"/"Saving…"/"Testing…") |
| **Textarea** (composer) | `textarea` | auto-grow 3–6 rows | default · focus-ring · disabled · **error** (`border-danger` + msg) |
| **Input** (base URL, model, token) | `input` | `password` for key/token | default · focus · disabled · error · read-only (501) |
| **Radio group** (Provider) | `radio-group` | segmented (Local/API) | default · checked · focus · disabled |
| **Select** (Preset) | `select` | — | default · open · focus · disabled |
| **Run list row** | custom on `command`/list | status-colored | default · hover · **selected** (`aria-selected`) · running (pulse) · disabled/stale |
| **Step card** (timeline) | `collapsible` + `card` | proposed/executed/rejected | collapsed · expanded (`aria-expanded`) · rejected (danger chip) · executed-failed |
| **Status pill** (connection) | `badge` | connected/connecting/disconnected/auth-failed | 4 states, glyph+text each |
| **Model chip** (top bar) | `button` ghost + `badge` | — | default · hover · focus (opens Settings) · absent ("—") |
| **Result block** | `card` + `<pre>` | output/error | output · error · empty ("no output") · scroll-overflow |
| **Copy button** | `button` ghost sm | — | default · hover · **copied** (label "Copied ✓" 1.5s) · fallback (no clipboard) |
| **Connection sheet** | `dialog` (native popover, §Pattern 5) | — | idle · connecting · error(401/429/network) · success |
| **Settings view** | route + `card` sections | mutable/read-only | editable · **read-only (501)** (disabled + banner) |
| **Banner** (drop / lagged / 501) | `alert` | info/warning/danger | one line, dismiss-none for persistent states |
| **Skeleton** | `skeleton` | row / field / block | shimmer, reduced-motion → static |
| **Empty state** | custom | list / composer / result | title + subtitle + first-action control |
| **Toast/live-region** | `sonner` or aria-live div | polite/assertive | announcements only (`04 §6`), not for run failures (those are content) |

### 3.1 Button CVA (concrete, tuned)

```ts
const button = cva(
  "inline-flex items-center justify-center gap-1.5 rounded-md text-callout font-medium " +
  "transition-colors duration-[120ms] focus-visible:outline-none focus-visible:ring-2 " +
  "focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background " +
  "disabled:pointer-events-none disabled:opacity-50",
  { variants: {
      variant: {
        primary:   "bg-primary text-primary-foreground hover:bg-primary/90 active:bg-primary/80",
        secondary: "bg-card-raised text-foreground border border-border hover:bg-card-raised/80",
        ghost:     "text-muted-foreground hover:text-foreground hover:bg-card-raised/60",
        link:      "text-muted-foreground hover:text-foreground underline-offset-4 hover:underline",
      },
      size: { sm:"h-7 px-2.5", md:"h-8 px-3", lg:"h-9 px-4", icon:"size-8" },
    }, defaultVariants:{ variant:"primary", size:"md" } }
)
```

Heights (28/32/36px) are macOS control sizes, not the shadcn 40px default — the
deliberate HIG tuning the brief requires.

### 3.2 Focus ring (global utility)

`focus-visible:ring-2 ring-ring ring-offset-2 ring-offset-background` — 2px accent,
2px offset, ≥3:1 against control and canvas (verified in `08`). Never removed.

---

## 4. Iconography

System-weight line glyphs (inline SVG, no icon-font payload): status glyphs
(◔◐✓✕⊘), connection (◉○◐⚠), gear, plus, chevron, copy. All have text equivalents;
none is the sole carrier of meaning. Keep the SVG set tiny (bundle budget).

---

## 5. Why this is not stock shadcn (brief §4.5 / §5)

- 13px body + macOS type ramp (shadcn defaults to 14–16px).
- 28/32/36px control heights (shadcn: 36–40px).
- OKLCH blue-undertone dark ground with vibrancy/hairline elevation (shadcn:
  neutral zinc, heavier shadows).
- Event-semantic color aliases bound to the real `RunEvent` union — a
  Kelter-specific token layer that stock shadcn has no concept of.
