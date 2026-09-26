---
name: admin-ui-change
description: Change xlite Admin UI with progressive reading of docs/design.md chapters. Use for admin pages, components, dialogs, lists, selection styling, settings IA, tokens, or any user-visible admin UI work. Never load every design chapter unless editing the whole system.
---

# Admin UI change (progressive design docs)

## Rule
[`docs/design.md`](../../../docs/design.md) is the **entry** (Overview + Hard rules + PR checklist). Detail chapters live under [`docs/design/`](../../../docs/design/). **Do not** Read every chapter by default. Load the entry, then only the files the task needs.

If editing the design system itself, open only the chapters you change (plus Hard rules / PR checklist).

## Always load
1. `docs/design.md` (whole file — judgment, reader jobs, hard rules, PR checklist)
2. `docs/design/surfaces.md` (surface levels: `bg-canvas` / level-1 floating cards / level-2 tonal groups — every card, panel or shadow decision)
3. `AGENTS.md` §始终生效 → UI + Admin i18n + 列表排序与检索；细则 [`docs/ai/agents/ui-entry.md`](../../../docs/ai/agents/ui-entry.md)
4. Global settings IA: settings page is the only global-config home
5. Name the **reader job** (list / detail dialog / dashboard / settings) before choosing layout

## Route by task (chapter files)

| Task signal | Also read |
| --- | --- |
| Colors, dark mode, badges, status, brand narrative | `docs/design/colors.md` |
| Token table / hex values | `docs/design/tokens.md` |
| Page title, subtitle, density, i18n wrap | `docs/design/typography.md`, `docs/design/layout.md` (no casual subtitles) |
| Page shell, sidebar/topbar, page types, filters | `docs/design/layout.md` |
| Button / Input / Select / Switch / Tabs sizes and states | `docs/design/components.md` + `docs/design/tokens.md` |
| Select / toggle causes jump | `docs/design/layout.md` (Layout stability) |
| Shadows, selected-row depth, radius, card/panel surfaces, canvas tint | `docs/design/surfaces.md` (+ `docs/design/tokens.md` for `canvas` / `shadow-card`) |
| New list page, row actions, create entry | `docs/design/components.md` (Entity list pattern) + `docs/ai/agents/ui-entry.md` (列表排序与检索) |
| Edit/detail dialog | `docs/design/components.md` (Entity detail dialog, Overlay a11y) |
| Settings page | `docs/design/components.md` (Settings page) |
| Quick anti-patterns | `docs/design/dos-donts.md` |
| Token / CSS baseline change | `docs/design/tokens.md` + `admin` `index.css` / Tailwind; then change order in PR checklist footer |

## Stack lock
Any user-visible Admin UI loads this skill and `docs/design.md` (not only tasks that say "admin page").

1. Implement in `admin/` React + `components/ui`. **Greenfield HTML** (`*.html` + inline CSS) is a stop.
2. Title + redundant subtitle + hero is **Marketing stack** / **Casual subtitle**.
3. If you would create a file outside `admin/` for UI, you are on the wrong stack.

## Do not invent
If a chapter and an old page disagree, treat the page as drift unless the user asked to change the spec. Prefer shared primitives under `admin/src/components/ui/*` and tokens in `admin/src/index.css`. **Do not invent** a parallel palette or class vocabulary. Named anti-patterns: [`docs/design/dos-donts.md`](../../../docs/design/dos-donts.md).

Surface decisions are **not** free-form: `npm run lint` runs `admin/scripts/check-design-tokens.mjs`, which fails on native palette classes, hard-coded colors, `rounded-2xl/3xl`, `!important` spacing, and card-surface drift (`bg-card` + neutral border or `shadow-sm`). If a site legitimately needs a hairline (controls, table frames, dashed empty states, status-tinted borders), say so in the delivery note instead of weakening the guard.

## Verify
1. Re-check **PR checklist** in `docs/design.md`.
2. New/changed user-visible strings exist in both `admin/src/lib/i18n/locales/zh.ts` and `en.ts` (do not rely on `t` English fallback alone).
3. `cd admin && npm run lint`
4. Delivery note: reader job, chapters followed, and any named anti-pattern you avoided.
5. Mechanical: dialog `max-h-[85vh]`, table scroll inside its container, no layout jump on select.

## When promoting new UI lessons
Add the lesson to the matching chapter under `docs/design/` (or Hard rules in the entry). Update this routing table if a new file appears. Mirror this skill to `.agents/skills/admin-ui-change/SKILL.md`.
