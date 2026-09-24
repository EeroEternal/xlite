import type { ReactNode } from "react"
import { Search } from "lucide-react"

import { Input } from "@/components/ui/input"
import { cn } from "@/lib/utils"

export interface EntityListToolbarProps {
  searchValue: string
  onSearchChange: (value: string) => void
  searchPlaceholder: string
  /**
   * Left-side filter selectors (Project, Organization, Protocol, Status, etc.)
   */
  filters?: ReactNode
  /**
   * Right-side sorting selector (e.g. Sort direction & field)
   */
  sort?: ReactNode
  /**
   * Right-side result count text or badge (e.g. "Total 12 items" / "总共 12 个")
   */
  resultCount?: ReactNode
  /**
   * Right-side auxiliary action buttons (e.g. Refresh, Export, Custom columns)
   */
  rightActions?: ReactNode
  /**
   * Backwards-compatible action slot (rendered alongside left filters)
   */
  actions?: ReactNode
  /**
   * Layout mode:
   * - "auto" / "single-row": Single line on wide viewports (left search+filters, right sort+count)
   * - "two-row": Explicit 2-row layout (Row 1: 100% width search bar; Row 2: left filters + right sort/count)
   */
  layout?: "auto" | "single-row" | "two-row"
  /**
   * Custom width class for search input in single-row mode (default: "w-64 sm:w-72 lg:w-80")
   */
  searchWidthClass?: string
  className?: string
}

export function EntityListToolbar({
  searchValue,
  onSearchChange,
  searchPlaceholder,
  filters,
  sort,
  resultCount,
  rightActions,
  actions,
  layout = "auto",
  searchWidthClass = "flex-1 min-w-[180px]",
  className,
}: EntityListToolbarProps) {
  const hasLeftFilters = Boolean(filters || actions)
  const hasRightGroup = Boolean(sort || rightActions || resultCount)

  if (layout === "two-row") {
    return (
      <div className={cn("mb-6 flex w-full flex-col gap-3", className)}>
        {/* Row 1: Full-width search bar matching the table/card below */}
        <div className="relative w-full">
          <Search
            aria-hidden="true"
            className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground"
          />
          <Input
            value={searchValue}
            onChange={(event) => onSearchChange(event.target.value)}
            placeholder={searchPlaceholder}
            aria-label={searchPlaceholder}
            className="h-8 w-full rounded-lg pl-9 text-sm"
          />
        </div>

        {/* Row 2: Left filters and right sort/count aligned to edges */}
        {(hasLeftFilters || hasRightGroup) && (
          <div className="flex flex-wrap items-center justify-between gap-2 w-full">
            <div className="flex flex-wrap items-center gap-2 min-w-0">
              {filters}
              {actions}
            </div>
            {hasRightGroup && (
              <div className="flex items-center gap-2 shrink-0 ml-auto">
                {sort}
                {rightActions}
                {resultCount ? (
                  <span className="text-xs text-muted-foreground tabular-nums">{resultCount}</span>
                ) : null}
              </div>
            )}
          </div>
        )}
      </div>
    )
  }

  return (
    <div className={cn("mb-6 flex w-full flex-wrap items-center justify-between gap-3", className)}>
      {/* Left Wing: Search input + Left Filters */}
      <div className="flex flex-wrap items-center gap-2.5 flex-1 min-w-0">
        <div className={cn("relative min-w-0", searchWidthClass)}>
          <Search
            aria-hidden="true"
            className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground"
          />
          <Input
            value={searchValue}
            onChange={(event) => onSearchChange(event.target.value)}
            placeholder={searchPlaceholder}
            aria-label={searchPlaceholder}
            className="h-8 w-full rounded-lg pl-9 text-sm"
          />
        </div>
        {filters}
        {actions}
      </div>

      {/* Right Wing: Sort + Secondary Actions + Result Count (aligned right) */}
      {hasRightGroup && (
        <div className="flex items-center gap-2.5 shrink-0 ml-auto">
          {sort}
          {rightActions}
          {resultCount ? (
            <span className="text-xs text-muted-foreground tabular-nums">{resultCount}</span>
          ) : null}
        </div>
      )}
    </div>
  )
}
