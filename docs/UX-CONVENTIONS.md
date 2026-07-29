# UX Conventions

Living conventions for the Carrel frontend. Each section is enforced by a
scanner test in `src/lib/uxConsistency.audit.test.ts` where possible, so drift
fails CI rather than waiting on review.

## Spacing — 4px grid with a 2px sub-grid for compact components

- **Layout spacing** uses Tailwind's standard scale (`p-4`, `gap-3`, `mb-8`),
  which maps to 4px multiples. This is the default — reach for it first.
- **Compact components** (icon buttons, list rows, badges, search results) may
  use Tailwind's half-step classes (`p-1.5`, `py-1.5`, `mt-0.5`, `gap-1.5`).
  These resolve to 2px / 6px / 10px and form a deliberate 2px sub-grid for
  fine-grained nudges.
- **Arbitrary `[Npx]` / `[Nrem]` spacing values are banned unless the value is
  a 4px multiple.** A scanner test enforces this — `p-[7px]`, `mt-[13px]`, and
  similar one-offs fail CI. CSS variables and `calc()` expressions are
  allowed.

If you find yourself reaching for `p-[5px]`, the answer is almost always `p-1`
(4px) or `p-1.5` (6px). If you reach for `mt-[15px]`, the answer is `mt-4`
(16px).

## Inline SVG icons — strokeWidth 1.5 or 2

- **Outline icons** (Heroicons-style): `strokeWidth="1.5"`. Used for icons
  where the stroke carries the meaning (chevrons, arrows, tab-bar glyphs).
- **Filled-edge icons** (chunkier, square-cap glyphs): `strokeWidth="2"`.
  Used for header buttons, badges, X / + / checkmark glyphs.
- **Loading spinners** (`<svg className="animate-spin">…`): `strokeWidth="3"`
  or `"4"` allowed — small spinners need a thicker arc to read.

A scanner test enforces this — values like `1.75`, `2.5`, `3` (outside a
spinner), and `4` (outside a spinner) fail CI. Pick the cluster that matches
the icon family you're cribbing from; don't introduce a third stroke weight.

## Animation timing — 150 / 200 / 300 ms

Tailwind `duration-*` classes must come from the cluster the codebase
already converged on:

- **`duration-150`** — micro-interactions (button hover, focus rings, small
  state toggles).
- **`duration-200`** — default for transitions on widgets that move or
  rearrange (toggles, dropdowns, side-panel reveals).
- **`duration-300`** — larger transitions (modals, large drawers, full-row
  reveals).

A scanner test enforces this — `duration-250`, `duration-450`, and arbitrary
`duration-[180ms]` brackets fail CI.

CSS `@keyframes` durations live in `src/index.css` under `--animate-*` theme
tokens (`fade-in 0.18s`, `slide-in-* 0.22s`, `progress-fill 0.6s`,
`shimmer 1.5s`). Those are per-animation tuning and not subject to the
Tailwind cluster rule, but new keyframes should reuse one of those tokens
where possible rather than introducing a fresh duration.

The `prefers-reduced-motion: reduce` block in `src/index.css` collapses
animation/transition durations to `0.01ms` — that override is exempt from
all duration rules.

## Error surfaces — toast vs inline vs dialog

Three surfaces exist; each has a clear job. Pick by *how the user must
react*, not by how loud the message feels.

| Surface | When to use | API |
|---|---|---|
| **Toast** | Transient feedback after a deliberate user action that the user can shrug off and move on from. The view is still usable. Auto-dismisses in `TOAST_AUTO_DISMISS_MS` (4 s). | `useToast().addToast(msg, "success" \| "error" \| "info")` from `components/Toast.tsx` |
| **Inline error** | Operation-local failure tied to a specific field, button, or panel. Stays visible until the user retries or dismisses. | Local `useState` for the message, rendered as `{error && <p className="text-red-500 text-sm">{error}</p>}` (or the `bg-red-50` banner pattern at screen scope) |
| **Dialog** | Decisions: confirmations, destructive actions, choices that block the user from continuing. | Reach for the shared `ConfirmDialog` (`components/ConfirmDialog.tsx`) first; for richer forms use a modal with `role="dialog" aria-modal="true"` + `useFocusTrap` (see `BulkEditDialog`, `EditBookDialog`, `MissingFileDialog`) |

Decision rules:

1. **If the user must decide → Dialog.** A toast or inline message gives no
   way to choose between options.
2. **If the failure prevents the surrounding view from doing its job →
   Inline error.** A toast disappears in 4 s; a stuck-state failure shouldn't.
   Library-scope and Settings-scope errors use the `bg-red-50` banner at the
   top of the screen; component-scope errors use a small `text-red-500` line.
3. **Otherwise → Toast.** Bulk-action results, "Copied to clipboard",
   secondary "could not load X" failures where the page still works.

Anti-patterns to avoid:

- **Both** a toast *and* an inline error for the same failure — pick one.
- A toast for an error that recurs every render (it'll re-fire forever).
- A dialog for a non-decision message ("Saved successfully" should be a
  toast).
- A persistent banner for a transient success ("Imported 3 books" should be
  a toast).

The `useToast` hook lives at `components/Toast.tsx`. There is no
corresponding error-banner hook — banners live close to the screen state
they describe and don't generalize.

## Destructive actions — confirm vs undo

Two patterns, picked by reversibility. **Never use the browser `confirm()`** —
it's unstyled, unthemed, and gives no context.

| Action shape | Pattern | API |
|---|---|---|
| Reversible (delete book, bulk delete, remove-from-collection) | **Undo toast** — optimistically hide, fire the backend call only after a 5 s window, cancel on Undo | `useUndoableRemoval(addToast)` from `lib/useUndoableRemoval.ts` → `remove(ids, { message, undoLabel, commit, onError })`; filter `pendingIds` out of the list |
| Irreversible / high-stakes (delete profile, remove catalog, bulk delete confirmation) | **Styled confirm** — a blocking decision dialog | `ConfirmDialog` (`components/ConfirmDialog.tsx`): `title`, `message`, `confirmLabel`, `destructive`, `confirmDisabled`, `onConfirm`, `onCancel` |

Notes:

- The undo toast is **deferred execution**, not DB soft-delete: the
  irreversible work (file deletion) simply never runs if the user undoes.
  `Toast` supports this via `addToast(msg, type, { durationMs, action, onTimeout })`
  — `action` is the Undo button (cancels), `onTimeout` is the commit.
- Big-but-reversible ops can use **both**: a styled confirm *and* a follow-up
  undo toast (e.g. bulk delete). Keep the confirm copy honest — say "you'll
  have a few seconds to undo", not "cannot be undone".
- A deferred/undo `commit` that refreshes a view must read the **current**
  target at commit time (a ref), not a value captured when the toast was
  created — the user may navigate during the window.

## Async operations — never fail silently

Every `invoke()` (or any async action) needs a visible outcome:

- **Don't** swallow errors with `.catch(() => {})`. A toggle that fails to
  persist must revert its optimistic state **and** surface the failure
  (inline error or error toast).
- A success that closes a dialog / leaves no visible state change needs a
  confirmation toast ("Saved").
- A long operation (import, chapter load, download) needs progress: a
  determinate counter/bar when the backend emits progress events, an honest
  indeterminate state otherwise — **never a fabricated percentage**.
- A persisted setting that is write-only (no readback) needs an "unsaved"
  indicator and/or save-on-blur so the value isn't lost on close.

## Shared UI components

Reach for these before hand-rolling:

| Component | Use |
|---|---|
| `ConfirmDialog` | Destructive / blocking confirmations (replaces `confirm()`) |
| `OverflowMenu` | Tuck low-frequency header/toolbar actions behind a `⋯` menu instead of a flat icon row |
| `EmptyState` | Library-scope empty states |
| `ReaderSkeleton` | Reader loading placeholder — `variant="full"` (route fallback) or `variant="content"` (chapter load) |
| `useFocusTrap` | Tab/Escape trapping + autofocus for any modal |
| `useUndoableRemoval` | Deferred-execution undo for list removals |

## Dark mode — semantic tokens by default; risk-shade colors need `dark:`

Carrel's primary theming is **CSS-variable-based**. Tokens like `bg-paper`,
`text-ink`, `text-ink-muted`, `border-warm-border`, `bg-warm-subtle`,
`text-accent`, etc. swap automatically when the `.dark` class is set on the
root. Reach for these first — they are the only way to stay theme-correct
without thinking about it.

When you must use a non-semantic Tailwind palette color (typically for
status: errors, warnings, info), the rule is:

| Property | Risk shades | Why |
|---|---|---|
| `bg-{palette}-50/100/200` | light tints | unreadable on dark surfaces |
| `text-{palette}-700/800/900` | deep tints | unreadable as text on dark surfaces |
| `border-{palette}-50/100/200` | light borders | invisible on dark surfaces |

Each of these must have a `dark:` companion in the same `className` string,
matching the same property and palette. Common pattern:

```jsx
className="bg-red-50 dark:bg-red-900/20 text-red-700 dark:text-red-300 border-red-200 dark:border-red-900/40"
```

A scanner test enforces this — risk-shade classes lacking a dark companion
fail CI. Mid-saturation accent fills (`bg-red-600` for destructive buttons,
`bg-blue-500` for info pills) stay the same in both themes and are exempt.
