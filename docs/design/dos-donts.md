# Do's and Don'ts

Named **generated-design** failures (call them by name in reviews):

| Name | Looks like |
| --- | --- |
| **Rainbow accent** | Hard-coded `bg-violet-*` / second primary |
| **Selection bar** | Hand-rolled theme-colored left border on a selected row or sidebar item; the shared `CardListItem` indicator bar is the sanctioned exception |
| **Casual subtitle** | Card subtitle that restates the title |
| **Squeezed table** | Evidence table narrower than the pane |
| **Card hairline** | A 1px `border border-border` used as the only separation for a card sitting on the tinted canvas, instead of `shadow-card` on white |
| **Shadow stack** | A level-1 card (shadow) containing level-2 groups that also carry shadows instead of a tonal fill |
| **Tiled create** | Create/edit flattened inline instead of a Dialog |
| **Metadata option** | Select label `name · protocol · N keys` |
| **Slash glue** | `org/project` in one cell |
| **Layout jump** | Wizard/select inserts unreserved height |
| **Native confirm** | `window.confirm` / `alert` |
| **Invented token** | New hex or radius instead of `tokens.md` / `index.css` |
| **Invented radius** | `rounded-xl` / `rounded-2xl` / `rounded-3xl` / `rounded-[10px]` / `rounded-r-xl` instead of the 4/6/8/full scale |
| **Greenfield HTML** | New `index.html` + inline CSS/JS instead of `admin/` React |
| **Marketing stack** | Title + redundant subtitle + hero, as if this were a landing page |
| **Fifty-nine hundred** | Inventing a 50–900 color ramp instead of semantic + alpha |
| **Extra primary** | Two filled primary buttons in one view |
| **Chrome action** | Business Save/Create in the top bar |
| **Type zoo** | 24px/30px headlines or extra weights beyond 400/500/600 |
| **Tab as nav** | Tabs used as hierarchy instead of sibling content |

- Do use semantic tokens for all core surfaces, text, and states.
- Do keep page titles and primary content aligned to the same left edge.
- Do preserve visible focus treatment and accessible naming for icon-only actions.
- Do communicate status with text plus a dot or icon; color alone is never sufficient.
- Do allow long Chinese, English, key, and model text to wrap predictably.
- Do keep horizontal scrolling confined to the table region when a table truly needs it.
- Do remove duplicated in-card titles when the page header already names the entity.
- Do keep entity detail / edit in a compact Dialog following the Entity detail dialog pattern.
- Do open all entity creation (including multi-step API Key / Route wizards) as a Dialog over the list — never replace the page.
- Do dismiss Dialog / Sheet / AlertDialog / Popover when the user clicks the overlay outside the popup or presses Escape.
- Do confirm deletes and other destructive actions with `AlertDialog` / `ConfirmAlertDialog`; use toast for short non-blocking feedback.
- Don't add hard-coded page colors or create a second brand accent.
- Don't separate level-1 cards from the canvas with a hairline; use white + `shadow-card` on a tinted canvas.
- Don't nest a second elevation inside a card; use `bg-muted/*` tone.
- Don't repaint the canvas per page or reintroduce `shadow-sm` / `shadow-md` / arbitrary `shadow-[…]` on card surfaces.
- Don't rely on `truncate` without a width constraint or responsive fallback.
- Don't use a theme-colored left border / vertical accent to mark selected table rows or sidebar items; use `bg-primary/10` instead of a near-invisible muted gray, and take a card-list indicator from `CardListItem` rather than drawing your own.
- Don't hide destructive actions inside visually neutral buttons.
- Don't let modal, chart, or badge overlays cover live content.
- Don't break the shared shell just to make one page look unique.
- Don't turn entity detail editing into a full-page form or a multi-step wizard.
- Don't trap the user in a popup by ignoring outside clicks or Escape; always wire `onOpenChange` so overlay dismiss works.
- Don't use native browser popups (`confirm` / `alert` / `prompt`) for delete confirmations or any Admin UX prompt.
- Don't render `组织/项目` or `org / project` as a single label or value; keep organization and project names in separate fields.
- Don't stuff Select / Combobox options with multi-field `·` summaries (name · protocol · strategy · N keys). Option text is the name only.
- Don't let select/expand interactions insert unreserved height that jitters the dialog or wizard (see Layout stability).
- Don't add a subtitle under a page or card title unless it carries an instruction the title cannot express.
