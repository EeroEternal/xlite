import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

type PageContainerSize = "default" | "wide"

interface PageContainerProps {
  children: ReactNode
  className?: string
  /**
   * Console content width tier. Two tiers only — do not pass raw `max-w-[…]`
   * for console pages: `default` (1400px) for every standard page, `wide`
   * (1680px) for data-dense pages (dashboard, analytics, billing, logs and the
   * other chart/table surfaces). Reading surfaces (settings, help) may still cap
   * explicitly for line length; see docs/design/layout.md.
   */
  size?: PageContainerSize
}

const PAGE_CONTAINER_WIDTH: Record<PageContainerSize, string> = {
  default: "max-w-[1400px]",
  wide: "max-w-[1680px]",
}

export function PageContainer({ children, className, size = "default" }: PageContainerProps) {
  return (
    <div
      className={cn(
        "mx-auto w-full flex flex-col flex-1 min-h-0 h-full",
        PAGE_CONTAINER_WIDTH[size],
        className,
      )}
    >
      {children}
    </div>
  )
}
