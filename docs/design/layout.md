# Layout

The layout system is built around a shared shell and consistent left-edge alignment. Page framing is structural, not decorative.

## Shell

Fixed **sidebar + top bar + fluid work area**. Content width is not capped to a board size.

| Part | Size |
| --- | --- |
| Sidebar expanded | 256px (`--sidebar-width-expanded`) |
| Sidebar collapsed | 48px |
| Top bar | 56px (`h-14`) |
| Menu row | 32px tall, 8px pad, 12px icon gap, 6px radius, 14/20 type |
| Page padding | 32px sides; 32–40px top; ≥32px bottom |
| Module gap | 24px |
| Card gap | 16px |
| Control gaps | 8 / 12 / 16 |

Spacing scale (4px base): **4, 8, 12, 16, 24, 32, 40, 48**. Prefer this ladder; do not invent 10px / 18px.

**Top bar does not carry business actions.** Left: sidebar trigger. Right: theme, language, logout. Page primary actions stay in `PageHeader`.

Sidebar entries: `admin/src/components/layout/app-sidebar.tsx`. Do not invent a second menu. The sidebar lists the five scope surfaces only — see the closed-set rule in [`design.md`](../design.md#scope).

**Content width.** Two console tiers, expressed through `PageContainer size` — never as a per-page `max-w-[…]`:
`size="default"` (1400px) is what every current page uses. `size="wide"` (1680px) exists for genuinely data-dense tables, but no page uses it today; pick it deliberately rather than by habit. `PageHeader` does not carry its own cap — it inherits the container, so the page title and the content below always share one left edge.

**Canvas.** The work area behind page content is `bg-canvas` (tinted neutral), not white. Level-1 cards float on it with `shadow-card`; see [`surfaces.md`](surfaces.md). Do not repaint the canvas per page (no page-level `bg-muted/10` tints).

## Page types

Pick one. Same tokens; different structure.

| Type | Structure | Used by |
| --- | --- | --- |
| Single-column table | Toolbar (search / sort / count) → full-width table | providers, models, API keys |
| Dashboard KPI | KPI card row → optional module | overview |
| Settings | Read-only fact rows + operations | settings |

A content page is: title + page actions → optional toolbar row → main. Do not put filters in the top bar or beside the title, and do not invent a fourth page type.

- Pages use the shared composition: `PageShell`, `PageHeader`, `PageContainer`.
- The page title and the primary content block must share the same left boundary.
- Page headers belong inside the same content container as the page body, not floated into the global `SiteHeader`.
- `PageHeader` is a single compact row: page title on the left, the entity create / primary page action on the right (one row, no wrap). Do not put search, sort, or filters in the header — those belong in the toolbar under the title. **Do not render a page-level or card-level subtitle under the title**; instructional copy belongs in empty states, dialogs, or docs.
- The toolbar row sits directly under `PageHeader`: search / filters on the left, sort and result count on the right, using `EntityListToolbar`.
- Cards are the default containment primitive for tables, forms, KPI rows, and empty states.

Spacing strategy:

- Base unit is 4px, but most operational spacing lands on 8px, 16px, 24px, and 32px.
- Content grouping should feel compact but never cramped; dense pages compress vertical whitespace before compromising text readability.
- Fixed-height cards are discouraged unless the design explicitly reserves overflow handling. When a fixed-height card does manage spacing through explicit `CardHeader` / `CardContent` padding, remove the base `Card` vertical gap and padding (`gap-0 py-0`) so nested blocks do not get squeezed against the card edges.
- When a card row reserves height, sibling cards in the same grid row keep a consistent height; do not let one card become visibly taller through content volume.

Containment rules:

- High-density cards should use `overflow-hidden` and `min-h-0` to prevent clipping and broken nested scrolling.
- When tables need horizontal scrolling, the scroll must stay inside the table container and must not widen the whole page or card.
- Two-column and panel layouts should collapse vertically when width is constrained rather than squeezing text into overflow.
- If content exceeds the reserved height of a card row, the overflow should be absorbed by the card's internal scroll region rather than stretching one card beyond its peers.

### Layout stability (no jitter)

Selection, expand/collapse, and validation must not shove the rest of the form or dialog up or down. Layout shift is a defect, especially inside dialogs.

Rules:

- Reserve space for secondary details that appear after a choice (for example the provider-specific fields that appear once a provider is selected in the channel dialog). Prefer a fixed `min-h-*` slot that always mounts; fill it with real content after selection, or with a short placeholder when empty. Do not mount/unmount the block only when a value is chosen.
- Multi-example / code switchers inside dialogs and cards must use **fixed-height Tabs** with internal scrolling (`min-h-*`) rather than uncoordinated expand/collapse that resizes the dialog.
- Avoid conditional blocks that change overall dialog height on first interaction after open when those details are part of the primary path. Prefer placeholder height over `null` until the user chooses.
- Scroll containers (`overflow-y-auto` dialog bodies) must not jump scroll position when content height changes within the reserved slots.
- Validation errors may appear below actions, but should not push critical controls out of view without a reserved error strip when errors are common in that dialog.
