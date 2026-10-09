# xlite Admin UI Design Specification

`docs/design.md` is the **only UI design entry**. Chapter details live under [`docs/design/`](design/). Do **not** load every chapter by default — use skill [`admin-ui-change`](../.agents/skills/admin-ui-change/SKILL.md).

## Overview

xlite Admin UI is a dense operational console, not a marketing site. The interface should feel controlled, technical, and calm: white and near-neutral surfaces, one clearly recognizable primary accent, and explicit semantic state colors for success, warning, and destructive paths.

The visual baseline is unified across pages. Every page should read as part of the same system by reusing the same semantic tokens, shared layout shell, card treatment, and dialog structure. When a case is not covered by a literal token, prefer consistency with the existing admin UI guidance over introducing a new visual dialect.

The intended tone is pragmatic and high-signal:

- dense enough for operators handling providers, models, channels, and API keys
- restrained enough to avoid dashboard noise
- readable in both English and Chinese without layout breakage
- accessible enough that focus, state, and destructive actions are always unambiguous

Composition:

- Palette is **semantic color + alpha** (10% selected fill, 15% status hover, 20% focus ring). Do not invent a 50–900 ramp.
- Chrome is fixed sidebar (256px / 48px) + top bar (56px) + fluid work area. The top bar holds collapse, theme, language, logout — **not** business actions. Those belong in `PageHeader`.
- **One primary button per view.** Secondary / ghost for the rest.
- Pick one of the three page types below. Do not invent a fourth chrome.

## How agents use this file

Inside the repo, this file is **judgment**, not a CSS dump. Three layers (same idea as a public `design.md` + stylesheet + checks):

| Layer | Lives here | Agent must |
| --- | --- | --- |
| **Judgment** | This file: reader job, hierarchy, copy, composition | Frame the page for the operator's job before picking widgets |
| **Vocabulary** | [`design/tokens.md`](design/tokens.md) + `admin/src/index.css` + `admin/src/components/ui/*` | Use named tokens and shared components. **Do not invent** colors, radii, or a parallel class set |
| **Checks** | PR checklist below + skill [`admin-ui-change`](../.agents/skills/admin-ui-change/SKILL.md) | Fail closed on mechanical rules (dialog height, i18n, lint). Subjective hierarchy is still a human call |

Do not collapse chapters into one mega-prompt. Load this entry, then **only** the chapter the task needs. Vague phrases ("keep it clean") are not a spec; named anti-patterns in [`design/dos-donts.md`](design/dos-donts.md) are.

**Reader jobs** (same visual language, different structure — do not force one template):

| Job | Page type | Shape |
| --- | --- | --- |
| Scan and act | 单栏表格型 | Toolbar (search / sort / count) → full-width table; create and edit open a Dialog over the list |
| Glance then drill | 数据看板型 | KPI row → main module |
| Change global config | Settings | Settings page only |

## Agent reading map

Always load this file (Overview + Hard rules + PR checklist below). Then open **only** the matching chapter files.

| Task | Read |
| --- | --- |
| Tokens / brand hex table | [`design/tokens.md`](design/tokens.md) |
| Colors / dark mode / status | [`design/colors.md`](design/colors.md) |
| Titles, density, wrapping | [`design/typography.md`](design/typography.md), [`design/layout.md`](design/layout.md) |
| Shell, cards, page composition, toolbars, filters | [`design/layout.md`](design/layout.md) |
| List / detail dialog / create dialog | [`design/components.md`](design/components.md) |
| Shadows, radius, surfaces | [`design/surfaces.md`](design/surfaces.md) |
| Quick anti-patterns | [`design/dos-donts.md`](design/dos-donts.md) |
| Editing the visual baseline | touched chapter(s) + Hard rules / PR checklist; token table in `design/tokens.md` |

## Scope

xlite Admin has exactly four top-level surfaces — **overview, models (with providers and model instances), API keys, settings** — plus login. This is a **closed set**. There is no organization, project, route, user, role, quota, billing, or plugin domain, and no routing-strategy concept: the data plane is byte passthrough with weighted-random selection plus priority failover only. Do not add pages, nav entries, filters, columns, or copy for those concepts; adding a real capability is a product decision that starts in [`architecture.md`](architecture.md) and `AGENTS.md` rule 9, not in a page. Providers and models share one menu: a model row is a model instance (public name + one provider binding); providers are created inline from inner-provider presets (or custom) while adding a model, and the page shows only model instances.

Applies to `admin/src/pages/*`, `admin/src/components/*`, and `admin/src/components/ui/*`. Third-party internals and code-highlight themes are out of scope for wrapper-controlled styling only.

## Hard rules

0. **Stack lock.** Admin UI is `admin/` (React + `admin/src/components/ui`). **Never** add a parallel page as raw `index.html` + inline CSS/JS, a second app, or CDN Bootstrap. Reports and one-off HTML belong outside this tree, not in `admin/`.
1. No hard-coded page colors (`#hex`, `bg-violet-*`, `text-red-*`, `bg-gray-*`) in product pages.
2. No native `<select>`; use the shared `Select` component.
3. Do not invent a second primary button color. **One primary button per view.**
4. Dialogs keep `DialogHeader` / `DialogFooter` structure. All entity creation and edit opens as a Dialog/Modal over the page — never flattened/tiled inline across the page, never replacing the whole view. Every xlite create form is a compact single-page Dialog; there are no multi-step wizards.
5. **Dialog viewport bounds, width stability & footer visibility**: all popups and dialogs must never exceed screen height or width (`max-h-[85vh]` with `overflow-y-auto`). Dialog containers keep a stable, fixed responsive width (`w-full max-w-lg` / `max-w-xl` / `max-w-2xl` / `max-w-3xl`) and **must never shrinkwrap or jitter in width or height** when fields or tabs are switched. Forms inside dialogs stay compact by default to avoid internal scrolling, with submit/cancel action buttons (`DialogFooter`) **always prominently visible** without the user scrolling to find them. Large blocks of examples or technical tokens inside dialogs use collapsible accordions (or tabs) rather than unconstrained vertical stacking that pushes action buttons or headers off-screen.
6. Popups (Dialog / Sheet / AlertDialog / Popover) must close when the user clicks outside the popup content (overlay / dimmed area) or presses Escape; do not disable overlay dismiss without an explicit, documented exception.
7. Operational and destructive actions are triggered by dedicated buttons opening an interactive modal with validation and cancel/submit footers, leaving the main content area focused on list review and status rather than stacking flat inline input cards.
8. Never use native browser popups (`window.confirm` / `alert` / `prompt`). Delete and other destructive confirms use `AlertDialog` / `ConfirmAlertDialog`; transient feedback uses toast.
9. Prefer `t()` for copy; reserve wrap/truncate strategy for long IDs, keys, and model names.
10. The canonical X-series brand asset uses `#2744A5`; brand identity, interactive primary, and semantic status colors remain separate roles.
11. Visible keyboard focus is mandatory, and icon-only controls require an accessible name and tooltip.
12. Entity lists and the sidebar follow quiet selection: light primary fill (`bg-primary/10`) and weight only. **Never** draw a theme-colored left border or vertical accent bar on table rows or sidebar items — there is no sanctioned exception.
13. Entity detail / edit uses a compact Dialog: an overview block of labeled fields, optional level-2 grouped rows, and a `DialogFooter` with Cancel plus one primary Save. Prefer opening it from the row's Edit action.
14. Layout stability: selecting a value or toggling a section must not shove the rest of the dialog up or down. Reserve a fixed slot for post-selection details rather than mounting a block at zero height.
15. Shared overlays must provide dialog semantics, focus trap, initial focus, and focus restore.
16. **No casual subtitles.** Do not add page, card, or section subtitles that restate the title or fill space. Use a subtitle only when it carries an instruction the title cannot express. Prefer no subtitle.
17. **Surface levels.** Level 1 (on the canvas) is `bg-card` + `rounded-lg` + `shadow-card` with **no hairline border**; level 2 (inside a card or dialog) is a tonal fill (`bg-muted/30`) with no border and no shadow; controls keep their own treatment. The canvas is `bg-canvas` and is never repainted per page. Status-tinted borders are the only allowed exception on a level-1 card. See [`design/surfaces.md`](design/surfaces.md).
18. **Live data is real.** Never substitute preview payloads for an empty API response: an empty gateway must look empty (zeros, `—`, empty states), not busy.

## PR checklist

0. No new standalone HTML/JS document; Admin stays in `admin/` React.
1. Colors come from semantic tokens (`primary` / `destructive` / `success` / `warning` / `muted`); surfaces follow the level model (level 1 floats with `shadow-card`, level 2 uses tone, no hairline on cards).
2. Buttons use shared `Button` variants; **one primary per view**; no native selects; business actions are in `PageHeader`, not the top bar.
3. Dialogs follow shared structure and overlay accessibility; overlay / Escape dismiss works; no native `confirm`/`alert`; delete uses `AlertDialog`; selected rows/sidebar use `bg-primary/10` without a theme accent bar.
4. Long Chinese/English/key/model text does not overflow or obscure metrics.
5. Table horizontal scroll stays inside the table container.
6. Selecting options or toggling optional sections does not jitter dialog layout (reserved slots / min-height).
7. `npm run lint` passes (this includes the design-token guard `admin/scripts/check-design-tokens.mjs`: native palette classes, hard-coded colors, `rounded-2xl/3xl`, `!important` spacing overrides, and card-surface drift such as `bg-card` + neutral border or `shadow-sm` fail the gate).
8. New cards and pages do not add decorative subtitles.
9. User-visible surface change → `cd admin && npx tsc -b --noEmit && npm run lint && npx vitest run` are green, and the delivery note names the reader job and the design chapters followed. This repo has **no screenshot/visual-diff suite** — do not cite committed Linux baselines, an `e2e/visual/` spec, or a `--visual-update` flag; none of them exist.

Change order when editing surfaces: tokens (`index.css` / Tailwind) → `components/ui/*` → pages/components. Appearance changes are guarded mechanically by the design-token lint plus unit tests, and otherwise by the delivery note; there is no pixel baseline to regenerate.
