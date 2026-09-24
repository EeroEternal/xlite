# xlite Admin UI Design Specification

`docs/design.md` is the **only UI design entry**. Chapter details live under [`docs/design/`](design/). Do **not** load every chapter by default — use skill [`admin-ui-change`](../.agents/skills/admin-ui-change/SKILL.md).

## Overview

xlite Admin UI is a dense operational console, not a marketing site. The interface should feel controlled, technical, and calm: white and near-neutral surfaces, one clearly recognizable primary accent, and explicit semantic state colors for success, warning, and destructive paths.

The visual baseline is unified across pages. Every page should read as part of the same system by reusing the same semantic tokens, shared layout shell, card treatment, and dialog structure. When a case is not covered by a literal token, prefer consistency with the existing admin UI guidance over introducing a new visual dialect.

The intended tone is pragmatic and high-signal:

- dense enough for operators handling models, providers, channels, and API keys
- restrained enough to avoid dashboard noise
- readable in both English and Chinese without layout breakage
- accessible enough that focus, state, and destructive actions are always unambiguous

Composition (from the enterprise visual spec v1.0):

- Palette is **semantic color + alpha** (10% selected fill, 15% status hover, 20% focus ring). Do not invent a 50–900 ramp.
- Chrome is fixed sidebar (256px / 48px) + top bar (56px) + fluid work area. The top bar holds collapse, theme, language, org, notifications, user — **not** business actions. Those belong in `PageHeader`. While demo mode is on, the top bar also shows Exit demo (viewing-mode chrome, not a page action).
- **One primary button per view.** Secondary / ghost for the rest.
- Pick one of the six page types below. Do not invent a seventh chrome.

## How agents use this file

Inside the repo, this file is **judgment**, not a CSS dump. Three layers (same idea as a public `design.md` + stylesheet + checks):

| Layer | Lives here | Agent must |
| --- | --- | --- |
| **Judgment** | This file: reader job, hierarchy, copy, composition | Frame the page for the operator's job before picking widgets |
| **Vocabulary** | [`design/tokens.md`](design/tokens.md) + `admin/src/index.css` + `admin/src/components/ui/*` | Use named tokens and shared components. **Do not invent** colors, radii, or a parallel class set |
| **Checks** | PR checklist below + skill [`admin-ui-change`](../.agents/skills/admin-ui-change/SKILL.md) | Fail closed on mechanical rules (dialog height, i18n, lint). Subjective hierarchy is still a human call |

Do not collapse chapters into one mega-prompt. Load this entry, then **only** the chapter the task needs. Vague phrases ("keep it clean") are not a spec; named anti-patterns in [`design/dos-donts.md`](design/dos-donts.md) are.

**Reader jobs** (same visual language, different structure — do not force one template):

| Job | Page type (visual spec) | Shape |
| --- | --- | --- |
| Scan and act | 单栏表格型 | Filter/search → full-width table; create opens a Dialog over the list |
| Inspect while scanning | 列表+详情型 | Master–detail workspace |
| Browse a catalog | 双栏目录型 | Directory list + large content pane |
| Operate continuously | 工作台型 | History/nav + working pane (chat) |
| Glance then drill | 数据看板型 / 混合信息型 | KPI row → main module → optional auxiliaries; reserved card heights |
| Complete a valid object | Dialog / wizard | Create Dialog; Route create is the multi-step wizard |
| Change global config | Settings | Settings page only |

New rules enter this file only when a failure **repeats** (promote-lesson: ≥ 2 sessions) or a deterministic check can catch it. One pretty screenshot is not a new Hard rule.

## Agent reading map

Always load this file (Overview + Hard rules + PR checklist below). Then open **only** the matching chapter files.

| Task | Read |
| --- | --- |
| Tokens / brand hex table | [`design/tokens.md`](design/tokens.md) |
| Colors / dark mode / status | [`design/colors.md`](design/colors.md) |
| Titles, density, wrapping | [`design/typography.md`](design/typography.md), [`design/layout.md`](design/layout.md) |
| Shell, cards, dashboard rows, filters | [`design/layout.md`](design/layout.md) |
| Wizard / select height jump | [`design/layout.md`](design/layout.md) → Layout stability |
| List / master–detail / detail dialog / wizard | [`design/components.md`](design/components.md) (+ layout stability when needed) |
| Shadows, radius | [`design/surfaces.md`](design/surfaces.md) |
| Quick anti-patterns | [`design/dos-donts.md`](design/dos-donts.md) |
| Editing the visual baseline | touched chapter(s) + Hard rules / PR checklist; token table in `design/tokens.md` |

## Scope

Applies to `admin/src/pages/*`, `admin/src/components/*`, and `admin/src/components/ui/*`.
Third-party internals and code-highlight themes are out of scope for wrapper-controlled styling only.

## Hard rules

0. **Stack lock.** Admin UI is `admin/` (React + `admin/src/components/ui`). **Never** add a parallel page as raw `index.html` + inline CSS/JS, a second app, or CDN Bootstrap. Reports and one-off HTML belong outside this tree (or a documented public stylesheet), not in `admin/`.
1. No hard-coded page colors (`#hex`, `bg-violet-*`, `text-red-*`, `bg-gray-*`) in product pages.
2. No native `<select>`; use the shared `Select` component.
3. Do not invent a second primary button color. **One primary button per view.**
4. Dialogs must keep `DialogHeader` / `DialogFooter` structure. Entity **detail / edit** uses the Entity detail dialog pattern (compact `max-w-3xl` Dialog). All entity creation — including create dialogs and operational submissions (such as quota increase requests) — opens as a Dialog/Modal over the page (never flattened/tiled inline across the page or replacing the whole view). API Key create is a compact single-page Dialog (name, project, route, folded call boundaries). Route create remains a multi-step Dialog wizard (`sm:max-w-5xl`, stepper + main panel + summary sidebar). Overlay / Escape dismiss rules apply to both.
5. **Dialog viewport bounds, width stability & footer visibility**: All popups and dialogs must never exceed screen height or width (`max-h-[85vh]` or `max-h-[90vh]` with `overflow-y-auto`). Dialog containers must maintain stable, fixed responsive width (`w-[calc(100vw-2rem)] sm:w-[560px] max-w-[560px]` or `w-full max-w-2xl`) and **must never shrinkwrap or jitter in width or height** when form fields, protocols, or tabs are switched. Forms inside dialogs should be compact by default to avoid internal scrolling, with submit/cancel action buttons (`DialogFooter`) **always prominently visible in the viewport** without requiring the user to scroll down to find the buttons. Large blocks of examples, technical tokens, or secondary options inside dialogs must use collapsible accordions (or tabs) rather than vertical unconstrained stacking that pushes action buttons or headers off-screen.
6. Popups (Dialog / Sheet / AlertDialog / Popover) must close when the user clicks outside the popup content (overlay / dimmed area) or presses Escape; do not disable overlay dismiss without an explicit, documented exception.
7. Operational submissions, applications, and creation actions (e.g. Quota Increase Application, Token Rotation, Model Bindings) must be triggered via dedicated action buttons opening an interactive modal dialog with validation and cancel/submit footers, leaving the main content area focused on list review, status, and audits rather than stacking flat inline input cards.
8. Never use native browser popups (`window.confirm` / `alert` / `prompt`). Delete and other destructive confirms use `AlertDialog` / `ConfirmAlertDialog`; transient feedback uses toast.
9. Prefer `t()` for copy; reserve wrap/truncate strategy for long IDs, keys, and model names.
10. The canonical X-series brand asset uses `#2744A5`; brand identity, interactive primary, and semantic status colors remain separate roles.
11. Visible keyboard focus is mandatory, and icon-only controls require an accessible name and tooltip.
12. Entity lists and the sidebar follow quiet selection: light primary fill (`bg-primary/10`) and weight only. **One sanctioned accent:** the shared card list (`CardListItem` in `components/entity-list/EntityCardList.tsx`, used by compact-left master–detail pages) adds its 1/3-height centered primary indicator bar to the selected card. Never draw that bar anywhere else, and never use a theme-colored left border on table rows or sidebar items — no near-invisible muted grays, no parallel selection dialects.
13. Entity detail / edit dialogs follow the Entity detail dialog pattern (`ApiKeyDetailDialog` / `RouteDetailDialog`): compact overview card + optional two-column operational cards; open from `⋯` Edit by default.
14. Master–detail browsers follow the Master–detail workspace pattern: side-by-side floating panes on wide screens, Sheet/Dialog on narrow screens, never absolute overlay over the list.
15. Never join organization and project names with `/` in one control, option, cell, or chip. Show organization name or project name alone; when both are required, use separate labeled fields or columns.
16. Select / Combobox options must show only the entity display name (or other single identity). Never concatenate protocol, strategy, counts, or other metadata into option labels. Put those details outside the menu after selection.
17. Layout stability: reserve height for post-selection details and strategy-dependent panels in create wizards and dense forms. Multi-example code switchers must use **fixed-height Tab containers** with internal scrolling rather than layout-shifting toggles. Selection/tab switching must not cause visible vertical jump or dialog resize.
18. Shared overlays must provide dialog semantics, focus trap, initial focus, and focus restore.
19. **No casual subtitles.** Do not add page, card, or section subtitles that restate the title or fill space. Use a subtitle only when it carries an instruction the title cannot express. Prefer no subtitle.
20. **Surface levels.** Level 1 (on the canvas) is `bg-card` + `rounded-lg` + `shadow-card` with **no hairline border**; level 2 (inside a card or dialog) is a tonal fill (`bg-muted/30`) with no border and no shadow; controls keep their own treatment. The canvas is `bg-canvas` and is never repainted per page. Status-tinted borders are the only allowed exception on a level-1 card. See [`design/surfaces.md`](design/surfaces.md).
21. **Live data is real, or it is labelled.** Never substitute preview payloads for an empty API response: an empty gateway must look empty (zeros, `—`, empty states), not busy. First visit starts in **demo mode**; the top bar shows Exit demo on every page (the global label — do not add per-page demo badges). After the operator exits, a one-time hint points at the top-right user menu and the preference is persisted. `admin/src/lib/demo-mode.test.ts` covers the toggle; the dashboard's empty state has its own screenshot baseline.

## PR checklist

0. No new standalone HTML/JS document; Admin stays in `admin/` React.
1. Colors come from semantic tokens (`primary` / `destructive` / `success` / `warning` / `muted`); surfaces follow the level model (level 1 floats with `shadow-card`, level 2 uses tone, no hairline on cards).
2. Buttons use shared `Button` variants; **one primary per view**; no native selects; business actions are in `PageHeader`, not the top bar.
3. Dialogs follow shared structure and overlay accessibility; entity detail dialogs follow the compact Entity detail dialog pattern; overlay / Escape dismiss works; no native `confirm`/`alert`; delete uses `AlertDialog`; selected rows/sidebar use `bg-primary/10` without a theme accent bar.
4. Master–detail pages keep list and detail as independent floating panes on wide screens and fall back to Sheet/Dialog on narrow screens.
5. Long Chinese/English/key/model text does not overflow or obscure metrics.
6. Table horizontal scroll stays inside the table container.
7. Selecting routes/strategies or toggling optional sections does not jitter dialog or wizard layout (reserved slots / min-height).
8. `npm run lint` passes (this includes the design-token guard `admin/scripts/check-design-tokens.mjs`: native palette classes, hard-coded colors, `rounded-2xl/3xl`, `!important` spacing overrides, and card-surface drift such as `bg-card` + neutral border or `shadow-sm` fail the gate).
9. New cards and pages do not add decorative subtitles under titles.
10. User-visible surface change → the **Visual regression** suite is green locally (`scripts/ci_local.sh --full`), and any *intended* change of appearance is captured by regenerating the Linux baselines (`scripts/ci_local.sh --visual-update`) and committing them in the same change (never by loosening the diff threshold).

Change order when editing the visual baseline: tokens (`index.css` / Tailwind) → `components/ui/*` → pages/components → desktop + mobile screenshot regression. The last step is mechanised: `e2e/visual/visual.spec.ts` (Playwright, bundled Chromium, frozen clock) compares committed Linux baselines. CI is local on this Linux machine, so generate baselines with `scripts/ci_local.sh --visual-update` (or compare with `--full`) and commit the changed PNGs — a macOS/Windows run can never produce a matching baseline.

