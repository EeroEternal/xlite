import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

interface PageShellProps {
  children: ReactNode
  className?: string
}

export function PageShell({ children, className }: PageShellProps) {
  return (
    <div className={cn("page-transition scrollbar-hide flex min-h-0 flex-1 flex-col p-6 xl:p-8", className)}>
      {children}
    </div>
  )
}
