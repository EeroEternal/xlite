# Surfaces

Elevation, depth, and shape language for Admin UI.

## Surface levels

The console is built from exactly three surface levels. Pick one; do not invent a fourth.

| Level | Where it sits | Treatment |
| --- | --- | --- |
| **Canvas** | The page background behind everything (`bg-canvas`) | Tinted neutral. Never white, never a hard shadow |
| **Level 1 — floating card** | Directly on the canvas: cards, tables, list panes, KPI tiles | `bg-card` + `rounded-lg` + `shadow-card`, **no hairline border** |
| **Level 2 — tonal group** | Inside a level-1 card or a dialog: metric tiles, option rows, labeled field rows, nested groups | `bg-muted/30` (or `bg-muted/40`), no border, no shadow |
| **Controls** | Inputs, selects, tab tracks, pills, chips, segmented switches | Keep their own hairline or tone treatment; they are not cards |

Why: on a tinted canvas a 1px border plus a white fill reads as a hard grid of boxes and fragments the page. Elevation carries "this is a card"; tone carries "this is grouped inside a card".

## Elevation & Depth

Depth is tokenized and deliberately restrained — two API-visible shadows only:

- `shadow-card` — every level-1 surface (`--shadow-card`).
- `shadow-card-hover` — interactive level-1 surfaces on hover (`--shadow-card-hover`).

Rules:

- Never invent an arbitrary shadow (`shadow-[…rgba(…)]`) or reuse a Tailwind default (`shadow-sm` / `shadow-md`) on a card surface. The guard in `admin/scripts/check-design-tokens.mjs` rejects both.
- Do not stack a second elevation inside a level-1 card: nested groups use tone.
- Dialogs may use stronger elevation than cards, but only enough to separate modal context from the page.
- Selected rows and the current sidebar route use a light primary fill (`bg-primary/10`) plus medium weight. Do not use raised surfaces, and do not add theme-colored left-edge bars or vertical accent indicators to table rows or sidebar items.
- Status-tinted borders (`border-warning/50`, `border-destructive/40`) are the one exception: when a border carries meaning rather than separation, keep it.
- Floating hints, masks, or overlays must reserve interior space and must not cover the first readable line of card content.

## Shapes

The shape language is modest and engineered. Corners are soft enough to feel modern, but not so rounded that operational tooling starts to feel playful.

- Default radius is 8px (`rounded-lg`).
- Small inner treatments may reduce to 4px or 6px when needed by nested elements.
- Pills and badges can use full rounding.
- Sharp corners and fully rounded controls should not be mixed randomly within the same screen.
- The scale is exactly `rounded` / `rounded-sm` (4px) / `rounded-md` (6px) / `rounded-lg` (8px) / `rounded-full` (`rounded-none` resets it). Anything else is the **Invented radius** anti-pattern: `rounded-xl` (12px), `rounded-2xl` / `rounded-3xl`, arbitrary values (`rounded-[10px]`) and out-of-scale directional variants (`rounded-r-xl`) are rejected by `npm run lint` (`invented-radius`), which allowlists the scale instead of blacklisting known class names.

Shape rules:

- Primary interactive controls, cards, dialogs, and inputs should all resolve through the same radius scale.
- If a component needs emphasis, do not invent a new corner language; use color, spacing, or layout instead.

## Canonical references

- Level 1: `admin/src/components/ui/card.tsx` and the list cards in `admin/src/pages/api-keys.tsx` / `providers.tsx` / `models.tsx`; KPI tiles in `admin/src/pages/overview.tsx`.
- Level 2: the enable/toggle strips in the entity dialogs (`admin/src/pages/providers.tsx`, `models.tsx`, `api-keys.tsx`) and the fact rows in `admin/src/pages/settings.tsx`.
- Tokens: [`tokens.md`](tokens.md) (`canvas`, `shadow-card`, `shadow-card-hover`).
