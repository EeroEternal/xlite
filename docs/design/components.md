# Components

Components follow semantic roles before custom styling. Reuse baseline UI primitives and preserve their intended variants.

- Buttons: use semantic variants for primary, secondary, ghost, link, and destructive actions. Do not restyle a button into a new primary system with ad hoc classes.
- Dialogs and alert dialogs: keep the `header -> title -> description -> content -> footer` structure and align primary and secondary actions consistently.
- Dialog viewport bounds & collapsible content (global): all dialogs, sheets, and alert overlays must strictly bound their vertical and horizontal footprint within the screen (`max-h-[85vh]` with `overflow-y-auto`). Large blocks of examples, technical tokens, or long JSON/cURL blocks inside dialogs must use collapsible accordions or tabs, never unconstrained vertical stacking that pushes confirmation buttons or dialog headers off-screen.
- Overlay dismiss (global): clicking the dimmed area outside a popup (Dialog / Sheet / AlertDialog / Popover) must close it. Escape must also close it. Do not block `onOpenChange(false)` for outside clicks or Escape unless a rare, explicitly documented exception is required (for example an irreversible one-shot secret reveal that is still unfinished). Prefer the shared `Dialog` / `AlertDialog` / `Sheet` primitives — they already dismiss on overlay click; never reimplement a modal that ignores outside clicks.
- No native browser popups: never use `window.confirm`, `window.alert`, or `window.prompt` (or the bare `confirm` / `alert` / `prompt` globals) in Admin UI. Destructive actions (delete, rotate, replace) must use shared `AlertDialog` / `ConfirmAlertDialog`. Short success or error feedback uses toast; blocking confirmations use our Dialog/AlertDialog overlays so styling, i18n, and overlay-dismiss rules stay consistent.
- All entity creation and edit opens as a Dialog over the list — never a full-page form or page replacement. Every xlite form (provider, model, channel, API key) is a compact single-page Dialog; there are no multi-step wizards. Cancel / overlay click / Escape close the dialog.
- Toasts: transient feedback should use the shared top-center toast pattern instead of a blocking modal. Success feedback uses the solid `success` green surface with `success-foreground` text, keeps copy short, and disappears without interrupting the current workflow.
- Inputs, selects, and switches: favor the shared component set instead of raw HTML controls. Native `select` should not appear in product pages.
- **One primary button per view.** Height 40px, radius 6px, pad 12×8. Hover darkens primary (`#1F3A89`); active `#182F70`; disabled 50% opacity; keyboard focus is a 2px primary ring.
- Secondary button: white fill, 1px border, hover `muted`. Icon button: 28×28, 16px icon, transparent until hover.
- Form `Input`: 40px, radius 4px, 1px border. Focus = primary border + 2px ring. Error = destructive border. Disabled = muted fill.
- Toolbar `Select`: 32px, radius 8px. Panel max-height 240px, scroll inside. Show 3–5 options when possible.
- `Switch`: track 36×20, thumb 16, radius 10px, 150ms ease. On = primary track, not a new blue.
- `Tabs`: sibling content switch under the page title (2–8 items). Active = primary text + 2px primary underline. **Not** hierarchy navigation. No icon+label mix. Single selection.
- Select / Combobox option labels show **only the entity identity** (usually the display name). Do not pack status, counts, or other secondary fields into the option string with `·` / `/`. After the user picks a value, secondary facts may appear in a separate summary row, labeled field, or the detail dialog — never inside the dropdown options themselves.
- Badges: reserve for status and compact metadata; they must support longer text when localization expands labels.
- Tables: selected state belongs to `bg-primary/10` and medium-weight text. Do not use a theme-colored left border as a selection indicator.
- Cards: use them as containers for major sections, but avoid duplicating the page title as a second identical card title.

### Entity list pattern (as implemented)

Canonical reference: `admin/src/pages/api-keys.tsx`; `models.tsx` uses the same shape. New list pages must follow it instead of inventing a second row language.

Structure:

1. `PageShell` → `PageContainer` → `PageHeader`: page title on the left, one primary create action on the right.
2. `EntityListToolbar`: search input on the left (placeholder enumerates the fields actually searched); sort `Select` and the right-aligned result count on the right.
3. One level-1 `Card` (`py-0 overflow-hidden`) containing a `Table`. The empty state is a centered muted paragraph inside that card, not a separate tile.
4. Sort option labels include direction (`名称（A→Z）` / `Name (Z→A)`), never a bare field name.
5. Status column uses `Badge` (`success` for enabled, `inactive` for disabled).
6. Trailing actions cell uses inline ghost `Button size="sm"` actions (domain action / Edit / Delete). Destructive actions set a `pending*` state and confirm through `ConfirmAlertDialog`.
7. Long keys and URLs render with `font-mono text-xs`; they may wrap but must not widen the page.
8. One semantic per column — do not concatenate unrelated fields with `·`.

### Entity detail / edit dialog (as implemented)

Canonical references: the model-instance dialog in `admin/src/pages/models.tsx` and the create/edit dialogs in `admin/src/pages/api-keys.tsx`.

Shell:

1. `DialogContent className="w-full max-w-lg"` for a simple entity form; `flex max-h-[85vh] w-full max-w-3xl flex-col overflow-hidden` when the dialog carries a table.
2. `DialogHeader` holds only `DialogTitle` (entity name, plus a short suffix when the dialog manages a sub-list).
3. Body: `space-y-4`. A labeled field is `space-y-2` with `Label` + control + optional `text-meta-sm text-muted-foreground` hint.
4. Toggle rows use one level-2 strip: `flex items-center justify-between rounded-lg bg-muted/30 px-3 py-2` with `Label` and `Switch`.
5. `DialogFooter`: `Button variant="outline"` Cancel + one primary Save. When the body can overflow, put `min-h-0 flex-1 overflow-y-auto` on it so the footer stays visible without scrolling.
6. One-shot secrets render full width in `rounded-lg bg-muted/30 p-3 font-mono text-sm break-all` with a short once-only hint and copy / download actions.
7. Long bodies stay compact; do not turn the dialog into a wizard or a full page.

### Settings page (as implemented)

`admin/src/pages/settings.tsx` is the only home for global configuration. Global config must not be duplicated into other pages, the top bar, or a second nav entry.

- Read-only runtime facts render as level-2 rows (`rounded-lg bg-muted/30 px-4 py-3`) with a `text-label-sm text-muted-foreground` label and a value below.
- Operations (catalog import / export) live in their own level-1 card with a short instruction line and the action buttons.
- An import that can destroy existing data confirms through `ConfirmAlertDialog` before running.
- To add a new global capability, extend this page's information architecture first, then implement the form.

### Overlay accessibility

Shared `Dialog` / `AlertDialog` / `Sheet` must:

1. Expose `role="dialog"` and `aria-modal="true"`, and associate title/description.
2. Move focus into the overlay on open and restore focus to the trigger on close.
3. Keep Tab / Shift+Tab focus inside the overlay while open.
4. Close on Escape and overlay click unless an explicitly documented exception applies.
5. Give icon-only actions an accessible name and Tooltip.
